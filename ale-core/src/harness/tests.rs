use super::*;
use simulation::{predicate, task, Desktop, Scenario, SettingsGrant, SettingsPlanner};

async fn run_with(
    task: &Task,
    planner: &mut dyn Planner,
    desktop: &mut dyn Environment,
    auth: &mut dyn Authorizer,
) -> Report {
    let (_sender, receiver) = watch::channel(false);
    run(task, planner, desktop, auth, receiver).await
}

async fn scenario(scenario: Scenario) -> (Report, Desktop) {
    let mut desktop = Desktop::new(scenario);
    let report = run_with(
        &task("zoom").unwrap(),
        &mut SettingsPlanner,
        &mut desktop,
        &mut SettingsGrant,
    )
    .await;
    (report, desktop)
}

#[tokio::test]
async fn three_goals_require_save_and_observed_applied_values() {
    for (name, value) in [("zoom", "125"), ("theme", "dark"), ("mute", "true")] {
        let task = task(name).unwrap();
        let mut desktop = Desktop::new(Scenario::Normal);
        let report = run_with(
            &task,
            &mut SettingsPlanner,
            &mut desktop,
            &mut SettingsGrant,
        )
        .await;
        assert_eq!(report.status, Status::Completed, "{}", report.reason);
        assert_eq!(desktop.applied(name), Some(value));
        assert_eq!(desktop.applied_inputs, 4); // menu, settings, draft edit, save
        assert_eq!(report.attempts.len(), 4);
        assert!(report
            .attempts
            .iter()
            .all(|a| a.verdict == Verdict::Satisfied));
        assert!(report.events.iter().any(|e| matches!(
            e,
            Event::GoalChecked {
                verdict: Verdict::Unknown,
                ..
            }
        )));
        assert!(matches!(
            report.events[report.events.len() - 2],
            Event::GoalChecked {
                verdict: Verdict::Satisfied,
                ..
            }
        ));
        let encoded = serde_json::to_vec(&report).unwrap();
        let decoded: Report = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.status, Status::Completed);
    }
}

#[tokio::test]
async fn known_no_input_failure_reobserves_and_recovers() {
    let (report, desktop) = scenario(Scenario::Recover).await;
    assert_eq!(report.status, Status::Completed);
    assert_eq!(desktop.dispatches, 5);
    assert_eq!(desktop.applied_inputs, 4);
    assert!(matches!(report.attempts[0].receipt, Receipt::NotApplied(_)));
    assert!(
        report
            .events
            .iter()
            .filter(|e| matches!(e, Event::Observed { .. }))
            .count()
            > desktop.dispatches
    );
}

#[tokio::test]
async fn repeated_no_progress_stops_without_infinite_retries() {
    let (report, desktop) = scenario(Scenario::NoProgress).await;
    assert_eq!(report.status, Status::NeedsUser);
    assert_eq!(desktop.dispatches, 2);
    assert_eq!(desktop.applied_inputs, 0);
}

#[tokio::test]
async fn uncertain_or_silent_non_idempotent_input_is_never_replayed() {
    for scenario_id in [Scenario::LostReceipt, Scenario::SilentClick] {
        let (report, desktop) = scenario(scenario_id).await;
        assert_eq!(report.status, Status::OutcomeUnknown);
        assert_eq!(desktop.dispatches, 1);
        assert_eq!(desktop.applied("zoom"), Some("100"));
    }
}

#[tokio::test]
async fn lost_save_receipt_can_be_resolved_only_by_actual_goal_evidence() {
    let (report, desktop) = scenario(Scenario::LostReceiptAfterSave).await;
    assert_eq!(report.status, Status::Completed);
    assert_eq!(desktop.applied_inputs, 4);
    assert!(matches!(
        report.attempts.last().unwrap().receipt,
        Receipt::Uncertain(_)
    ));
    assert_eq!(desktop.applied("zoom"), Some("125"));
}

