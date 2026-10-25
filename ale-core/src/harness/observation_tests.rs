use super::*;
use simulation::{task, Desktop, Scenario, SettingsGrant, SettingsPlanner};

struct ObservingPlanner;
#[async_trait]
impl Planner for ObservingPlanner {
    async fn decide(&mut self, context: PlannerContext<'_>) -> Result<Decision, String> {
        let mut decision = SettingsPlanner.decide(context).await?;
        if let Decision::Act { step } = &mut decision {
            if matches!(step.operation, Operation::Invoke) {
                step.expected.clear();
            }
        }
        Ok(decision)
    }
}

async fn run_observed(
    name: &str,
    environment: &mut dyn Environment,
    planner: &mut dyn Planner,
) -> Report {
    let mut task = task(name).unwrap();
    task.allow_observation_steps = true;
    let (_sender, receiver) = watch::channel(false);
    run(&task, planner, environment, &mut SettingsGrant, receiver).await
}

#[tokio::test]
async fn unknown_next_page_is_observed_without_inventing_postconditions() {
    for name in ["zoom", "theme", "mute"] {
        let mut desktop = Desktop::new(Scenario::Normal);
        let report = run_observed(name, &mut desktop, &mut ObservingPlanner).await;
        assert_eq!(report.status, Status::Completed, "{}", report.reason);
        assert_eq!(desktop.dispatches, 4);
        for attempt in &report.attempts[..2] {
            assert!(attempt.step.expected.is_empty());
            assert_eq!(attempt.verdict, Verdict::Unknown);
            assert!(attempt.observation_changed);
        }
        assert!(report
            .events
            .iter()
            .any(|event| matches!(event, Event::ObservationChecked { changed: true, .. })));
        assert!(report.events.iter().any(|event| matches!(
            event,
            Event::GoalChecked {
                verdict: Verdict::Satisfied,
                ..
            }
        )));
    }
}

#[tokio::test]
async fn observation_steps_keep_failure_and_uncertainty_matrix() {
    for (scenario, status, actions) in [
        (Scenario::Recover, Status::Completed, 5),
        (Scenario::NoProgress, Status::NeedsUser, 2),
        (Scenario::LostReceipt, Status::OutcomeUnknown, 1),
        (Scenario::SilentClick, Status::OutcomeUnknown, 1),
        (Scenario::WindowChanged, Status::OutcomeUnknown, 1),
        (Scenario::LostReceiptAfterSave, Status::Completed, 4),
        (Scenario::DelayedSave, Status::Completed, 4),
    ] {
        let mut desktop = Desktop::new(scenario);
        let report = run_observed("zoom", &mut desktop, &mut ObservingPlanner).await;
        assert_eq!(report.status, status, "{scenario:?}: {}", report.reason);
        assert_eq!(desktop.dispatches, actions, "{scenario:?}");
    }
}

struct UncertainAfterChange(Desktop);
#[async_trait]
impl Environment for UncertainAfterChange {
    async fn observe(&mut self) -> Result<Observation, String> {
        self.0.observe().await
    }
    async fn execute(&mut self, request: &ActionRequest) -> Receipt {
        self.0.execute(request).await;
        Receipt::Uncertain("Acknowledgement lost after the menu changed".into())
    }
}

#[tokio::test]
async fn a_changed_ui_cannot_clear_an_uncertain_receipt() {
    let mut desktop = UncertainAfterChange(Desktop::new(Scenario::Normal));
    let report = run_observed("zoom", &mut desktop, &mut ObservingPlanner).await;
    assert_eq!(report.status, Status::OutcomeUnknown);
    assert_eq!(desktop.0.dispatches, 1);
    assert!(report.attempts[0].observation_changed);
    assert_eq!(report.attempts[0].verdict, Verdict::Unknown);
}

struct CurrentControlPlanner;
#[async_trait]
impl Planner for CurrentControlPlanner {
    async fn decide(&mut self, context: PlannerContext<'_>) -> Result<Decision, String> {
        Ok(Decision::Act {
            step: Step {
                target_id: context.observation.nodes[0].id.clone(),
                operation: Operation::Invoke,
                expected: vec![],
                explanation: "Inspect the next page".into(),
            },
        })
    }
}

