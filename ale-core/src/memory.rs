use crate::{AleError, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub use crate::context::MemoryEntry;

const MEMORY_SCHEMA_VERSION: u32 = 1;
pub const MAX_MEMORY_FILE_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemoryFile {
    version: u32,
    memories: Vec<MemoryEntry>,
}

impl Default for MemoryFile {
    fn default() -> Self {
        Self {
            version: MEMORY_SCHEMA_VERSION,
            memories: Vec::new(),
        }
    }
}

/// Local persistent memory store inspired by claude-mem's durable observations.
///
/// The first implementation uses JSON so desktop and Android builds stay simple.
/// The API is intentionally storage-agnostic so it can be backed by SQLite/FTS later.
pub struct MemoryStore {
    path: PathBuf,
    memories: Vec<MemoryEntry>,
    available: bool,
}

/// 从一次交互中提取候选长期记忆。
///
/// 规则优先提取稳定偏好、设备/环境事实以及显式要求记住的信息。
pub fn extract_memories(question: &str, answer: &str) -> Vec<MemoryEntry> {
    let question = normalize_text(question);
    let answer = normalize_text(answer);
    let mut memories = Vec::new();

    memories.extend(extract_explicit_preference(&question));
    memories.extend(extract_environment_memory(&question));
    memories.extend(extract_accessibility_memory(&question));
    memories.extend(extract_explicit_remember_request(&question));

    if memories.is_empty() {
        memories.extend(extract_soft_signals(&question, &answer));
    }

    dedupe_candidates(memories)
}

impl MemoryStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            memories: Vec::new(),
            available: true,
        }
    }

    pub fn default_path() -> PathBuf {
        dirs::data_dir()
            .or_else(dirs::config_dir)
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ale-my-eyes")
            .join("memory.json")
    }

    pub fn load_or_create(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let mut store = Self::new(path);
        store.load()?;
        Ok(store)
    }

    /// A damaged/oversized store must never prevent startup or be overwritten by an empty store.
    pub fn load_preserving(path: impl Into<PathBuf>) -> Self {
        let mut store = Self::new(path);
        if store.load().is_err() {
            store.available = false;
            crate::diagnostics::record("memory_disabled_preserve_original", &[]);
            tracing::warn!(
                stage = "memory_load",
                result = "disabled_preserve_original",
                "Memory unavailable; original file preserved"
            );
        }
        store
    }

    pub fn available(&self) -> bool {
        self.available
    }

    fn require_available(&self) -> Result<()> {
        if !self.available {
            return Err(AleError::ConfigError("Memory is disabled because its file could not be loaded; preserve and repair the original file before retrying".into()));
        }
        Ok(())
    }

    pub fn load(&mut self) -> Result<()> {
        self.require_available()?;
        if !self.path.exists() {
            self.save()?;
            return Ok(());
        }
        let file = std::fs::File::open(&self.path)?;
        if file.metadata()?.len() > MAX_MEMORY_FILE_BYTES {
            return Err(AleError::ConfigError(
                "Memory file exceeds 32 MiB; original file preserved".into(),
            ));
        }
        let mut content = String::new();
        file.take(MAX_MEMORY_FILE_BYTES + 1)
            .read_to_string(&mut content)?;
        if content.len() as u64 > MAX_MEMORY_FILE_BYTES {
            return Err(AleError::ConfigError(
                "Memory file exceeds 32 MiB; original file preserved".into(),
            ));
        }
        let memories = if content.trim().is_empty() {
            Vec::new()
        } else {
            let file: MemoryFile = serde_json::from_str(&content)?;
            if file.version != MEMORY_SCHEMA_VERSION {
                return Err(AleError::ConfigError(
                    "Unsupported memory schema; original file preserved".into(),
                ));
            }
            let mut memories = file.memories;
            for memory in &mut memories {
                normalize_entry(memory)?;
            }
            memories
        };
        self.memories = memories;
        Ok(())
    }

    fn save_entries(&self, memories: &[MemoryEntry]) -> Result<()> {
        self.require_available()?;
        #[derive(Serialize)]
        struct FileRef<'a> {
            version: u32,
            memories: &'a [MemoryEntry],
        }
        struct LimitedBuffer(Vec<u8>);
        impl Write for LimitedBuffer {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.0.len().saturating_add(bytes.len()) as u64 > MAX_MEMORY_FILE_BYTES {
                    return Err(std::io::Error::other(
                        "Memory file would exceed 32 MiB; existing memories preserved",
                    ));
                }
                self.0.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut output = LimitedBuffer(Vec::new());
        serde_json::to_writer_pretty(
            &mut output,
            &FileRef {
                version: MEMORY_SCHEMA_VERSION,
                memories,
            },
        )?;
        crate::config::atomic_config_write(&self.path, &output.0)
    }

    pub fn save(&self) -> Result<()> {
        self.save_entries(&self.memories)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn memories(&self) -> &[MemoryEntry] {
        &self.memories
    }

    pub fn into_memories(self) -> Vec<MemoryEntry> {
        self.memories
    }

    pub fn add(&mut self, entry: MemoryEntry) -> Result<bool> {
        Ok(self.add_many(vec![entry])? > 0)
    }

    /// One atomic commit for a whole interaction; failure leaves memory and disk unchanged.
    pub fn add_many(&mut self, entries: Vec<MemoryEntry>) -> Result<usize> {
        self.require_available()?;
        let mut next = self.memories.clone();
        let mut added = 0;
        for mut entry in entries {
            normalize_entry(&mut entry)?;
            if !next.iter().any(|old| same_memory(old, &entry)) {
                next.push(entry);
                added += 1;
            }
        }
        if added > 0 {
            self.save_entries(&next)?;
            self.memories = next;
        }
        Ok(added)
    }

    pub fn delete(&mut self, id: &str) -> Result<bool> {
        self.require_available()?;
        let next: Vec<_> = self
            .memories
            .iter()
            .filter(|m| m.id != id)
            .cloned()
            .collect();
        if next.len() == self.memories.len() {
            return Ok(false);
        }
        self.save_entries(&next)?;
        self.memories = next;
        Ok(true)
    }

    pub fn clear(&mut self) -> Result<()> {
        self.save_entries(&[])?;
        self.memories.clear();
        Ok(())
    }

    pub fn search(&self, query: &str, limit: usize) -> Vec<&MemoryEntry> {
        let terms = extract_terms(query);
        let mut scored = self
            .memories
            .iter()
            .map(|memory| {
                let searchable = format!(
                    "{} {} {}",
                    memory.content,
                    memory.source,
                    memory.tags.join(" ")
                )
                .to_lowercase();
                let term_score = terms
                    .iter()
                    .filter(|term| searchable.contains(term.as_str()))
                    .count() as f32;
                (memory, term_score * 2.0 + memory.importance)
            })
            .collect::<Vec<_>>();

        let keep = limit.min(scored.len());
        if keep < scored.len() {
            scored.select_nth_unstable_by(keep, |a, b| b.1.total_cmp(&a.1));
            scored.truncate(keep);
        }
        scored.sort_by(|a, b| b.1.total_cmp(&a.1));
        scored
            .into_iter()
            .filter(|(_, score)| *score > 0.0)
            .take(limit)
            .map(|(memory, _)| memory)
            .collect()
    }
}

