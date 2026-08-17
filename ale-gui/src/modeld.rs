use ale_core::config::{AppConfig, CloudApiConfig};
use ale_core::model_ipc::{
    read_message, write_message, IpcEnvelope, IpcReply, IpcReplyStatus, IpcRequestKind,
    MODEL_IPC_VERSION,
};
use ale_core::model_scheduler::{
    CancelModelJob, JobPrivacy, ModelCapability, ModelJob, ModelRuntimeConfig,
    RemoteEndpointConfig, RemotePlanningJob, RemotePlanningResult, RemoteProviderSet,
    SchedulerHealth, SchedulerPriority, SpeechRecognitionJob, SpeechRecognitionResult,
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
use tokio::process::{Child, Command};
use tokio::sync::{oneshot, Mutex};

#[cfg(unix)]
type LocalStream = tokio::net::UnixStream;
#[cfg(windows)]
type LocalStream = tokio::net::windows::named_pipe::NamedPipeClient;
type LocalWriter = tokio::io::WriteHalf<LocalStream>;
type PendingReplies = HashMap<String, oneshot::Sender<Result<IpcReply, String>>>;

#[derive(Clone)]
pub struct ModeldClient {
    writer: Arc<Mutex<LocalWriter>>,
    pending: Arc<Mutex<PendingReplies>>,
    alive: Arc<AtomicBool>,
    instance_id: uuid::Uuid,
    _process: Arc<ModeldProcess>,
}

const MAX_CONSECUTIVE_PROCESS_FAILURES: u8 = 3;

#[derive(Clone)]
pub struct SupervisedModeldClient {
    state: Arc<Mutex<SupervisorState>>,
}

struct SupervisorState {
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
    pub async fn start(config: &AppConfig) -> Result<Self, String> {
        let executable = modeld_executable()?;
        let endpoint = modeld_endpoint();
        let mut token = vec![0_u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut token);

        let mut child = Command::new(&executable)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("无法启动模型调度器 {}: {error}", executable.display()))?;
        let bootstrap = serde_json::json!({
            "endpoint": endpoint,
            "token_base64": base64::engine::general_purpose::STANDARD.encode(&token),
        });
        let mut stdin = child
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
        let (mut reader, writer) = tokio::io::split(stream);
        let pending = Arc::new(Mutex::new(PendingReplies::new()));
        let reader_pending = pending.clone();
        let alive = Arc::new(AtomicBool::new(true));
        let reader_alive = alive.clone();
        tokio::spawn(async move {
            loop {
                match read_message::<_, IpcReply>(&mut reader).await {
                    Ok(reply) => {
                        if let Some(sender) = reader_pending.lock().await.remove(&reply.request_id)
                        {
                            let _ = sender.send(Ok(reply));
                        }
                    }
                    Err(error) => {
                        reader_alive.store(false, Ordering::Release);
                        let message = format!("模型调度器连接已关闭: {error}");
                        for (_, sender) in reader_pending.lock().await.drain() {
                            let _ = sender.send(Err(message.clone()));
                        }
                        break;
                    }
                }
            }
        });
        let process = Arc::new(ModeldProcess {
            child: std::sync::Mutex::new(child),
            #[cfg(unix)]
            endpoint: PathBuf::from(&endpoint),
        });
        let client = Self {
            writer: Arc::new(Mutex::new(writer)),
            pending,
            alive,
            instance_id: uuid::Uuid::new_v4(),
            _process: process,
        };
        client.call_raw(IpcRequestKind::Authenticate, token).await?;
        if !config.cloud_api.api_key.trim().is_empty() {
            client.configure_remote(config).await?;
        }
        client.configure_models(config).await?;
        Ok(client)
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
            request_id: request_id.to_string(),
            capability: ModelCapability::SpeechRecognition,
            priority: SchedulerPriority::InteractiveRequest,
            deadline_unix_ms: unix_millis() + 30_000,
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

    async fn configure_remote(&self, config: &AppConfig) -> Result<(), String> {
        let providers = RemoteProviderSet {
            primary: endpoint(&config.cloud_api),
            backup: config.remote_routing.backup.as_ref().map(endpoint),
            backup_enabled: config.remote_routing.backup_enabled,
            backup_pre_authorized: config.remote_routing.backup_pre_authorized,
            circuit_failure_threshold: config.remote_routing.circuit_failure_threshold,
            circuit_open_seconds: config.remote_routing.circuit_open_seconds,
        };
        let _: serde_json::Value = self
            .call_json(IpcRequestKind::ConfigureRemote, &providers)
            .await?;
        Ok(())
    }

    async fn configure_models(&self, config: &AppConfig) -> Result<(), String> {
        let models_dir = PathBuf::from(&config.models.models_dir);
        let runtime = ModelRuntimeConfig {
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
        };
        let _: serde_json::Value = self
            .call_json(IpcRequestKind::ConfigureModels, &runtime)
            .await?;
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
        payload: Vec<u8>,
    ) -> Result<IpcReply, String> {
        let (sender, receiver) = oneshot::channel();
        if self
            .pending
            .lock()
            .await
            .insert(request_id.to_string(), sender)
            .is_some()
        {
            return Err("模型调度器请求 ID 重复".to_string());
        }
        let write_result = write_message(
            &mut *self.writer.lock().await,
            &IpcEnvelope {
                protocol_version: MODEL_IPC_VERSION,
                request_id: request_id.to_string(),
                kind: kind as i32,
                payload,
            },
        )
        .await;
        if let Err(error) = write_result {
            self.alive.store(false, Ordering::Release);
            self.pending.lock().await.remove(request_id);
            return Err(error.to_string());
        }
        let reply = match tokio::time::timeout(Duration::from_secs(95), receiver).await {
            Ok(Ok(reply)) => reply?,
            Ok(Err(_)) => return Err("模型调度器响应通道已关闭".to_string()),
            Err(_) => {
                self.pending.lock().await.remove(request_id);
                return Err("模型调度器响应超时".to_string());
            }
        };
        if reply.protocol_version != MODEL_IPC_VERSION || reply.request_id != request_id {
            return Err("模型调度器返回了无法关联的响应".to_string());
        }
        if reply.status == IpcReplyStatus::Error as i32 {
            return Err(format!("{}: {}", reply.error_code, reply.error_message));
        }
        Ok(reply)
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}

impl SupervisedModeldClient {
    pub async fn start(config: &AppConfig) -> Self {
        let (client, consecutive_failures, last_error) = match ModeldClient::start(config).await {
            Ok(client) => (Some(client), 0, None),
            Err(error) => (None, 1, Some(error)),
        };
        Self {
            state: Arc::new(Mutex::new(SupervisorState {
                config: config.clone(),
                client,
                consecutive_failures,
                restart_blocked: false,
                last_error,
            })),
        }
    }

    pub async fn initial_error(&self) -> Option<String> {
        self.state.lock().await.last_error.clone()
    }

    pub async fn health(&self) -> Result<SchedulerHealth, String> {
        let client = self.connection().await?;
        let result = client.health().await;
        self.record_result(&client, result.is_ok()).await;
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

    pub async fn cancel(&self, target_request_id: &str) -> Result<(), String> {
        let client = self.connection().await?;
        let result = client.cancel(target_request_id).await;
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

    async fn connection(&self) -> Result<ModeldClient, String> {
        let mut state = self.state.lock().await;
        if state.client.as_ref().is_some_and(ModeldClient::is_alive) {
            return Ok(state
                .client
                .as_ref()
                .expect("checked modeld client")
                .clone());
        }
        if state.client.take().is_some() {
            record_process_failure(&mut state, "模型调度器进程已退出".to_string());
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

        match ModeldClient::start(&state.config).await {
            Ok(client) => {
                state.client = Some(client.clone());
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
        if succeeded {
            state.consecutive_failures = 0;
            state.last_error = None;
        } else if !client.is_alive() {
            state.client = None;
            record_process_failure(&mut state, "模型调度器连接已关闭".to_string());
        }
    }
}

fn record_process_failure(state: &mut SupervisorState, error: String) {
    state.consecutive_failures = state.consecutive_failures.saturating_add(1);
    state.last_error = Some(error);
    if state.consecutive_failures >= MAX_CONSECUTIVE_PROCESS_FAILURES {
        state.restart_blocked = true;
    }
}

fn endpoint(config: &CloudApiConfig) -> RemoteEndpointConfig {
    RemoteEndpointConfig {
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

    fn state() -> SupervisorState {
        SupervisorState {
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
        };
        supervisor.retry_after_user_request().await;
        let inner = supervisor.state.lock().await;
        assert_eq!(inner.consecutive_failures, 0);
        assert!(!inner.restart_blocked);
        assert!(inner.last_error.is_none());
    }
}
