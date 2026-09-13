use ale_core::child_process::ManagedAsyncChild as Child;
use ale_core::config::{AppConfig, CloudApiConfig};
use ale_core::model_ipc::{
    write_message, IpcEnvelope, IpcReply, IpcReplyStatus, IpcRequestKind, MODEL_IPC_VERSION,
};
use ale_core::model_scheduler::SchedulerConfiguration;
use ale_core::model_scheduler::{
    CancelModelJob, GroundingJob, GroundingResult, JobPrivacy, LocalPlanningJob,
    LocalPlanningResult, ModelCapability, ModelJob, ModelRuntimeConfig, RemoteEndpointConfig,
    RemotePlanningJob, RemotePlanningResult, RemoteProviderSet, SchedulerHealth, SchedulerPriority,
    SpeechRecognitionJob, SpeechRecognitionResult, StateVerificationJob, StateVerificationResult,
};
use base64::Engine;
use rand::RngCore;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::{oneshot, Mutex};

#[cfg(unix)]
type LocalStream = tokio::net::UnixStream;
#[cfg(windows)]
type LocalStream = tokio::net::windows::named_pipe::NamedPipeClient;
struct PendingReply {
    sender: oneshot::Sender<Result<IpcReply, String>>,
    _bytes: tokio::sync::OwnedSemaphorePermit,
    _job: Option<tokio::sync::OwnedSemaphorePermit>,
}
type PendingReplies = HashMap<String, PendingReply>;
struct Outbound {
    envelope: IpcEnvelope,
    queued: tokio::time::Instant,
}
const MAX_PENDING: usize = 64;
const MAX_INFLIGHT_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone)]
pub struct ModeldClient {
    writer: tokio::sync::mpsc::Sender<Outbound>,
    byte_budget: Arc<tokio::sync::Semaphore>,
    job_slots: Arc<tokio::sync::Semaphore>,
    pending: Arc<Mutex<PendingReplies>>,
    alive: Arc<AtomicBool>,
    instance_id: uuid::Uuid,
    configuration: Arc<std::sync::Mutex<Option<SchedulerConfiguration>>>,
    _process: Arc<ModeldProcess>,
}

tokio::task_local! { static REQUEST_CONTEXT: (ModeldClient, RemoteProviderSet, ModelRuntimeConfig); }

struct PendingCall {
    client: ModeldClient,
    id: String,
    cancel: bool,
    complete: bool,
    queued: bool,
}
impl Drop for PendingCall {
    fn drop(&mut self) {
        if self.complete {
            return;
        }
        let client = self.client.clone();
        let id = self.id.clone();
        let cancel = self.cancel;
        let queued = self.queued;
        tokio::spawn(async move {
            if !queued || !cancel {
                client.pending.lock().await.remove(&id);
            }
            // Sent requests retain admission until the peer finishes cancelling
            // or the connection closes. A dropped waiter cannot bypass limits.
            if queued && cancel && client.is_alive() {
                let _ = tokio::time::timeout(Duration::from_secs(3), client.cancel(&id)).await;
            }
        });
    }
}

const MAX_CONSECUTIVE_PROCESS_FAILURES: u8 = 3;

#[derive(Clone)]
pub struct SupervisedModeldClient {
    state: Arc<Mutex<SupervisorState>>,
    health_gate: Arc<Mutex<()>>,
}

struct SupervisorState {
    retired: Option<ModeldClient>,
    health_failures: u8,
    healthy_since: Option<tokio::time::Instant>,
    next_start: tokio::time::Instant,
    last_health: Option<(tokio::time::Instant, Result<SchedulerHealth, String>)>,
    revision: u64,
    config: AppConfig,
    client: Option<ModeldClient>,
    consecutive_failures: u8,
    restart_blocked: bool,
    last_error: Option<String>,
}

struct ModeldProcess {
    child: std::sync::Mutex<Child>,
    #[cfg(unix)]
    endpoint: PathBuf,
}

impl Drop for ModeldProcess {
    fn drop(&mut self) {
        if let Ok(child) = self.child.get_mut() {
            let _ = child.start_kill();
        }
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.endpoint);
    }
}

impl ModeldClient {
    pub async fn start(config: &AppConfig, revision: u64) -> Result<Self, String> {
        tokio::time::timeout(Duration::from_secs(45), Self::start_inner(config, revision))
            .await
            .map_err(|_| "MODELD_START_TIMEOUT".to_string())?
    }
    async fn start_inner(config: &AppConfig, revision: u64) -> Result<Self, String> {
        let endpoint = modeld_endpoint();
        let mut token = vec![0_u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut token);
        let diagnostic = ale_core::diagnostics::current()
            .map(|sink| (sink.directory.clone(), sink.session.clone()));
        let mut child = tokio::task::spawn_blocking(move || {
            let executable = modeld_executable()?;
            let mut command = Command::new(&executable);
            command
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if let Some((directory, session)) = diagnostic {
                command
                    .env("ALE_DIAGNOSTIC_DIRECTORY", directory)
                    .env("ALE_DIAGNOSTIC_SESSION", session);
            }
            Child::spawn(&mut command).map_err(|_| "MODELD_START_FAILED".to_string())
        })
        .await
        .map_err(|_| "MODELD_START_TASK_FAILED".to_string())??;
        let bootstrap = serde_json::json!({
            "endpoint": endpoint,
            "token_base64": base64::engine::general_purpose::STANDARD.encode(&token),
        });
        let mut stdin = child
            .child
            .stdin
            .take()
            .ok_or_else(|| "模型调度器启动管道不可用".to_string())?;
        stdin
            .write_all(format!("{}\n", bootstrap).as_bytes())
            .await
            .map_err(|error| format!("无法初始化模型调度器: {error}"))?;
        stdin
            .shutdown()
            .await
            .map_err(|error| format!("无法关闭模型调度器启动管道: {error}"))?;

        let stream = connect_with_timeout(&endpoint).await?;
        let process = Arc::new(ModeldProcess {
            child: std::sync::Mutex::new(child),
            #[cfg(unix)]
            endpoint: PathBuf::from(&endpoint),
        });
        let client = Self::from_stream(stream, process);
        client.call_raw(IpcRequestKind::Authenticate, token).await?;
        client.configure(config, revision).await?;
        Ok(client)
    }

