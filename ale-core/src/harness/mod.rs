//! Device-independent observe/decide/authorize/act/verify runtime.
//!
//! A host supplies observations, guarded tools, a planner and authorization.
//! The runner owns success criteria, budgets and evidence; a planner cannot
//! declare success or widen a task's grant. See HARNESS.md for adapter contracts.

pub mod cloud;
pub mod simulation;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::Instant;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WindowIdentity {
    pub process_id: u32,
    pub window_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Node {
    /// Unique within one observation; never reuse without resolving again.
    pub id: String,
    pub role: String,
    pub label: String,
    pub enabled: bool,
    pub properties: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    /// Monotonically increasing acquisition ID, even when the UI is unchanged.
    pub sequence: u64,
    pub window: WindowIdentity,
    pub nodes: Vec<Node>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Predicate {
    pub role: String,
    pub label: String,
    pub property: String,
    pub equals: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Satisfied,
    Unsatisfied,
    Unknown,
}

/// Missing/ambiguous controls and missing properties are unknown, not false.
pub fn verify(observation: &Observation, conditions: &[Predicate]) -> Verdict {
    if conditions.is_empty() {
        return Verdict::Unknown;
    }
    let mut unknown = false;
    let mut unsatisfied = false;
    for condition in conditions {
        let mut matches = observation
            .nodes
            .iter()
            .filter(|node| node.role == condition.role && node.label == condition.label);
        let first = matches.next();
        if matches.next().is_some() {
            unknown = true;
            continue;
        }
        match first.and_then(|node| node.properties.get(&condition.property)) {
            Some(value) if value == &condition.equals => {}
            Some(_) => unsatisfied = true,
            None => unknown = true,
        }
    }
    if unsatisfied {
        Verdict::Unsatisfied
    } else if unknown {
        Verdict::Unknown
    } else {
        Verdict::Satisfied
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    Invoke,
    SetValue,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Invoke,
    /// A replace/set operation, never append or type-at-cursor.
    SetValue {
        value: String,
    },
}

impl Operation {
    fn kind(&self) -> OperationKind {
        match self {
            Self::Invoke => OperationKind::Invoke,
            Self::SetValue { .. } => OperationKind::SetValue,
        }
    }

    fn repeat_safe(&self) -> bool {
        matches!(self, Self::SetValue { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub target_id: String,
    pub operation: Operation,
    pub expected: Vec<Predicate>,
    pub explanation: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
pub enum Decision {
    Act { step: Step },
    Finish,
    NeedHelp { reason: String },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Limits {
    pub max_actions: usize,
    pub max_decisions: usize,
    pub max_consecutive_failures: usize,
    pub verification_reads: usize,
    pub settle_ms: u64,
    pub call_timeout_ms: u64,
    pub task_timeout_ms: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_actions: 12,
            max_decisions: 20,
            max_consecutive_failures: 2,
            verification_reads: 3,
            settle_ms: 200,
            call_timeout_ms: 10_000,
            task_timeout_ms: 120_000,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub goal: String,
    pub window: WindowIdentity,
    /// Host/user-owned, immutable during a run. Never copied from a model's step.
    pub success: Vec<Predicate>,
    pub allowed_operations: Vec<OperationKind>,
    /// Explicit permission to disclose the structured observations to a cloud planner.
    pub allow_cloud: bool,
    pub limits: Limits,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ActionRequest {
    pub id: String,
    pub observation: Observation,
    pub step: Step,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", content = "detail", rename_all = "snake_case")]
pub enum Receipt {
    Dispatched,
    /// Adapter guarantees no input or side effect was issued.
    NotApplied(String),
    /// Input may have been issued; never automatically retry this receipt.
    Uncertain(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attempt {
    pub action_id: String,
    pub step: Step,
    pub receipt: Receipt,
    pub verdict: Verdict,
}

#[derive(Serialize)]
pub struct PlannerContext<'a> {
    pub task: &'a Task,
    pub observation: &'a Observation,
    pub attempts: &'a [Attempt],
}

#[async_trait]
pub trait Planner: Send {
    fn uses_cloud(&self) -> bool {
        false
    }
    async fn decide(&mut self, context: PlannerContext<'_>) -> Result<Decision, String>;
}

#[async_trait]
pub trait Environment: Send {
    /// Acquire new state, not a cached snapshot. Sequence IDs must increase.
    async fn observe(&mut self) -> Result<Observation, String>;

    /// Re-resolve and validate the request's window and target immediately before
    /// issuing input. A changed/ambiguous/disabled target must return NotApplied.
    /// Cancellation/drop must stop further input. If input may already have been
    /// issued, return Uncertain. Do not spawn untracked input-producing work.
    async fn execute(&mut self, request: &ActionRequest) -> Receipt;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    Allow,
    Deny,
    NeedsUser,
}

#[async_trait]
pub trait Authorizer: Send {
    /// This decision is bound to this exact step, observation and task.
    async fn authorize(&mut self, task: &Task, request: &ActionRequest) -> Approval;
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Completed,
    NeedsUser,
    Cancelled,
    BudgetExceeded,
    OutcomeUnknown,
    Failed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Observed {
        observation: Observation,
    },
    GoalChecked {
        sequence: u64,
        verdict: Verdict,
    },
    Decided {
        decision: Decision,
    },
    Authorized {
        action_id: String,
        approval: Approval,
    },
    StateChanged,
    Dispatching {
        request: ActionRequest,
    },
    Executed {
        action_id: String,
        receipt: Receipt,
    },
    StepChecked {
        action_id: String,
        sequence: u64,
        verdict: Verdict,
    },
    Stopped {
        status: Status,
        reason: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub task_id: String,
    pub task: Task,
    pub status: Status,
    pub reason: String,
    pub elapsed_ms: u64,
    pub actions: usize,
    pub decisions: usize,
    pub attempts: Vec<Attempt>,
    pub events: Vec<Event>,
}

struct Run<'a> {
    task: &'a Task,
    started: Instant,
    deadline: Instant,
    cancellation: watch::Receiver<bool>,
    last_sequence: Option<u64>,
    report: Report,
}

#[derive(Debug)]
enum Interrupted {
    Cancelled,
    Timeout,
}

impl Run<'_> {
    async fn bounded<T>(&mut self, future: impl Future<Output = T>) -> Result<T, Interrupted> {
        let until = self
            .deadline
            .min(Instant::now() + Duration::from_millis(self.task.limits.call_timeout_ms));
        tokio::select! {
            biased;
            _ = async {
                loop {
                    if *self.cancellation.borrow_and_update() { break; }
                    if self.cancellation.changed().await.is_err() {
                        // Dropping the host's cancellation handle revokes the run.
                        break;
                    }
                }
            } => Err(Interrupted::Cancelled),
            _ = tokio::time::sleep_until(until) => Err(Interrupted::Timeout),
            result = future => Ok(result),
        }
    }

    fn finish(mut self, status: Status, reason: impl Into<String>) -> Report {
        self.report.elapsed_ms = self.started.elapsed().as_millis() as u64;
        self.report.status = status.clone();
        self.report.reason = reason.into();
        self.report.events.push(Event::Stopped {
            status,
            reason: self.report.reason.clone(),
        });
        self.report
    }

    fn interrupted(self, error: Interrupted, effect_possible: bool) -> Report {
        if effect_possible {
            return self.finish(
                Status::OutcomeUnknown,
                "Interrupted during/after input; do not replay",
            );
        }
        match error {
            Interrupted::Cancelled => self.finish(Status::Cancelled, "Cancelled before input"),
            Interrupted::Timeout => {
                self.finish(Status::BudgetExceeded, "Call or task deadline reached")
            }
        }
    }

    async fn observe(&mut self, environment: &mut dyn Environment) -> Result<Observation, Stop> {
        let observation = self
            .bounded(environment.observe())
            .await
            .map_err(Stop::Interrupted)?
            .map_err(Stop::Invalid)?;
        if self
            .last_sequence
            .is_some_and(|last| observation.sequence <= last)
        {
            return Err(Stop::Invalid("Stale observation sequence".into()));
        }
        let mut ids = std::collections::HashSet::new();
        if observation.nodes.len() > 512
            || observation
                .nodes
                .iter()
                .any(|node| node.id.is_empty() || !ids.insert(&node.id))
        {
            return Err(Stop::Invalid(
                "Invalid or ambiguous observation nodes".into(),
            ));
        }
        self.last_sequence = Some(observation.sequence);
        self.report.events.push(Event::Observed {
            observation: observation.clone(),
        });
        if observation.window != self.task.window {
            return Err(Stop::WrongWindow);
        }
        Ok(observation)
    }

    fn stopped(self, stop: Stop, effect_possible: bool) -> Report {
        match stop {
            Stop::Interrupted(error) => self.interrupted(error, effect_possible),
            _ if effect_possible => self.finish(
                Status::OutcomeUnknown,
                "Cannot verify after input; do not replay",
            ),
            Stop::WrongWindow => self.finish(Status::NeedsUser, "Foreground window changed"),
            Stop::Invalid(reason) => self.finish(Status::Failed, reason),
        }
    }

    fn goal(&mut self, observation: &Observation) -> Verdict {
        let verdict = verify(observation, &self.task.success);
        self.report.events.push(Event::GoalChecked {
            sequence: observation.sequence,
            verdict: verdict.clone(),
        });
        verdict
    }
}

enum Stop {
    Interrupted(Interrupted),
    Invalid(String),
    WrongWindow,
}

fn valid_conditions(conditions: &[Predicate]) -> bool {
    !conditions.is_empty()
        && conditions.len() <= 16
        && conditions.iter().all(|condition| {
            !condition.role.trim().is_empty()
                && !condition.label.trim().is_empty()
                && !condition.property.trim().is_empty()
                && condition.equals.len() <= 4096
        })
}

/// Run one explicitly scoped task. No retries or resume occur outside this call.
pub async fn run(
    task: &Task,
    planner: &mut dyn Planner,
    environment: &mut dyn Environment,
    authorizer: &mut dyn Authorizer,
    cancellation: watch::Receiver<bool>,
) -> Report {
    let mut run = Run {
        task,
        started: Instant::now(),
        deadline: Instant::now() + Duration::from_millis(task.limits.task_timeout_ms.min(600_000)),
        cancellation,
        last_sequence: None,
        report: Report {
            task_id: task.id.clone(),
            task: task.clone(),
            status: Status::Failed,
            reason: String::new(),
            elapsed_ms: 0,
            actions: 0,
            decisions: 0,
            attempts: Vec::new(),
            events: Vec::new(),
        },
    };
    let limits = &task.limits;
    if task.id.trim().is_empty()
        || task.goal.trim().is_empty()
        || task.window.window_id.trim().is_empty()
        || !valid_conditions(&task.success)
        || limits.max_actions == 0
        || limits.max_actions > 100
        || limits.max_decisions == 0
        || limits.max_decisions > 200
        || limits.max_consecutive_failures == 0
        || limits.max_consecutive_failures > 10
        || limits.verification_reads == 0
        || limits.verification_reads > 10
        || limits.call_timeout_ms == 0
        || limits.call_timeout_ms > 120_000
        || limits.task_timeout_ms == 0
        || limits.task_timeout_ms > 600_000
        || limits.settle_ms >= limits.call_timeout_ms
    {
        return run.finish(Status::Failed, "Invalid task contract or limits");
    }
    if planner.uses_cloud() && !task.allow_cloud {
        return run.finish(
            Status::NeedsUser,
            "Cloud observation disclosure is not authorized",
        );
    }
    let mut failures = 0;
    loop {
        let observation = match run.observe(environment).await {
            Ok(observation) => observation,
            Err(stop) => return run.stopped(stop, false),
        };
        if run.goal(&observation) == Verdict::Satisfied {
            return run.finish(
                Status::Completed,
                "Task goal verified from observed properties",
            );
        }
        if run.report.actions >= limits.max_actions || run.report.decisions >= limits.max_decisions
        {
            return run.finish(
                Status::BudgetExceeded,
                "Action or decision budget exhausted",
            );
        }
        // Context is a bounded copy so the runner can independently enforce deadlines.
        let attempts = run.report.attempts.clone();
        run.report.decisions += 1;
        let decision = match run
            .bounded(planner.decide(PlannerContext {
                task,
                observation: &observation,
                attempts: &attempts,
            }))
            .await
        {
            Ok(Ok(decision)) => decision,
            Ok(Err(error)) => {
                return run.finish(Status::Failed, format!("Planner failed: {error}"))
            }
            Err(error) => return run.interrupted(error, false),
        };
        run.report.events.push(Event::Decided {
            decision: decision.clone(),
        });
        let step = match decision {
            Decision::Finish => {
                return run.finish(
                    Status::NeedsUser,
                    "Planner claimed completion without task evidence",
                )
            }
            Decision::NeedHelp { reason } => return run.finish(Status::NeedsUser, reason),
            Decision::Act { step } => step,
        };
        if !task.allowed_operations.contains(&step.operation.kind()) {
            return run.finish(Status::NeedsUser, "Operation is outside the task grant");
        }
        if !valid_conditions(&step.expected)
            || step.explanation.trim().is_empty()
            || matches!(&step.operation, Operation::SetValue { value } if value.len() > 4096)
            || !observation
                .nodes
                .iter()
                .any(|node| node.id == step.target_id && node.enabled)
        {
            return run.finish(
                Status::NeedsUser,
                "Invalid step, disabled target or missing target",
            );
        }
        if verify(&observation, &step.expected) == Verdict::Satisfied {
            return run.finish(
                Status::NeedsUser,
                "Step postcondition already holds; no action justified",
            );
        }
        let mut request = ActionRequest {
            id: format!("{}:{}", task.id, run.report.decisions),
            observation: observation.clone(),
            step,
        };
        let approval = match run.bounded(authorizer.authorize(task, &request)).await {
            Ok(approval) => approval,
            Err(error) => return run.interrupted(error, false),
        };
        run.report.events.push(Event::Authorized {
            action_id: request.id.clone(),
            approval: approval.clone(),
        });
        match approval {
            Approval::Deny => return run.finish(Status::Cancelled, "User denied the action"),
            Approval::NeedsUser => {
                return run.finish(Status::NeedsUser, "Action requires user approval")
            }
            Approval::Allow => {}
        }
        // Approval may have taken a long time. Re-observe before preparing input.
        let fresh = match run.observe(environment).await {
            Ok(fresh) => fresh,
            Err(stop) => return run.stopped(stop, false),
        };
        if fresh.nodes != observation.nodes {
            run.report.events.push(Event::StateChanged);
            continue;
        }
        request.observation = fresh;
        run.report.events.push(Event::Dispatching {
            request: request.clone(),
        });
        run.report.actions += 1;
        let receipt = match run.bounded(environment.execute(&request)).await {
            Ok(receipt) => receipt,
            Err(error) => {
                run.report.events.push(Event::Executed {
                    action_id: request.id,
                    receipt: Receipt::Uncertain(format!("{error:?} during execution")),
                });
                return run.interrupted(error, true);
            }
        };
        run.report.events.push(Event::Executed {
            action_id: request.id.clone(),
            receipt: receipt.clone(),
        });
        let effect_possible = !matches!(receipt, Receipt::NotApplied(_));
        let mut verdict = Verdict::Unknown;
        let mut goal_met = false;
        for read in 0..limits.verification_reads {
            if read > 0 {
                if let Err(error) = run
                    .bounded(tokio::time::sleep(Duration::from_millis(limits.settle_ms)))
                    .await
                {
                    return run.interrupted(error, effect_possible);
                }
            }
            let after = match run.observe(environment).await {
                Ok(after) => after,
                Err(stop) => return run.stopped(stop, effect_possible),
            };
            verdict = verify(&after, &request.step.expected);
            run.report.events.push(Event::StepChecked {
                action_id: request.id.clone(),
                sequence: after.sequence,
                verdict: verdict.clone(),
            });
            goal_met = run.goal(&after) == Verdict::Satisfied;
            if goal_met || verdict == Verdict::Satisfied {
                break;
            }
        }
        let repeat_safe = request.step.operation.repeat_safe();
        run.report.attempts.push(Attempt {
            action_id: request.id,
            step: request.step,
            receipt: receipt.clone(),
            verdict: verdict.clone(),
        });
        if goal_met {
            return run.finish(Status::Completed, "Task goal verified after action");
        }
        if matches!(receipt, Receipt::Uncertain(_)) {
            // Even a weak step postcondition cannot clear an uncertain side effect.
            return run.finish(
                Status::OutcomeUnknown,
                "Input outcome uncertain; task goal not verified; do not replay",
            );
        }
        if verdict == Verdict::Satisfied {
            failures = 0;
        } else {
            failures += 1;
            if effect_possible && !repeat_safe {
                return run.finish(
                    Status::OutcomeUnknown,
                    "Non-idempotent action not verified; do not replay",
                );
            }
            if failures >= limits.max_consecutive_failures {
                return run.finish(
                    Status::NeedsUser,
                    "Repeated actions made no verified progress",
                );
            }
        }
    }
}

#[cfg(test)]
mod tests;