struct MovingIds {
    sequence: u64,
    actions: usize,
    cycle_pages: bool,
}
#[async_trait]
impl Environment for MovingIds {
    async fn observe(&mut self) -> Result<Observation, String> {
        self.sequence += 1;
        let label = if self.cycle_pages && self.actions % 2 == 1 {
            "Back"
        } else {
            "Menu"
        };
        Ok(Observation {
            sequence: self.sequence,
            window: task("zoom")?.window,
            nodes: vec![Node {
                id: format!("ephemeral-{}", self.sequence),
                role: "control".into(),
                label: label.into(),
                enabled: true,
                properties: [("visible".into(), "true".into())].into(),
            }],
        })
    }
    async fn execute(&mut self, _: &ActionRequest) -> Receipt {
        self.actions += 1;
        Receipt::Dispatched
    }
}

struct Allow;
#[async_trait]
impl Authorizer for Allow {
    async fn authorize(&mut self, _: &Task, _: &ActionRequest) -> Approval {
        Approval::Allow
    }
}

// Preflight must be stable while node IDs may change only *after* dispatch.
struct StablePreflight(MovingIds);
#[async_trait]
impl Environment for StablePreflight {
    async fn observe(&mut self) -> Result<Observation, String> {
        let mut observation = self.0.observe().await?;
        observation.nodes[0].id = format!("page-after-{}", self.0.actions);
        Ok(observation)
    }
    async fn execute(&mut self, request: &ActionRequest) -> Receipt {
        self.0.execute(request).await
    }
}

async fn run_synthetic(environment: &mut dyn Environment) -> Report {
    let mut task = task("zoom").unwrap();
    task.allow_observation_steps = true;
    let (_sender, receiver) = watch::channel(false);
    run(
        &task,
        &mut CurrentControlPlanner,
        environment,
        &mut Allow,
        receiver,
    )
    .await
}

#[tokio::test]
async fn fresh_ids_and_sequences_alone_do_not_count_as_progress() {
    let mut environment = StablePreflight(MovingIds {
        sequence: 0,
        actions: 0,
        cycle_pages: false,
    });
    let report = run_synthetic(&mut environment).await;
    assert_eq!(report.status, Status::OutcomeUnknown);
    assert_eq!(environment.0.actions, 1);
    assert!(!report.attempts[0].observation_changed);
}

#[tokio::test]
async fn returning_to_the_same_state_cannot_replay_an_exploratory_click() {
    let mut environment = StablePreflight(MovingIds {
        sequence: 0,
        actions: 0,
        cycle_pages: true,
    });
    let report = run_synthetic(&mut environment).await;
    assert_eq!(report.status, Status::NeedsUser);
    assert_eq!(environment.0.actions, 2);
    assert!(report.reason.contains("already visited state"));
    assert!(report
        .attempts
        .iter()
        .all(|attempt| attempt.observation_changed));
}

#[test]
fn semantic_comparison_ignores_order_and_ids_but_detects_values_and_windows() {
    let base = Observation {
        sequence: 1,
        window: task("zoom").unwrap().window,
        nodes: vec![
            Node {
                id: "1".into(),
                role: "edit".into(),
                label: "Zoom".into(),
                enabled: true,
                properties: [("value".into(), "100".into())].into(),
            },
            Node {
                id: "2".into(),
                role: "button".into(),
                label: "Menu".into(),
                enabled: true,
                properties: BTreeMap::new(),
            },
        ],
    };
    let mut after = base.clone();
    after.sequence += 1;
    after.nodes.reverse();
    for node in &mut after.nodes {
        node.id.push_str("-new");
    }
    assert!(same_ui_state(&base, &after));
    after.nodes[1]
        .properties
        .insert("value".into(), "125".into());
    assert!(!same_ui_state(&base, &after));
    let mut other = base.clone();
    other.window.window_id.push_str("-other");
    assert!(!same_ui_state(&base, &other));
}

#[tokio::test]
async fn observation_mode_still_rejects_empty_set_value_evidence() {
    let mut task = task("zoom").unwrap();
    task.allow_observation_steps = true;
    let mut planner = tests::FixedPlanner(Decision::Act {
        step: Step {
            target_id: "menu".into(),
            operation: Operation::SetValue {
                value: "125".into(),
            },
            expected: vec![],
            explanation: "Invalid unverified replacement".into(),
        },
    });
    let mut environment = Desktop::new(Scenario::Normal);
    let report = tests::run_with(&task, &mut planner, &mut environment, &mut Allow).await;
    assert_eq!(report.status, Status::NeedsUser);
    assert_eq!(environment.dispatches, 0);
}

