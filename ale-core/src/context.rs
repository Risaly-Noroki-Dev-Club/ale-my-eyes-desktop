use crate::cloud::CloudMessage;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

/// 上下文条目角色
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    System,
}

/// 上下文条目
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextEntry {
    pub role: Role,
    pub content: String,
    pub image_ref: Option<String>,
    pub token_count: usize,
}

/// 上下文管理器：对话历史 + token 预算 + 旧对话压缩。
pub struct ContextManager {
    /// 对话历史
    conversation: VecDeque<ContextEntry>,
    /// 对话摘要（压缩后的旧对话）
    conversation_summary: Option<String>,
    /// 当前 token 估算
    current_tokens: usize,
    /// 最大 token 预算
    max_tokens: usize,
    /// 会话累计消耗 token
    session_tokens_used: usize,
    /// 系统提示
    system_prompt: String,
}

impl ContextManager {
    pub fn new(max_tokens: usize) -> Self {
        Self {
            conversation: VecDeque::new(),
            conversation_summary: None,
            current_tokens: 0,
            max_tokens,
            session_tokens_used: 0,
            system_prompt: Self::default_system_prompt(),
        }
    }

    pub fn with_system_prompt(mut self, prompt: String) -> Self {
        self.system_prompt = prompt;
        self
    }

    fn default_system_prompt() -> String {
        "你是 Ale, My Eyes! 智能视觉辅助助手。你可以看到用户的摄像头画面或电脑屏幕，帮助用户理解视觉内容并执行操作。请用简洁自然的中文回答。".to_string()
    }

    /// 添加用户消息
    pub fn add_user_message(&mut self, content: String) {
        let token_count = self.estimate_tokens(&content);
        self.conversation.push_back(ContextEntry {
            role: Role::User,
            content,
            image_ref: None,
            token_count,
        });
        self.current_tokens += token_count;
        self.maybe_compact();
    }

    /// 添加助手消息
    pub fn add_assistant_message(&mut self, content: String) {
        let token_count = self.estimate_tokens(&content);
        self.conversation.push_back(ContextEntry {
            role: Role::Assistant,
            content,
            image_ref: None,
            token_count,
        });
        self.current_tokens += token_count;
        self.maybe_compact();
    }

    /// 添加带图像引用的用户消息
    pub fn add_user_message_with_image(&mut self, content: String, image_ref: String) {
        let token_count = self.estimate_tokens(&content) + 100; // 图像描述的 token 估算
        self.conversation.push_back(ContextEntry {
            role: Role::User,
            content,
            image_ref: Some(image_ref),
            token_count,
        });
        self.current_tokens += token_count;
        self.maybe_compact();
    }

    /// 构建发送给 AI 的消息列表
    pub fn build_messages(&self, current_question: &str) -> Vec<CloudMessage> {
        let mut messages = Vec::new();

        // 1. 系统提示
        messages.push(CloudMessage {
            role: "system".to_string(),
            content: self.system_prompt.clone(),
        });

        // 2. 对话摘要
        if let Some(ref summary) = self.conversation_summary {
            messages.push(CloudMessage {
                role: "system".to_string(),
                content: format!("之前的对话摘要：{}", summary),
            });
        }

        // 3. 最近对话
        for entry in self.conversation.iter().rev().take(10).rev() {
            let role = match entry.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::System => "system",
            };
            messages.push(CloudMessage {
                role: role.to_string(),
                content: entry.content.clone(),
            });
        }

        // 4. 当前问题
        messages.push(CloudMessage {
            role: "user".to_string(),
            content: current_question.to_string(),
        });