fn normalize_entry(entry: &mut MemoryEntry) -> Result<()> {
    entry.content = entry.content.trim().to_string();
    entry.source = entry.source.trim().to_string();
    if !entry.importance.is_finite() {
        return Err(AleError::ConfigError(
            "Memory importance must be finite".into(),
        ));
    }
    entry.importance = entry.importance.clamp(0.0, 1.0);
    entry.tags = entry
        .tags
        .iter()
        .map(|tag| tag.trim().to_lowercase())
        .filter(|tag| !tag.is_empty())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    if entry.content.is_empty() {
        return Err(AleError::ConfigError(
            "memory content cannot be empty".to_string(),
        ));
    }
    if entry.source.is_empty() {
        entry.source = "unknown".to_string();
    }
    if entry.created_at.timestamp() == 0 {
        entry.created_at = Utc::now();
    }

    Ok(())
}

fn normalize_text(text: &str) -> String {
    text.trim().replace(['\n', '\r', '\t'], " ")
}

fn extract_explicit_preference(question: &str) -> Vec<MemoryEntry> {
    let mut memories = Vec::new();

    if question.contains("简洁") {
        memories.push(preference_memory(
            "用户偏好：简洁回答",
            vec!["偏好", "表达"],
            0.85,
        ));
    }
    if question.contains("详细") || question.contains("更详细") {
        memories.push(preference_memory(
            "用户偏好：回答更详细",
            vec!["偏好", "表达"],
            0.85,
        ));
    }
    if question.contains("中文") {
        memories.push(preference_memory(
            "用户偏好：使用中文回复",
            vec!["偏好", "语言"],
            0.9,
        ));
    }
    if question.contains("英文") {
        memories.push(preference_memory(
            "用户偏好：使用英文回复",
            vec!["偏好", "语言"],
            0.9,
        ));
    }
    if question.contains("语速") && question.contains("慢") {
        memories.push(preference_memory(
            "用户偏好：语速慢一点",
            vec!["偏好", "语音"],
            0.85,
        ));
    }
    if question.contains("语速") && question.contains("快") {
        memories.push(preference_memory(
            "用户偏好：语速快一点",
            vec!["偏好", "语音"],
            0.85,
        ));
    }

    for marker in [
        "我喜欢",
        "我偏好",
        "我习惯",
        "我希望",
        "我想要",
        "请用",
        "尽量",
    ] {
        if let Some(tail) = extract_tail(question, marker) {
            if let Some(content) = build_preference_from_tail(tail) {
                memories.push(preference_memory(content, vec!["偏好"], 0.8));
            }
        }
    }

    memories
}