    fn from_stream(stream: LocalStream, process: Arc<ModeldProcess>) -> Self {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let pending = Arc::new(Mutex::new(PendingReplies::new()));
        let alive = Arc::new(AtomicBool::new(true));
        let byte_budget = Arc::new(tokio::sync::Semaphore::new(MAX_INFLIGHT_BYTES));
        let reader_pending = pending.clone();
        let reader_alive = alive.clone();
        let reader_budget = byte_budget.clone();
        tokio::spawn(async move {
            loop {
                match ale_core::model_ipc::read_message_with_budget::<_, IpcReply>(
                    &mut reader,
                    reader_budget.clone(),
                )
                .await
                {
                    Ok((reply, _reservation)) => {
                        ale_core::diagnostics::record(
                            "ipc_reply",
                            &[
                                (
                                    "operation_id",
                                    ale_core::diagnostics::correlation_id(&reply.request_id),
                                ),
                                ("bytes", reply.payload.len() as u64),
                                ("status", reply.status as u64),
                            ],
                        );
                        if let Some(pending) = reader_pending.lock().await.remove(&reply.request_id)
                        {
                            let _ = pending.sender.send(Ok(reply));
                        }
                    }
                    Err(_) => {
                        reader_alive.store(false, Ordering::Release);
                        for (_, pending) in reader_pending.lock().await.drain() {
                            let _ = pending.sender.send(Err("IPC_CONNECTION_CLOSED".into()));
                        }
                        ale_core::diagnostics::record("ipc_connection_closed", &[]);
                        break;
                    }
                }
            }
        });
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Outbound>(MAX_PENDING);
        let writer_pending = pending.clone();
        let writer_alive = alive.clone();
        let writer_process = Arc::downgrade(&process);
        tokio::spawn(async move {
            while let Some(outbound) = receiver.recv().await {
                if !writer_alive.load(Ordering::Acquire) {
                    break;
                }
                let id = &outbound.envelope.request_id;
                {
                    let mut pending = writer_pending.lock().await;
                    if pending.get(id).is_none_or(|call| call.sender.is_closed()) {
                        pending.remove(id);
                        continue;
                    }
                }
                let started = tokio::time::Instant::now();
                let result = tokio::time::timeout(
                    Duration::from_secs(3),
                    write_message(&mut writer, &outbound.envelope),
                )
                .await;
                let success = matches!(result, Ok(Ok(())));
                ale_core::diagnostics::record(
                    "ipc_frame_write",
                    &[
                        ("operation_id", ale_core::diagnostics::correlation_id(id)),
                        ("bytes", outbound.envelope.payload.len() as u64),
                        (
                            "queue_ms",
                            started.duration_since(outbound.queued).as_millis() as u64,
                        ),
                        ("elapsed_ms", started.elapsed().as_millis() as u64),
                        ("success", success as u64),
                    ],
                );
                if !success {
                    // Once any byte may have been written, retire the transport.
                    writer_alive.store(false, Ordering::Release);
                    if let Some(process) = writer_process.upgrade() {
                        if let Ok(mut child) = process.child.lock() {
                            let _ = child.start_kill();
                        }
                    }
                    break;
                }
            }
            writer_alive.store(false, Ordering::Release);
            receiver.close();
            for (_, pending) in writer_pending.lock().await.drain() {
                let _ = pending
                    .sender
                    .send(Err("IPC_WRITE_FAILED_OR_CLOSED".into()));
            }
        });
        Self {
            writer: sender,
            byte_budget,
            job_slots: Arc::new(tokio::sync::Semaphore::new(32)),
            pending,
            alive,
            instance_id: uuid::Uuid::new_v4(),
            configuration: Arc::new(std::sync::Mutex::new(None)),
            _process: process,
        }
    }

    pub async fn health(&self) -> Result<SchedulerHealth, String> {
        self.call_json(IpcRequestKind::Health, &serde_json::Value::Null)
            .await
    }