        messages
    }

    /// 压缩对话（当 token 接近上限时）
    fn maybe_compact(&mut self) {
        if self.current_tokens < self.max_tokens * 70 / 100 {
            return;
        }

        // 保留最近 6 条消息，压缩更早的对话
        let keep = 6usize;
        let compress_count = self.conversation.len().saturating_sub(keep);
        if compress_count < 3 {
            return;
        }

        let mut summary_parts = Vec::new();
        for _ in 0..compress_count {
            if let Some(entry) = self.conversation.pop_front() {
                self.current_tokens = self.current_tokens.saturating_sub(entry.token_count);
                if entry.role == Role::User || entry.role == Role::Assistant {
                    summary_parts.push(format!(
                        "[{}]: {}",
                        match entry.role {
                            Role::User => "用户",
                            Role::Assistant => "助手",
                            _ => "系统",
                        },
                        // 截断过长的内容
                        if entry.content.chars().count() > 100 {
                            format!("{}...", entry.content.chars().take(100).collect::<String>())
                        } else {
                            entry.content
                        }
                    ));
                }
            }
        }

        let new_summary = if let Some(existing) = &self.conversation_summary {
            format!("{}\n{}", existing, summary_parts.join("\n"))
        } else {
            summary_parts.join("\n")
        };

        // 进一步压缩：如果摘要也太长，只保留最后部分
        self.conversation_summary = Some(if new_summary.chars().count() > 1500 {
            format!(
                "...{}",
                new_summary
                    .chars()
                    .rev()
                    .take(1500)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<String>()
            )
        } else {
            new_summary
        });
    }

    /// 累加 API 调用消耗的 token
    pub fn add_tokens(&mut self, tokens: usize) {
        self.session_tokens_used += tokens;
    }

    /// 获取会话累计 token 消耗
    pub fn session_tokens(&self) -> usize {
        self.session_tokens_used
    }

    /// 简单的 token 估算（1 中文字符 ≈ 2 tokens，1 英文单词 ≈ 1 token）
    fn estimate_tokens(&self, text: &str) -> usize {
        let chinese_chars = text.chars().filter(|c| *c as u32 > 0x4E00).count();
        let other_chars = text.len() - chinese_chars;
        chinese_chars * 2 + other_chars / 4
    }

    /// 清空上下文
    pub fn clear(&mut self) {
        self.conversation.clear();
        self.conversation_summary = None;
        self.current_tokens = 0;
        self.session_tokens_used = 0;
    }

    /// 获取当前对话轮数
    pub fn conversation_turns(&self) -> usize {
        self.conversation.len()
    }

    /// 获取当前 token 估算
    pub fn estimated_tokens(&self) -> usize {
        self.current_tokens
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_context_manager_basic() {
        let mut ctx = ContextManager::new(4000);
        ctx.add_user_message("你好".to_string());
        ctx.add_assistant_message("你好！有什么可以帮助你的？".to_string());

        assert_eq!(ctx.conversation_turns(), 2);
        assert!(ctx.estimated_tokens() > 0);
    }

    #[test]
    fn test_build_messages() {
        let mut ctx = ContextManager::new(4000);
        ctx.add_user_message("这是什么？".to_string());
        ctx.add_assistant_message("这是一只猫。".to_string());

        let messages = ctx.build_messages("它在做什么？");
        assert!(messages.len() >= 3); // system + history + current
        assert!(messages.last().unwrap().content.contains("它在做什么？"));
    }

    #[test]
    fn test_compact() {
        let mut ctx = ContextManager::new(100); // 很小的 token 预算
        for i in 0..20 {
            ctx.add_user_message(format!("消息 {}", i));
            ctx.add_assistant_message(format!("回复 {}", i));
        }

        // 应该触发压缩
        assert!(ctx.conversation_summary.is_some());
        assert!(ctx.conversation_turns() < 40);
    }

    #[test]
    fn test_compact_handles_multibyte_text() {
        let mut ctx = ContextManager::new(10);
        for _ in 0..10 {
            ctx.add_user_message("这是用于验证摘要截断不会破坏 UTF-8 边界的中文消息。".repeat(10));
        }
        assert!(ctx.conversation_summary.is_some());
    }

    #[test]
    fn test_clear() {
        let mut ctx = ContextManager::new(4000);
        ctx.add_user_message("test".to_string());

        ctx.clear();
        assert_eq!(ctx.conversation_turns(), 0);
        assert_eq!(ctx.estimated_tokens(), 0);
    }
}