#[tokio::test]
async fn delayed_save_polls_without_issuing_a_second_save() {
    let (report, desktop) = scenario(Scenario::DelayedSave).await;
    assert_eq!(report.status, Status::Completed);
    assert_eq!(desktop.applied_inputs, 4);
    assert!(report.events.iter().any(|e| matches!(
        e,
        Event::StepChecked {
            verdict: Verdict::Unknown,
            ..
        }
    )));
}

#[tokio::test]
async fn foreground_change_after_input_is_an_unknown_outcome() {
    let (report, desktop) = scenario(Scenario::WindowChanged).await;
    assert_eq!(report.status, Status::OutcomeUnknown);
    assert_eq!(desktop.dispatches, 1);
}

struct FixedPlanner(Decision);
#[async_trait]
impl Planner for FixedPlanner {
    async fn decide(&mut self, _: PlannerContext<'_>) -> Result<Decision, String> {
        Ok(self.0.clone())
    }
}

struct FixedAuth(Approval);
#[async_trait]
impl Authorizer for FixedAuth {
    async fn authorize(&mut self, _: &Task, _: &ActionRequest) -> Approval {
        self.0.clone()
    }
}

fn first_step() -> Step {
    Step {
        target_id: "menu".into(),
        operation: Operation::Invoke,
        expected: vec![predicate("Settings", "visible", "true")],
        explanation: "Open menu".into(),
    }
}

#[tokio::test]
async fn model_cannot_declare_success_without_goal_evidence() {
    let mut desktop = Desktop::new(Scenario::Normal);
    let report = run_with(
        &task("zoom").unwrap(),
        &mut FixedPlanner(Decision::Finish),
        &mut desktop,
        &mut SettingsGrant,
    )
    .await;
    assert_eq!(report.status, Status::NeedsUser);
    assert_eq!(desktop.dispatches, 0);
}

#[tokio::test]
async fn denied_or_pending_approval_never_dispatches_input() {
    for (approval, status) in [
        (Approval::Deny, Status::Cancelled),
        (Approval::NeedsUser, Status::NeedsUser),
    ] {
        let mut desktop = Desktop::new(Scenario::Normal);
        let report = run_with(
            &task("zoom").unwrap(),
            &mut SettingsPlanner,
            &mut desktop,
            &mut FixedAuth(approval),
        )
        .await;
        assert_eq!(report.status, status);
        assert_eq!(desktop.dispatches, 0);
    }
}

#[tokio::test]
async fn model_cannot_widen_operation_grant_even_if_authorizer_allows() {
    let mut task = task("zoom").unwrap();
    task.allowed_operations = vec![OperationKind::SetValue];
    let mut desktop = Desktop::new(Scenario::Normal);
    let report = run_with(
        &task,
        &mut SettingsPlanner,
        &mut desktop,
        &mut FixedAuth(Approval::Allow),
    )
    .await;
    assert_eq!(report.status, Status::NeedsUser);
    assert_eq!(desktop.dispatches, 0);
}

#[tokio::test]
async fn absent_target_empty_evidence_and_already_true_postcondition_are_rejected() {
    let mut missing = first_step();
    missing.target_id = "imaginary-button".into();
    let mut empty = first_step();
    empty.expected.clear();
    let mut true_already = first_step();
    true_already.expected = vec![predicate("Menu", "visible", "true")];
    for step in [missing, empty, true_already] {
        let mut desktop = Desktop::new(Scenario::Normal);
        let report = run_with(
            &task("zoom").unwrap(),
            &mut FixedPlanner(Decision::Act { step }),
            &mut desktop,
            &mut FixedAuth(Approval::Allow),
        )
        .await;
        assert_eq!(report.status, Status::NeedsUser);
        assert_eq!(desktop.dispatches, 0);
    }
}

#[tokio::test]
async fn action_budget_stops_before_extra_input() {
    let mut task = task("zoom").unwrap();
    task.limits.max_actions = 2;
    let mut desktop = Desktop::new(Scenario::Normal);
    let report = run_with(
        &task,
        &mut SettingsPlanner,
        &mut desktop,
        &mut SettingsGrant,
    )
    .await;
    assert_eq!(report.status, Status::BudgetExceeded);
    assert_eq!(desktop.dispatches, 2);
    assert_eq!(desktop.applied("zoom"), Some("100"));
}

