//! Deterministic desktop simulator and state-driven reference planner.
//! This is a contract demonstration, not evidence about a real model or UIA.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scenario {
    Normal,
    Recover,
    NoProgress,
    LostReceipt,
    LostReceiptAfterSave,
    SilentClick,
    DelayedSave,
    WindowChanged,
}

impl std::str::FromStr for Scenario {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "normal" => Ok(Self::Normal),
            "recover" => Ok(Self::Recover),
            "no-progress" => Ok(Self::NoProgress),
            "lost-receipt" => Ok(Self::LostReceipt),
            "lost-receipt-after-save" => Ok(Self::LostReceiptAfterSave),
            "silent-click" => Ok(Self::SilentClick),
            "delayed-save" => Ok(Self::DelayedSave),
            "window-changed" => Ok(Self::WindowChanged),
            _ => Err("Unknown scenario: use normal, recover, no-progress, lost-receipt, lost-receipt-after-save, silent-click, delayed-save, window-changed".into()),
        }
    }
}

pub fn predicate(label: &str, property: &str, equals: &str) -> Predicate {
    Predicate {
        role: "control".into(),
        label: label.into(),
        property: property.into(),
        equals: equals.into(),
    }
}

pub fn task(name: &str) -> Result<Task, String> {
    let (label, value) = match name {
        "zoom" => ("Applied zoom", "125"),
        "theme" => ("Applied theme", "dark"),
        "mute" => ("Applied mute", "true"),
        _ => return Err("Unknown demo task: use zoom, theme or mute".into()),
    };
    Ok(Task {
        id: format!("simulation-{name}"),
        goal: format!("Set {label} to {value} and save"),
        window: WindowIdentity {
            process_id: 42,
            window_id: "simulated-settings-window".into(),
        },
        success: vec![predicate(label, "value", value)],
        allowed_operations: vec![OperationKind::Invoke, OperationKind::SetValue],
        allow_cloud: false,
        limits: Limits {
            settle_ms: 1,
            ..Default::default()
        },
    })
}