fn extract_environment_memory(question: &str) -> Vec<MemoryEntry> {
    let mut memories = Vec::new();
    let lower = question.to_lowercase();

    if lower.contains("firefox") {
        memories.push(preference_memory(
            "用户常用 Firefox 浏览网页",
            vec!["环境", "应用"],
            0.7,
        ));
    }
    if lower.contains("chrome") {
        memories.push(preference_memory(
            "用户常用 Chrome 浏览网页",
            vec!["环境", "应用"],
            0.7,
        ));
    }
    if lower.contains("windows") {
        memories.push(preference_memory(
            "用户主要使用 Windows 设备",
            vec!["环境", "设备"],
            0.75,
        ));
    }
    if lower.contains("mac") || lower.contains("macos") || lower.contains("os x") {
        memories.push(preference_memory(
            "用户主要使用 macOS 设备",
            vec!["环境", "设备"],
            0.75,
        ));
    }
    if lower.contains("linux") {
        memories.push(preference_memory(
            "用户主要使用 Linux 设备",
            vec!["环境", "设备"],
            0.75,
        ));
    }
    if lower.contains("android") {
        memories.push(preference_memory(
            "用户主要使用 Android 设备",
            vec!["环境", "设备"],
            0.75,
        ));
    }
    if lower.contains("iphone") || lower.contains("ios") {
        memories.push(preference_memory(
            "用户主要使用 iPhone / iOS 设备",
            vec!["环境", "设备"],
            0.75,
        ));
    }

    memories
}

fn extract_accessibility_memory(question: &str) -> Vec<MemoryEntry> {
    let mut memories = Vec::new();
    let lower = question.to_lowercase();

    if lower.contains("无障碍") || lower.contains("屏幕阅读器") || lower.contains("辅助")
    {
        memories.push(preference_memory(
            "用户需要无障碍辅助支持",
            vec!["无障碍", "辅助"],
            0.9,
        ));
    }

    if lower.contains("屏幕") && lower.contains("阅读") {
        memories.push(preference_memory(
            "用户关注屏幕阅读和视觉辅助",
            vec!["无障碍", "视觉"],
            0.85,
        ));
    }

    memories
}

fn extract_explicit_remember_request(question: &str) -> Vec<MemoryEntry> {
    let lower = question.to_lowercase();
    if !(lower.contains("记住")
        || lower.contains("以后")
        || lower.contains("下次")
        || lower.contains("保存"))
    {
        return Vec::new();
    }

    let mut memories = Vec::new();
    for marker in [
        "我喜欢",
        "我偏好",
        "我习惯",
        "我希望",
        "我想要",
        "请用",
        "尽量",
    ] {
        if let Some(tail) = extract_tail(question, marker) {
            if let Some(content) = build_preference_from_tail(tail) {
                memories.push(preference_memory(content, vec!["显式记忆", "偏好"], 0.95));
            }
        }
    }

    if memories.is_empty() {
        memories.push(preference_memory(
            format!("用户明确要求记住：{}", summarize_text(question, 36)),
            vec!["显式记忆"],
            0.95,
        ));
    }

    memories
}

fn extract_soft_signals(question: &str, answer: &str) -> Vec<MemoryEntry> {
    let mut memories = Vec::new();
    let combined = format!("{} {}", question, answer);
    let lower = combined.to_lowercase();

    if lower.contains("需要") && lower.contains("辅助") {
        memories.push(preference_memory("用户需要辅助支持", vec!["需求"], 0.8));
    }

    if lower.contains("回答") && lower.contains("简洁") {
        memories.push(preference_memory(
            "用户偏好简洁回答",
            vec!["偏好", "表达"],
            0.8,
        ));
    }

    memories
}