    pub async fn remote_plan(
        &self,
        request_id: &str,
        question: String,
        image: Option<&[u8]>,
        tools: Option<Vec<serde_json::Value>>,
    ) -> Result<RemotePlanningResult, String> {
        let planning = RemotePlanningJob {
            question,
            image_base64: image
                .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes)),
            tools,
        };
        let job = ModelJob {
            runtime_snapshot: None,
            remote_snapshot: None,
            request_id: request_id.to_string(),
            capability: ModelCapability::RemotePlanning,
            priority: SchedulerPriority::InteractiveRequest,
            deadline_unix_ms: unix_millis() + 90_000,
            risk_ceiling: ale_core::actions::RiskLevel::High,
            snapshot_id: None,
            privacy: JobPrivacy {
                allow_remote: true,
                allow_full_screenshot: image.is_some(),
                allow_sensitive_content: false,
            },
            payload: serde_json::to_value(planning).map_err(|error| error.to_string())?,
        };
        self.call_json_with_id(request_id, IpcRequestKind::Schedule, &job)
            .await
    }

    pub async fn local_plan(
        &self,
        request_id: &str,
        snapshot_id: &str,
        question: String,
        image: &[u8],
        application_id: Option<String>,
    ) -> Result<LocalPlanningResult, String> {
        let payload = LocalPlanningJob {
            question,
            image_base64: base64::engine::general_purpose::STANDARD.encode(image),
            application_id,
        };
        let job = ModelJob {
            runtime_snapshot: None,
            remote_snapshot: None,
            request_id: request_id.to_string(),
            capability: ModelCapability::LocalPlanning,
            priority: SchedulerPriority::InteractiveRequest,
            deadline_unix_ms: unix_millis() + 90_000,
            risk_ceiling: ale_core::actions::RiskLevel::Medium,
            snapshot_id: Some(snapshot_id.to_string()),
            privacy: JobPrivacy::default(),
            payload: serde_json::to_value(payload).map_err(|error| error.to_string())?,
        };
        self.call_json_with_id(request_id, IpcRequestKind::Schedule, &job)
            .await
    }

    pub async fn ground(
        &self,
        request_id: &str,
        snapshot_id: &str,
        grounding: GroundingJob,
    ) -> Result<GroundingResult, String> {
        let job = ModelJob {
            runtime_snapshot: None,
            remote_snapshot: None,
            request_id: request_id.to_string(),
            capability: ModelCapability::ElementGrounding,
            priority: SchedulerPriority::InteractiveRequest,
            deadline_unix_ms: unix_millis() + 30_000,
            risk_ceiling: ale_core::actions::RiskLevel::Medium,
            snapshot_id: Some(snapshot_id.to_string()),
            privacy: JobPrivacy::default(),
            payload: serde_json::to_value(grounding).map_err(|error| error.to_string())?,
        };
        self.call_json_with_id(request_id, IpcRequestKind::Schedule, &job)
            .await
    }

    #[allow(dead_code)]
    pub async fn verify(
        &self,
        request_id: &str,
        snapshot_id: &str,
        verification: StateVerificationJob,
        image: &[u8],
    ) -> Result<StateVerificationResult, String> {
        let verification = StateVerificationJob {
            image_base64: base64::engine::general_purpose::STANDARD.encode(image),
            ..verification
        };
        let job = ModelJob {
            runtime_snapshot: None,
            remote_snapshot: None,
            request_id: request_id.to_string(),
            capability: ModelCapability::StateVerification,
            priority: SchedulerPriority::StateVerification,
            deadline_unix_ms: unix_millis() + 30_000,
            risk_ceiling: ale_core::actions::RiskLevel::Low,
            snapshot_id: Some(snapshot_id.to_string()),
            privacy: JobPrivacy::default(),
            payload: serde_json::to_value(verification).map_err(|error| error.to_string())?,
        };
        self.call_json_with_id(request_id, IpcRequestKind::Schedule, &job)
            .await
    }

    pub async fn transcribe_wav(
        &self,
        request_id: &str,
        wav: &[u8],
        allow_remote: bool,
    ) -> Result<SpeechRecognitionResult, String> {
        let speech = SpeechRecognitionJob {
            wav_base64: base64::engine::general_purpose::STANDARD.encode(wav),
            allow_remote,
        };
        let job = ModelJob {
            runtime_snapshot: None,
            remote_snapshot: None,
            request_id: request_id.to_string(),
            capability: ModelCapability::SpeechRecognition,
            priority: SchedulerPriority::InteractiveRequest,
            deadline_unix_ms: unix_millis() + 85_000,
            risk_ceiling: ale_core::actions::RiskLevel::Low,
            snapshot_id: None,
            privacy: JobPrivacy {
                allow_remote,
                allow_full_screenshot: false,
                allow_sensitive_content: false,
            },
            payload: serde_json::to_value(speech).map_err(|error| error.to_string())?,
        };
        self.call_json_with_id(request_id, IpcRequestKind::Schedule, &job)
            .await
    }

    pub async fn cancel(&self, target_request_id: &str) -> Result<(), String> {
        let _: serde_json::Value = self
            .call_json(
                IpcRequestKind::Cancel,
                &CancelModelJob {
                    target_request_id: target_request_id.to_string(),
                },
            )
            .await?;
        Ok(())
    }

    async fn configure(&self, config: &AppConfig, revision: u64) -> Result<(), String> {
        let config = config.clone();
        let configuration = tokio::task::spawn_blocking(move || SchedulerConfiguration {
            revision,
            providers: provider_set(&config, revision),
            models: runtime_config(&config),
        })
        .await
        .map_err(|_| "MODELD_CONFIG_TASK_FAILED".to_string())?;
        let _: serde_json::Value = self
            .call_json(IpcRequestKind::ConfigureScheduler, &configuration)
            .await?;
        *self.configuration.lock().unwrap() = Some(configuration);
        Ok(())
    }

    async fn call_json<T: serde::de::DeserializeOwned>(
        &self,
        kind: IpcRequestKind,
        value: &impl Serialize,
    ) -> Result<T, String> {
        self.call_json_with_id(&uuid::Uuid::new_v4().to_string(), kind, value)
            .await
    }

    async fn call_json_with_id<T: serde::de::DeserializeOwned>(
        &self,
        request_id: &str,
        kind: IpcRequestKind,
        value: &impl Serialize,
    ) -> Result<T, String> {
        let payload = serde_json::to_vec(value).map_err(|error| error.to_string())?;
        let reply = self.call(request_id, kind, payload).await?;
        serde_json::from_slice(&reply.payload).map_err(|error| error.to_string())
    }

    async fn call_raw(&self, kind: IpcRequestKind, payload: Vec<u8>) -> Result<IpcReply, String> {
        self.call(&uuid::Uuid::new_v4().to_string(), kind, payload)
            .await
    }

    async fn call(
        &self,
        request_id: &str,
        kind: IpcRequestKind,
        mut payload: Vec<u8>,
    ) -> Result<IpcReply, String> {
        let deadline = ale_core::model_api::deadline();
        if tokio::time::Instant::now() >= deadline {
            return Err("DEADLINE_EXCEEDED".into());
        }
        // Each stage has its own IPC ID so a late response or cancellation can
        // never match a later stage of the same phone request.
        let stage_id = format!("{request_id}:{}", uuid::Uuid::new_v4());
        let request_id = if kind == IpcRequestKind::Schedule {
            stage_id.as_str()
        } else {
            request_id
        };
        if kind == IpcRequestKind::Schedule {
            let mut job: ModelJob = serde_json::from_slice(&payload).map_err(|e| e.to_string())?;
            job.request_id = request_id.to_string();
            job.deadline_unix_ms = job.deadline_unix_ms.min(
                unix_millis()
                    + deadline
                        .saturating_duration_since(tokio::time::Instant::now())
                        .as_millis() as i64,
            );
            let configured = self.configuration.lock().unwrap().clone();
            job.runtime_snapshot = REQUEST_CONTEXT
                .try_with(|(_, _, runtime)| runtime.clone())
                .ok()
                .or_else(|| configured.as_ref().map(|config| config.models.clone()))
                .or(job.runtime_snapshot);
            job.remote_snapshot = REQUEST_CONTEXT
                .try_with(|(_, providers, _)| providers.clone())
                .ok()
                .or_else(|| configured.as_ref().map(|config| config.providers.clone()))
                .or(job.remote_snapshot);
            payload = serde_json::to_vec(&job).map_err(|e| e.to_string())?;
        }
        if !self.is_alive() {
            return Err("IPC_CONNECTION_CLOSED".into());
        }
        let job = if kind == IpcRequestKind::Schedule {
            Some(
                self.job_slots
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| "SCHEDULER_BUSY")?,
            )
        } else {
            None
        };
        let envelope = IpcEnvelope {
            protocol_version: MODEL_IPC_VERSION,
            request_id: request_id.into(),
            kind: kind as i32,
            payload,
        };
        let bytes = ale_core::model_ipc::frame_size(&envelope);
        if bytes > ale_core::model_ipc::MAX_MODEL_IPC_MESSAGE_BYTES {
            return Err("IPC_FRAME_TOO_LARGE".into());
        }
        let reservation = self
            .byte_budget
            .clone()
            .try_acquire_many_owned(bytes as u32)
            .map_err(|_| "IPC_INFLIGHT_LIMIT")?;
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.pending.lock().await;
            if pending.contains_key(request_id) {
                return Err("DUPLICATE_REQUEST_ID".into());
            }
            if pending.len() >= MAX_PENDING {
                return Err("IPC_QUEUE_FULL".into());
            }
            pending.insert(
                request_id.to_string(),
                PendingReply {
                    sender,
                    _bytes: reservation,
                    _job: job,
                },
            );
        }
        let mut guard = PendingCall {
            client: self.clone(),
            id: request_id.into(),
            cancel: kind == IpcRequestKind::Schedule,
            complete: false,
            queued: false,
        };
        if self
            .writer
            .try_send(Outbound {
                envelope,
                queued: tokio::time::Instant::now(),
            })
            .is_err()
        {
            self.pending.lock().await.remove(request_id);
            guard.complete = true;
            return Err("IPC_QUEUE_FULL_OR_CLOSED".into());
        }
        guard.queued = true;
        let reply = match tokio::time::timeout_at(deadline, receiver).await {
            Ok(Ok(reply)) => reply?,
            Ok(Err(_)) => return Err("模型调度器响应通道已关闭".to_string()),
            Err(_) => {
                ale_core::diagnostics::record(
                    "ipc_reply_timeout",
                    &[(
                        "operation_id",
                        ale_core::diagnostics::correlation_id(request_id),
                    )],
                );
                return Err("模型调度器响应超时".to_string());
            }
        };
        guard.cancel = false;
        guard.complete = true;
        if reply.protocol_version != MODEL_IPC_VERSION || reply.request_id != request_id {
            return Err("模型调度器返回了无法关联的响应".to_string());
        }
        if reply.status == IpcReplyStatus::Error as i32 {
            return Err(format!("{}: {}", reply.error_code, reply.error_message));
        }
        Ok(reply)
    }

    async fn terminate(&self) -> bool {
        self.alive.store(false, Ordering::Release);
        if let Ok(mut child) = self._process.child.lock() {
            let _ = child.start_kill();
        }
        for (_, pending) in self.pending.lock().await.drain() {
            let _ = pending
                .sender
                .send(Err("MODEL_SCHEDULER_UNAVAILABLE".into()));
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        loop {
            let exited = self
                ._process
                .child
                .lock()
                .map(|mut child| matches!(child.try_wait(), Ok(Some(_))))
                .unwrap_or(false);
            if exited {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                ale_core::diagnostics::record("modeld_reap_unconfirmed", &[]);
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

impl SupervisedModeldClient {
    pub async fn update_config(&self, config: AppConfig) -> Result<(), String> {
        let mut state = self.state.lock().await;
        let revision = state.revision + 1;
        if let Some(client) = state.client.clone() {
            if let Err(error) = client.configure(&config, revision).await {
                // A missing acknowledgement leaves the peer's commit uncertain.
                // Retire it; the next instance starts from our last committed config.
                state.client = None;
                state.last_health = None;
                if !client.terminate().await {
                    state.retired = Some(client);
                }
                return Err(error);
            }
        }
        state.config = config;
        state.revision = revision;
        state.restart_blocked = false;
        state.consecutive_failures = 0;
        state.health_failures = 0;
        state.healthy_since = None;
        state.last_health = None;
        state.last_error = None;
        ale_core::diagnostics::record("modeld_config_committed", &[("revision", revision)]);
        Ok(())
    }
    pub async fn with_request<T>(
        &self,
        future: impl std::future::Future<Output = Result<T, String>>,
    ) -> Result<T, String> {
        let client = self.connection().await?;
        let (providers, config) = {
            let state = self.state.lock().await;
            (
                provider_set(&state.config, state.revision),
                state.config.clone(),
            )
        };
        let runtime = tokio::task::spawn_blocking(move || runtime_config(&config))
            .await
            .map_err(|_| "MODELD_CONFIG_TASK_FAILED".to_string())?;
        REQUEST_CONTEXT
            .scope((client, providers, runtime), future)
            .await
    }
    pub async fn start(config: &AppConfig) -> Self {
        let (client, consecutive_failures, last_error) = match ModeldClient::start(config, 0).await
        {
            Ok(client) => (Some(client), 0, None),
            Err(error) => (None, 1, Some(error)),
        };
        let supervisor = Self {
            state: Arc::new(Mutex::new(SupervisorState {
                retired: None,
                health_failures: 0,
                healthy_since: None,
                next_start: tokio::time::Instant::now(),
                last_health: None,
                revision: 0,
                config: config.clone(),
                client,
                consecutive_failures,
                restart_blocked: false,
                last_error,
            })),
            health_gate: Arc::new(Mutex::new(())),
        };
        let weak_state = Arc::downgrade(&supervisor.state);
        let weak_gate = Arc::downgrade(&supervisor.health_gate);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(3));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let (Some(state), Some(health_gate)) = (weak_state.upgrade(), weak_gate.upgrade())
                else {
                    break;
                };
                let current = Self { state, health_gate };
                let _ = current.health().await;
            }
        });
        supervisor
    }

    pub async fn initial_error(&self) -> Option<String> {
        self.state.lock().await.last_error.clone()
    }

    pub async fn health(&self) -> Result<SchedulerHealth, String> {
        let _gate = self.health_gate.lock().await;
        if let Some((when, result)) = &self.state.lock().await.last_health {
            if when.elapsed() < Duration::from_secs(3) {
                return result.clone();
            }
        }
        let client = self.connection().await?;
        let result = tokio::time::timeout(Duration::from_secs(3), client.health())
            .await
            .map_err(|_| "MODELD_HEALTH_TIMEOUT".to_string())
            .and_then(|result| result);
        let terminate = {
            let mut state = self.state.lock().await;
            if state
                .client
                .as_ref()
                .is_none_or(|current| current.instance_id != client.instance_id)
            {
                return result;
            }
            state.last_health = Some((tokio::time::Instant::now(), result.clone()));
            if result.is_ok() {
                state.health_failures = 0;
                let since = state
                    .healthy_since
                    .get_or_insert(tokio::time::Instant::now());
                if since.elapsed() >= Duration::from_secs(60) {
                    state.consecutive_failures = 0;
                    state.last_error = None;
                }
                false
            } else {
                state.healthy_since = None;
                state.health_failures = state.health_failures.saturating_add(1);
                if state.health_failures >= 3 || !client.is_alive() {
                    state.retired = state.client.take();
                    record_process_failure(&mut state, "MODELD_UNRESPONSIVE".into());
                    true
                } else {
                    false
                }
            }
        };
        ale_core::diagnostics::record(
            if result.is_ok() {
                "modeld_health_ok"
            } else {
                "modeld_health_failed"
            },
            &[],
        );
        if terminate {
            client.terminate().await;
        }
        result
    }

    pub async fn remote_plan(
        &self,
        request_id: &str,
        question: String,
        image: Option<&[u8]>,
        tools: Option<Vec<serde_json::Value>>,
    ) -> Result<RemotePlanningResult, String> {
        let client = self.connection().await?;
        let result = client.remote_plan(request_id, question, image, tools).await;
        self.record_result(&client, result.is_ok()).await;
        result
    }

    pub async fn local_plan(
        &self,
        request_id: &str,
        snapshot_id: &str,
        question: String,
        image: &[u8],
        application_id: Option<String>,
    ) -> Result<LocalPlanningResult, String> {
        let client = self.connection().await?;
        let result = client
            .local_plan(request_id, snapshot_id, question, image, application_id)
            .await;
        self.record_result(&client, result.is_ok()).await;
        result
    }

    pub async fn ground(
        &self,
        request_id: &str,
        snapshot_id: &str,
        grounding: GroundingJob,
    ) -> Result<GroundingResult, String> {
        let client = self.connection().await?;
        let result = client.ground(request_id, snapshot_id, grounding).await;
        self.record_result(&client, result.is_ok()).await;
        result
    }

    #[allow(dead_code)]
    pub async fn verify(
        &self,
        request_id: &str,
        snapshot_id: &str,
        verification: StateVerificationJob,
        image: &[u8],
    ) -> Result<StateVerificationResult, String> {
        let client = self.connection().await?;
        let result = client
            .verify(request_id, snapshot_id, verification, image)
            .await;
        self.record_result(&client, result.is_ok()).await;
        result
    }

    pub async fn transcribe_wav(
        &self,
        request_id: &str,
        wav: &[u8],
        allow_remote: bool,
    ) -> Result<SpeechRecognitionResult, String> {
        let client = self.connection().await?;
        let result = client.transcribe_wav(request_id, wav, allow_remote).await;
        self.record_result(&client, result.is_ok()).await;
        result
    }

    pub async fn retry_after_user_request(&self) {
        let mut state = self.state.lock().await;
        if state.restart_blocked {
            state.client = None;
            state.consecutive_failures = 0;
            state.restart_blocked = false;
            state.last_error = None;
        }
    }

    async fn terminate_current_process_for_acceptance(&self) -> Result<(), String> {
        let client = self
            .state
            .lock()
            .await
            .client
            .clone()
            .ok_or_else(|| "模型调度器尚未启动".to_string())?;
        let result = client
            ._process
            .child
            .lock()
            .map_err(|_| "模型调度器进程锁失败".to_string())?
            .start_kill()
            .map_err(|error| format!("无法终止模型调度器验收进程: {error}"));
        result
    }

    async fn connection(&self) -> Result<ModeldClient, String> {
        if let Ok(client) = REQUEST_CONTEXT.try_with(|(client, _, _)| client.clone()) {
            return Ok(client);
        }
        let mut state = self.state.lock().await;
        if state.client.as_ref().is_some_and(ModeldClient::is_alive) {
            return Ok(state
                .client
                .as_ref()
                .expect("checked modeld client")
                .clone());
        }
        if let Some(client) = state.client.take() {
            state.retired = Some(client);
            record_process_failure(&mut state, "模型调度器进程已退出".to_string());
        }
        if let Some(retired) = &state.retired {
            if !retired.terminate().await {
                return Err("MODELD_PREVIOUS_EXIT_UNCONFIRMED".into());
            }
            state.retired = None;
        }
        if state.restart_blocked {
            return Err(format!(
                "MODEL_SCHEDULER_RESTART_BLOCKED: {}",
                state
                    .last_error
                    .as_deref()
                    .unwrap_or("模型调度器连续失败三次，请手动重试或更新配置")
            ));
        }

        tokio::time::sleep_until(state.next_start).await;
        match ModeldClient::start(&state.config, state.revision).await {
            Ok(client) => {
                state.client = Some(client.clone());
                state.health_failures = 0;
                state.healthy_since = None;
                state.last_health = None;
                Ok(client)
            }
            Err(error) => {
                record_process_failure(&mut state, error.clone());
                Err(error)
            }
        }
    }

    async fn record_result(&self, client: &ModeldClient, succeeded: bool) {
        let mut state = self.state.lock().await;
        let is_current = state
            .client
            .as_ref()
            .is_some_and(|current| current.instance_id == client.instance_id);
        if !is_current {
            return;
        }
        if !succeeded && !client.is_alive() {
            state.retired = state.client.take();
            record_process_failure(&mut state, "模型调度器连接已关闭".to_string());
        }
    }
}

