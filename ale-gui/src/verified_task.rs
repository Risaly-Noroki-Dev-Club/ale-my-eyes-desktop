//! Verified-task contract for the desktop harness.
//!
//! This module owns the trust boundary between an untrusted cloud model and the
//! local `ale_core::harness` runner. The model may only propose a bounded list
//! of UIA steps; the host validates every field, rebuilds the [`Task`] from the
//! user's own transcription, and maps runner events onto the Misaka-Network
//! protocol. It is deliberately platform independent so it can be tested on the
//! macOS host; the Windows adapter lives in [`crate::uia_environment`].
//!
//! Nothing in this module executes input or decides success: `Task.success` is
//! copied from the last confirmed step's postcondition and is never re-read from
//! a model decision later.

use ale_core::harness::{
    Approval, Decision, Event, Limits, Operation, OperationKind, Predicate, Receipt, Report,
    Status, Task, Verdict, WindowIdentity,
};
use ale_core::remote::{CommandPreview, ExecutionState, ExecutionStatus, ProgressStage};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fmt;

/// `CommandPreview.task_mode` for the verified path (Misaka-Network v4).
pub const TASK_MODE_VERIFIED: &str = "verified";

/// Host-side limits on the model proposal. These are enforced before the task
/// is built and are independent of the runner's own `Limits`.
pub const MAX_STEPS: usize = 12;
pub const MAX_ARGUMENT_BYTES: usize = 32 * 1024;
pub const MAX_GOAL_CHARS: usize = 2_000;
pub const MAX_EXPLANATION_CHARS: usize = 2_000;
pub const MAX_TARGET_CHARS: usize = 200;
pub const MAX_VALUE_CHARS: usize = 2_000;
pub const MAX_EQUALS_CHARS: usize = 1_024;
pub const MAX_SUMMARY_CHARS: usize = 300;

/// Postcondition properties the host knows how to verify. `enabled` is derived
/// from `Node.enabled` by the UIA adapter; the rest come from UIA patterns.
pub const PROPERTY_WHITELIST: [&str; 4] = ["value", "enabled", "toggle", "selected"];

/// Rejection reason for a model proposal. Codes are stable for diagnostics and
/// tests; the message is for logs, not for the phone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedTaskError {
    pub code: &'static str,
    pub message: String,
}

impl VerifiedTaskError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for VerifiedTaskError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for VerifiedTaskError {}

fn invalid(code: &'static str, field: &str, detail: impl Into<String>) -> VerifiedTaskError {
    VerifiedTaskError::new(code, format!("{field}: {}", detail.into()))
}

/// Model-stated risk level. It is informational only; the host does not derive
/// confirmation requirements from it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifiedRisk {
    Low,
    Medium,
    High,
}

impl VerifiedRisk {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "low" => Some(Self::Low),
            "medium" => Some(Self::Medium),
            "high" => Some(Self::High),
            _ => None,
        }
    }
}

/// A control target as confirmed by the user. Role and label are matched
/// exactly against the observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedTarget {
    pub role: String,
    pub label: String,
}

impl VerifiedTarget {
    /// Human readable description used in previews.
    pub fn describe(&self) -> String {
        format!("{}「{}」", role_label(&self.role), self.label)
    }
}

/// One confirmed step. The operation/value pair is immutable for the whole run;
/// the runner's per-decision planner cannot widen it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedStep {
    pub operation: OperationKind,
    pub target: VerifiedTarget,
    pub value: Option<String>,
    pub expected: Predicate,
    pub summary: String,
}

impl VerifiedStep {
    /// Convert to the runner's step for one concrete observation node.
    pub fn to_harness_step(&self, target_id: &str) -> ale_core::harness::Step {
        ale_core::harness::Step {
            target_id: target_id.to_string(),
            operation: self.harness_operation(),
            expected: vec![self.expected.clone()],
            explanation: self.summary.clone(),
        }
    }

    pub fn harness_operation(&self) -> Operation {
        match self.operation {
            OperationKind::Invoke => Operation::Invoke,
            OperationKind::SetValue => Operation::SetValue {
                value: self.value.clone().unwrap_or_default(),
            },
        }
    }

    /// Human readable postcondition for the confirmation screen.
    pub fn verification_text(&self) -> String {
        format!(
            "{}的{}应为「{}」",
            self.target.describe(),
            property_label(&self.expected.property),
            self.expected.equals
        )
    }
}

/// Validated proposal: the only thing that may be shown to the user and turned
/// into a [`Task`] after confirmation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedProposal {
    /// Goal as stated by the model. Never used as `Task.goal`; that comes from
    /// the user's own transcription in [`build_task`].
    pub goal: String,
    pub explanation: String,
    pub risk: VerifiedRisk,
    pub steps: Vec<VerifiedStep>,
}

