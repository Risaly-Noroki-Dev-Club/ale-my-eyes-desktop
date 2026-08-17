use crate::{AleError, Result};
use serde::{Deserialize, Serialize};

const MAX_ACTIONS: usize = 20;
const MAX_TYPED_TEXT_CHARS: usize = 2_000;
const MAX_WAIT_MS: u64 = 30_000;

/// 操作风险等级
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// 低风险：滚动、移动鼠标、查看内容
    Low,
    /// 中风险：点击、打字、快捷键
    Medium,
    /// 高风险：删除文件、关闭应用、系统设置修改
    High,
}

/// 自动化操作指令
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Action {
    /// 鼠标点击
    Click { x: f64, y: f64, button: MouseButton },
    /// 鼠标双击
    DoubleClick { x: f64, y: f64 },
    /// 鼠标移动
    MouseMove { x: f64, y: f64 },
    /// 滚动
    Scroll {
        x: f64,
        y: f64,
        delta_x: f64,
        delta_y: f64,
    },
    /// 输入文本
    Type { text: String },
    /// 按键
    Key { key: String, modifiers: Vec<String> },
    /// 等待
    Wait { ms: u64 },
    /// 打开应用
    OpenApp { name: String },
    /// 关闭应用
    CloseApp { name: String },
    /// 打开 URL
    OpenUrl { url: String },
    /// 文件操作
    FileOperation {
        operation: FileOp,
        path: String,
        target: Option<String>,
    },
}

/// 鼠标按键
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

/// 文件操作类型
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileOp {
    Create,
    Delete,
    Move,
    Copy,
    Rename,
}

/// 操作计划：一组有序的操作指令
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActionPlan {
    /// 操作列表（按执行顺序）
    pub actions: Vec<Action>,
    /// 整体风险等级
    pub risk_level: RiskLevel,
    /// AI 对操作的解释说明
    pub explanation: String,
    /// 是否需要用户确认
    pub requires_confirmation: bool,
}

impl Action {
    /// 获取单个操作的风险等级
    pub fn risk_level(&self) -> RiskLevel {
        match self {
            Action::MouseMove { .. } | Action::Scroll { .. } | Action::Wait { .. } => {
                RiskLevel::Low
            }
            Action::Click { .. }
            | Action::DoubleClick { .. }
            | Action::Type { .. }
            | Action::Key { .. }
            | Action::OpenApp { .. }
            | Action::OpenUrl { .. } => RiskLevel::Medium,
            Action::CloseApp { .. } | Action::FileOperation { .. } => RiskLevel::High,
        }
    }

    /// 获取操作的自然语言描述（用于 TTS 播报）
    pub fn describe(&self) -> String {
        match self {
            Action::Click { x, y, button } => {
                format!("在坐标 ({}, {}) 处{:?}键点击", x, y, button)
            }
            Action::DoubleClick { x, y } => {
                format!("在坐标 ({}, {}) 处双击", x, y)
            }
            Action::MouseMove { x, y } => {
                format!("移动鼠标到 ({}, {})", x, y)
            }
            Action::Scroll { delta_y, .. } => {
                if *delta_y > 0.0 {
                    "向下滚动".to_string()
                } else {
                    "向上滚动".to_string()
                }
            }
            Action::Type { text } => {
                format!("输入文字（{} 个字符）", text.chars().count())
            }
            Action::Key { key, modifiers } => {
                if modifiers.is_empty() {
                    format!("按下 {} 键", key)
                } else {
                    format!("按下 {}+{}", modifiers.join("+"), key)
                }
            }
            Action::Wait { ms } => {
                format!("等待 {} 毫秒", ms)
            }
            Action::OpenApp { name } => {
                format!("打开应用 {}", name)
            }
            Action::CloseApp { name } => {
                format!("关闭应用 {}", name)
            }
            Action::OpenUrl { url } => {
                format!("打开网址 {}", url)
            }
            Action::FileOperation {
                operation, path, ..
            } => {
                let op_str = match operation {
                    FileOp::Create => "创建",
                    FileOp::Delete => "删除",
                    FileOp::Move => "移动",
                    FileOp::Copy => "复制",
                    FileOp::Rename => "重命名",
                };
                format!("{}文件 {}", op_str, path)
            }
        }
    }
}

