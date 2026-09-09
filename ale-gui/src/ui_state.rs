use ale_core::remote::{DecisionKind, RemoteMessage};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

#[derive(Clone)]
pub enum Control {
    Pause(bool),
    Disconnect,
    Reply(RemoteMessage),
}

#[derive(Clone)]
pub struct Decision {
    pub request: String,
    pub id: Option<String>,
    pub kind: String,
    pub text: String,
    pub expires: Instant,
}

#[derive(Clone)]
pub struct Session {
    pub name: String,
    pub address: String,
    pub paused: bool,
    pub task: String,
    pub output: String,
    pub latency: Option<(u64, Instant)>,
    pub decision: Option<Decision>,
    pub controls: mpsc::Sender<Control>,
}

#[derive(Clone)]
pub struct Activity {
    pub time: u64,
    pub session: String,
    pub event: String,
}

#[derive(Default)]
pub struct Snapshot {
    pub sessions: BTreeMap<String, Session>,
    pub logs: VecDeque<Activity>,
    pub mute_speech: bool,
}

#[derive(Clone, Default)]
pub struct UiHub(pub Arc<Mutex<Snapshot>>);

impl UiHub {
    pub fn log(&self, id: &str, event: &str) {
        let mut state = self.0.lock().unwrap();
        state.logs.push_front(Activity {
            time: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            session: id.to_string(),
            event: event.to_string(),
        });
        state.logs.truncate(200);
    }

    pub fn observe(&self, id: &str, message: &RemoteMessage) {
        let mut state = self.0.lock().unwrap();
        let Some(session) = state.sessions.get_mut(id) else {
            return;
        };
        let event =
            match message {
                RemoteMessage::ProgressUpdate(update) => {
                    session.task = format!("{:?}", update.stage);
                    format!("{:?}", update.stage)
                }
                RemoteMessage::CommandPreview(preview) => {
                    session.output = preview.response_text.clone();
                    session.task = "PreviewReady".into();
                    if preview.has_plan {
                        session.decision = Some(Decision {
                            request: preview.request_id.clone(),
                            id: None,
                            kind: "ConfirmPlan".into(),
                            text: format!(
                                "{}\n{}",
                                preview.confirmation_text,
                                preview.action_steps.join("\n")
                            ),
                            expires: Instant::now() + Duration::from_secs(120),
                        });
                    }
                    "PreviewReady".to_string()
                }
                RemoteMessage::DecisionRequest(request) => {
                    session.decision = Some(Decision {
                        request: request.request_id.clone(),
                        id: Some(request.decision_id.clone()),
                        kind: match request.kind {
                            DecisionKind::UseRemoteModel => "UseRemoteModel",
                            DecisionKind::UploadFullScreenshot => "UploadFullScreenshot",
                            DecisionKind::RiskChanged => "RiskChanged",
                        }
                        .into(),
                        text: request.prompt.clone(),
                        expires: Instant::now()
                            + Duration::from_secs(request.expires_in_seconds.into()),
                    });
                    session.task = "AwaitingDecision".into();
                    "AwaitingDecision".to_string()
                }
                RemoteMessage::AssistantOutput(output) => {
                    // display_text is the protocol's redacted display representation.
                    session.output = output.display_text.clone();
                    return;
                }
                RemoteMessage::ExecutionStatus(status) => {
                    session.task = format!("{:?}", status.state);
                    if session
                        .decision
                        .as_ref()
                        .is_some_and(|decision| decision.request == status.request_id)
                    {
                        session.decision = None;
                    }
                    format!("{:?}", status.state)
                }
                RemoteMessage::Error(error) => {
                    if !session.paused {
                        session.task = if error.code == "CANCELLED" {
                            "Cancelled"
                        } else {
                            "Failed"
                        }
                        .into();
                    }
                    session.output = error.message.clone();
                    if session.decision.as_ref().is_some_and(|decision| {
                        error.request_id.as_deref() == Some(&decision.request)
                    }) {
                        session.decision = None;
                    }
                    "Failed".to_string()
                }
                _ => return,
            };
        drop(state);
        self.log(id, &event);
    }
}

pub struct SessionGuard(pub UiHub, pub String);
impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.0 .0.lock().unwrap().sessions.remove(&self.1);
        self.0.log(&self.1, "Disconnected");
    }
}

tokio::task_local! {
    pub static OBSERVER: (UiHub, String);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unrelated_error_does_not_dismiss_a_pending_confirmation() {
        let hub = UiHub::default();
        let (controls, _) = mpsc::channel(8);
        hub.0.lock().unwrap().sessions.insert(
            "phone".into(),
            Session {
                name: "Phone".into(),
                address: "127.0.0.1".into(),
                paused: false,
                task: "AwaitingDecision".into(),
                output: String::new(),
                latency: None,
                controls,
                decision: Some(Decision {
                    request: "current".into(),
                    id: None,
                    kind: "HighRiskPlan".into(),
                    text: "Confirm the complete plan".into(),
                    expires: Instant::now() + Duration::from_secs(120),
                }),
            },
        );
        let error = |request: &str| {
            RemoteMessage::Error(ale_core::remote::RemoteError {
                request_id: Some(request.into()),
                code: "CANCELLED".into(),
                message: "Cancelled".into(),
            })
        };
        hub.observe("phone", &error("old-request"));
        assert!(hub.0.lock().unwrap().sessions["phone"].decision.is_some());
        hub.observe("phone", &error("current"));
        assert!(hub.0.lock().unwrap().sessions["phone"].decision.is_none());
    }

    #[test]
    fn log_is_bounded_and_session_drop_removes_connection() {
        let hub = UiHub::default();
        for _ in 0..250 {
            hub.log("one", "Connected");
        }
        assert_eq!(hub.0.lock().unwrap().logs.len(), 200);
        let (controls, _) = mpsc::channel(8);
        hub.0.lock().unwrap().sessions.insert(
            "one".into(),
            Session {
                name: "Phone".into(),
                address: "127.0.0.1".into(),
                paused: false,
                task: "Ready".into(),
                output: String::new(),
                latency: None,
                decision: None,
                controls,
            },
        );
        drop(SessionGuard(hub.clone(), "one".into()));
        assert!(hub.0.lock().unwrap().sessions.is_empty());
    }
}