/// The `propose_verified_task` tool schema appended to the cloud request in
/// verified mode. The runner never sees this schema; it only guards the model.
pub fn propose_verified_task_tool() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "propose_verified_task",
            "description": "Propose a short, verified desktop task. Every step targets exactly one accessibility control by role and name and declares a postcondition the host can read back. Use invoke for buttons/menu items, set_value for exact text replacement. The user must confirm before anything is executed; the host may reject the plan. Do not guess controls you cannot see in the provided accessibility tree.",
            "parameters": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "goal": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_GOAL_CHARS,
                        "description": "One sentence restating the user's goal."
                    },
                    "explanation": {
                        "type": "string",
                        "minLength": 1,
                        "maxLength": MAX_EXPLANATION_CHARS,
                        "description": "Short user-facing explanation of the plan."
                    },
                    "risk_level": {
                        "type": "string",
                        "enum": ["low", "medium", "high"]
                    },
                    "steps": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": MAX_STEPS,
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "operation": {
                                    "type": "string",
                                    "enum": ["invoke", "set_value"]
                                },
                                "target": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "properties": {
                                        "role": {
                                            "type": "string",
                                            "minLength": 1,
                                            "maxLength": MAX_TARGET_CHARS
                                        },
                                        "label": {
                                            "type": "string",
                                            "minLength": 1,
                                            "maxLength": MAX_TARGET_CHARS
                                        }
                                    },
                                    "required": ["role", "label"]
                                },
                                "value": {
                                    "type": "string",
                                    "maxLength": MAX_VALUE_CHARS,
                                    "description": "Required for set_value, forbidden for invoke. Replaces the current content exactly."
                                },
                                "expected": {
                                    "type": "object",
                                    "additionalProperties": false,
                                    "properties": {
                                        "role": {
                                            "type": "string",
                                            "minLength": 1,
                                            "maxLength": MAX_TARGET_CHARS
                                        },
                                        "label": {
                                            "type": "string",
                                            "minLength": 1,
                                            "maxLength": MAX_TARGET_CHARS
                                        },
                                        "property": {
                                            "type": "string",
                                            "enum": PROPERTY_WHITELIST
                                        },
                                        "equals": {
                                            "type": "string",
                                            "maxLength": MAX_EQUALS_CHARS
                                        }
                                    },
                                    "required": ["role", "label", "property", "equals"]
                                },
                                "summary": {
                                    "type": "string",
                                    "minLength": 1,
                                    "maxLength": MAX_SUMMARY_CHARS
                                }
                            },
                            "required": ["operation", "target", "expected", "summary"]
                        }
                    }
                },
                "required": ["goal", "explanation", "risk_level", "steps"]
            }
        }
    })
}

/// Returns true when `name` is the verified-task proposal tool.
pub fn is_verified_task_tool(name: &str) -> bool {
    name == "propose_verified_task"
}

