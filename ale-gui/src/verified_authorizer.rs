//! Authorizer for verified tasks.
//!
//! The user confirms an exact step list (operation, target role/label, exact
//! `SetValue` payload and postcondition). The runner asks this authorizer before
//! every dispatch; only a step that still matches a confirmed grant in the
//! current observation is allowed. Anything else is handed back to the user,
//! never silently executed and never downgraded to a different operation.

use crate::verified_task::{VerifiedProposal, VerifiedStep, VerifiedTarget};
use ale_core::harness::{
    ActionRequest, Approval, Authorizer, Operation, OperationKind, Predicate, Task, WindowIdentity,
};
use std::future::Future;
use std::pin::Pin;

struct StepGrant {
    operation: OperationKind,
    target: VerifiedTarget,
    value: Option<String>,
    expected: Predicate,
}

impl StepGrant {
    fn from_step(step: &VerifiedStep) -> Self {
        Self {
            operation: step.operation,
            target: step.target.clone(),
            value: step.value.clone(),
            expected: step.expected.clone(),
        }
    }

    fn matches(&self, node: &ale_core::harness::Node, request: &ActionRequest) -> bool {
        if request.step.expected.len() != 1 || request.step.expected[0] != self.expected {
            return false;
        }
        if node.role != self.target.role || node.label != self.target.label {
            return false;
        }
        match (&request.step.operation, self.operation) {
            (Operation::Invoke, OperationKind::Invoke) => true,
            (Operation::SetValue { value }, OperationKind::SetValue) => {
                self.value.as_deref() == Some(value.as_str())
            }
            _ => false,
        }
    }
}

/// Grants derived from the user-confirmed proposal, bounded to one window.
pub struct ConfirmedTaskAuthorizer {
    window: WindowIdentity,
    grants: Vec<StepGrant>,
}

impl ConfirmedTaskAuthorizer {
    pub fn new(window: WindowIdentity, proposal: &VerifiedProposal) -> Self {
        let grants = proposal.steps.iter().map(StepGrant::from_step).collect();
        Self { window, grants }
    }

    pub fn window(&self) -> &WindowIdentity {
        &self.window
    }

    fn authorize_inner(&self, task: &Task, request: &ActionRequest) -> Approval {
        // The task was built for one foreground window; a different window is a
        // different task and needs a new confirmation.
        if task.window != self.window || request.observation.window != self.window {
            return Approval::NeedsUser;
        }
        if !task
            .allowed_operations
            .contains(&operation_kind(&request.step.operation))
        {
            return Approval::NeedsUser;
        }
        let Some(node) = request
            .observation
            .nodes
            .iter()
            .find(|node| node.id == request.step.target_id)
        else {
            return Approval::NeedsUser;
        };
        if !node.enabled {
            return Approval::NeedsUser;
        }
        if self.grants.iter().any(|grant| grant.matches(node, request)) {
            Approval::Allow
        } else {
            // A step outside the confirmed list (different control, different
            // exact value or different postcondition) is not a denial by the
            // user; it requires explicit user involvement.
            Approval::NeedsUser
        }
    }
}

fn operation_kind(operation: &Operation) -> OperationKind {
    match operation {
        Operation::Invoke => OperationKind::Invoke,
        Operation::SetValue { .. } => OperationKind::SetValue,
    }
}