#[tokio::test]
async fn already_completed_task_needs_no_model_call_or_action() {
    let mut task = task("zoom").unwrap();
    task.success[0].equals = "100".into();
    let mut desktop = Desktop::new(Scenario::Normal);
    let report = run_with(
        &task,
        &mut FixedPlanner(Decision::NeedHelp {
            reason: "must not be called".into(),
        }),
        &mut desktop,
        &mut SettingsGrant,
    )
    .await;
    assert_eq!(report.status, Status::Completed);
    assert_eq!(report.decisions, 0);
    assert_eq!(desktop.dispatches, 0);
}

#[tokio::test]
async fn unknown_is_not_false_or_success_and_all_goal_conditions_are_required() {
    let mut desktop = Desktop::new(Scenario::Normal);
    let mut observation = desktop.observe().await.unwrap();
    let condition = predicate("Applied zoom", "value", "100");
    assert_eq!(
        verify(&observation, std::slice::from_ref(&condition)),
        Verdict::Satisfied
    );
    assert_eq!(verify(&observation, &[]), Verdict::Unknown);
    assert_eq!(
        verify(&observation, &[predicate("Missing", "value", "100")]),
        Verdict::Unknown
    );
    assert_eq!(
        verify(&observation, &[predicate("Applied zoom", "missing", "100")]),
        Verdict::Unknown
    );
    assert_eq!(
        verify(
            &observation,
            &[
                condition.clone(),
                predicate("Applied theme", "value", "dark")
            ]
        ),
        Verdict::Unsatisfied
    );
    observation.nodes.push(
        observation
            .nodes
            .iter()
            .find(|n| n.label == "Applied zoom")
            .unwrap()
            .clone(),
    );
    assert_eq!(verify(&observation, &[condition]), Verdict::Unknown);
}

struct AlteredDesktop {
    inner: Desktop,
    alteration: &'static str,
}
#[async_trait]
impl Environment for AlteredDesktop {
    async fn observe(&mut self) -> Result<Observation, String> {
        let mut observation = self.inner.observe().await?;
        match self.alteration {
            "stale" => observation.sequence = 1,
            "duplicate" => observation.nodes.push(observation.nodes[0].clone()),
            "disabled" => observation.nodes[0].enabled = false,
            "preflight-change" if self.inner.observations == 2 => {
                observation.nodes[0].label = "Changed menu".into()
            }
            "always-change" => {
                observation.nodes[0].label = format!("Menu {}", observation.sequence)
            }
            "wrong-window" => observation.window.process_id = 999,
            "post-read-error" if self.inner.dispatches > 0 => {
                return Err("capture unavailable".into())
            }
            _ => {}
        }
        Ok(observation)
    }
    async fn execute(&mut self, request: &ActionRequest) -> Receipt {
        self.inner.execute(request).await
    }
}

#[tokio::test]
async fn stale_duplicate_disabled_and_wrong_window_states_never_execute() {
    for alteration in ["stale", "duplicate", "disabled", "wrong-window"] {
        let mut desktop = AlteredDesktop {
            inner: Desktop::new(Scenario::Normal),
            alteration,
        };
        let report = run_with(
            &task("zoom").unwrap(),
            &mut SettingsPlanner,
            &mut desktop,
            &mut SettingsGrant,
        )
        .await;
        assert_ne!(report.status, Status::Completed, "{alteration}");
        assert_eq!(desktop.inner.dispatches, 0, "{alteration}");
    }
}

#[tokio::test]
async fn state_change_during_approval_requires_new_decision_and_approval() {
    let mut desktop = AlteredDesktop {
        inner: Desktop::new(Scenario::Normal),
        alteration: "preflight-change",
    };
    let report = run_with(
        &task("zoom").unwrap(),
        &mut SettingsPlanner,
        &mut desktop,
        &mut SettingsGrant,
    )
    .await;
    assert_eq!(report.status, Status::Completed);
    assert_eq!(report.decisions, 5);
    assert_eq!(desktop.inner.dispatches, 4);
    assert!(report
        .events
        .iter()
        .any(|e| matches!(e, Event::StateChanged)));
}