/// Parse and strictly validate the arguments of one `propose_verified_task`
/// call. Unknown fields, out-of-range values and non-whitelisted operations or
/// properties are rejected rather than silently repaired.
pub fn parse_verified_task_arguments(
    arguments: &str,
) -> Result<VerifiedProposal, VerifiedTaskError> {
    if arguments.len() > MAX_ARGUMENT_BYTES {
        return Err(VerifiedTaskError::new(
            "ARGUMENTS_TOO_LARGE",
            format!("arguments exceed {MAX_ARGUMENT_BYTES} bytes"),
        ));
    }
    let wire: WireProposal = serde_json::from_str(arguments)
        .map_err(|error| VerifiedTaskError::new("MALFORMED_ARGUMENTS", error.to_string()))?;
    validate_wire_proposal(wire)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireProposal {
    goal: String,
    explanation: String,
    risk_level: String,
    steps: Vec<WireStep>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireStep {
    operation: String,
    target: WireTarget,
    value: Option<String>,
    expected: WireExpected,
    summary: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireTarget {
    role: String,
    label: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WireExpected {
    role: String,
    label: String,
    property: String,
    equals: String,
}

fn validate_wire_proposal(wire: WireProposal) -> Result<VerifiedProposal, VerifiedTaskError> {
    let goal = validated_text("goal", wire.goal, MAX_GOAL_CHARS, false)?;
    let explanation = validated_text(
        "explanation",
        wire.explanation,
        MAX_EXPLANATION_CHARS,
        false,
    )?;
    let risk = VerifiedRisk::parse(&wire.risk_level).ok_or_else(|| {
        invalid(
            "INVALID_RISK",
            "risk_level",
            format!("expected low|medium|high, got {:?}", wire.risk_level),
        )
    })?;
    if wire.steps.is_empty() {
        return Err(VerifiedTaskError::new(
            "NO_STEPS",
            "at least one step is required",
        ));
    }
    if wire.steps.len() > MAX_STEPS {
        return Err(VerifiedTaskError::new(
            "TOO_MANY_STEPS",
            format!("{} steps exceed the limit of {MAX_STEPS}", wire.steps.len()),
        ));
    }
    let mut steps = Vec::with_capacity(wire.steps.len());
    for (index, step) in wire.steps.into_iter().enumerate() {
        steps.push(validate_wire_step(index, step)?);
    }
    Ok(VerifiedProposal {
        goal,
        explanation,
        risk,
        steps,
    })
}

fn validate_wire_step(index: usize, step: WireStep) -> Result<VerifiedStep, VerifiedTaskError> {
    let field = |name: &str| format!("steps[{index}].{name}");
    let operation = match step.operation.as_str() {
        "invoke" => OperationKind::Invoke,
        "set_value" => OperationKind::SetValue,
        other => {
            return Err(invalid(
                "INVALID_OPERATION",
                &field("operation"),
                format!("unsupported operation {other:?}"),
            ))
        }
    };
    let target = VerifiedTarget {
        role: validated_text(
            &field("target.role"),
            step.target.role,
            MAX_TARGET_CHARS,
            true,
        )?,
        label: validated_text(
            &field("target.label"),
            step.target.label,
            MAX_TARGET_CHARS,
            true,
        )?,
    };
    let value = match (operation, step.value) {
        (OperationKind::Invoke, None) => None,
        (OperationKind::Invoke, Some(_)) => {
            return Err(invalid(
                "INVALID_VALUE",
                &field("value"),
                "invoke must not carry a value",
            ))
        }
        (OperationKind::SetValue, None) => {
            return Err(invalid(
                "INVALID_VALUE",
                &field("value"),
                "set_value must carry an exact replacement value",
            ))
        }
        (OperationKind::SetValue, Some(value)) => {
            let value = validated_text(&field("value"), value, MAX_VALUE_CHARS, false)?;
            Some(value)
        }
    };
    let expected = validate_expected(index, step.expected)?;
    let summary = match step.summary {
        Some(summary) if !summary.trim().is_empty() => {
            validated_text(&field("summary"), summary, MAX_SUMMARY_CHARS, true)?
        }
        _ => default_summary(operation, &target, value.as_deref()),
    };
    Ok(VerifiedStep {
        operation,
        target,
        value,
        expected,
        summary,
    })
}

fn validate_expected(index: usize, expected: WireExpected) -> Result<Predicate, VerifiedTaskError> {
    let field = |name: &str| format!("steps[{index}].expected.{name}");
    let role = validated_text(&field("role"), expected.role, MAX_TARGET_CHARS, true)?;
    let label = validated_text(&field("label"), expected.label, MAX_TARGET_CHARS, true)?;
    if !PROPERTY_WHITELIST.contains(&expected.property.as_str()) {
        return Err(invalid(
            "INVALID_PROPERTY",
            &field("property"),
            format!(
                "unsupported property {:?}; expected one of {PROPERTY_WHITELIST:?}",
                expected.property
            ),
        ));
    }
    // Values are compared exactly, so they are never trimmed. An empty expected
    // string is valid because some UIA properties legitimately read as empty.
    if expected.equals.chars().count() > MAX_EQUALS_CHARS {
        return Err(invalid(
            "INVALID_EXPECTED",
            &field("equals"),
            format!("exceeds {MAX_EQUALS_CHARS} characters"),
        ));
    }
    Ok(Predicate {
        role,
        label,
        property: expected.property,
        equals: expected.equals,
    })
}

/// Trim and length check one text field. When `required` is set, an empty value
/// after trimming is rejected.
fn validated_text(
    field: &str,
    value: String,
    max_chars: usize,
    required: bool,
) -> Result<String, VerifiedTaskError> {
    let trimmed = value.trim();
    if required && trimmed.is_empty() {
        return Err(invalid("EMPTY_FIELD", field, "must not be empty"));
    }
    if trimmed.chars().count() > max_chars {
        return Err(invalid(
            "FIELD_TOO_LONG",
            field,
            format!("exceeds {max_chars} characters"),
        ));
    }
    Ok(trimmed.to_string())
}

fn default_summary(
    operation: OperationKind,
    target: &VerifiedTarget,
    value: Option<&str>,
) -> String {
    match operation {
        OperationKind::Invoke => format!("点击{}", target.describe()),
        OperationKind::SetValue => {
            let display: String = value.unwrap_or_default().chars().take(40).collect();
            format!("将{}设置为「{}」", target.describe(), display)
        }
    }
}

/// Build the runner task after the user confirmed the proposal.
///
/// * `user_goal` is the transcription of what the user asked for, never the
///   model's `goal` field.
/// * `window` is the foreground window observed by the host before planning.
/// * `success` is the postcondition of the last confirmed step, so the runner
///   verifies the original goal, not a model claim.
/// * `allow_cloud` is the disclosure decision already made for this request.
pub fn build_task(
    proposal: &VerifiedProposal,
    user_goal: &str,
    window: &WindowIdentity,
    allow_cloud: bool,
) -> Result<Task, VerifiedTaskError> {
    let goal = user_goal.trim();
    if goal.is_empty() {
        return Err(VerifiedTaskError::new(
            "EMPTY_USER_GOAL",
            "the user's transcribed goal is empty",
        ));
    }
    if window.window_id.trim().is_empty() || window.process_id == 0 {
        return Err(VerifiedTaskError::new(
            "INVALID_WINDOW",
            "the foreground window identity is incomplete",
        ));
    }
    let last = proposal
        .steps
        .last()
        .ok_or_else(|| VerifiedTaskError::new("NO_STEPS", "the proposal has no steps"))?;
    let mut allowed_operations: Vec<OperationKind> = Vec::new();
    for step in &proposal.steps {
        if !allowed_operations.contains(&step.operation) {
            allowed_operations.push(step.operation);
        }
    }
    Ok(Task {
        id: format!("verified-{}", uuid::Uuid::new_v4()),
        goal: goal.to_string(),
        window: window.clone(),
        success: vec![last.expected.clone()],
        allowed_operations,
        allow_cloud,
        limits: Limits::default(),
    })
}

/// Readable step summaries for `CommandPreview.action_steps`.
pub fn action_summaries(proposal: &VerifiedProposal) -> Vec<String> {
    proposal
        .steps
        .iter()
        .map(|step| step.summary.clone())
        .collect()
}

/// Readable postconditions for `CommandPreview.verification`.
pub fn verification_texts(proposal: &VerifiedProposal) -> Vec<String> {
    proposal
        .steps
        .iter()
        .map(VerifiedStep::verification_text)
        .collect()
}

/// Host-built preview of a verified task. Convert it to the protocol message
/// only when the request is actually handed back to the phone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedPreview {
    pub response_text: String,
    pub action_steps: Vec<String>,
    pub verification: Vec<String>,
    pub confirmation_text: String,
}

pub fn build_preview(proposal: &VerifiedProposal) -> VerifiedPreview {
    let action_steps = action_summaries(proposal);
    let verification = verification_texts(proposal);
    let mut response_text = proposal.explanation.clone();
    if !response_text.is_empty() {
        response_text.push('\n');
    }
    response_text.push_str(&format!(
        "共 {} 步；每步执行后会读取界面属性验证，全部完成后核对后置条件。",
        proposal.steps.len()
    ));
    if !verification.is_empty() {
        response_text.push_str("\n后置条件：");
        for condition in &verification {
            response_text.push_str("\n- ");
            response_text.push_str(condition);
        }
    }
    let confirmation_text = format!(
        "准备执行 {} 个步骤：{}。完成后将核对：{}。是否继续？",
        proposal.steps.len(),
        action_steps.join("；"),
        verification.join("；")
    );
    VerifiedPreview {
        response_text,
        action_steps,
        verification,
        confirmation_text,
    }
}

impl VerifiedPreview {
    /// Build the Misaka-Network v4 preview message. The caller still sanitizes
    /// every user-visible string before sending it to the phone.
    pub fn into_command_preview(self, request_id: &str) -> CommandPreview {
        CommandPreview {
            request_id: request_id.to_string(),
            response_text: self.response_text,
            action_steps: self.action_steps,
            confirmation_text: self.confirmation_text,
            requires_confirmation: true,
            has_plan: true,
            task_mode: Some(TASK_MODE_VERIFIED.to_string()),
            verification: Some(self.verification),
        }
    }
}

/// Protocol-level terminal state for a runner status. The protocol has no
/// dedicated budget state, so `BudgetExceeded` is reported as a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifiedTerminalState {
    Completed,
    Failed,
    Cancelled,
    OutcomeUnknown,
    NeedsUser,
}

impl VerifiedTerminalState {
    pub fn execution_state(self) -> ExecutionState {
        match self {
            Self::Completed => ExecutionState::Completed,
            Self::Failed => ExecutionState::Failed,
            Self::Cancelled => ExecutionState::Cancelled,
            Self::OutcomeUnknown => ExecutionState::OutcomeUnknown,
            Self::NeedsUser => ExecutionState::NeedsUser,
        }
    }

    /// Localized message for the phone. Raw runner reasons stay in diagnostics
    /// and audit logs because they are English contract text.
    pub fn message(self) -> &'static str {
        match self {
            Self::Completed => "任务已完成，并已验证目标状态。",
            Self::Failed => "任务执行失败，未继续输入。",
            Self::Cancelled => "已取消，未继续执行。",
            Self::OutcomeUnknown => "结果不确定：操作可能已经执行，请勿自动重试，请在桌面上确认。",
            Self::NeedsUser => "任务已停止，需要你确认或接手。",
        }
    }
}

pub fn terminal_state(status: &Status) -> VerifiedTerminalState {
    match status {
        Status::Completed => VerifiedTerminalState::Completed,
        Status::Failed | Status::BudgetExceeded => VerifiedTerminalState::Failed,
        Status::Cancelled => VerifiedTerminalState::Cancelled,
        Status::OutcomeUnknown => VerifiedTerminalState::OutcomeUnknown,
        Status::NeedsUser => VerifiedTerminalState::NeedsUser,
    }
}

/// Map a finished run report onto the protocol execution status.
pub fn terminal_status(request_id: &str, report: &Report) -> ExecutionStatus {
    let state = terminal_state(&report.status);
    ExecutionStatus {
        request_id: request_id.to_string(),
        state: state.execution_state(),
        message: state.message().to_string(),
        actions_executed: report.actions,
    }
}

/// Map one runner event onto a throttled progress update. Terminal events and
/// high-frequency observation events return `None`; the caller sends the final
/// [`ExecutionStatus`] instead.
pub fn event_progress(event: &Event) -> Option<(ProgressStage, String)> {
    match event {
        Event::Observed { .. } => None,
        Event::GoalChecked { verdict, .. } => (verdict == &Verdict::Satisfied).then(|| {
            (
                ProgressStage::VerifiedVerify,
                "已读取到任务目标状态".to_string(),
            )
        }),
        Event::Decided { decision } => match decision {
            Decision::Act { step } => Some((
                ProgressStage::VerifiedStep,
                format!("正在执行：{}", step.explanation),
            )),
            Decision::Finish => Some((
                ProgressStage::VerifiedVerify,
                "正在核对任务目标".to_string(),
            )),
            Decision::NeedHelp { reason } => Some((
                ProgressStage::AwaitingDecision,
                format!("任务需要你确认：{reason}"),
            )),
        },
        Event::Authorized { approval, .. } => match approval {
            Approval::NeedsUser => Some((
                ProgressStage::AwaitingDecision,
                "这一步需要你确认".to_string(),
            )),
            Approval::Allow | Approval::Deny => None,
        },
        Event::StateChanged => Some((
            ProgressStage::VerifiedRecover,
            "界面状态已变化，正在重新核对".to_string(),
        )),
        Event::Dispatching { request } => Some((
            ProgressStage::VerifiedStep,
            format!("正在执行：{}", request.step.explanation),
        )),
        Event::Executed { receipt, .. } => match receipt {
            Receipt::Dispatched => Some((
                ProgressStage::VerifiedVerify,
                "操作已发出，正在验证结果".to_string(),
            )),
            Receipt::NotApplied(_) => Some((
                ProgressStage::VerifiedRecover,
                "操作未发出，正在重新观察".to_string(),
            )),
            Receipt::Uncertain(_) => Some((
                ProgressStage::VerifiedVerify,
                "操作结果不确定，正在复核".to_string(),
            )),
        },
        Event::StepChecked { verdict, .. } => (verdict == &Verdict::Unknown).then(|| {
            (
                ProgressStage::VerifiedVerify,
                "界面暂未反映变化，正在继续读取".to_string(),
            )
        }),
        Event::Stopped { .. } => None,
    }
}

/// Chinese display name for common UIA control types. Unknown roles fall back
/// to the raw UIA programmatic name so previews never invent a control name.
fn role_label(role: &str) -> &str {
    match role {
        "ControlType.Button" => "按钮",
        "ControlType.Edit" => "输入框",
        "ControlType.Text" => "文本",
        "ControlType.CheckBox" => "复选框",
        "ControlType.RadioButton" => "单选按钮",
        "ControlType.ComboBox" => "下拉框",
        "ControlType.ListItem" => "列表项",
        "ControlType.MenuItem" => "菜单项",
        "ControlType.Window" => "窗口",
        "ControlType.Pane" => "面板",
        "ControlType.TabItem" => "标签页",
        "ControlType.Slider" => "滑块",
        "ControlType.Spinner" => "数值框",
        _ => role,
    }
}

fn property_label(property: &str) -> &str {
    match property {
        "value" => "值",
        "enabled" => "可用状态",
        "toggle" => "开关状态",
        "selected" => "选中状态",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_json() -> serde_json::Value {
        json!({
            "goal": "把缩放调到 125 并保存",
            "explanation": "打开设置，把缩放改为 125，然后保存。",
            "risk_level": "medium",
            "steps": [
                {
                    "operation": "invoke",
                    "target": {"role": "ControlType.Button", "label": "设置"},
                    "expected": {"role": "ControlType.Text", "label": "当前缩放", "property": "value", "equals": "100"},
                    "summary": "点击设置"
                },
                {
                    "operation": "set_value",
                    "target": {"role": "ControlType.Edit", "label": "缩放"},
                    "value": "125",
                    "expected": {"role": "ControlType.Edit", "label": "缩放", "property": "value", "equals": "125"},
                    "summary": "将缩放设置为 125"
                },
                {
                    "operation": "invoke",
                    "target": {"role": "ControlType.Button", "label": "保存"},
                    "expected": {"role": "ControlType.Text", "label": "已保存", "property": "value", "equals": "125"},
                    "summary": "点击保存"
                }
            ]
        })
    }

    fn parse(value: serde_json::Value) -> Result<VerifiedProposal, VerifiedTaskError> {
        parse_verified_task_arguments(&value.to_string())
    }

    fn window() -> WindowIdentity {
        WindowIdentity {
            process_id: 4321,
            window_id: "123456".to_string(),
        }
    }

    fn reject_code(value: serde_json::Value) -> &'static str {
        parse(value).expect_err("proposal must be rejected").code
    }

    #[test]
    fn tool_schema_matches_the_host_contract() {
        let tool = propose_verified_task_tool();
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "propose_verified_task");
        let parameters = &tool["function"]["parameters"];
        assert_eq!(
            parameters["required"],
            json!(["goal", "explanation", "risk_level", "steps"])
        );
        assert_eq!(parameters["properties"]["steps"]["minItems"], 1);
        assert_eq!(parameters["properties"]["steps"]["maxItems"], MAX_STEPS);
        assert_eq!(
            parameters["properties"]["steps"]["items"]["properties"]["expected"]["properties"]
                ["property"]["enum"],
            json!(PROPERTY_WHITELIST)
        );
        assert_eq!(
            parameters["properties"]["steps"]["items"]["properties"]["value"]["maxLength"],
            MAX_VALUE_CHARS
        );
        assert!(is_verified_task_tool("propose_verified_task"));
        assert!(!is_verified_task_tool("execute_action_plan"));
    }

    #[test]
    fn valid_proposal_parses_with_expected_fields() {
        let proposal = parse(valid_json()).expect("valid proposal");
        assert_eq!(proposal.steps.len(), 3);
        assert_eq!(proposal.risk, VerifiedRisk::Medium);
        assert_eq!(proposal.steps[0].operation, OperationKind::Invoke);
        assert_eq!(proposal.steps[1].operation, OperationKind::SetValue);
        assert_eq!(proposal.steps[1].value.as_deref(), Some("125"));
        assert_eq!(proposal.steps[0].expected.label, "当前缩放");
        assert_eq!(proposal.steps[2].summary, "点击保存");
    }

    #[test]
    fn missing_summary_is_synthesized_not_trusted() {
        let mut value = valid_json();
        let _ = value["steps"][0].as_object_mut().unwrap().remove("summary");
        let proposal = parse(value).expect("valid proposal");
        assert_eq!(proposal.steps[0].summary, "点击按钮「设置」");
    }

    #[test]
    fn rejects_unknown_top_level_and_nested_fields() {
        let mut value = valid_json();
        value["override_goal"] = json!(true);
        assert_eq!(reject_code(value), "MALFORMED_ARGUMENTS");

        let mut value = valid_json();
        value["steps"][0]["command"] = json!("powershell");
        assert_eq!(reject_code(value), "MALFORMED_ARGUMENTS");

        let mut value = valid_json();
        value["steps"][0]["expected"]["regex"] = json!(".*");
        assert_eq!(reject_code(value), "MALFORMED_ARGUMENTS");
    }

    #[test]
    fn rejects_malformed_and_oversized_arguments() {
        assert_eq!(
            parse_verified_task_arguments("not json").unwrap_err().code,
            "MALFORMED_ARGUMENTS"
        );
        assert_eq!(
            parse_verified_task_arguments(&"x".repeat(MAX_ARGUMENT_BYTES + 1))
                .unwrap_err()
                .code,
            "ARGUMENTS_TOO_LARGE"
        );
    }

    #[test]
    fn rejects_step_count_out_of_range() {
        let mut value = valid_json();
        value["steps"] = json!([]);
        assert_eq!(reject_code(value), "NO_STEPS");

        let mut value = valid_json();
        let step = value["steps"][0].clone();
        value["steps"] = json!(vec![step; MAX_STEPS + 1]);
        assert_eq!(reject_code(value), "TOO_MANY_STEPS");
    }

    #[test]
    fn rejects_non_whitelisted_operations_properties_and_risk() {
        let mut value = valid_json();
        value["steps"][2]["operation"] = json!("shell");
        assert_eq!(reject_code(value), "INVALID_OPERATION");

        let mut value = valid_json();
        value["steps"][0]["expected"]["property"] = json!("class_name");
        assert_eq!(reject_code(value), "INVALID_PROPERTY");

        let mut value = valid_json();
        value["risk_level"] = json!("critical");
        assert_eq!(reject_code(value), "INVALID_RISK");
    }

    #[test]
    fn rejects_value_rule_violations() {
        let mut value = valid_json();
        value["steps"][2]["value"] = json!("125");
        assert_eq!(reject_code(value), "INVALID_VALUE");

        let mut value = valid_json();
        let _ = value["steps"][1].as_object_mut().unwrap().remove("value");
        assert_eq!(reject_code(value), "INVALID_VALUE");

        let mut value = valid_json();
        value["steps"][1]["value"] = json!("x".repeat(MAX_VALUE_CHARS + 1));
        assert_eq!(reject_code(value), "FIELD_TOO_LONG");

        // An exact replacement with an empty string is a legitimate clear.
        let mut value = valid_json();
        value["steps"][1]["value"] = json!("");
        value["steps"][1]["expected"]["equals"] = json!("");
        assert!(parse(value).is_ok());
    }

    #[test]
    fn rejects_empty_and_overlong_fields() {
        for (path, bad, code) in [
            (("target", "role"), json!(""), "EMPTY_FIELD"),
            (("target", "label"), json!("  "), "EMPTY_FIELD"),
            (("expected", "role"), json!(""), "EMPTY_FIELD"),
            (("expected", "label"), json!(""), "EMPTY_FIELD"),
        ] {
            let mut value = valid_json();
            value["steps"][0][path.0][path.1] = bad;
            assert_eq!(reject_code(value), code);
        }

        let mut value = valid_json();
        value["steps"][0]["target"]["label"] = json!("y".repeat(MAX_TARGET_CHARS + 1));
        assert_eq!(reject_code(value), "FIELD_TOO_LONG");

        let mut value = valid_json();
        value["steps"][0]["expected"]["equals"] = json!("y".repeat(MAX_EQUALS_CHARS + 1));
        assert_eq!(reject_code(value), "INVALID_EXPECTED");

        let mut value = valid_json();
        value["goal"] = json!("g".repeat(MAX_GOAL_CHARS + 1));
        assert_eq!(reject_code(value), "FIELD_TOO_LONG");

        let mut value = valid_json();
        value["steps"][0]["summary"] = json!("s".repeat(MAX_SUMMARY_CHARS + 1));
        assert_eq!(reject_code(value), "FIELD_TOO_LONG");
    }

    #[test]
    fn build_task_uses_user_goal_window_and_last_postcondition() {
        let proposal = parse(valid_json()).unwrap();
        let user_goal = "帮我调大缩放";
        let task = build_task(&proposal, user_goal, &window(), true).expect("task");
        assert!(task.id.starts_with("verified-"));
        assert_eq!(task.goal, user_goal);
        assert_eq!(task.window, window());
        assert_eq!(task.success, vec![proposal.steps[2].expected.clone()]);
        assert_eq!(
            task.allowed_operations,
            vec![OperationKind::Invoke, OperationKind::SetValue]
        );
        assert!(task.allow_cloud);
        let defaults = Limits::default();
        assert_eq!(task.limits.max_actions, defaults.max_actions);
        assert_eq!(
            task.limits.max_consecutive_failures,
            defaults.max_consecutive_failures
        );
        assert_eq!(task.limits.task_timeout_ms, defaults.task_timeout_ms);
    }

    #[test]
    fn build_task_deduplicates_allowed_operations() {
        let mut value = valid_json();
        value["steps"] = json!([value["steps"][0].clone(), value["steps"][0].clone(),]);
        let proposal = parse(value).unwrap();
        let task = build_task(&proposal, "目标", &window(), false).unwrap();
        assert_eq!(task.allowed_operations, vec![OperationKind::Invoke]);
    }

    #[test]
    fn build_task_rejects_empty_user_goal_or_bad_window() {
        let proposal = parse(valid_json()).unwrap();
        assert_eq!(
            build_task(&proposal, "   ", &window(), true)
                .unwrap_err()
                .code,
            "EMPTY_USER_GOAL"
        );
        let bad_window = WindowIdentity {
            process_id: 0,
            window_id: String::new(),
        };
        assert_eq!(
            build_task(&proposal, "目标", &bad_window, true)
                .unwrap_err()
                .code,
            "INVALID_WINDOW"
        );
    }

    #[test]
    fn preview_exposes_steps_and_postconditions() {
        let proposal = parse(valid_json()).unwrap();
        let preview = build_preview(&proposal);
        assert_eq!(preview.action_steps.len(), 3);
        assert_eq!(preview.verification.len(), 3);
        assert!(preview.verification[1].contains("输入框「缩放」"));
        assert!(preview.verification[1].contains("值"));
        assert!(preview.confirmation_text.contains("是否继续"));
        assert!(
            preview.response_text.contains("后置条件"),
            "postconditions must be visible on the work confirmation card"
        );
        let message = preview.into_command_preview("req-1");
        assert_eq!(message.request_id, "req-1");
        assert_eq!(message.task_mode.as_deref(), Some(TASK_MODE_VERIFIED));
        assert_eq!(message.verification.as_ref().map(Vec::len), Some(3));
        assert!(message.requires_confirmation);
        assert!(message.has_plan);
    }

    #[test]
    fn terminal_states_map_to_protocol_states() {
        assert_eq!(
            terminal_state(&Status::Completed),
            VerifiedTerminalState::Completed
        );
        assert_eq!(
            terminal_state(&Status::NeedsUser),
            VerifiedTerminalState::NeedsUser
        );
        assert_eq!(
            terminal_state(&Status::Cancelled),
            VerifiedTerminalState::Cancelled
        );
        assert_eq!(
            terminal_state(&Status::OutcomeUnknown),
            VerifiedTerminalState::OutcomeUnknown
        );
        assert_eq!(
            terminal_state(&Status::Failed),
            VerifiedTerminalState::Failed
        );
        assert_eq!(
            terminal_state(&Status::BudgetExceeded),
            VerifiedTerminalState::Failed
        );
        assert_eq!(
            VerifiedTerminalState::OutcomeUnknown.execution_state(),
            ExecutionState::OutcomeUnknown
        );
        assert_eq!(
            VerifiedTerminalState::NeedsUser.execution_state(),
            ExecutionState::NeedsUser
        );
        assert_eq!(
            VerifiedTerminalState::Completed.execution_state(),
            ExecutionState::Completed
        );
    }

    #[test]
    fn terminal_status_uses_localized_message_and_action_count() {
        let proposal = parse(valid_json()).unwrap();
        let task = build_task(&proposal, "目标", &window(), true).unwrap();
        let report = Report {
            task_id: task.id.clone(),
            task,
            status: Status::OutcomeUnknown,
            reason: "Input outcome uncertain; do not replay".to_string(),
            elapsed_ms: 10,
            actions: 2,
            decisions: 3,
            attempts: Vec::new(),
            events: Vec::new(),
        };
        let status = terminal_status("req-9", &report);
        assert_eq!(status.request_id, "req-9");
        assert_eq!(status.state, ExecutionState::OutcomeUnknown);
        assert_eq!(status.actions_executed, 2);
        assert!(status.message.contains("请勿自动重试"));
    }

    #[test]
    fn event_progress_maps_runner_events() {
        let proposal = parse(valid_json()).unwrap();
        let step = proposal.steps[0].to_harness_step("0/1");
        let act = Event::Decided {
            decision: Decision::Act { step: step.clone() },
        };
        assert_eq!(
            event_progress(&act).map(|(stage, _)| stage),
            Some(ProgressStage::VerifiedStep)
        );
        assert_eq!(
            event_progress(&Event::StateChanged).map(|(stage, _)| stage),
            Some(ProgressStage::VerifiedRecover)
        );
        let dispatch = Event::Dispatching {
            request: ale_core::harness::ActionRequest {
                id: "a1".to_string(),
                observation: ale_core::harness::Observation {
                    sequence: 1,
                    window: window(),
                    nodes: Vec::new(),
                },
                step,
            },
        };
        assert_eq!(
            event_progress(&dispatch).map(|(stage, _)| stage),
            Some(ProgressStage::VerifiedStep)
        );
        assert_eq!(
            event_progress(&Event::Executed {
                action_id: "a1".to_string(),
                receipt: Receipt::Dispatched,
            })
            .map(|(stage, _)| stage),
            Some(ProgressStage::VerifiedVerify)
        );
        assert_eq!(
            event_progress(&Event::Stopped {
                status: Status::Completed,
                reason: "done".into()
            }),
            None
        );
        assert_eq!(
            event_progress(&Event::Observed {
                observation: ale_core::harness::Observation {
                    sequence: 1,
                    window: window(),
                    nodes: Vec::new(),
                }
            }),
            None
        );
        assert!(event_progress(&Event::Executed {
            action_id: "a".into(),
            receipt: Receipt::Uncertain("maybe".into()),
        })
        .is_some());
        assert!(event_progress(&Event::Decided {
            decision: Decision::NeedHelp {
                reason: "找不到控件".into()
            }
        })
        .is_some());
    }

    #[test]
    fn harness_step_conversion_keeps_confirmed_operation() {
        let proposal = parse(valid_json()).unwrap();
        let set_value = proposal.steps[1].to_harness_step("0/2");
        assert_eq!(set_value.target_id, "0/2");
        assert_eq!(set_value.explanation, "将缩放设置为 125");
        match set_value.operation {
            Operation::SetValue { value } => assert_eq!(value, "125"),
            Operation::Invoke => panic!("expected SetValue"),
        }
        let invoke = proposal.steps[0].to_harness_step("0/1");
        assert!(matches!(invoke.operation, Operation::Invoke));
    }
}