fn node(id: &str, label: &str, property: &str, value: &str) -> Node {
    Node {
        id: id.into(),
        role: "control".into(),
        label: label.into(),
        enabled: true,
        properties: [(property.into(), value.into())].into(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Home,
    Menu,
    Settings,
}

pub struct Desktop {
    pub scenario: Scenario,
    pub dispatches: usize,
    pub applied_inputs: usize,
    pub observations: usize,
    sequence: u64,
    page: Page,
    applied: BTreeMap<String, String>,
    draft: BTreeMap<String, String>,
    pending_save: Option<usize>,
    last: Option<Observation>,
}

impl Desktop {
    pub fn new(scenario: Scenario) -> Self {
        let applied = [
            ("zoom".into(), "100".into()),
            ("theme".into(), "light".into()),
            ("mute".into(), "false".into()),
        ]
        .into();
        Self {
            scenario,
            dispatches: 0,
            applied_inputs: 0,
            observations: 0,
            sequence: 0,
            page: Page::Home,
            draft: BTreeMap::new(),
            applied,
            pending_save: None,
            last: None,
        }
    }

    pub fn applied(&self, name: &str) -> Option<&str> {
        self.applied.get(name).map(String::as_str)
    }

    fn nodes(&self) -> Vec<Node> {
        match self.page {
            Page::Home => {
                let mut nodes = vec![node("menu", "Menu", "visible", "true")];
                nodes.extend(self.applied.iter().map(|(key, value)| {
                    node(
                        &format!("applied-{key}"),
                        &format!("Applied {key}"),
                        "value",
                        value,
                    )
                }));
                nodes
            }
            Page::Menu => vec![node("settings", "Settings", "visible", "true")],
            Page::Settings => {
                let mut nodes = vec![
                    node("panel", "Settings panel", "open", "true"),
                    node("save", "Save", "visible", "true"),
                ];
                nodes.extend(self.draft.iter().map(|(key, value)| {
                    node(
                        &format!("draft-{key}"),
                        &format!("Draft {key}"),
                        "value",
                        value,
                    )
                }));
                nodes
            }
        }
    }

    fn save(&mut self) {
        self.applied = self.draft.clone();
        self.page = Page::Home;
    }
}

#[async_trait]
impl Environment for Desktop {
    async fn observe(&mut self) -> Result<Observation, String> {
        self.observations += 1;
        self.sequence += 1;
        if let Some(remaining) = self.pending_save.as_mut() {
            *remaining -= 1;
            if *remaining == 0 {
                self.pending_save = None;
                self.save();
            }
        }
        let mut window = task("zoom")?.window;
        if self.scenario == Scenario::WindowChanged && self.dispatches > 0 {
            window.window_id = "another-window".into();
        }
        let observation = Observation {
            sequence: self.sequence,
            window,
            nodes: self.nodes(),
        };
        self.last = Some(observation.clone());
        Ok(observation)
    }

    async fn execute(&mut self, request: &ActionRequest) -> Receipt {
        self.dispatches += 1;
        if self.last.as_ref() != Some(&request.observation)
            || self.nodes() != request.observation.nodes
        {
            return Receipt::NotApplied("Target changed before input".into());
        }
        if self.scenario == Scenario::NoProgress
            || (self.scenario == Scenario::Recover && self.dispatches == 1)
        {
            return Receipt::NotApplied("Injected temporary target failure".into());
        }
        if self.scenario == Scenario::LostReceipt {
            return Receipt::Uncertain("Injected transport ambiguity".into());
        }
        if self.scenario == Scenario::SilentClick {
            return Receipt::Dispatched;
        }
        let is_save = request.step.target_id == "save";
        match (
            &request.step.operation,
            request.step.target_id.as_str(),
            self.page,
        ) {
            (Operation::Invoke, "menu", Page::Home) => self.page = Page::Menu,
            (Operation::Invoke, "settings", Page::Menu) => {
                self.page = Page::Settings;
                self.draft = self.applied.clone();
            }
            (Operation::SetValue { value }, id, Page::Settings) if id.starts_with("draft-") => {
                let Some(field) = self.draft.get_mut(&id[6..]) else {
                    return Receipt::NotApplied("Unknown setting".into());
                };
                *field = value.clone();
            }
            (Operation::Invoke, "save", Page::Settings) => {
                if self.scenario == Scenario::DelayedSave {
                    self.pending_save = Some(2);
                } else {
                    self.save();
                }
            }
            _ => return Receipt::NotApplied("Unsupported action in current state".into()),
        }
        self.applied_inputs += 1;
        if is_save && self.scenario == Scenario::LostReceiptAfterSave {
            Receipt::Uncertain("Save acknowledged late".into())
        } else {
            Receipt::Dispatched
        }
    }
}

/// A reference planner driven by observations, not a queue of canned decisions.
pub struct SettingsPlanner;

#[async_trait]
impl Planner for SettingsPlanner {
    async fn decide(&mut self, context: PlannerContext<'_>) -> Result<Decision, String> {
        let goal = context.task.success.first().ok_or("No goal")?;
        let name = goal.label.strip_prefix("Applied ").ok_or("Unknown goal")?;
        let nodes = &context.observation.nodes;
        let (id, operation, expected) = if nodes.iter().any(|n| n.id == "menu") {
            (
                "menu".into(),
                Operation::Invoke,
                predicate("Settings", "visible", "true"),
            )
        } else if nodes.iter().any(|n| n.id == "settings") {
            (
                "settings".into(),
                Operation::Invoke,
                predicate("Settings panel", "open", "true"),
            )
        } else if let Some(field) = nodes.iter().find(|n| n.id == format!("draft-{name}")) {
            if field.properties.get("value") != Some(&goal.equals) {
                (
                    field.id.clone(),
                    Operation::SetValue {
                        value: goal.equals.clone(),
                    },
                    predicate(&field.label, "value", &goal.equals),
                )
            } else {
                ("save".into(), Operation::Invoke, goal.clone())
            }
        } else {
            return Ok(Decision::NeedHelp {
                reason: "No supported control visible".into(),
            });
        };
        Ok(Decision::Act {
            step: Step {
                target_id: id,
                operation,
                expected: vec![expected],
                explanation: "Advance the explicitly scoped simulated settings task".into(),
            },
        })
    }
}

/// Grants only the demo's menu/settings/save controls and the goal's exact value.
/// Never use this simulator authorizer for real desktop input.
pub struct SettingsGrant;

#[async_trait]
impl Authorizer for SettingsGrant {
    async fn authorize(&mut self, task: &Task, request: &ActionRequest) -> Approval {
        let Some(goal) = task.success.first() else {
            return Approval::Deny;
        };
        let Some(name) = goal.label.strip_prefix("Applied ") else {
            return Approval::Deny;
        };
        let permitted = match &request.step.operation {
            Operation::Invoke => matches!(
                request.step.target_id.as_str(),
                "menu" | "settings" | "save"
            ),
            Operation::SetValue { value } => {
                request.step.target_id == format!("draft-{name}") && value == &goal.equals
            }
        };
        if permitted && request.observation.window == task.window {
            Approval::Allow
        } else {
            Approval::Deny
        }
    }
}

pub async fn demo(name: &str, scenario: Scenario) -> Result<Report, String> {
    let task = task(name)?;
    let (_sender, receiver) = watch::channel(false);
    Ok(run(
        &task,
        &mut SettingsPlanner,
        &mut Desktop::new(scenario),
        &mut SettingsGrant,
        receiver,
    )
    .await)
}