#[tokio::test]
async fn continuously_changing_ui_exhausts_decisions_without_input() {
    let mut desktop = AlteredDesktop {
        inner: Desktop::new(Scenario::Normal),
        alteration: "always-change",
    };
    let mut task = task("zoom").unwrap();
    task.limits.max_decisions = 3;
    let report = run_with(
        &task,
        &mut SettingsPlanner,
        &mut desktop,
        &mut SettingsGrant,
    )
    .await;
    assert_eq!(report.status, Status::BudgetExceeded);
    assert_eq!(desktop.inner.dispatches, 0);
}

#[tokio::test]
async fn capture_failure_after_input_preserves_uncertainty() {
    let mut desktop = AlteredDesktop {
        inner: Desktop::new(Scenario::Normal),
        alteration: "post-read-error",
    };
    let report = run_with(
        &task("zoom").unwrap(),
        &mut SettingsPlanner,
        &mut desktop,
        &mut SettingsGrant,
    )
    .await;
    assert_eq!(report.status, Status::OutcomeUnknown);
    assert_eq!(desktop.inner.dispatches, 1);
}

struct HangingPlanner;
#[async_trait]
impl Planner for HangingPlanner {
    async fn decide(&mut self, _: PlannerContext<'_>) -> Result<Decision, String> {
        std::future::pending().await
    }
}

#[tokio::test(start_paused = true)]
async fn model_deadline_is_bounded_before_any_input() {
    let mut desktop = Desktop::new(Scenario::Normal);
    let report = run_with(
        &task("zoom").unwrap(),
        &mut HangingPlanner,
        &mut desktop,
        &mut SettingsGrant,
    )
    .await;
    assert_eq!(report.status, Status::BudgetExceeded);
    assert_eq!(desktop.dispatches, 0);
}

struct SlowApproval;
#[async_trait]
impl Authorizer for SlowApproval {
    async fn authorize(&mut self, _: &Task, _: &ActionRequest) -> Approval {
        tokio::time::sleep(Duration::from_millis(3)).await;
        Approval::Allow
    }
}

#[tokio::test(start_paused = true)]
async fn total_deadline_covers_multiple_successful_calls_and_pending_approval() {
    let mut task = task("zoom").unwrap();
    task.limits.task_timeout_ms = 5;
    let mut desktop = Desktop::new(Scenario::Normal);
    let report = run_with(&task, &mut SettingsPlanner, &mut desktop, &mut SlowApproval).await;
    assert_eq!(report.status, Status::BudgetExceeded);
    assert_eq!(desktop.dispatches, 1);
    assert_eq!(report.elapsed_ms, 5);
}

struct SilentSetOnce {
    inner: Desktop,
    suppressed: bool,
}
#[async_trait]
impl Environment for SilentSetOnce {
    async fn observe(&mut self) -> Result<Observation, String> {
        self.inner.observe().await
    }
    async fn execute(&mut self, request: &ActionRequest) -> Receipt {
        if !self.suppressed && matches!(request.step.operation, Operation::SetValue { .. }) {
            self.suppressed = true;
            return Receipt::Dispatched;
        }
        self.inner.execute(request).await
    }
}

#[tokio::test]
async fn exact_set_value_can_recover_after_unchanged_readback_without_duplicate_append() {
    let mut desktop = SilentSetOnce {
        inner: Desktop::new(Scenario::Normal),
        suppressed: false,
    };
    let report = run_with(
        &task("zoom").unwrap(),
        &mut SettingsPlanner,
        &mut desktop,
        &mut SettingsGrant,
    )
    .await;
    assert_eq!(report.status, Status::Completed);
    assert_eq!(report.actions, 5);
    assert_eq!(desktop.inner.applied("zoom"), Some("125"));
    let failed = &report.attempts[2];
    assert_eq!(failed.receipt, Receipt::Dispatched);
    assert_eq!(failed.verdict, Verdict::Unsatisfied);
    assert_eq!(report.attempts[3].verdict, Verdict::Satisfied);
}