impl ActionPlan {
    /// 创建新的操作计划
    pub fn new(explanation: String) -> Self {
        Self {
            actions: Vec::new(),
            risk_level: RiskLevel::Low,
            explanation,
            requires_confirmation: false,
        }
    }

    /// 添加操作并更新风险等级
    pub fn add_action(&mut self, action: Action) {
        let risk = action.risk_level();
        if risk > self.risk_level {
            self.risk_level = risk;
        }
        self.requires_confirmation = self.risk_level >= RiskLevel::Medium;
        self.actions.push(action);
    }

    /// 根据实际动作重新计算风险，避免信任外部模型返回的风险字段。
    pub fn recompute_risk(&mut self) {
        self.risk_level = self
            .actions
            .iter()
            .map(Action::risk_level)
            .max()
            .unwrap_or(RiskLevel::Low);
        self.requires_confirmation = self.risk_level >= RiskLevel::Medium;
    }

    /// Validate untrusted model output before it is presented or executed.
    pub fn validate(&self) -> Result<()> {
        if self.actions.is_empty() {
            return Err(AleError::ConfigError("自动化计划不能为空".to_string()));
        }
        if self.actions.len() > MAX_ACTIONS {
            return Err(AleError::ConfigError(format!(
                "自动化计划最多允许 {MAX_ACTIONS} 步"
            )));
        }
        for action in &self.actions {
            match action {
                Action::Click { x, y, .. }
                | Action::DoubleClick { x, y }
                | Action::MouseMove { x, y }
                | Action::Scroll { x, y, .. }
                    if !x.is_finite() || !y.is_finite() =>
                {
                    return Err(AleError::ConfigError("自动化坐标必须是有限数".to_string()));
                }
                Action::Scroll {
                    delta_x, delta_y, ..
                } if !delta_x.is_finite() || !delta_y.is_finite() => {
                    return Err(AleError::ConfigError("滚动距离必须是有限数".to_string()));
                }
                Action::Type { text } if text.chars().count() > MAX_TYPED_TEXT_CHARS => {
                    return Err(AleError::ConfigError(format!(
                        "单次输入最多允许 {MAX_TYPED_TEXT_CHARS} 个字符"
                    )));
                }
                Action::Wait { ms } if *ms > MAX_WAIT_MS => {
                    return Err(AleError::ConfigError(format!(
                        "单次等待最多允许 {MAX_WAIT_MS} 毫秒"
                    )));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// 获取所有操作的描述列表
    pub fn describe_steps(&self) -> Vec<String> {
        self.actions.iter().map(|a| a.describe()).collect()
    }

    /// 获取完整的语音播报文本
    pub fn speak_text(&self) -> String {
        let mut text = self.explanation.clone();
        if self.requires_confirmation {
            text.push_str("\n这个操作");
            match self.risk_level {
                RiskLevel::High => text.push_str("风险较高"),
                RiskLevel::Medium => text.push_str("需要确认"),
                _ => {}
            }
            text.push_str("，请确认是否执行。");
        }
        text
    }
}

/// 从 AI 响应 JSON 解析操作计划
pub fn parse_action_plan(json: &str) -> std::result::Result<ActionPlan, serde_json::Error> {
    let mut plan: ActionPlan = serde_json::from_str(json)?;
    plan.recompute_risk();
    plan.validate()
        .map_err(|error| serde_json::Error::io(std::io::Error::other(error.to_string())))?;
    Ok(plan)
}

/// 从 OpenAI-compatible function call arguments 解析操作计划。
///
/// 兼容两种常见形态：完整 ActionPlan，或 `{ "plan": ActionPlan }` 包装对象。
pub fn parse_action_plan_arguments(
    json: &str,
) -> std::result::Result<ActionPlan, serde_json::Error> {
    parse_action_plan(json).or_else(|_| {
        let value: serde_json::Value = serde_json::from_str(json)?;
        let mut plan: ActionPlan = serde_json::from_value(value["plan"].clone())?;
        plan.recompute_risk();
        plan.validate()
            .map_err(|error| serde_json::Error::io(std::io::Error::other(error.to_string())))?;
        Ok(plan)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_action_risk_levels() {
        let click = Action::Click {
            x: 100.0,
            y: 200.0,
            button: MouseButton::Left,
        };
        assert_eq!(click.risk_level(), RiskLevel::Medium);

        let scroll = Action::Scroll {
            x: 0.0,
            y: 0.0,
            delta_x: 0.0,
            delta_y: -1.0,
        };
        assert_eq!(scroll.risk_level(), RiskLevel::Low);

        let delete = Action::FileOperation {
            operation: FileOp::Delete,
            path: "/tmp/test.txt".to_string(),
            target: None,
        };
        assert_eq!(delete.risk_level(), RiskLevel::High);
    }

    #[test]
    fn test_action_plan_risk_escalation() {
        let mut plan = ActionPlan::new("测试操作".to_string());
        plan.add_action(Action::MouseMove { x: 0.0, y: 0.0 });
        assert_eq!(plan.risk_level, RiskLevel::Low);
        assert!(!plan.requires_confirmation);

        plan.add_action(Action::Click {
            x: 0.0,
            y: 0.0,
            button: MouseButton::Left,
        });
        assert_eq!(plan.risk_level, RiskLevel::Medium);
        assert!(plan.requires_confirmation);

        plan.add_action(Action::FileOperation {
            operation: FileOp::Delete,
            path: "/tmp/test".to_string(),
            target: None,
        });
        assert_eq!(plan.risk_level, RiskLevel::High);
        assert!(plan.requires_confirmation);
    }

    #[test]
    fn test_action_describe() {
        let click = Action::Click {
            x: 100.0,
            y: 200.0,
            button: MouseButton::Left,
        };
        assert!(click.describe().contains("100"));
        assert!(click.describe().contains("200"));

        let type_action = Action::Type {
            text: "hello".to_string(),
        };
        assert!(type_action.describe().contains("5"));
        assert!(!type_action.describe().contains("hello"));
    }

    #[test]
    fn test_action_plan_serialization() {
        let mut plan = ActionPlan::new("打开浏览器".to_string());
        plan.add_action(Action::OpenApp {
            name: "firefox".to_string(),
        });

        let json = serde_json::to_string(&plan).unwrap();
        assert!(json.contains("open_app"));
        assert!(json.contains("firefox"));
    }

    #[test]
    fn test_parse_action_plan() {
        let json = r#"{
            "actions": [
                {"type": "click", "x": 100.0, "y": 200.0, "button": "left"}
            ],
            "risk_level": "medium",
            "explanation": "点击按钮",
            "requires_confirmation": false
        }"#;

        let plan = parse_action_plan(json).unwrap();
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.explanation, "点击按钮");
    }

    #[test]
    fn test_parse_action_plan_arguments_direct() {
        let json = r#"{
            "actions": [
                {"type": "click", "x": 10.0, "y": 20.0, "button": "left"}
            ],
            "risk_level": "medium",
            "explanation": "点击按钮",
            "requires_confirmation": false
        }"#;

        let plan = parse_action_plan_arguments(json).unwrap();
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.risk_level, RiskLevel::Medium);
        assert!(plan.requires_confirmation);
    }

    #[test]
    fn test_parse_action_plan_risk_ignores_claimed_risk() {
        let json = r#"{
            "actions": [
                {"type": "file_operation", "operation": "delete", "path": "/tmp/test"}
            ],
            "risk_level": "low",
            "explanation": "删除文件",
            "requires_confirmation": false
        }"#;

        let plan = parse_action_plan_arguments(json).unwrap();
        assert_eq!(plan.risk_level, RiskLevel::High);
        assert!(plan.requires_confirmation);
    }

    #[test]
    fn test_plan_validation_rejects_unsafe_limits() {
        let mut plan = ActionPlan::new("test".to_string());
        plan.actions = vec![Action::Type {
            text: "x".repeat(MAX_TYPED_TEXT_CHARS + 1),
        }];
        assert!(plan.validate().is_err());

        plan.actions = vec![Action::Click {
            x: f64::NAN,
            y: 0.0,
            button: MouseButton::Left,
        }];
        assert!(plan.validate().is_err());
    }

    #[test]
    fn test_parse_action_plan_arguments_wrapped() {
        let json = r#"{
            "plan": {
                "actions": [
                    {"type": "open_app", "name": "notepad"}
                ],
                "risk_level": "medium",
                "explanation": "打开记事本",
                "requires_confirmation": false
            }
        }"#;

        let plan = parse_action_plan_arguments(json).unwrap();
        assert_eq!(plan.actions.len(), 1);
        assert_eq!(plan.explanation, "打开记事本");
        assert_eq!(plan.risk_level, RiskLevel::Medium);
    }
}