#[tokio::test]
async fn a_ui_change_never_counts_as_goal_completion_or_bypasses_approval() {
    struct FinishAfterOne;
    #[async_trait]
    impl Planner for FinishAfterOne {
        async fn decide(&mut self, context: PlannerContext<'_>) -> Result<Decision, String> {
            if context.attempts.is_empty() {
                ObservingPlanner.decide(context).await
            } else {
                Ok(Decision::Finish)
            }
        }
    }
    let mut environment = Desktop::new(Scenario::Normal);
    let report = run_observed("zoom", &mut environment, &mut FinishAfterOne).await;
    assert_eq!(report.status, Status::NeedsUser);
    assert_eq!(environment.dispatches, 1);
    assert_eq!(environment.applied("zoom"), Some("100"));
    let mut task = task("zoom").unwrap();
    task.allow_observation_steps = true;
    let mut environment = Desktop::new(Scenario::Normal);
    let report = tests::run_with(
        &task,
        &mut ObservingPlanner,
        &mut environment,
        &mut tests::FixedAuth(Approval::Deny),
    )
    .await;
    assert_eq!(report.status, Status::Cancelled);
    assert_eq!(environment.dispatches, 0);
}

#[test]
fn old_task_and_attempt_records_keep_strict_defaults() {
    let mut value = serde_json::to_value(task("zoom").unwrap()).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .remove("allow_observation_steps");
    assert!(
        !serde_json::from_value::<Task>(value)
            .unwrap()
            .allow_observation_steps
    );
    let attempt = serde_json::json!({"action_id":"old", "step":tests::first_step(),
        "receipt":{"outcome":"dispatched"}, "verdict":"unknown"});
    assert!(
        !serde_json::from_value::<Attempt>(attempt)
            .unwrap()
            .observation_changed
    );
}

#[tokio::test]
async fn cloud_tool_accepts_unpredicted_invoke_only_when_host_enabled() {
    let arguments = serde_json::json!({"decision":"act", "step":{
        "target_id":"menu", "operation":{"kind":"invoke"}, "expected":[], "explanation":"Observe the menu"}});
    let (url, captured) =
        crate::model_api::tests::mock_http("200 OK", tests::cloud_response(arguments), "").await;
    let api = crate::cloud::RemoteCloudApi::new(crate::cloud::CloudConfig {
        api_url: url,
        ..Default::default()
    });
    let mut planner = cloud::CloudPlanner::new(std::sync::Arc::new(api));
    let mut task = task("zoom").unwrap();
    task.allow_cloud = true;
    task.allow_observation_steps = true;
    let observation = Desktop::new(Scenario::Normal).observe().await.unwrap();
    let decision = planner
        .decide(PlannerContext {
            task: &task,
            observation: &observation,
            attempts: &[],
        })
        .await
        .unwrap();
    assert!(matches!(decision, Decision::Act { step } if step.expected.is_empty()));
    captured.await.unwrap();
}

#[tokio::test]
async fn invented_postconditions_cannot_bypass_the_exploration_replay_guard() {
    struct InventedPostcondition;
    #[async_trait]
    impl Planner for InventedPostcondition {
        async fn decide(&mut self, context: PlannerContext<'_>) -> Result<Decision, String> {
            let repeat = context.attempts.len() >= 2;
            let mut decision = CurrentControlPlanner.decide(context).await?;
            if repeat {
                if let Decision::Act { step } = &mut decision {
                    step.expected =
                        vec![simulation::predicate("Invented panel", "visible", "true")];
                }
            }
            Ok(decision)
        }
    }
    let mut environment = StablePreflight(MovingIds {
        sequence: 0,
        actions: 0,
        cycle_pages: true,
    });
    let mut task = task("zoom").unwrap();
    task.allow_observation_steps = true;
    let (_sender, receiver) = watch::channel(false);
    let report = run(
        &task,
        &mut InventedPostcondition,
        &mut environment,
        &mut Allow,
        receiver,
    )
    .await;
    assert_eq!(report.status, Status::NeedsUser);
    assert_eq!(environment.0.actions, 2);
    assert!(report.reason.contains("already visited state"));
}
