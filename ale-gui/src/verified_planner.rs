//! Deterministic planner for user-confirmed verified tasks.
//!
//! The user confirms a fixed, ordered step list; [`ScriptedPlanner`] executes
//! exactly that list. It never invents a step, never widens a grant and never
//! repeats an input the runner reports as already dispatched (except an exact
//! `SetValue` retry, which is repeat-safe by contract). When the confirmed
//! target is missing, ambiguous or disabled it hands the task back to the user
//! instead of guessing.

use crate::verified_task::{VerifiedStep, VerifiedTarget};
use ale_core::harness::{
    verify, Attempt, Decision, Node, Observation, Operation, OperationKind, Planner,
    PlannerContext, Receipt, Task, Verdict,
};
use std::future::Future;
use std::pin::Pin;

pub struct ScriptedPlanner {
    steps: Vec<VerifiedStep>,
    next: usize,
}

impl ScriptedPlanner {
    pub fn new(steps: Vec<VerifiedStep>) -> Self {
        Self { steps, next: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.steps.len().saturating_sub(self.next)
    }

    fn decide_inner(
        &mut self,
        task: &Task,
        observation: &Observation,
        attempts: &[Attempt],
    ) -> Decision {
        if observation.window != task.window {
            return Decision::NeedHelp {
                reason: "前台窗口已变化".to_string(),
            };
        }
        while self.next < self.steps.len() {
            let step = &self.steps[self.next];
            if step_holds(observation, step) {
                self.next += 1;
                continue;
            }
            let matches = matching_nodes(observation, &step.target);
            let target = match matches.as_slice() {
                [] => {
                    return Decision::NeedHelp {
                        reason: format!("未找到已确认的控件{}", step.target.describe()),
                    }
                }
                [target] => *target,
                many => {
                    return Decision::NeedHelp {
                        reason: format!(
                            "{}存在 {} 个同名控件，无法安全确定目标",
                            step.target.describe(),
                            many.len()
                        ),
                    }
                }
            };
            if !target.enabled {
                return Decision::NeedHelp {
                    reason: format!("{}当前不可用", step.target.describe()),
                };
            }
            if let Some(attempt) = last_attempt_for_step(attempts, step) {
                if matches!(attempt.receipt, Receipt::Dispatched)
                    && attempt.verdict != Verdict::Satisfied
                {
                    match step.operation {
                        // An exact replacement is repeat-safe: rewriting the same
                        // value cannot append or compound input.
                        OperationKind::SetValue => {}
                        // A dispatch without verified effect must not be clicked
                        // again; the runner normally ends the task first.
                        OperationKind::Invoke => {
                            return Decision::NeedHelp {
                                reason: format!(
                                    "{}的点击已发出但未能验证，不重复点击",
                                    step.target.describe()
                                ),
                            }
                        }
                    }
                }
                if matches!(attempt.receipt, Receipt::Uncertain(_)) {
                    return Decision::NeedHelp {
                        reason: "上一次输入结果不确定，需要人工确认".to_string(),
                    };
                }
            }
            return Decision::Act {
                step: step.to_harness_step(&target.id),
            };
        }
        // Every confirmed step was dispatched. The runner still owns the final
        // goal check; a Finish here is a claim without evidence and is turned
        // into NeedsUser by the runner.
        Decision::Finish
    }
}

impl Planner for ScriptedPlanner {
    // Hand-desugared `#[async_trait]` implementation: the deterministic planner
    // needs no async work and this keeps `ale-gui` free of a new dependency.
    fn decide<'life0, 'life1, 'async_trait>(
        &'life0 mut self,
        context: PlannerContext<'life1>,
    ) -> Pin<Box<dyn Future<Output = Result<Decision, String>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            Ok(self.decide_inner(context.task, context.observation, context.attempts))
        })
    }
}

fn matching_nodes<'a>(observation: &'a Observation, target: &VerifiedTarget) -> Vec<&'a Node> {
    observation
        .nodes
        .iter()
        .filter(|node| node.role == target.role && node.label == target.label)
        .collect()
}

fn step_holds(observation: &Observation, step: &VerifiedStep) -> bool {
    verify(observation, std::slice::from_ref(&step.expected)) == Verdict::Satisfied
}

fn last_attempt_for_step<'a>(attempts: &'a [Attempt], step: &VerifiedStep) -> Option<&'a Attempt> {
    attempts
        .iter()
        .rev()
        .find(|attempt| attempt_is_for_step(attempt, step))
}