pub async fn run_supervisor_acceptance(
    models_dir: PathBuf,
    report_path: PathBuf,
) -> Result<(), String> {
    let mut config = AppConfig::default();
    config.models.models_dir = models_dir.to_string_lossy().into_owned();
    let supervisor = SupervisedModeldClient::start(&config).await;
    let initial_error = supervisor.initial_error().await;
    let initial_health = supervisor.health().await;
    let initial_ok = initial_health.is_ok();
    let initial_instance = supervisor
        .state
        .lock()
        .await
        .client
        .as_ref()
        .map(|client| client.instance_id);

    let kill_error = if initial_ok {
        supervisor
            .terminate_current_process_for_acceptance()
            .await
            .err()
    } else {
        Some("initial modeld health check failed".to_string())
    };
    tokio::time::sleep(Duration::from_millis(500)).await;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let mut restart_errors = Vec::new();
    let restarted_health = loop {
        match supervisor.health().await {
            Ok(health) => {
                let instance = supervisor
                    .state
                    .lock()
                    .await
                    .client
                    .as_ref()
                    .map(|client| client.instance_id);
                if instance != initial_instance {
                    break Some(health);
                }
                if tokio::time::Instant::now() >= deadline {
                    break None;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(error) if tokio::time::Instant::now() < deadline => {
                restart_errors.push(error);
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(error) => {
                restart_errors.push(error);
                break None;
            }
        }
    };
    let restarted_ok = restarted_health.is_some();
    let passed = initial_ok && kill_error.is_none() && restarted_ok;
    let report = serde_json::json!({
        "schema_version": 2,
        "build": ale_core::diagnostics::build_info(),
        "passed": passed,
        "scope": "process_supervisor_without_gpu_requirements",
        "no_input_executed": true,
        "desktop_binary_remained_alive": true,
        "initial_error": initial_error,
        "initial_health": initial_health.ok(),
        "kill_error": kill_error,
        "restart_errors": restart_errors,
        "restarted_health": restarted_health,
    });
    if let Some(parent) = report_path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(
        &report_path,
        serde_json::to_vec_pretty(&report).map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())?;
    passed
        .then_some(())
        .ok_or_else(|| "桌面 modeld 监督器验收失败".to_string())
}

fn record_process_failure(state: &mut SupervisorState, error: String) {
    state.consecutive_failures = state.consecutive_failures.saturating_add(1);
    state.last_error = Some(error);
    state.next_start = tokio::time::Instant::now()
        + Duration::from_secs(1u64 << state.consecutive_failures.saturating_sub(1).min(2));
    ale_core::diagnostics::record(
        "modeld_instance_failed",
        &[("failures", u64::from(state.consecutive_failures))],
    );
    if state.consecutive_failures >= MAX_CONSECUTIVE_PROCESS_FAILURES {
        state.restart_blocked = true;
    }
}

fn runtime_config(config: &AppConfig) -> ModelRuntimeConfig {
    let models_dir = PathBuf::from(&config.models.models_dir);
    let runtime_dir = models_dir.join(".runtime");
    let gguf_dir = runtime_dir.join("gguf");
    let qwen_dir = {
        let large = gguf_dir.join(&config.model_scheduler.qwen_large_model);
        if large.is_dir() {
            large
        } else {
            gguf_dir.join(&config.model_scheduler.qwen_model)
        }
    };
    let llama_name = if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    };
    ModelRuntimeConfig {
        models_dir: models_dir.to_string_lossy().into_owned(),
        sensevoice_model: models_dir
            .join("SenseVoiceSmall")
            .join("model.int8.onnx")
            .to_string_lossy()
            .into_owned(),
        sensevoice_tokens: models_dir
            .join("SenseVoiceSmall")
            .join("tokens.txt")
            .to_string_lossy()
            .into_owned(),
        llama_server: Some(
            runtime_dir
                .join("tools")
                .join("llama-b10472-vulkan")
                .join(llama_name)
                .to_string_lossy()
                .into_owned(),
        ),
        qwen_model: Some(
            qwen_dir
                .join("model-q4_k_m.gguf")
                .to_string_lossy()
                .into_owned(),
        ),
        qwen_mmproj: Some(
            qwen_dir
                .join("mmproj-model-f16.gguf")
                .to_string_lossy()
                .into_owned(),
        ),
        showui_model: Some(
            gguf_dir
                .join(&config.model_scheduler.grounding_model)
                .join("model-q4_k_m.gguf")
                .to_string_lossy()
                .into_owned(),
        ),
        showui_mmproj: Some(
            gguf_dir
                .join(&config.model_scheduler.grounding_model)
                .join("mmproj-model-f16.gguf")
                .to_string_lossy()
                .into_owned(),
        ),
        capability_manifest: Some(
            runtime_dir
                .join("runtime-capabilities.json")
                .to_string_lossy()
                .into_owned(),
        ),
    }
}

fn provider_set(config: &AppConfig, revision: u64) -> RemoteProviderSet {
    RemoteProviderSet {
        revision,
        primary: endpoint(&config.cloud_api),
        transcription: config
            .transcription
            .enabled
            .then(|| endpoint(&config.transcription.endpoint)),
        backup: config.remote_routing.backup.as_ref().map(endpoint),
        backup_enabled: config.remote_routing.backup_enabled,
        backup_pre_authorized: config.remote_routing.backup_pre_authorized,
        circuit_failure_threshold: config.remote_routing.circuit_failure_threshold,
        circuit_open_seconds: config.remote_routing.circuit_open_seconds,
    }
}

fn endpoint(config: &CloudApiConfig) -> RemoteEndpointConfig {
    RemoteEndpointConfig {
        wire_api: config.wire_api,
        provider: config.provider.clone(),
        api_key: config.api_key.clone(),
        api_url: config.api_url.clone(),
        model: config.model.clone(),
        max_tokens: config.max_tokens,
        timeout_seconds: config.timeout,
    }
}

fn modeld_executable() -> Result<PathBuf, String> {
    let name = if cfg!(windows) {
        "ale-modeld.exe"
    } else {
        "ale-modeld"
    };
    let current = std::env::current_exe().map_err(|error| error.to_string())?;
    let sibling = current
        .parent()
        .ok_or_else(|| "无法定位桌面程序目录".to_string())?
        .join(name);
    sibling
        .is_file()
        .then_some(sibling)
        .ok_or_else(|| format!("未找到模型调度器 {name}"))
}

fn modeld_endpoint() -> String {
    #[cfg(windows)]
    {
        return format!(
            r"\\.\pipe\ale-my-eyes-modeld-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        );
    }
    #[cfg(unix)]
    {
        let base = dirs::runtime_dir().unwrap_or_else(std::env::temp_dir);
        let filename = format!(
            "ale-my-eyes-modeld-{}-{}.sock",
            std::process::id(),
            uuid::Uuid::new_v4()
        );
        let endpoint = base.join(&filename);
        if endpoint.as_os_str().as_encoded_bytes().len() <= 95 {
            endpoint.to_string_lossy().into_owned()
        } else {
            PathBuf::from("/tmp")
                .join(filename)
                .to_string_lossy()
                .into_owned()
        }
    }
}

#[cfg(unix)]
async fn connect_with_timeout(endpoint: &str) -> Result<LocalStream, String> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        match tokio::net::UnixStream::connect(endpoint).await {
            Ok(stream) => return Ok(stream),
            Err(error) if tokio::time::Instant::now() < deadline => {
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) {
                    return Err(error.to_string());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(error) => return Err(format!("模型调度器启动超时: {error}")),
        }
    }
}

#[cfg(windows)]
async fn connect_with_timeout(endpoint: &str) -> Result<LocalStream, String> {
    use tokio::net::windows::named_pipe::ClientOptions;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        match ClientOptions::new().open(endpoint) {
            Ok(stream) => return Ok(stream),
            Err(error) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::WouldBlock
                ) {
                    return Err(error.to_string());
                }
            }
            Err(error) => return Err(format!("模型调度器启动超时: {error}")),
        }
    }
}

fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

#[cfg(test)]
mod supervisor_tests {
    use super::*;
    #[cfg(unix)]
    use ale_core::model_ipc::read_message;

    #[cfg(unix)]
    async fn mock_client() -> (ModeldClient, LocalStream) {
        let (stream, peer) = LocalStream::pair().unwrap();
        let mut child = Child::spawn(&mut Command::new("true")).unwrap();
        child.wait().await.unwrap();
        let client = ModeldClient::from_stream(
            stream,
            Arc::new(ModeldProcess {
                child: std::sync::Mutex::new(child),
                endpoint: PathBuf::from(format!("/tmp/ale-unused-{}.sock", uuid::Uuid::new_v4())),
            }),
        );
        (client, peer)
    }

    #[cfg(unix)]
    fn test_job() -> ModelJob {
        ModelJob {
            runtime_snapshot: None,
            request_id: "same-phone-request".into(),
            capability: ModelCapability::RemotePlanning,
            priority: SchedulerPriority::InteractiveRequest,
            deadline_unix_ms: unix_millis() + 90_000,
            risk_ceiling: ale_core::actions::RiskLevel::Low,
            snapshot_id: None,
            privacy: JobPrivacy::default(),
            payload: serde_json::Value::Null,
            remote_snapshot: None,
        }
    }