#[tokio::test(start_paused = true)]
async fn cancellation_interrupts_inflight_model_without_waiting_for_timeout() {
    let (sender, receiver) = watch::channel(false);
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(5)).await;
        sender.send(true).unwrap();
    });
    let start = Instant::now();
    let mut desktop = Desktop::new(Scenario::Normal);
    let report = run(
        &task("zoom").unwrap(),
        &mut HangingPlanner,
        &mut desktop,
        &mut SettingsGrant,
        receiver,
    )
    .await;
    assert_eq!(report.status, Status::Cancelled);
    assert!(start.elapsed() < Duration::from_millis(100));
    assert_eq!(desktop.dispatches, 0);
}

struct HangingInput {
    inner: Desktop,
    cancellation: Option<watch::Sender<bool>>,
}
#[async_trait]
impl Environment for HangingInput {
    async fn observe(&mut self) -> Result<Observation, String> {
        self.inner.observe().await
    }
    async fn execute(&mut self, request: &ActionRequest) -> Receipt {
        self.inner.execute(request).await;
        if let Some(sender) = &self.cancellation {
            sender.send(true).unwrap();
        }
        std::future::pending().await
    }
}

#[tokio::test(start_paused = true)]
async fn execution_timeout_and_cancellation_are_unknown_not_safe_failures() {
    for cancel in [false, true] {
        let (sender, receiver) = watch::channel(false);
        let mut desktop = HangingInput {
            inner: Desktop::new(Scenario::Normal),
            cancellation: cancel.then(|| sender.clone()),
        };
        let report = run(
            &task("zoom").unwrap(),
            &mut SettingsPlanner,
            &mut desktop,
            &mut SettingsGrant,
            receiver,
        )
        .await;
        assert_eq!(report.status, Status::OutcomeUnknown);
        assert_eq!(desktop.inner.applied_inputs, 1);
        assert!(report.events.iter().any(|e| matches!(
            e,
            Event::Executed {
                receipt: Receipt::Uncertain(_),
                ..
            }
        )));
        drop(sender);
    }
}

#[tokio::test]
async fn cancellation_before_start_and_dropped_host_handle_prevent_all_observation() {
    for cancel in [false, true] {
        let (sender, receiver) = watch::channel(cancel);
        if !cancel {
            drop(sender);
        }
        let mut desktop = Desktop::new(Scenario::Normal);
        let report = run(
            &task("zoom").unwrap(),
            &mut SettingsPlanner,
            &mut desktop,
            &mut SettingsGrant,
            receiver,
        )
        .await;
        assert_eq!(report.status, Status::Cancelled);
        assert_eq!(desktop.observations, 0);
    }
}

#[tokio::test]
async fn invalid_goal_and_limits_fail_before_observation() {
    for variant in 0..3 {
        let mut task = task("zoom").unwrap();
        match variant {
            0 => task.success.clear(),
            1 => task.limits.max_actions = 0,
            _ => task.limits.task_timeout_ms = u64::MAX,
        }
        let mut desktop = Desktop::new(Scenario::Normal);
        let report = run_with(
            &task,
            &mut SettingsPlanner,
            &mut desktop,
            &mut SettingsGrant,
        )
        .await;
        assert_eq!(report.status, Status::Failed);
        assert_eq!(desktop.observations, 0);
    }
}

#[tokio::test]
async fn cloud_disclosure_requires_explicit_task_permission() {
    let api = crate::cloud::RemoteCloudApi::new(crate::cloud::CloudConfig {
        api_url: "http://127.0.0.1:1".into(),
        ..Default::default()
    });
    let mut planner = cloud::CloudPlanner::new(std::sync::Arc::new(api));
    let mut desktop = Desktop::new(Scenario::Normal);
    let report = run_with(
        &task("zoom").unwrap(),
        &mut planner,
        &mut desktop,
        &mut SettingsGrant,
    )
    .await;
    assert_eq!(report.status, Status::NeedsUser);
    assert_eq!(desktop.observations, 0);
}