fn attempt_is_for_step(attempt: &Attempt, step: &VerifiedStep) -> bool {
    attempt.step.expected.len() == 1
        && attempt.step.expected[0] == step.expected
        && match (&attempt.step.operation, step.operation) {
            (Operation::Invoke, OperationKind::Invoke) => true,
            (Operation::SetValue { value }, OperationKind::SetValue) => {
                step.value.as_deref() == Some(value.as_str())
            }
            _ => false,
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verified_authorizer::ConfirmedTaskAuthorizer;
    use crate::verified_task::{build_task, parse_verified_task_arguments, VerifiedProposal};
    use ale_core::harness::{
        run, ActionRequest, Environment, Event, Limits, Node, Predicate, Receipt, Status,
        WindowIdentity,
    };
    use serde_json::json;
    use std::collections::BTreeMap;
    use tokio::sync::watch;

    fn window() -> WindowIdentity {
        WindowIdentity {
            process_id: 100,
            window_id: "42".to_string(),
        }
    }

    fn node(id: &str, role: &str, label: &str, enabled: bool, props: &[(&str, &str)]) -> Node {
        Node {
            id: id.to_string(),
            role: role.to_string(),
            label: label.to_string(),
            enabled,
            properties: props
                .iter()
                .map(|(key, value)| (key.to_string(), value.to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    fn save_button(id: &str) -> Node {
        node(id, "ControlType.Button", "保存", true, &[])
    }

    fn status(value: &str) -> Node {
        node("0/2", "ControlType.Text", "状态", true, &[("value", value)])
    }

    fn saved_predicate() -> Predicate {
        Predicate {
            role: "ControlType.Text".to_string(),
            label: "状态".to_string(),
            property: "value".to_string(),
            equals: "已保存".to_string(),
        }
    }

    fn proposal_json() -> serde_json::Value {
        json!({
            "goal": "保存设置",
            "explanation": "点击保存并核对状态。",
            "risk_level": "low",
            "steps": [{
                "operation": "invoke",
                "target": {"role": "ControlType.Button", "label": "保存"},
                "expected": {"role": "ControlType.Text", "label": "状态", "property": "value", "equals": "已保存"},
                "summary": "点击保存"
            }]
        })
    }

    fn proposal() -> VerifiedProposal {
        parse_verified_task_arguments(&proposal_json().to_string()).expect("valid proposal")
    }

    fn task(proposal: &VerifiedProposal) -> Task {
        build_task(proposal, "保存设置", &window(), true).expect("task")
    }

    fn observation(sequence: u64, nodes: Vec<Node>) -> Observation {
        Observation {
            sequence,
            window: window(),
            nodes,
        }
    }

    async fn decide(
        planner: &mut ScriptedPlanner,
        task: &Task,
        observation: &Observation,
        attempts: &[Attempt],
    ) -> Decision {
        planner
            .decide(PlannerContext {
                task,
                observation,
                attempts,
            })
            .await
            .expect("planner")
    }

    fn attempt_for(
        step: &VerifiedStep,
        action_id: &str,
        receipt: Receipt,
        verdict: Verdict,
    ) -> Attempt {
        Attempt {
            action_id: action_id.to_string(),
            step: step.to_harness_step("0/1"),
            receipt,
            verdict,
            observation_changed: false,
        }
    }

    #[tokio::test]
    async fn acts_in_confirmed_order_and_skips_verified_steps() {
        let mut value = proposal_json();
        value["steps"] = json!([
            {
                "operation": "invoke",
                "target": {"role": "ControlType.Button", "label": "打开"},
                "expected": {"role": "ControlType.Text", "label": "状态", "property": "value", "equals": "已打开"},
                "summary": "点击打开"
            },
            {
                "operation": "invoke",
                "target": {"role": "ControlType.Button", "label": "保存"},
                "expected": {"role": "ControlType.Text", "label": "状态", "property": "value", "equals": "已保存"},
                "summary": "点击保存"
            }
        ]);
        let proposal =
            parse_verified_task_arguments(&value.to_string()).expect("valid multi-step proposal");
        let task = build_task(&proposal, "保存设置", &window(), true).unwrap();
        // The first step already holds, so it must be skipped without dispatch.
        let first = observation(
            1,
            vec![
                save_button("0/1"),
                node("0/3", "ControlType.Button", "打开", true, &[]),
                status("已打开"),
            ],
        );
        let mut planner = ScriptedPlanner::new(proposal.steps.clone());
        let decision = decide(&mut planner, &task, &first, &[]).await;
        match decision {
            Decision::Act { step } => {
                assert_eq!(step.explanation, "点击保存");
                assert_eq!(step.target_id, "0/1");
            }
            other => panic!("expected act, got {other:?}"),
        }
        assert_eq!(planner.next, 1);

        // After dispatch the state satisfies the goal; planner finishes.
        let after = observation(
            2,
            vec![
                node("0/3", "ControlType.Button", "打开", true, &[]),
                status("已保存"),
            ],
        );
        assert!(matches!(
            decide(&mut planner, &task, &after, &[]).await,
            Decision::Finish
        ));
    }

    #[tokio::test]
    async fn zero_or_multiple_matches_hand_back_to_user() {
        let proposal = proposal();
        let task = task(&proposal);
        let mut planner = ScriptedPlanner::new(proposal.steps.clone());
        let missing = observation(1, vec![status("未保存")]);
        assert!(matches!(
            decide(&mut planner, &task, &missing, &[]).await,
            Decision::NeedHelp { .. }
        ));

        let ambiguous = observation(
            1,
            vec![save_button("0/1"), save_button("0/4"), status("未保存")],
        );
        assert!(matches!(
            decide(&mut planner, &task, &ambiguous, &[]).await,
            Decision::NeedHelp { .. }
        ));

        let disabled = observation(
            1,
            vec![
                node("0/1", "ControlType.Button", "保存", false, &[]),
                status("未保存"),
            ],
        );
        assert!(matches!(
            decide(&mut planner, &task, &disabled, &[]).await,
            Decision::NeedHelp { .. }
        ));
    }

    #[tokio::test]
    async fn dispatched_invoke_without_verified_effect_is_not_repeated() {
        let proposal = proposal();
        let task = task(&proposal);
        let step = &proposal.steps[0];
        let attempts = vec![attempt_for(
            step,
            "a1",
            Receipt::Dispatched,
            Verdict::Unsatisfied,
        )];
        let mut planner = ScriptedPlanner::new(proposal.steps.clone());
        let state = observation(1, vec![save_button("0/1"), status("未保存")]);
        assert!(matches!(
            decide(&mut planner, &task, &state, &attempts).await,
            Decision::NeedHelp { .. }
        ));
        // The planner did not advance or dispatch.
        assert_eq!(planner.next, 0);
    }

    #[tokio::test]
    async fn not_applied_set_value_is_retried_at_a_fresh_target_id() {
        let mut value = proposal_json();
        value["steps"] = json!([{
            "operation": "set_value",
            "target": {"role": "ControlType.Edit", "label": "缩放"},
            "value": "125",
            "expected": {"role": "ControlType.Edit", "label": "缩放", "property": "value", "equals": "125"},
            "summary": "将缩放设置为 125"
        }]);
        let proposal =
            parse_verified_task_arguments(&value.to_string()).expect("valid set_value proposal");
        let task = build_task(&proposal, "修改缩放", &window(), true).unwrap();
        let step = &proposal.steps[0];
        let attempts = vec![attempt_for(
            step,
            "a1",
            Receipt::NotApplied("target moved".to_string()),
            Verdict::Unknown,
        )];
        let mut planner = ScriptedPlanner::new(proposal.steps.clone());
        let state = observation(
            1,
            vec![node(
                "0/7",
                "ControlType.Edit",
                "缩放",
                true,
                &[("value", "100")],
            )],
        );
        match decide(&mut planner, &task, &state, &attempts).await {
            Decision::Act { step } => assert_eq!(step.target_id, "0/7"),
            other => panic!("expected retry, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn exhausted_script_reports_finish_not_success() {
        let mut value = proposal_json();
        value["steps"][0]["expected"]["equals"] = json!("从未出现");
        let proposal = parse_verified_task_arguments(&value.to_string()).unwrap();
        let task = build_task(&proposal, "保存设置", &window(), true).unwrap();
        let mut planner = ScriptedPlanner::new(proposal.steps.clone());
        // Pretend every step was dispatched: planner exhausts the script.
        planner.next = planner.steps.len();
        let state = observation(1, vec![save_button("0/1"), status("未保存")]);
        assert!(matches!(
            decide(&mut planner, &task, &state, &[]).await,
            Decision::Finish
        ));
    }

    struct MockEnvironment {
        window: WindowIdentity,
        nodes: Vec<Node>,
        sequence: u64,
        mode: MockMode,
        observed: usize,
        dispatched: usize,
    }

    enum MockMode {
        /// Clicking save changes the status text, so the goal becomes observable.
        ApplyOnInvoke,
        /// Report dispatched but never change the UI.
        Silent,
    }

    impl MockEnvironment {
        fn new(nodes: Vec<Node>, mode: MockMode) -> Self {
            Self {
                window: window(),
                nodes,
                sequence: 0,
                mode,
                observed: 0,
                dispatched: 0,
            }
        }
    }

    impl Environment for MockEnvironment {
        fn observe<'life0, 'async_trait>(
            &'life0 mut self,
        ) -> Pin<Box<dyn Future<Output = Result<Observation, String>> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                self.sequence += 1;
                self.observed += 1;
                Ok(Observation {
                    sequence: self.sequence,
                    window: self.window.clone(),
                    nodes: self.nodes.clone(),
                })
            })
        }

        fn execute<'life0, 'life1, 'async_trait>(
            &'life0 mut self,
            request: &'life1 ActionRequest,
        ) -> Pin<Box<dyn Future<Output = Receipt> + Send + 'async_trait>>
        where
            'life0: 'async_trait,
            'life1: 'async_trait,
            Self: 'async_trait,
        {
            Box::pin(async move {
                self.dispatched += 1;
                if matches!(self.mode, MockMode::ApplyOnInvoke)
                    && matches!(request.step.operation, Operation::Invoke)
                {
                    if let Some(node) = self.nodes.iter_mut().find(|node| node.label == "状态") {
                        node.properties
                            .insert("value".to_string(), "已保存".to_string());
                    }
                }
                Receipt::Dispatched
            })
        }
    }

    async fn run_to_report(
        proposal: &VerifiedProposal,
        environment: &mut MockEnvironment,
        cancel_first: bool,
    ) -> (Status, usize) {
        let task = build_task(proposal, "保存设置", &window(), true).unwrap();
        let mut planner = ScriptedPlanner::new(proposal.steps.clone());
        let mut authorizer = ConfirmedTaskAuthorizer::new(window(), proposal);
        let (cancel, cancellation) = watch::channel(false);
        if cancel_first {
            cancel.send(true).unwrap();
        }
        let report = run(
            &task,
            &mut planner,
            environment,
            &mut authorizer,
            cancellation,
        )
        .await;
        (report.status, environment.dispatched)
    }

    #[tokio::test]
    async fn full_run_completes_when_the_goal_is_observed() {
        let proposal = proposal();
        let mut environment = MockEnvironment::new(
            vec![save_button("0/1"), status("未保存")],
            MockMode::ApplyOnInvoke,
        );
        let (status, dispatched) = run_to_report(&proposal, &mut environment, false).await;
        assert_eq!(status, Status::Completed);
        assert_eq!(dispatched, 1);
        assert!(environment.observed >= 2);
    }

    #[tokio::test]
    async fn full_run_needs_user_when_the_target_is_ambiguous() {
        let proposal = proposal();
        let mut environment = MockEnvironment::new(
            vec![save_button("0/1"), save_button("0/4"), status("未保存")],
            MockMode::ApplyOnInvoke,
        );
        let (status, dispatched) = run_to_report(&proposal, &mut environment, false).await;
        assert_eq!(status, Status::NeedsUser);
        assert_eq!(dispatched, 0);
    }

    #[tokio::test]
    async fn full_run_reports_outcome_unknown_for_unverified_click() {
        let proposal = proposal();
        let mut environment =
            MockEnvironment::new(vec![save_button("0/1"), status("未保存")], MockMode::Silent);
        let (status, dispatched) = run_to_report(&proposal, &mut environment, false).await;
        assert_eq!(status, Status::OutcomeUnknown);
        assert_eq!(dispatched, 1);
    }

    #[tokio::test]
    async fn full_run_cancelled_before_input() {
        let proposal = proposal();
        let mut environment = MockEnvironment::new(
            vec![save_button("0/1"), status("未保存")],
            MockMode::ApplyOnInvoke,
        );
        let (status, dispatched) = run_to_report(&proposal, &mut environment, true).await;
        assert_eq!(status, Status::Cancelled);
        assert_eq!(dispatched, 0);
    }

    #[tokio::test]
    async fn full_run_wrong_foreground_window_hands_back_without_input() {
        let proposal = proposal();
        let mut environment = MockEnvironment::new(
            vec![save_button("0/1"), status("未保存")],
            MockMode::ApplyOnInvoke,
        );
        let task = build_task(&proposal, "保存设置", &window(), true).unwrap();
        let mut planner = ScriptedPlanner::new(proposal.steps.clone());
        let mut authorizer = ConfirmedTaskAuthorizer::new(window(), &proposal);
        let mut stale_task = task.clone();
        stale_task.window = WindowIdentity {
            process_id: 100,
            window_id: "999".to_string(),
        };
        let (_cancel, cancellation) = watch::channel(false);
        let report = run(
            &stale_task,
            &mut planner,
            &mut environment,
            &mut authorizer,
            cancellation,
        )
        .await;
        assert_eq!(report.status, Status::NeedsUser);
        assert_eq!(environment.dispatched, 0);
        assert!(matches!(report.events.last(), Some(Event::Stopped { .. })));
    }

    #[tokio::test]
    async fn limits_stay_within_runner_contract() {
        let proposal = proposal();
        let task = build_task(&proposal, "保存设置", &window(), true).unwrap();
        assert_eq!(task.limits.max_actions, Limits::default().max_actions);
        assert!(task.limits.settle_ms < task.limits.call_timeout_ms);
        assert_eq!(task.success, vec![saved_predicate()]);
    }
}