    #[cfg(unix)]
    async fn reply(peer: &mut LocalStream, request_id: String) {
        write_message(
            peer,
            &IpcReply {
                protocol_version: MODEL_IPC_VERSION,
                request_id,
                status: IpcReplyStatus::Ok as i32,
                payload: b"null".to_vec(),
                error_code: String::new(),
                error_message: String::new(),
            },
        )
        .await
        .unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hot_update_keeps_inflight_snapshot_and_process_and_bounds_all_stages() {
        let (client, mut peer) = mock_client().await;
        let instance = client.instance_id;
        let mut inner = state();
        inner.config.cloud_api.model = "old-model".into();
        inner.client = Some(client);
        let supervisor = SupervisedModeldClient {
            state: Arc::new(Mutex::new(inner)),
            health_gate: Arc::new(Mutex::new(())),
        };
        let server = tokio::spawn(async move {
            let mut jobs = Vec::new();
            for _ in 0..4 {
                let request: IpcEnvelope = read_message(&mut peer).await.unwrap();
                if request.kind == IpcRequestKind::Schedule as i32 {
                    jobs.push(serde_json::from_slice::<ModelJob>(&request.payload).unwrap());
                }
                reply(&mut peer, request.request_id).await;
            }
            jobs
        });
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        ale_core::model_api::DEADLINE
            .scope(
                deadline,
                supervisor.with_request(async {
                    let client = supervisor.connection().await?;
                    let _: serde_json::Value = client
                        .call_json_with_id(
                            "same-phone-request",
                            IpcRequestKind::Schedule,
                            &test_job(),
                        )
                        .await?;
                    let mut updated = AppConfig::default();
                    updated.cloud_api.model = "new-model".into();
                    supervisor.update_config(updated).await?;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    let _: serde_json::Value = client
                        .call_json_with_id(
                            "same-phone-request",
                            IpcRequestKind::Schedule,
                            &test_job(),
                        )
                        .await?;
                    Ok(())
                }),
            )
            .await
            .unwrap();
        supervisor
            .with_request(async {
                let client = supervisor.connection().await?;
                let _: serde_json::Value = client
                    .call_json_with_id("same-phone-request", IpcRequestKind::Schedule, &test_job())
                    .await?;
                Ok(())
            })
            .await
            .unwrap();
        let jobs = server.await.unwrap();
        assert_eq!(
            jobs[0].remote_snapshot.as_ref().unwrap().primary.model,
            "old-model"
        );
        assert_eq!(
            jobs[1].remote_snapshot.as_ref().unwrap().primary.model,
            "old-model"
        );
        assert_eq!(
            jobs[2].remote_snapshot.as_ref().unwrap().primary.model,
            "new-model"
        );
        assert_ne!(jobs[0].request_id, jobs[1].request_id);
        assert!((jobs[1].deadline_unix_ms - jobs[0].deadline_unix_ms).abs() <= 5);
        assert_eq!(
            supervisor
                .state
                .lock()
                .await
                .client
                .as_ref()
                .unwrap()
                .instance_id,
            instance
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropped_stage_cancels_original_instance_and_late_reply_cannot_match_next_stage() {
        let (client, mut peer) = mock_client().await;
        let first_client = client.clone();
        let first = tokio::spawn(async move {
            first_client
                .call_json_with_id::<serde_json::Value>(
                    "same-phone-request",
                    IpcRequestKind::Schedule,
                    &test_job(),
                )
                .await
        });
        let old: IpcEnvelope = read_message(&mut peer).await.unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        let cancellation: IpcEnvelope =
            tokio::time::timeout(Duration::from_secs(1), read_message(&mut peer))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(cancellation.kind, IpcRequestKind::Cancel as i32);
        let target: CancelModelJob = serde_json::from_slice(&cancellation.payload).unwrap();
        assert_eq!(target.target_request_id, old.request_id);
        reply(&mut peer, cancellation.request_id).await;
        let second_client = client.clone();
        let second = tokio::spawn(async move {
            second_client
                .call_json_with_id::<serde_json::Value>(
                    "same-phone-request",
                    IpcRequestKind::Schedule,
                    &test_job(),
                )
                .await
        });
        let new: IpcEnvelope = read_message(&mut peer).await.unwrap();
        assert_ne!(old.request_id, new.request_id);
        reply(&mut peer, old.request_id).await;
        reply(&mut peer, new.request_id).await;
        assert!(tokio::time::timeout(Duration::from_secs(1), second)
            .await
            .unwrap()
            .unwrap()
            .is_ok());
        assert!(client.pending.lock().await.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn full_job_capacity_still_allows_health_and_cancel() {
        let (client, mut peer) = mock_client().await;
        let mut tasks = Vec::new();
        let mut ids = Vec::new();
        for index in 0..32 {
            let sender = client.clone();
            tasks.push(tokio::spawn(async move {
                sender
                    .call_json_with_id::<serde_json::Value>(
                        &format!("job-{index}"),
                        IpcRequestKind::Schedule,
                        &test_job(),
                    )
                    .await
            }));
            let request: IpcEnvelope = read_message(&mut peer).await.unwrap();
            ids.push(request.request_id);
        }
        let error = client
            .call_json_with_id::<serde_json::Value>("excess", IpcRequestKind::Schedule, &test_job())
            .await
            .unwrap_err();
        assert_eq!(error, "SCHEDULER_BUSY");
        assert_eq!(client.job_slots.available_permits(), 0);
        let health_client = client.clone();
        let health = tokio::spawn(async move {
            health_client
                .call_raw(IpcRequestKind::Health, b"null".to_vec())
                .await
        });
        let request: IpcEnvelope = read_message(&mut peer).await.unwrap();
        assert_eq!(request.kind, IpcRequestKind::Health as i32);
        reply(&mut peer, request.request_id).await;
        assert!(health.await.unwrap().is_ok());
        tasks[0].abort();
        let cancelled: IpcEnvelope = read_message(&mut peer).await.unwrap();
        assert_eq!(cancelled.kind, IpcRequestKind::Cancel as i32);
        assert_eq!(client.job_slots.available_permits(), 0);
        reply(&mut peer, cancelled.request_id).await;
        for id in ids {
            reply(&mut peer, id).await;
        }
        for task in tasks {
            let _ = task.await;
        }
        for _ in 0..20 {
            if client.pending.lock().await.is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(client.job_slots.available_permits(), 32);
        assert_eq!(client.byte_budget.available_permits(), MAX_INFLIGHT_BYTES);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn stalled_partial_write_retires_transport_and_fails_all_waiters() {
        let (client, mut peer) = mock_client().await;
        let sender = client.clone();
        let started = tokio::time::Instant::now();
        let large = tokio::spawn(async move {
            sender
                .call_raw(IpcRequestKind::ConfigureRemote, vec![0; 8 * 1024 * 1024])
                .await
        });
        use tokio::io::AsyncReadExt;
        let length = peer.read_u32().await.unwrap();
        assert!(length > 8 * 1024 * 1024);
        // Keep the peer open, but stop draining after the frame header.
        let health_client = client.clone();
        let health = tokio::spawn(async move { health_client.health().await });
        assert!(tokio::time::timeout(Duration::from_secs(5), large)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert!(tokio::time::timeout(Duration::from_secs(1), health)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert!(!client.is_alive());
        assert!(client.pending.lock().await.is_empty());
        assert_eq!(client.byte_budget.available_permits(), MAX_INFLIGHT_BYTES);
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn duplicate_control_request_is_rejected_and_eof_clears_waiters() {
        let (client, mut peer) = mock_client().await;
        let first_client = client.clone();
        let first = tokio::spawn(async move {
            first_client
                .call("duplicate", IpcRequestKind::Health, vec![])
                .await
        });
        let _: IpcEnvelope = read_message(&mut peer).await.unwrap();
        assert_eq!(
            client
                .call("duplicate", IpcRequestKind::Health, vec![])
                .await
                .unwrap_err(),
            "DUPLICATE_REQUEST_ID"
        );
        drop(peer);
        assert!(tokio::time::timeout(Duration::from_secs(1), first)
            .await
            .unwrap()
            .unwrap()
            .is_err());
        assert!(client.pending.lock().await.is_empty());
    }

    fn state() -> SupervisorState {
        SupervisorState {
            retired: None,
            health_failures: 0,
            healthy_since: None,
            next_start: tokio::time::Instant::now(),
            last_health: None,
            revision: 0,
            config: AppConfig::default(),
            client: None,
            consecutive_failures: 0,
            restart_blocked: false,
            last_error: None,
        }
    }

    #[test]
    fn process_restart_is_blocked_after_three_consecutive_failures() {
        let mut state = state();
        record_process_failure(&mut state, "one".to_string());
        record_process_failure(&mut state, "two".to_string());
        assert!(!state.restart_blocked);
        record_process_failure(&mut state, "three".to_string());
        assert!(state.restart_blocked);
        assert_eq!(state.consecutive_failures, 3);
    }

    #[tokio::test]
    async fn explicit_user_retry_clears_a_blocked_restart_budget() {
        let mut inner = state();
        inner.consecutive_failures = MAX_CONSECUTIVE_PROCESS_FAILURES;
        inner.restart_blocked = true;
        inner.last_error = Some("failed".to_string());
        let supervisor = SupervisedModeldClient {
            state: Arc::new(Mutex::new(inner)),
            health_gate: Arc::new(Mutex::new(())),
        };
        supervisor.retry_after_user_request().await;
        let inner = supervisor.state.lock().await;
        assert_eq!(inner.consecutive_failures, 0);
        assert!(!inner.restart_blocked);
        assert!(inner.last_error.is_none());
    }
}