fn cloud_response(arguments: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"choices":[{"finish_reason":"tool_calls","message":{"content":null,"tool_calls":[{"id":"decision","type":"function","function":{"name":"next_task_step","arguments":arguments.to_string()}}]}}]})
}

#[tokio::test]
async fn real_cloud_adapter_rejects_malformed_or_overbroad_decisions() {
    for arguments in [
        serde_json::json!({"decision":"act","step":{"target_id":"menu","operation":{"kind":"shell","command":"anything"},"expected":[],"explanation":"bad"}}),
        serde_json::json!({"decision":"finish","override_goal":true}),
        serde_json::json!({"decision":"act","step":{"target_id":"menu","operation":{"kind":"invoke"},"expected":[],"explanation":"bad"}}),
    ] {
        let (url, captured) =
            crate::model_api::tests::mock_http("200 OK", cloud_response(arguments), "").await;
        let api = crate::cloud::RemoteCloudApi::new(crate::cloud::CloudConfig {
            api_url: url,
            ..Default::default()
        });
        let mut planner = cloud::CloudPlanner::new(std::sync::Arc::new(api));
        let mut task = task("zoom").unwrap();
        task.allow_cloud = true;
        let mut desktop = Desktop::new(Scenario::Normal);
        let report = run_with(&task, &mut planner, &mut desktop, &mut SettingsGrant).await;
        assert_eq!(report.status, Status::Failed);
        assert_eq!(desktop.dispatches, 0);
        captured.await.unwrap();
    }
}

#[tokio::test]
async fn cloud_http_adapter_drives_full_loop_from_fresh_state_and_attempt_history() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut sequences = Vec::new();
        for index in 0..4 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut raw = Vec::new();
            let body = loop {
                let mut buffer = [0; 4096];
                let count = socket.read(&mut buffer).await.unwrap();
                assert!(count > 0);
                raw.extend_from_slice(&buffer[..count]);
                if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&raw[..end]).to_lowercase();
                    let length: usize = headers
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .unwrap()
                        .trim()
                        .parse()
                        .unwrap();
                    if raw.len() >= end + 4 + length {
                        break raw[end + 4..end + 4 + length].to_vec();
                    }
                }
            };
            let request: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(request["tool_choice"], "required");
            let context: serde_json::Value =
                serde_json::from_str(request["messages"][1]["content"].as_str().unwrap()).unwrap();
            let task: Task = serde_json::from_value(context["task"].clone()).unwrap();
            let observation: Observation =
                serde_json::from_value(context["observation"].clone()).unwrap();
            let attempts: Vec<Attempt> =
                serde_json::from_value(context["attempts"].clone()).unwrap();
            assert_eq!(attempts.len(), index);
            sequences.push(observation.sequence);
            let decision = SettingsPlanner
                .decide(PlannerContext {
                    task: &task,
                    observation: &observation,
                    attempts: &attempts,
                })
                .await
                .unwrap();
            let response = cloud_response(serde_json::to_value(decision).unwrap()).to_string();
            let headers = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len());
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
        }
        assert!(sequences.windows(2).all(|w| w[0] < w[1]));
    });
    let api = crate::cloud::RemoteCloudApi::new(crate::cloud::CloudConfig {
        api_url: format!("http://{address}/v1"),
        ..Default::default()
    });
    let mut planner = cloud::CloudPlanner::new(std::sync::Arc::new(api));
    let mut task = task("zoom").unwrap();
    task.allow_cloud = true;
    let mut desktop = Desktop::new(Scenario::Normal);
    let report = run_with(&task, &mut planner, &mut desktop, &mut SettingsGrant).await;
    if report.status != Status::Completed {
        server.abort();
    }
    assert_eq!(report.status, Status::Completed, "{}", report.reason);
    assert_eq!(desktop.applied("zoom"), Some("125"));
    tokio::time::timeout(Duration::from_secs(2), server)
        .await
        .unwrap()
        .unwrap();
}