fn build_preference_from_tail(tail: &str) -> Option<String> {
    let tail = trim_tail(tail);
    if tail.is_empty() {
        return None;
    }

    if tail.contains("中文") {
        return Some("用户偏好：使用中文回复".to_string());
    }
    if tail.contains("英文") {
        return Some("用户偏好：使用英文回复".to_string());
    }
    if tail.contains("简洁") || tail.contains("简单") {
        return Some("用户偏好：简洁回答".to_string());
    }
    if tail.contains("详细") {
        return Some("用户偏好：回答更详细".to_string());
    }
    if tail.contains("慢") && tail.contains("语速") {
        return Some("用户偏好：语速慢一点".to_string());
    }
    if tail.contains("快") && tail.contains("语速") {
        return Some("用户偏好：语速快一点".to_string());
    }
    if tail.contains("无障碍") || tail.contains("辅助") {
        return Some("用户需要无障碍辅助支持".to_string());
    }

    let cleaned = summarize_text(&tail, 32);
    if cleaned.is_empty() {
        None
    } else {
        Some(format!("用户偏好：{}", cleaned))
    }
}

fn preference_memory(content: impl Into<String>, tags: Vec<&str>, importance: f32) -> MemoryEntry {
    let mut entry = MemoryEntry::new(content.into(), importance, "auto-extract".to_string());
    entry.tags = tags.into_iter().map(|tag| tag.to_string()).collect();
    entry
}

fn extract_tail<'a>(text: &'a str, marker: &str) -> Option<&'a str> {
    text.find(marker).map(|index| &text[index + marker.len()..])
}

fn trim_tail(text: &str) -> String {
    let text = text
        .trim_start_matches(|c: char| {
            matches!(c, ':' | '：' | ' ' | '，' | ',' | '。' | ';' | '；')
        })
        .trim();

    let mut end = text.len();
    for separator in [
        "，", ",", "。", ";", "；", "但", "不过", "另外", "并且", "以及",
    ] {
        if let Some(idx) = text.find(separator) {
            end = end.min(idx);
        }
    }
    text[..end].trim().to_string()
}

fn summarize_text(text: &str, max_chars: usize) -> String {
    let trimmed = trim_tail(text);
    trimmed
        .chars()
        .take(max_chars)
        .collect::<String>()
        .trim()
        .to_string()
}

fn dedupe_candidates(memories: Vec<MemoryEntry>) -> Vec<MemoryEntry> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();

    for mut memory in memories {
        if normalize_entry(&mut memory).is_err() {
            continue;
        }
        let key = memory.content.to_lowercase();
        if seen.insert(key) {
            deduped.push(memory);
        }
    }

    deduped
}

fn same_memory(left: &MemoryEntry, right: &MemoryEntry) -> bool {
    left.content.eq_ignore_ascii_case(&right.content)
}

fn extract_terms(text: &str) -> HashSet<String> {
    let mut terms = text
        .split(|c: char| !c.is_alphanumeric() && !is_cjk(c))
        .map(|term| term.trim().to_lowercase())
        .filter(|term| term.chars().count() >= 2)
        .collect::<HashSet<_>>();

    let cjk_chars = text.chars().filter(|c| is_cjk(*c)).collect::<Vec<_>>();
    for window in cjk_chars.windows(2) {
        terms.insert(window.iter().collect());
    }

    terms
}