impl Authorizer for ConfirmedTaskAuthorizer {
    // Hand-desugared `#[async_trait]` implementation; authorization is a
    // synchronous comparison, so no async machinery is needed.
    fn authorize<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 mut self,
        task: &'life1 Task,
        request: &'life2 ActionRequest,
    ) -> Pin<Box<dyn Future<Output = Approval> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { self.authorize_inner(task, request) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verified_task::{build_task, parse_verified_task_arguments};
    use ale_core::harness::Observation;
    use serde_json::json;
    use std::collections::BTreeMap;

    fn window() -> WindowIdentity {
        WindowIdentity {
            process_id: 7,
            window_id: "1001".to_string(),
        }
    }

    fn proposal() -> VerifiedProposal {
        let value = json!({
            "goal": "保存缩放",
            "explanation": "设置缩放并保存",
            "risk_level": "low",
            "steps": [
                {
                    "operation": "invoke",
                    "target": {"role": "ControlType.Button", "label": "保存"},
                    "expected": {"role": "ControlType.Text", "label": "状态", "property": "value", "equals": "已保存"},
                    "summary": "点击保存"
                },
                {
                    "operation": "set_value",
                    "target": {"role": "ControlType.Edit", "label": "缩放"},
                    "value": "125",
                    "expected": {"role": "ControlType.Edit", "label": "缩放", "property": "value", "equals": "125"},
                    "summary": "将缩放设置为 125"
                }
            ]
        });
        parse_verified_task_arguments(&value.to_string()).expect("valid proposal")
    }

    fn observation(sequence: u64, nodes: Vec<ale_core::harness::Node>) -> Observation {
        Observation {
            sequence,
            window: window(),
            nodes,
        }
    }

    fn node(id: &str, role: &str, label: &str, enabled: bool) -> ale_core::harness::Node {
        ale_core::harness::Node {
            id: id.to_string(),
            role: role.to_string(),
            label: label.to_string(),
            enabled,
            properties: BTreeMap::new(),
        }
    }

    fn nodes() -> Vec<ale_core::harness::Node> {
        vec![
            node("0/1", "ControlType.Button", "保存", true),
            node("0/2", "ControlType.Edit", "缩放", true),
            node("0/3", "ControlType.Text", "状态", true),
        ]
    }

    fn request(step: ale_core::harness::Step, target_id: &str) -> ActionRequest {
        ActionRequest {
            id: "action-1".to_string(),
            observation: observation(1, nodes()),
            step: ale_core::harness::Step {
                target_id: target_id.to_string(),
                ..step
            },
        }
    }

    fn authorize(
        authorizer: &mut ConfirmedTaskAuthorizer,
        task: &Task,
        request: &ActionRequest,
    ) -> Approval {
        authorizer.authorize_inner(task, request)
    }

    #[test]
    fn allows_confirmed_invoke_and_exact_set_value() {
        let proposal = proposal();
        let task = build_task(&proposal, "保存缩放", &window(), true).unwrap();
        let mut authorizer = ConfirmedTaskAuthorizer::new(window(), &proposal);

        let step = proposal.steps[0].to_harness_step("0/1");
        assert!(matches!(
            authorize(&mut authorizer, &task, &request(step, "0/1")),
            Approval::Allow
        ));

        let step = proposal.steps[1].to_harness_step("0/2");
        assert!(matches!(
            authorize(&mut authorizer, &task, &request(step, "0/2")),
            Approval::Allow
        ));
    }

    #[test]
    fn window_change_requires_the_user() {
        let proposal = proposal();
        let task = build_task(&proposal, "保存缩放", &window(), true).unwrap();
        let mut authorizer = ConfirmedTaskAuthorizer::new(window(), &proposal);
        let mut changed = request(proposal.steps[0].to_harness_step("0/1"), "0/1");
        changed.observation.window = WindowIdentity {
            process_id: 7,
            window_id: "2002".to_string(),
        };
        assert!(matches!(
            authorize(&mut authorizer, &task, &changed),
            Approval::NeedsUser
        ));

        let mut other_task = task.clone();
        other_task.window = WindowIdentity {
            process_id: 7,
            window_id: "2002".to_string(),
        };
        let unchanged = request(proposal.steps[0].to_harness_step("0/1"), "0/1");
        assert!(matches!(
            authorize(&mut authorizer, &other_task, &unchanged),
            Approval::NeedsUser
        ));
    }

    #[test]
    fn different_target_or_value_requires_the_user() {
        let proposal = proposal();
        let task = build_task(&proposal, "保存缩放", &window(), true).unwrap();
        let mut authorizer = ConfirmedTaskAuthorizer::new(window(), &proposal);

        // Same postcondition, but the observation resolves the target id to
        // another control: the confirmed role/label no longer match.
        let swapped = request(proposal.steps[0].to_harness_step("0/3"), "0/3");
        assert!(matches!(
            authorize(&mut authorizer, &task, &swapped),
            Approval::NeedsUser
        ));

        // Exact replacement value tampered with.
        let mut tampered = proposal.steps[1].to_harness_step("0/2");
        if let Operation::SetValue { value } = &mut tampered.operation {
            *value = "500".to_string();
        }
        assert!(matches!(
            authorize(&mut authorizer, &task, &request(tampered, "0/2")),
            Approval::NeedsUser
        ));

        // Postcondition tampered with.
        let mut tampered = proposal.steps[1].to_harness_step("0/2");
        tampered.expected[0].equals = "500".to_string();
        assert!(matches!(
            authorize(&mut authorizer, &task, &request(tampered, "0/2")),
            Approval::NeedsUser
        ));
    }

    #[test]
    fn unknown_disabled_or_out_of_grant_targets_require_the_user() {
        let proposal = proposal();
        let task = build_task(&proposal, "保存缩放", &window(), true).unwrap();
        let mut authorizer = ConfirmedTaskAuthorizer::new(window(), &proposal);

        // Missing target id.
        let step = proposal.steps[1].to_harness_step("0/99");
        assert!(matches!(
            authorize(&mut authorizer, &task, &request(step, "0/99")),
            Approval::NeedsUser
        ));

        // Disabled target.
        let mut disabled = request(proposal.steps[0].to_harness_step("0/1"), "0/1");
        for node in &mut disabled.observation.nodes {
            if node.id == "0/1" {
                node.enabled = false;
            }
        }
        assert!(matches!(
            authorize(&mut authorizer, &task, &disabled),
            Approval::NeedsUser
        ));

        // Operation removed from the task grant.
        let mut narrowed = task.clone();
        narrowed.allowed_operations = vec![OperationKind::Invoke];
        let set_value = request(proposal.steps[1].to_harness_step("0/2"), "0/2");
        assert!(matches!(
            authorize(&mut authorizer, &narrowed, &set_value),
            Approval::NeedsUser
        ));
    }

    #[test]
    fn empty_step_postconditions_never_match() {
        let proposal = proposal();
        let task = build_task(&proposal, "保存缩放", &window(), true).unwrap();
        let mut authorizer = ConfirmedTaskAuthorizer::new(window(), &proposal);
        let mut step = proposal.steps[0].to_harness_step("0/1");
        step.expected.clear();
        assert!(matches!(
            authorize(&mut authorizer, &task, &request(step, "0/1")),
            Approval::NeedsUser
        ));
    }
}
