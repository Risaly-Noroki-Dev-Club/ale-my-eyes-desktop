use crate::{AleError, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

pub use crate::context::MemoryEntry;

const MEMORY_SCHEMA_VERSION: u32 = 1;

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

    pub fn load(&mut self) -> Result<()> {
        if !self.path.exists() {
            self.save()?;
            return Ok(());
        }

        let content = std::fs::read_to_string(&self.path)?;
        if content.trim().is_empty() {
            self.memories.clear();
            return Ok(());
        }

        let file: MemoryFile = serde_json::from_str(&content)?;
        self.memories = dedupe(file.memories);
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file = MemoryFile {
            version: MEMORY_SCHEMA_VERSION,
            memories: self.memories.clone(),
        };
        let content = serde_json::to_string_pretty(&file)?;
        std::fs::write(&self.path, content)?;
        Ok(())
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

    pub fn add(&mut self, mut entry: MemoryEntry) -> Result<bool> {
        normalize_entry(&mut entry)?;
        if self
            .memories
            .iter()
            .any(|memory| same_memory(memory, &entry))
        {
            return Ok(false);
        }

        self.memories.push(entry);
        self.save()?;
        Ok(true)
    }

    pub fn delete(&mut self, id: &str) -> Result<bool> {
        let before = self.memories.len();
        self.memories.retain(|memory| memory.id != id);
        let deleted = self.memories.len() != before;
        if deleted {
            self.save()?;
        }
        Ok(deleted)
    }

    pub fn clear(&mut self) -> Result<()> {
        self.memories.clear();
        self.save()
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

        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
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

fn dedupe(memories: Vec<MemoryEntry>) -> Vec<MemoryEntry> {
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