fn is_cjk(c: char) -> bool {
    matches!(
        c as u32,
        0x4E00..=0x9FFF | 0x3400..=0x4DBF | 0x3040..=0x30FF | 0xAC00..=0xD7AF
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_path(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ale-memory-test-{}-{}.json",
            name,
            uuid::Uuid::new_v4()
        ));
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        path
    }

    #[test]
    fn oversized_or_invalid_memory_is_disabled_without_modifying_source() {
        let path = test_path("oversize");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(MAX_MEMORY_FILE_BYTES + 1).unwrap();
        drop(file);
        let mut store = MemoryStore::load_preserving(&path);
        assert!(!store.available());
        assert!(store
            .add(MemoryEntry::new("new".into(), 0.5, "test".into()))
            .is_err());
        assert!(store.clear().is_err());
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            MAX_MEMORY_FILE_BYTES + 1
        );
        std::fs::write(&path, b"{invalid").unwrap();
        let store = MemoryStore::load_preserving(&path);
        assert!(!store.available());
        assert_eq!(std::fs::read(&path).unwrap(), b"{invalid");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn failed_memory_commit_does_not_change_in_memory_state() {
        let path = test_path("failed-commit");
        let mut store = MemoryStore::load_or_create(&path).unwrap();
        store
            .add(MemoryEntry::new("old".into(), 0.5, "test".into()))
            .unwrap();
        let original = std::fs::read(&path).unwrap();
        let blocked = path.with_extension("directory");
        std::fs::create_dir(&blocked).unwrap();
        store.path = blocked.clone();
        assert!(store
            .add(MemoryEntry::new("new".into(), 0.5, "test".into()))
            .is_err());
        assert!(store.clear().is_err());
        assert_eq!(store.memories().len(), 1);
        assert_eq!(store.memories()[0].content, "old");
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_dir(blocked).unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn oversized_addition_preserves_existing_memories_and_file() {
        let path = test_path("oversized-addition");
        let mut store = MemoryStore::load_or_create(&path).unwrap();
        store
            .add(MemoryEntry::new("old".into(), 0.5, "test".into()))
            .unwrap();
        let original = std::fs::read(&path).unwrap();
        assert!(store
            .add(MemoryEntry::new(
                "x".repeat(MAX_MEMORY_FILE_BYTES as usize),
                0.5,
                "test".into()
            ))
            .is_err());
        assert_eq!(store.memories().len(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn batch_commit_preserves_all_unique_entries() {
        let path = test_path("batch");
        let mut store = MemoryStore::load_or_create(&path).unwrap();
        let entries = ["one", "two", "one"]
            .into_iter()
            .map(|s| MemoryEntry::new(s.into(), 0.5, "test".into()))
            .collect();
        assert_eq!(store.add_many(entries).unwrap(), 2);
        let loaded = MemoryStore::load_or_create(&path).unwrap();
        assert_eq!(loaded.memories().len(), 2);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn persists_and_loads_memories() {
        let path = test_path("persist");
        let mut store = MemoryStore::load_or_create(&path).unwrap();
        let mut entry = MemoryEntry::new(
            "用户喜欢简洁中文回答".to_string(),
            0.8,
            "conversation".to_string(),
        );
        entry.tags = vec!["preference".to_string(), "中文".to_string()];

        assert!(store.add(entry).unwrap());

        let loaded = MemoryStore::load_or_create(&path).unwrap();
        assert_eq!(loaded.memories().len(), 1);
        assert_eq!(loaded.memories()[0].content, "用户喜欢简洁中文回答");

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn rejects_duplicate_content() {
        let path = test_path("dedupe");
        let mut store = MemoryStore::load_or_create(&path).unwrap();

        assert!(store
            .add(MemoryEntry::new(
                "用户需要屏幕阅读辅助".to_string(),
                0.7,
                "conversation".to_string(),
            ))
            .unwrap());
        assert!(!store
            .add(MemoryEntry::new(
                "用户需要屏幕阅读辅助".to_string(),
                0.9,
                "conversation".to_string(),
            ))
            .unwrap());
        assert_eq!(store.memories().len(), 1);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn search_ranks_relevant_memory() {
        let path = test_path("search");
        let mut store = MemoryStore::load_or_create(&path).unwrap();

        store
            .add(MemoryEntry::new(
                "用户在电脑上使用 Firefox 浏览网页".to_string(),
                0.4,
                "screen".to_string(),
            ))
            .unwrap();
        store
            .add(MemoryEntry::new(
                "用户偏好音频回答速度慢一点".to_string(),
                0.9,
                "audio".to_string(),
            ))
            .unwrap();

        let results = store.search("浏览器网页", 1);
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("Firefox"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn extracts_explicit_preferences() {
        let memories =
            extract_memories("请记住我喜欢简洁中文回答，语速慢一点。", "好的，我会记住。");

        let contents = memories
            .iter()
            .map(|m| m.content.as_str())
            .collect::<Vec<_>>();
        assert!(contents.iter().any(|c| c.contains("简洁回答")));
        assert!(contents.iter().any(|c| c.contains("中文回复")));
        assert!(contents.iter().any(|c| c.contains("语速慢一点")));
    }

    #[test]
    fn extracts_accessibility_need() {
        let memories = extract_memories("我需要无障碍辅助支持。", "明白。");
        assert!(memories
            .iter()
            .any(|m| m.content.contains("无障碍辅助支持")));
    }

    #[test]
    fn extracts_explicit_remember_request() {
        let memories =
            extract_memories("以后请记住我常用 Firefox 浏览网页。", "我会记住这个偏好。");
        assert!(memories.iter().any(|m| m.content.contains("Firefox")));
    }
}
