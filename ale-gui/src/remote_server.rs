use crate::audit;
use crate::conversation::automation_tools;
use crate::modeld::SupervisedModeldClient;
use crate::platform::{self, ExecutionControl, PlatformService};
use crate::remote_crypto;
use ale_core::actions::{parse_action_plan_arguments, Action, ActionPlan};
use ale_core::model_scheduler::{
    validate_semantic_plan, BoundingBox, GroundingJob, GroundingModel, ModelCapability,
    SemanticPlan, StateVerificationJob, LOCAL_MEDIUM_RISK_THRESHOLD,
};
use ale_core::remote::{
    AssistantOutput, AssistantOutputKind, AudioChunk, AudioEnd, AudioFormat, AudioStart,
    CancelRequest, ClientHello, CommandInput, CommandPreview, CommandRequest, ConfirmExecution,
    DecisionKind, DecisionRequest, DecisionResponse, ExecutionState, ExecutionStatus, PairingInfo,
    Ping, Pong, ProgressStage, ProgressUpdate, RemoteError, RemoteMessage, ServerHello,
    DEFAULT_REMOTE_PORT, MAX_AUDIO_CHUNK_BYTES, MAX_RECORDING_SECONDS, MAX_SAMPLE_RATE_HZ,
    MIN_SAMPLE_RATE_HZ, REMOTE_PROTOCOL_VERSION,
};
use ale_core::AleEngine;
use base64::Engine;
use futures_util::{SinkExt, StreamExt};
use local_ip_address::list_afinet_netifas;
use qrcode::render::unicode;
use qrcode::QrCode;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::io::Cursor;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot, Mutex, Semaphore};
use tokio::task::JoinHandle;
use tokio_tungstenite::accept_async_with_config;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;

const MAX_CONNECTIONS: usize = 8;
const MAX_CONCURRENT_REQUESTS: usize = 4;
const MAX_TEXT_CHARS: usize = 2_000;
const MAX_TEXT_BYTES: usize = 8 * 1024;
const MAX_PENDING_PLANS: usize = 16;
const PENDING_PLAN_TTL: Duration = Duration::from_secs(120);
const MAX_REQUESTS_PER_MINUTE: usize = 30;
const MAX_TRACKED_REQUEST_CLIENTS: usize = 256;
const MAX_PAIRING_FAILURES_PER_MINUTE: usize = 5;
const MAX_TRACKED_PAIRING_CLIENTS: usize = 256;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(8);
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
#[cfg(not(test))]
const EXECUTION_TIMEOUT: Duration = Duration::from_secs(28);
#[cfg(test)]
const EXECUTION_TIMEOUT: Duration = Duration::from_millis(100);

struct AudioAssembler {
    request_id: String,
    sample_rate_hz: u32,
    channels: u16,
    next_sequence: u32,
    pcm: Vec<u8>,
}

impl AudioAssembler {
    fn new(start: AudioStart) -> Result<Self, RemoteError> {
        if start.request_id.is_empty() || start.request_id.len() > 64 {
            return Err(remote_error(
                Some(start.request_id),
                "INVALID_REQUEST_ID",
                "请求 ID 无效",
            ));
        }
        if start.format != AudioFormat::PcmS16Le
            || !(MIN_SAMPLE_RATE_HZ..=MAX_SAMPLE_RATE_HZ).contains(&start.sample_rate_hz)
            || start.channels != 1
        {
            return Err(remote_error(
                Some(start.request_id),
                "UNSUPPORTED_AUDIO_FORMAT",
                "仅支持 8-96 kHz 单声道 PCM16",
            ));
        }
        Ok(Self {
            request_id: start.request_id,
            sample_rate_hz: start.sample_rate_hz,
            channels: start.channels,
            next_sequence: 0,
            pcm: Vec::new(),
        })
    }

    fn push(&mut self, chunk: AudioChunk) -> Result<(), RemoteError> {
        if chunk.request_id != self.request_id || chunk.sequence != self.next_sequence {
            return Err(remote_error(
                Some(chunk.request_id),
                "INVALID_AUDIO_SEQUENCE",
                "音频块序号无效",
            ));
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(chunk.pcm_base64)
            .map_err(|_| {
                remote_error(
                    Some(chunk.request_id.clone()),
                    "INVALID_AUDIO_CHUNK",
                    "音频块不是有效 Base64",
                )
            })?;
        if decoded.is_empty()
            || decoded.len() > MAX_AUDIO_CHUNK_BYTES
            || decoded.len() % (usize::from(self.channels) * 2) != 0
        {
            return Err(remote_error(
                Some(chunk.request_id),
                "INVALID_AUDIO_CHUNK",
                "音频块尺寸无效",
            ));
        }
        let max_bytes = self.sample_rate_hz as usize
            * usize::from(self.channels)
            * 2
            * MAX_RECORDING_SECONDS as usize;
        if self.pcm.len().saturating_add(decoded.len()) > max_bytes {
            return Err(remote_error(
                Some(chunk.request_id),
                "AUDIO_TOO_LARGE",
                "音频超过 60 秒上限",
            ));
        }
        self.pcm.extend_from_slice(&decoded);
        self.next_sequence = self.next_sequence.checked_add(1).ok_or_else(|| {
            remote_error(
                Some(self.request_id.clone()),
                "AUDIO_TOO_LARGE",
                "音频块数量超限",
            )
        })?;
        Ok(())
    }

    fn finish(self, end: &AudioEnd) -> Result<Vec<u8>, RemoteError> {
        let bytes_per_frame = u64::from(self.channels) * 2;
        if end.request_id != self.request_id || end.chunk_count != self.next_sequence {
            return Err(remote_error(
                Some(end.request_id.clone()),
                "INVALID_AUDIO_SEQUENCE",
                "音频结束元数据与块序号不匹配",
            ));
        }
        if self.pcm.is_empty() {
            return Err(remote_error(
                Some(end.request_id.clone()),
                "EMPTY_AUDIO",
                "没有收到音频数据",
            ));
        }
        if end.total_frames != self.pcm.len() as u64 / bytes_per_frame {
            return Err(remote_error(
                Some(end.request_id.clone()),
                "INVALID_AUDIO_LENGTH",
                "音频帧数不匹配",
            ));
        }
        if end.sha256 != format!("{:x}", Sha256::digest(&self.pcm)) {
            return Err(remote_error(
                Some(end.request_id.clone()),
                "AUDIO_HASH_MISMATCH",
                "音频完整性校验失败",
            ));
        }

        let mut cursor = Cursor::new(Vec::new());
        {
            let spec = hound::WavSpec {
                channels: self.channels,
                sample_rate: self.sample_rate_hz,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            };
            let mut writer = hound::WavWriter::new(&mut cursor, spec).map_err(|error| {
                remote_error(
                    Some(end.request_id.clone()),
                    "WAV_ENCODING_FAILED",
                    error.to_string(),
                )
            })?;
            for sample in self.pcm.chunks_exact(2) {
                writer
                    .write_sample(i16::from_le_bytes([sample[0], sample[1]]))
                    .map_err(|error| {
                        remote_error(
                            Some(end.request_id.clone()),
                            "WAV_ENCODING_FAILED",
                            error.to_string(),
                        )
                    })?;
            }
            writer.finalize().map_err(|error| {
                remote_error(
                    Some(end.request_id.clone()),
                    "WAV_ENCODING_FAILED",
                    error.to_string(),
                )
            })?;
        }
        Ok(cursor.into_inner())
    }
}

pub struct RemoteServerHandle {
    pub pairing: PairingInfo,
    pub qr_text: String,
    shutdown: Option<oneshot::Sender<()>>,
    task: Option<JoinHandle<()>>,
}

#[derive(Clone)]
struct ConnectionContext {
    engine: Arc<Mutex<AleEngine>>,
    pairing: PairingInfo,
    platform: Arc<dyn PlatformService>,
    request_slots: Arc<Semaphore>,
    request_limiter: Arc<std::sync::Mutex<ClientRequestLimiter>>,
    pairing_limiter: Arc<std::sync::Mutex<PairingLimiter>>,
    modeld: Option<SupervisedModeldClient>,
    scheduler_enabled: bool,
    explicit_cloud_mode: bool,
    local_planning_available: bool,
}

struct DeferredRequest {
    request_id: String,
    decision_id: String,
    kind: DecisionKind,
    payload: RequestPayload,
    expires_at: Instant,
}

struct ProcessingRequest {
    request_id: String,
    task: JoinHandle<()>,
    modeld: Option<SupervisedModeldClient>,
}

impl Drop for ProcessingRequest {
    fn drop(&mut self) {
        self.task.abort();
        if let Some(modeld) = self.modeld.clone() {
            let request_id = self.request_id.clone();
            tokio::spawn(async move {
                if let Err(error) = modeld.cancel(&request_id).await {
                    tracing::debug!(%request_id, %error, "modeld cancellation was not acknowledged");
                }
            });
        }
    }
}

struct ProcessingResult {
    request_id: String,
    outcome: ProcessingOutcome,
}

struct RemoteExecution {
    request_id: String,
    control: ExecutionControl,
    task: JoinHandle<()>,
}

impl Drop for RemoteExecution {
    fn drop(&mut self) {
        self.control.cancel();
        self.task.abort();
    }
}

struct RemoteExecutionEvent {
    request_id: String,
    kind: RemoteExecutionEventKind,
}

enum RemoteExecutionEventKind {
    Complete(RemoteExecutionOutcome),
    TimedOut,
    FinishedAfterTimeout,
}

enum RemoteExecutionOutcome {
    Status(ExecutionStatus),
    Error { code: &'static str, message: String },
}

enum ConfirmAction {
    Immediate(ExecutionStatus),
    Execute {
        request_id: String,
        plan: ActionPlan,
    },
}

enum ProcessingOutcome {
    Progress(ProgressStage, String),
    Complete(CommandPreview, Option<ActionPlan>),
    Error { code: &'static str, message: String },
}

impl Drop for RemoteServerHandle {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        // Dropping the join handle detaches the task so it can unregister mDNS
        // and exit after receiving the shutdown signal.
        self.task.take();
    }
}

pub async fn start(engine: Arc<Mutex<AleEngine>>) -> Result<RemoteServerHandle, String> {
    let code = remote_crypto::pairing_code();
    let session_id = remote_crypto::session_id();
    let name = remote_crypto::device_name();
    let host = local_addresses()
        .into_iter()
        .next()
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let pairing = PairingInfo {
        host,
        port: DEFAULT_REMOTE_PORT,
        session_id,
        code,
        name,
    };
    let qr_text = render_qr(&pairing.uri()).unwrap_or_else(|_| pairing.uri());

    let listener = TcpListener::bind(("0.0.0.0", DEFAULT_REMOTE_PORT))
        .await
        .map_err(|error| error.to_string())?;
    let platform: Arc<dyn PlatformService> = Arc::from(platform::create_platform());
    let app_config = engine.lock().await.config().clone();
    let scheduler_enabled = app_config.model_scheduler.enabled;
    let explicit_cloud_mode = app_config.inference.mode == "cloud";
    let modeld = if scheduler_enabled {
        let client = SupervisedModeldClient::start(&app_config).await;
        if let Some(error) = client.initial_error().await {
            tracing::warn!("Model scheduler unavailable: {}", error);
        }
        Some(client)
    } else {
        None
    };
    let local_planning_available = if let Some(client) = &modeld {
        client
            .health()
            .await
            .map(|health| {
                health
                    .available_capabilities
                    .contains(&ModelCapability::LocalPlanning)
                    && health
                        .available_capabilities
                        .contains(&ModelCapability::ElementGrounding)
            })
            .unwrap_or(false)
    } else {
        false
    };
    let server_pairing = pairing.clone();
    let connection_slots = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let request_slots = Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS));
    let request_limiter = Arc::new(std::sync::Mutex::new(ClientRequestLimiter::new(
        MAX_REQUESTS_PER_MINUTE,
        Duration::from_secs(60),
        MAX_TRACKED_REQUEST_CLIENTS,
    )));
    let pairing_limiter = Arc::new(std::sync::Mutex::new(PairingLimiter::default()));
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();

    let task = tokio::spawn(async move {
        let mdns = advertise_mdns(&server_pairing);
        loop {
            let accepted = tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => accepted,
            };
            match accepted {
                Ok((stream, addr)) => {
                    let allowed = pairing_limiter
                        .lock()
                        .map(|mut limiter| limiter.allow(addr.ip(), Instant::now()))
                        .unwrap_or(false);
                    if !allowed {
                        tracing::warn!("Remote pairing rate limit reached for {}", addr.ip());
                        continue;
                    }
                    let Ok(connection_permit) = connection_slots.clone().try_acquire_owned() else {
                        tracing::warn!("Remote connection limit reached");
                        continue;
                    };
                    let context = ConnectionContext {
                        engine: engine.clone(),
                        pairing: server_pairing.clone(),
                        platform: platform.clone(),
                        request_slots: request_slots.clone(),
                        request_limiter: request_limiter.clone(),
                        pairing_limiter: pairing_limiter.clone(),
                        modeld: modeld.clone(),
                        scheduler_enabled,
                        explicit_cloud_mode,
                        local_planning_available,
                    };
                    tokio::spawn(async move {
                        let _connection_permit = connection_permit;
                        if let Err(error) = handle_connection(stream, addr, context).await {
                            tracing::warn!("Remote client disconnected: {}", error);
                        }
                    });
                }
                Err(error) => tracing::warn!("Remote accept failed: {}", error),
            }
        }
        if let Some(mdns) = mdns {
            mdns.stop();
        }
    });

    Ok(RemoteServerHandle {
        pairing,
        qr_text,
        shutdown: Some(shutdown_tx),
        task: Some(task),
    })
}

async fn handle_connection(
    stream: TcpStream,
    addr: SocketAddr,
    context: ConnectionContext,
) -> Result<(), String> {
    let ConnectionContext {
        engine,
        pairing,
        platform,
        request_slots,
        request_limiter,
        pairing_limiter,
        modeld,
        scheduler_enabled,
        explicit_cloud_mode,
        local_planning_available,
    } = context;
    let websocket_config = WebSocketConfig {
        max_message_size: Some(remote_crypto::MAX_ENCRYPTED_FRAME_BYTES),
        max_frame_size: Some(remote_crypto::MAX_ENCRYPTED_FRAME_BYTES),
        ..Default::default()
    };
    let mut socket = match tokio::time::timeout(
        HANDSHAKE_TIMEOUT,
        accept_async_with_config(stream, Some(websocket_config)),
    )
    .await
    {
        Ok(Ok(socket)) => socket,
        Ok(Err(error)) => {
            record_pairing_failure(&pairing_limiter, addr.ip());
            return Err(error.to_string());
        }
        Err(_) => {
            record_pairing_failure(&pairing_limiter, addr.ip());
            return Err("WEBSOCKET_HANDSHAKE_TIMEOUT".to_string());
        }
    };

    let client_handshake = match tokio::time::timeout(HANDSHAKE_TIMEOUT, socket.next()).await {
        Ok(Some(Ok(message))) => message,
        Ok(Some(Err(error))) => {
            record_pairing_failure(&pairing_limiter, addr.ip());
            return Err(error.to_string());
        }
        Ok(None) => {
            record_pairing_failure(&pairing_limiter, addr.ip());
            return Err("MISSING_HANDSHAKE".to_string());
        }
        Err(_) => {
            record_pairing_failure(&pairing_limiter, addr.ip());
            return Err("NOISE_HANDSHAKE_TIMEOUT".to_string());
        }
    };
    if !client_handshake.is_binary() || client_handshake.len() > 4096 {
        record_pairing_failure(&pairing_limiter, addr.ip());
        return Err("INVALID_HANDSHAKE".to_string());
    }
    let (mut secure, server_handshake) =
        match remote_crypto::server_handshake_reply(&pairing.code, &client_handshake.into_data()) {
            Ok(handshake) => handshake,
            Err(error) => {
                record_pairing_failure(&pairing_limiter, addr.ip());
                return Err(format!("PAIRING_FAILED: {error}"));
            }
        };
    if let Ok(mut limiter) = pairing_limiter.lock() {
        limiter.record_success(addr.ip());
    }
    socket
        .send(Message::Binary(server_handshake))
        .await
        .map_err(|error| error.to_string())?;

    send_secure(
        &mut socket,
        &mut secure,
        &RemoteMessage::ServerHello(ServerHello {
            protocol_version: REMOTE_PROTOCOL_VERSION,
            device_name: pairing.name.clone(),
            session_id: pairing.session_id.clone(),
        }),
    )
    .await?;

    let mut pending = PendingPlans::new(MAX_PENDING_PLANS, PENDING_PLAN_TTL);
    let mut active_audio: Option<AudioAssembler> = None;
    let (processing_tx, mut processing_rx) = mpsc::unbounded_channel::<ProcessingResult>();
    let mut processing: Option<ProcessingRequest> = None;
    let mut deferred: Option<DeferredRequest> = None;
    let (execution_tx, mut execution_rx) = mpsc::unbounded_channel::<RemoteExecutionEvent>();
    let mut execution: Option<RemoteExecution> = None;
    let mut decision_maintenance = tokio::time::interval(Duration::from_millis(250));
    decision_maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut unexpected_messages = 0_u8;
    loop {
        let frame = tokio::select! {
            _ = decision_maintenance.tick(), if deferred.is_some() => {
                if deferred
                    .as_ref()
                    .is_some_and(|decision| decision.expires_at <= Instant::now())
                {
                    let expired = deferred.take().expect("deferred request checked");
                    send_assistant_output(
                        &mut socket,
                        &mut secure,
                        Some(expired.request_id.clone()),
                        AssistantOutputKind::Error,
                        "等待决定超时，当前请求已取消",
                        true,
                    )
                    .await?;
                    send_remote_error(
                        &mut socket,
                        &mut secure,
                        Some(expired.request_id),
                        "DECISION_TIMEOUT",
                        "等待决定超时",
                    )
                    .await?;
                }
                continue;
            }
            result = processing_rx.recv(), if processing.is_some() => {
                let result = result.ok_or_else(|| "PROCESSING_CHANNEL_CLOSED".to_string())?;
                if processing
                    .as_ref()
                    .is_some_and(|request| request.request_id == result.request_id)
                {
                    if !matches!(&result.outcome, ProcessingOutcome::Progress(_, _)) {
                        processing.take();
                    }
                    publish_processing_result(
                        result,
                        &mut pending,
                        &mut socket,
                        &mut secure,
                    )
                    .await?;
                }
                continue;
            }
            result = execution_rx.recv(), if execution.is_some() => {
                let result = result.ok_or_else(|| "EXECUTION_CHANNEL_CLOSED".to_string())?;
                if execution
                    .as_ref()
                    .is_some_and(|request| request.request_id == result.request_id)
                {
                    match result.kind {
                        RemoteExecutionEventKind::Complete(outcome) => {
                            execution.take();
                            publish_execution_outcome(
                                &mut socket,
                                &mut secure,
                                result.request_id,
                                outcome,
                            )
                            .await?;
                        }
                        RemoteExecutionEventKind::TimedOut => {
                            send_remote_error(
                                &mut socket,
                                &mut secure,
                                Some(result.request_id),
                                "CONFIRM_TIMEOUT",
                                "桌面端执行超时，结果可能不确定",
                            )
                            .await?;
                        }
                        RemoteExecutionEventKind::FinishedAfterTimeout => {
                            execution.take();
                        }
                    }
                }
                continue;
            }
            frame = tokio::time::timeout(CONNECTION_IDLE_TIMEOUT, socket.next()) => {
                match frame {
                    Ok(Some(Ok(frame))) => frame,
                    Ok(Some(Err(error))) => return Err(error.to_string()),
                    Ok(None) => break,
                    Err(_) => return Err("CONNECTION_IDLE_TIMEOUT".to_string()),
                }
            }
        };
        if !frame.is_binary() {
            continue;
        }
        let Some(message) =
            secure.decrypt_frame(&frame.into_data(), remote_crypto::MAX_SECURE_MESSAGE_BYTES)?
        else {
            continue;
        };
        let handled = match message {
            RemoteMessage::ClientHello(ClientHello {
                protocol_version, ..
            }) => {
                if protocol_version != REMOTE_PROTOCOL_VERSION {
                    send_remote_error(
                        &mut socket,
                        &mut secure,
                        None,
                        "PROTOCOL_INCOMPATIBLE",
                        "客户端协议版本不受支持",
                    )
                    .await?;
                    return Err("PROTOCOL_INCOMPATIBLE".to_string());
                }
                true
            }
            RemoteMessage::CommandRequest(request) => {
                let request_id = request.request_id.clone();
                if !allow_request(&request_limiter, addr.ip()) {
                    send_remote_error(
                        &mut socket,
                        &mut secure,
                        Some(request_id),
                        "RATE_LIMITED",
                        "请求过于频繁",
                    )
                    .await?;
                    true
                } else if let Err((code, message)) = validate_command_request(&request) {
                    send_remote_error(&mut socket, &mut secure, Some(request_id), code, message)
                        .await?;
                    true
                } else if deferred.is_some() || processing.is_some() || execution.is_some() {
                    send_remote_error(
                        &mut socket,
                        &mut secure,
                        Some(request_id),
                        "SERVER_BUSY",
                        "当前连接正在处理另一请求",
                    )
                    .await?;
                    true
                } else if scheduler_enabled {
                    let payload = RequestPayload::Text(match request.input {
                        CommandInput::Text { text } => text,
                    });
                    if modeld.is_none() {
                        send_assistant_output(
                            &mut socket,
                            &mut secure,
                            Some(request_id.clone()),
                            AssistantOutputKind::Error,
                            "桌面模型调度器不可用，未切换到旧的直连路径",
                            true,
                        )
                        .await?;
                        send_remote_error(
                            &mut socket,
                            &mut secure,
                            Some(request_id),
                            "MODEL_SCHEDULER_UNAVAILABLE",
                            "桌面模型调度器不可用",
                        )
                        .await?;
                    } else if local_planning_available && !explicit_cloud_mode {
                        match start_remote_request(
                            engine.clone(),
                            platform.clone(),
                            modeld.clone(),
                            request_slots.clone(),
                            request_id.clone(),
                            payload,
                            processing_tx.clone(),
                            false,
                            false,
                            true,
                        ) {
                            Ok(request) => processing = Some(request),
                            Err((code, message)) => {
                                send_remote_error(
                                    &mut socket,
                                    &mut secure,
                                    Some(request_id),
                                    code,
                                    message,
                                )
                                .await?;
                            }
                        }
                    } else {
                        let (kind, prompt) =
                            initial_model_decision(explicit_cloud_mode, local_planning_available);
                        deferred = Some(
                            send_decision_request(
                                &mut socket,
                                &mut secure,
                                request_id,
                                payload,
                                kind,
                                prompt,
                            )
                            .await?,
                        );
                    }
                    true
                } else {
                    send_remote_error(
                        &mut socket,
                        &mut secure,
                        Some(request_id),
                        "MODEL_SCHEDULER_DISABLED",
                        "桌面模型调度器已关闭；远程请求不会使用旧直连路径",
                    )
                    .await?;
                    true
                }
            }
            RemoteMessage::AudioStart(start) => {
                let request_id = start.request_id.clone();
                if !allow_request(&request_limiter, addr.ip()) {
                    send_remote_error(
                        &mut socket,
                        &mut secure,
                        Some(request_id),
                        "RATE_LIMITED",
                        "请求过于频繁",
                    )
                    .await?;
                } else if active_audio.is_some()
                    || deferred.is_some()
                    || processing.is_some()
                    || execution.is_some()
                {
                    send_remote_error(
                        &mut socket,
                        &mut secure,
                        Some(request_id),
                        "AUDIO_BUSY",
                        "当前连接已有录音上传",
                    )
                    .await?;
                } else {
                    match AudioAssembler::new(start) {
                        Ok(upload) => active_audio = Some(upload),
                        Err(error) => {
                            send_remote_error_value(&mut socket, &mut secure, error).await?
                        }
                    }
                }
                true
            }
            RemoteMessage::AudioChunk(chunk) => {
                let result = active_audio
                    .as_mut()
                    .ok_or_else(|| {
                        remote_error(
                            Some(chunk.request_id.clone()),
                            "UNKNOWN_AUDIO_REQUEST",
                            "音频请求不存在",
                        )
                    })
                    .and_then(|upload| upload.push(chunk));
                if let Err(error) = result {
                    active_audio = None;
                    send_remote_error_value(&mut socket, &mut secure, error).await?;
                }
                true
            }
            RemoteMessage::AudioEnd(end) => {
                let request_id = end.request_id.clone();
                let result = active_audio
                    .take()
                    .ok_or_else(|| {
                        remote_error(
                            Some(request_id.clone()),
                            "UNKNOWN_AUDIO_REQUEST",
                            "音频请求不存在",
                        )
                    })
                    .and_then(|upload| upload.finish(&end));
                match result {
                    Ok(wav) => {
                        let payload = RequestPayload::AudioWav(wav);
                        if scheduler_enabled {
                            if modeld.is_none() {
                                send_assistant_output(
                                    &mut socket,
                                    &mut secure,
                                    Some(request_id.clone()),
                                    AssistantOutputKind::Error,
                                    "桌面模型调度器不可用，未切换到旧的直连路径",
                                    true,
                                )
                                .await?;
                                send_remote_error(
                                    &mut socket,
                                    &mut secure,
                                    Some(request_id),
                                    "MODEL_SCHEDULER_UNAVAILABLE",
                                    "桌面模型调度器不可用",
                                )
                                .await?;
                            } else if local_planning_available && !explicit_cloud_mode {
                                match start_remote_request(
                                    engine.clone(),
                                    platform.clone(),
                                    modeld.clone(),
                                    request_slots.clone(),
                                    request_id.clone(),
                                    payload,
                                    processing_tx.clone(),
                                    false,
                                    false,
                                    true,
                                ) {
                                    Ok(request) => processing = Some(request),
                                    Err((code, message)) => {
                                        send_remote_error(
                                            &mut socket,
                                            &mut secure,
                                            Some(request_id),
                                            code,
                                            message,
                                        )
                                        .await?;
                                    }
                                }
                            } else {
                                let (kind, prompt) = initial_model_decision(
                                    explicit_cloud_mode,
                                    local_planning_available,
                                );
                                deferred = Some(
                                    send_decision_request(
                                        &mut socket,
                                        &mut secure,
                                        request_id,
                                        payload,
                                        kind,
                                        prompt,
                                    )
                                    .await?,
                                );
                            }
                        } else {
                            send_remote_error(
                                &mut socket,
                                &mut secure,
                                Some(request_id),
                                "MODEL_SCHEDULER_DISABLED",
                                "桌面模型调度器已关闭；远程请求不会使用旧直连路径",
                            )
                            .await?;
                        }
                    }
                    Err(error) => send_remote_error_value(&mut socket, &mut secure, error).await?,
                }
                true
            }
            RemoteMessage::CancelRequest(CancelRequest { request_id }) => {
                if active_audio
                    .as_ref()
                    .is_some_and(|upload| upload.request_id == request_id)
                {
                    active_audio = None;
                }
                if processing
                    .as_ref()
                    .is_some_and(|request| request.request_id == request_id)
                {
                    processing.take();
                }
                if execution
                    .as_ref()
                    .is_some_and(|request| request.request_id == request_id)
                {
                    execution.take();
                }
                if deferred
                    .as_ref()
                    .is_some_and(|request| request.request_id == request_id)
                {
                    deferred.take();
                }
                if let Some(plan) = pending.take(&request_id, Instant::now()) {
                    audit::record("cancelled", "remote", &plan, None);
                }
                true
            }
            RemoteMessage::ConfirmExecution(confirm) => {
                if execution.is_some() {
                    send_remote_error(
                        &mut socket,
                        &mut secure,
                        Some(confirm.request_id),
                        "SERVER_BUSY",
                        "当前连接正在执行另一计划",
                    )
                    .await?;
                } else {
                    match prepare_confirm(confirm, &mut pending) {
                        ConfirmAction::Immediate(status) => {
                            let request_id = status.request_id.clone();
                            publish_execution_outcome(
                                &mut socket,
                                &mut secure,
                                request_id,
                                RemoteExecutionOutcome::Status(status),
                            )
                            .await?;
                        }
                        ConfirmAction::Execute { request_id, plan } => {
                            send_secure(
                                &mut socket,
                                &mut secure,
                                &RemoteMessage::ProgressUpdate(ProgressUpdate {
                                    request_id: request_id.clone(),
                                    stage: ProgressStage::Executing,
                                    message: "正在执行已确认的操作".to_string(),
                                }),
                            )
                            .await?;
                            execution = Some(start_remote_execution(
                                request_id,
                                plan,
                                platform.clone(),
                                modeld.clone(),
                                execution_tx.clone(),
                            ));
                        }
                    }
                }
                true
            }
            RemoteMessage::DecisionResponse(response) => {
                handle_decision_response(
                    response,
                    &mut deferred,
                    &mut processing,
                    &mut socket,
                    &mut secure,
                    engine.clone(),
                    platform.clone(),
                    modeld.clone(),
                    request_slots.clone(),
                    processing_tx.clone(),
                    execution.is_some(),
                )
                .await?
            }
            RemoteMessage::Ping(Ping { nonce }) => {
                send_secure(
                    &mut socket,
                    &mut secure,
                    &RemoteMessage::Pong(Pong { nonce }),
                )
                .await?;
                true
            }
            RemoteMessage::Pong(_) => true,
            RemoteMessage::ProgressUpdate(_)
            | RemoteMessage::DecisionRequest(_)
            | RemoteMessage::AssistantOutput(_) => false,
            _ => false,
        };
        if handled {
            unexpected_messages = 0;
        } else {
            unexpected_messages = unexpected_messages.saturating_add(1);
            if unexpected_messages >= 3 {
                return Err("PROTOCOL_VIOLATION".to_string());
            }
        }
    }

    Ok(())
}

fn allow_request(limiter: &Arc<std::sync::Mutex<ClientRequestLimiter>>, address: IpAddr) -> bool {
    limiter
        .lock()
        .map(|mut limiter| limiter.allow(address, Instant::now()))
        .unwrap_or(false)
}

fn initial_model_decision(
    explicit_cloud_mode: bool,
    local_planning_available: bool,
) -> (DecisionKind, &'static str) {
    if explicit_cloud_mode {
        return (
            DecisionKind::UploadFullScreenshot,
            "当前配置使用远端模型。是否允许本次上传完整桌面截图？",
        );
    }
    if !local_planning_available {
        return (
            DecisionKind::UseRemoteModel,
            "本地 Qwen 或定位模型不可用。是否改用远端大模型？",
        );
    }
    (
        DecisionKind::UseRemoteModel,
        "本地模型无法可靠完成当前任务。是否升级到远端大模型？",
    )
}

async fn send_decision_request(
    socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    secure: &mut remote_crypto::SecureChannel,
    request_id: String,
    payload: RequestPayload,
    kind: DecisionKind,
    prompt: &str,
) -> Result<DeferredRequest, String> {
    let decision_id = uuid::Uuid::new_v4().to_string();
    send_secure(
        socket,
        secure,
        &RemoteMessage::ProgressUpdate(ProgressUpdate {
            request_id: request_id.clone(),
            stage: ProgressStage::AwaitingDecision,
            message: "等待你的决定".to_string(),
        }),
    )
    .await?;
    send_secure(
        socket,
        secure,
        &RemoteMessage::DecisionRequest(DecisionRequest {
            request_id: request_id.clone(),
            decision_id: decision_id.clone(),
            kind,
            prompt: prompt.to_string(),
            expires_in_seconds: 30,
        }),
    )
    .await?;
    Ok(DeferredRequest {
        request_id,
        decision_id,
        kind,
        payload,
        expires_at: Instant::now() + Duration::from_secs(30),
    })
}

#[allow(clippy::too_many_arguments)]
async fn handle_decision_response(
    response: DecisionResponse,
    deferred: &mut Option<DeferredRequest>,
    processing: &mut Option<ProcessingRequest>,
    socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    secure: &mut remote_crypto::SecureChannel,
    engine: Arc<Mutex<AleEngine>>,
    platform: Arc<dyn PlatformService>,
    modeld: Option<SupervisedModeldClient>,
    request_slots: Arc<Semaphore>,
    results: mpsc::UnboundedSender<ProcessingResult>,
    execution_busy: bool,
) -> Result<bool, String> {
    let Some(waiting) = deferred.take() else {
        return Ok(false);
    };
    if response.request_id != waiting.request_id || response.decision_id != waiting.decision_id {
        *deferred = Some(waiting);
        return Ok(false);
    }
    if waiting.expires_at <= Instant::now() {
        send_remote_error(
            socket,
            secure,
            Some(waiting.request_id),
            "DECISION_TIMEOUT",
            "决定已过期",
        )
        .await?;
        return Ok(true);
    }

    match waiting.kind {
        DecisionKind::UseRemoteModel if !response.approved => {
            send_assistant_output(
                socket,
                secure,
                Some(waiting.request_id.clone()),
                AssistantOutputKind::Result,
                "已取消使用远端模型",
                true,
            )
            .await?;
            send_remote_error(
                socket,
                secure,
                Some(waiting.request_id),
                "CANCELLED",
                "用户拒绝使用远端模型",
            )
            .await?;
        }
        DecisionKind::UseRemoteModel => {
            if let Some(modeld) = &modeld {
                modeld.retry_after_user_request().await;
            }
            *deferred = Some(
                send_decision_request(
                    socket,
                    secure,
                    waiting.request_id,
                    waiting.payload,
                    DecisionKind::UploadFullScreenshot,
                    "是否允许本次向远端模型上传完整桌面截图？拒绝后只发送文字信息。",
                )
                .await?,
            );
        }
        DecisionKind::RiskChanged if !response.approved => {
            send_remote_error(
                socket,
                secure,
                Some(waiting.request_id),
                "CANCELLED",
                "用户拒绝风险变化",
            )
            .await?;
        }
        DecisionKind::UploadFullScreenshot | DecisionKind::RiskChanged => {
            let allow_full_screenshot =
                waiting.kind == DecisionKind::UploadFullScreenshot && response.approved;
            let Some(modeld) = modeld else {
                send_remote_error(
                    socket,
                    secure,
                    Some(waiting.request_id),
                    "MODEL_SCHEDULER_UNAVAILABLE",
                    "桌面模型调度器不可用",
                )
                .await?;
                return Ok(true);
            };
            match start_remote_request(
                engine,
                platform,
                Some(modeld),
                request_slots,
                waiting.request_id.clone(),
                waiting.payload,
                results,
                processing.is_some() || execution_busy,
                allow_full_screenshot,
                false,
            ) {
                Ok(request) => *processing = Some(request),
                Err((code, message)) => {
                    send_remote_error(socket, secure, Some(waiting.request_id), code, message)
                        .await?;
                }
            }
        }
    }
    Ok(true)
}

enum RequestPayload {
    Text(String),
    AudioWav(Vec<u8>),
}

struct RequestInference<'a> {
    progress: Option<&'a mpsc::UnboundedSender<ProcessingResult>>,
    modeld: Option<&'a SupervisedModeldClient>,
    allow_full_screenshot: bool,
    local: bool,
}

#[allow(clippy::too_many_arguments)]
fn start_remote_request(
    engine: Arc<Mutex<AleEngine>>,
    platform: Arc<dyn PlatformService>,
    modeld: Option<SupervisedModeldClient>,
    request_slots: Arc<Semaphore>,
    request_id: String,
    payload: RequestPayload,
    results: mpsc::UnboundedSender<ProcessingResult>,
    connection_busy: bool,
    allow_full_screenshot: bool,
    local_inference: bool,
) -> Result<ProcessingRequest, (&'static str, &'static str)> {
    if connection_busy {
        return Err(("SERVER_BUSY", "当前连接正在处理另一请求"));
    }
    let Ok(request_permit) = request_slots.try_acquire_owned() else {
        return Err(("SERVER_BUSY", "服务器正忙"));
    };
    let task_request_id = request_id.clone();
    let cancel_modeld = modeld.clone();
    let task = tokio::spawn(async move {
        let _request_permit = request_permit;
        let progress = results.clone();
        let outcome = match tokio::time::timeout(
            REQUEST_TIMEOUT,
            handle_request(
                engine,
                platform,
                &task_request_id,
                payload,
                RequestInference {
                    progress: Some(&progress),
                    modeld: modeld.as_ref(),
                    allow_full_screenshot,
                    local: local_inference,
                },
            ),
        )
        .await
        {
            Err(_) => ProcessingOutcome::Error {
                code: "REQUEST_TIMEOUT",
                message: "远程请求超时".to_string(),
            },
            Ok(Ok((preview, plan))) => ProcessingOutcome::Complete(preview, plan),
            Ok(Err(message)) => ProcessingOutcome::Error {
                code: "COMMAND_FAILED",
                message,
            },
        };
        let _ = results.send(ProcessingResult {
            request_id: task_request_id,
            outcome,
        });
    });
    Ok(ProcessingRequest {
        request_id,
        task,
        modeld: cancel_modeld,
    })
}

async fn publish_processing_result(
    result: ProcessingResult,
    pending: &mut PendingPlans,
    socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    secure: &mut remote_crypto::SecureChannel,
) -> Result<(), String> {
    match result.outcome {
        ProcessingOutcome::Progress(stage, message) => {
            send_secure(
                socket,
                secure,
                &RemoteMessage::ProgressUpdate(ProgressUpdate {
                    request_id: result.request_id,
                    stage,
                    message,
                }),
            )
            .await
        }
        ProcessingOutcome::Complete(mut preview, plan) => {
            if let Some(plan) = plan {
                audit::record("created", "remote", &plan, None);
                if pending
                    .insert(result.request_id.clone(), plan, Instant::now())
                    .is_err()
                {
                    return send_remote_error(
                        socket,
                        secure,
                        Some(result.request_id),
                        "PENDING_LIMIT",
                        "待确认计划数量已达上限",
                    )
                    .await;
                }
            }
            let sanitized_response = sanitize_speech_text(&preview.response_text);
            let sanitized_confirmation = sanitize_speech_text(&preview.confirmation_text);
            let mut sensitive = sanitized_response != preview.response_text
                || sanitized_confirmation != preview.confirmation_text;
            preview.response_text = sanitized_response;
            preview.confirmation_text = sanitized_confirmation;
            preview.action_steps = preview
                .action_steps
                .into_iter()
                .map(|step| {
                    let sanitized = sanitize_speech_text(&step);
                    sensitive |= sanitized != step;
                    sanitized
                })
                .collect();
            let speech_source = if preview.confirmation_text.is_empty() {
                preview.response_text.clone()
            } else {
                format!("{}。{}", preview.response_text, preview.confirmation_text)
            };
            let speech_text = sanitize_speech_text(&speech_source);
            sensitive |= speech_text != speech_source;
            send_secure(
                socket,
                secure,
                &RemoteMessage::CommandPreview(preview.clone()),
            )
            .await?;
            send_secure(
                socket,
                secure,
                &RemoteMessage::AssistantOutput(AssistantOutput {
                    request_id: Some(result.request_id),
                    kind: if preview.has_plan {
                        AssistantOutputKind::Confirmation
                    } else {
                        AssistantOutputKind::Information
                    },
                    display_text: preview.response_text,
                    speech_text,
                    interrupt: preview.has_plan,
                    sensitive,
                }),
            )
            .await
        }
        ProcessingOutcome::Error { code, message } => {
            let speech_text = sanitize_speech_text(&message);
            let sensitive = speech_text != message;
            send_secure(
                socket,
                secure,
                &RemoteMessage::AssistantOutput(AssistantOutput {
                    request_id: Some(result.request_id.clone()),
                    kind: AssistantOutputKind::Error,
                    display_text: speech_text.clone(),
                    speech_text,
                    interrupt: true,
                    sensitive,
                }),
            )
            .await?;
            send_remote_error(socket, secure, Some(result.request_id), code, &message).await
        }
    }
}

#[cfg(test)]
async fn handle_command(
    engine: Arc<Mutex<AleEngine>>,
    platform: Arc<dyn PlatformService>,
    request_id: &str,
    input: &CommandInput,
) -> Result<(CommandPreview, Option<ActionPlan>), String> {
    let payload = match input {
        CommandInput::Text { text } => RequestPayload::Text(text.clone()),
    };
    handle_request(
        engine,
        platform,
        request_id,
        payload,
        RequestInference {
            progress: None,
            modeld: None,
            allow_full_screenshot: true,
            local: false,
        },
    )
    .await
}

async fn handle_request(
    engine: Arc<Mutex<AleEngine>>,
    platform: Arc<dyn PlatformService>,
    request_id: &str,
    input: RequestPayload,
    inference: RequestInference<'_>,
) -> Result<(CommandPreview, Option<ActionPlan>), String> {
    let RequestInference {
        progress,
        modeld,
        allow_full_screenshot,
        local: local_inference,
    } = inference;
    let request_id = request_id.to_string();
    let question = match input {
        RequestPayload::Text(text) => text,
        RequestPayload::AudioWav(audio) => {
            report_progress(
                progress,
                &request_id,
                ProgressStage::Transcribing,
                "正在识别语音",
            );
            if let Some(client) = modeld {
                client.transcribe_wav(&request_id, &audio, true).await?.text
            } else {
                let engine = engine.lock().await;
                engine
                    .transcribe(&audio)
                    .await
                    .map_err(|error| error.to_string())?
            }
        }
    };

    report_progress(
        progress,
        &request_id,
        ProgressStage::CapturingState,
        "正在获取桌面状态",
    );
    let image = (allow_full_screenshot || local_inference)
        .then(|| platform.capture_image())
        .flatten();
    report_progress(
        progress,
        &request_id,
        ProgressStage::Planning,
        "正在规划操作",
    );
    if local_inference {
        let client = modeld.ok_or_else(|| "MODEL_SCHEDULER_UNAVAILABLE".to_string())?;
        let image = image
            .as_ref()
            .ok_or_else(|| "LOCAL_SCREENSHOT_UNAVAILABLE".to_string())?;
        let snapshot_id = captured_snapshot_id(image);
        let accessibility = platform.capture_accessibility(&image.coordinate_space);
        let prepared_question = {
            let engine = engine.lock().await;
            engine.prepare_vision_question(&question)
        };
        let result = client
            .local_plan(
                &request_id,
                &snapshot_id,
                prepared_question,
                &image.jpeg_data,
                accessibility
                    .as_ref()
                    .and_then(|snapshot| snapshot.application_id.clone()),
            )
            .await?;
        if result.snapshot_id != snapshot_id {
            return Err("SNAPSHOT_MISMATCH".to_string());
        }
        let plan = validate_semantic_plan(result.plan).map_err(|error| error.to_string())?;
        let mut response_text = format!("本地 {} 已生成语义计划。", result.model_id);
        let mut executable_plan = None;
        if let Some(target) = plan.steps.first().and_then(|step| step.target.clone()) {
            let candidate_bounds: Vec<BoundingBox> = accessibility
                .as_ref()
                .map(|snapshot| {
                    snapshot
                        .nodes
                        .iter()
                        .filter(|node| accessibility_node_matches(node, &target))
                        .map(|node| node.bounds.clone())
                        .collect()
                })
                .unwrap_or_default();
            let grounding = GroundingJob {
                image_base64: base64::engine::general_purpose::STANDARD.encode(&image.jpeg_data),
                target: target.clone(),
                image_width: image.coordinate_space.image_width,
                image_height: image.coordinate_space.image_height,
                candidate_bounds: candidate_bounds.clone(),
                model: GroundingModel::ShowUi,
            };
            match client.ground(&request_id, &snapshot_id, grounding).await {
                Ok(grounded)
                    if grounded.snapshot_id == snapshot_id
                        && grounded.selected.as_ref().is_some_and(|candidate| {
                            candidate.confidence >= LOCAL_MEDIUM_RISK_THRESHOLD
                        }) =>
                {
                    if let Some(selected) = grounded.selected {
                        let controlled = std::env::var("ALE_CONTROLLED_TEST_EXECUTION")
                            .ok()
                            .as_deref()
                            == Some("1")
                            && accessibility
                                .as_ref()
                                .and_then(|snapshot| snapshot.application_id.as_deref())
                                == Some("ALE MODEL RUNTIME CONTROLLED TEST");
                        if controlled {
                            let image_x = selected.click_x as f64
                                * image.coordinate_space.image_width.saturating_sub(1) as f64;
                            let image_y = selected.click_y as f64
                                * image.coordinate_space.image_height.saturating_sub(1) as f64;
                            let (desktop_x, desktop_y) = image
                                .coordinate_space
                                .map_point(image_x, image_y)
                                .map_err(|error| error.to_string())?;
                            let mut action_plan =
                                ActionPlan::new("在专用 AME 测试窗口执行一次受控点击".to_string());
                            action_plan.add_action(Action::ControlledTestClick {
                                x: desktop_x,
                                y: desktop_y,
                                window_title: "ALE MODEL RUNTIME CONTROLLED TEST".to_string(),
                                target_name: "SAVE button inside Settings dialog".to_string(),
                                snapshot_id: snapshot_id.clone(),
                            });
                            action_plan.validate().map_err(|error| error.to_string())?;
                            response_text.push_str(
                                "定位结果与唯一 UIA 候选一致；确认后只允许点击专用测试窗口。",
                            );
                            executable_plan = Some(action_plan);
                        } else {
                            response_text.push_str(
                                "定位结果与唯一 UIA 候选一致；当前为 dry-run，未创建执行动作。",
                            );
                        }
                    } else {
                        response_text
                            .push_str("定位模型已运行，但桌面没有唯一 UIA 候选；未创建执行坐标。");
                    }
                }
                primary => {
                    let fallback = GroundingJob {
                        image_base64: base64::engine::general_purpose::STANDARD
                            .encode(&image.jpeg_data),
                        target,
                        image_width: image.coordinate_space.image_width,
                        image_height: image.coordinate_space.image_height,
                        candidate_bounds,
                        model: GroundingModel::UiTars,
                    };
                    let fallback_result = client
                        .ground(&format!("{request_id}:uitars"), &snapshot_id, fallback)
                        .await;
                    response_text.push_str(match (primary, fallback_result) {
                        (Ok(_), Ok(_)) => {
                            "ShowUI 未得到唯一候选，UI-TARS 已复核；模型证据存在冲突，未创建执行坐标。"
                        }
                        (Err(_), Ok(_)) => {
                            "ShowUI 失败，UI-TARS 仅作为复核证据；不使用最后一个模型强制执行。"
                        }
                        (_, Err(_)) => {
                            "ShowUI/UI-TARS 未共同提供可信定位；未创建执行坐标。"
                        }
                    });
                }
            }
        }
        let confirmation_text = executable_plan
            .as_ref()
            .map(ActionPlan::speak_text)
            .unwrap_or_default();
        return Ok((
            CommandPreview {
                request_id,
                response_text,
                action_steps: plan.describe_steps(),
                confirmation_text,
                requires_confirmation: executable_plan.is_some(),
                has_plan: executable_plan.is_some(),
            },
            executable_plan,
        ));
    }
    let mut response = if let Some(client) = modeld {
        let prepared_question = {
            let engine = engine.lock().await;
            engine.prepare_vision_question(&question)
        };
        let mut result = client
            .remote_plan(
                &request_id,
                prepared_question,
                image.as_ref().map(|image| image.jpeg_data.as_slice()),
                image.as_ref().map(|_| semantic_automation_tools()),
            )
            .await?;
        if let Some(notice) = result.failover_notice.take() {
            result.response.content = format!("{notice}。{}", result.response.content);
        }
        result.response
    } else if let Some(ref image) = image {
        let engine = engine.lock().await;
        engine
            .ask_about_image_with_tools(&image.jpeg_data, &question, automation_tools())
            .await
            .map_err(|error| error.to_string())?
    } else if modeld.is_none() {
        let engine = engine.lock().await;
        let response = engine
            .ask_text(&question)
            .await
            .map_err(|error| error.to_string())?;
        return Ok((
            CommandPreview {
                request_id,
                response_text: response.content,
                action_steps: Vec::new(),
                confirmation_text: String::new(),
                requires_confirmation: false,
                has_plan: false,
            },
            None,
        ));
    } else {
        unreachable!("modeld branch handled above")
    };

    let mut action_steps = Vec::new();
    let mut plan = None;
    if modeld.is_some() {
        if let Some(calls) = response.tool_calls.as_ref() {
            let semantic = calls
                .iter()
                .filter(|call| call.function.name == "propose_semantic_plan")
                .collect::<Vec<_>>();
            if semantic.len() == 1 {
                if let Ok(parsed) = parse_semantic_plan_arguments(&semantic[0].function.arguments) {
                    action_steps = parsed.describe_steps();
                    response.content.push_str(
                        "\n\n已生成语义计划；本地 ShowUI/UI-TARS 未返回可信定位，未创建可执行坐标。",
                    );
                }
            }
        }
    } else if let Some(calls) = response.tool_calls {
        let executable = calls
            .iter()
            .filter(|call| call.function.name == "execute_action_plan")
            .collect::<Vec<_>>();
        if executable.len() == 1 {
            if let (Some(image), Ok(parsed)) = (
                image.as_ref(),
                parse_action_plan_arguments(&executable[0].function.arguments),
            ) {
                let parsed = image
                    .coordinate_space
                    .map_plan(parsed)
                    .map_err(|error| error.to_string())?;
                action_steps = parsed.describe_steps();
                plan = Some(parsed);
            }
        }
    }

    let confirmation_text = plan
        .as_ref()
        .map(ActionPlan::speak_text)
        .unwrap_or_default();
    let requires_confirmation = plan
        .as_ref()
        .map(|plan| plan.requires_confirmation)
        .unwrap_or(false);
    let has_plan = plan.is_some();

    Ok((
        CommandPreview {
            request_id,
            response_text: response.content,
            action_steps,
            confirmation_text,
            requires_confirmation,
            has_plan,
        },
        plan,
    ))
}

fn captured_snapshot_id(image: &crate::platform::CapturedImage) -> String {
    let mut hasher = Sha256::new();
    hasher.update(&image.jpeg_data);
    let space = image.coordinate_space;
    hasher.update(space.image_width.to_le_bytes());
    hasher.update(space.image_height.to_le_bytes());
    hasher.update(space.desktop_x.to_le_bytes());
    hasher.update(space.desktop_y.to_le_bytes());
    hasher.update(space.desktop_width.to_le_bytes());
    hasher.update(space.desktop_height.to_le_bytes());
    format!("snapshot-{:x}", hasher.finalize())
}

fn accessibility_node_matches(
    node: &crate::platform::AccessibilityNode,
    target: &ale_core::model_scheduler::TargetRef,
) -> bool {
    if target
        .node_id
        .as_deref()
        .is_some_and(|id| id == node.node_id)
    {
        return true;
    }
    if let (Some(expected), Some(actual)) = (target.role.as_deref(), node.role.as_deref()) {
        if !actual
            .to_ascii_lowercase()
            .contains(&expected.to_ascii_lowercase())
        {
            return false;
        }
    }
    let expected = target
        .label
        .as_deref()
        .or(target.visual_description.as_deref());
    match (expected, node.label.as_deref()) {
        (Some(expected), Some(actual)) => actual
            .to_ascii_lowercase()
            .contains(&expected.to_ascii_lowercase()),
        _ => false,
    }
}

fn parse_semantic_plan_arguments(arguments: &str) -> Result<SemanticPlan, String> {
    let value: serde_json::Value =
        serde_json::from_str(arguments).map_err(|error| error.to_string())?;
    let plan_value = value.get("plan").cloned().unwrap_or(value);
    let plan: SemanticPlan =
        serde_json::from_value(plan_value).map_err(|error| error.to_string())?;
    validate_semantic_plan(plan).map_err(|error| error.to_string())
}

fn semantic_automation_tools() -> Vec<serde_json::Value> {
    vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "propose_semantic_plan",
            "description": "Propose semantic desktop steps. Do not return coordinates. Every step must include an observable postcondition.",
            "parameters": {
                "type": "object",
                "properties": {
                    "goal": { "type": "string" },
                    "application_id": { "type": ["string", "null"] },
                    "steps": {
                        "type": "array",
                        "maxItems": 20,
                        "items": {
                            "type": "object",
                            "properties": {
                                "operation": { "type": "string" },
                                "target": {
                                    "type": ["object", "null"],
                                    "properties": {
                                        "node_id": { "type": ["string", "null"] },
                                        "role": { "type": ["string", "null"] },
                                        "label": { "type": ["string", "null"] },
                                        "visual_description": { "type": ["string", "null"] }
                                    }
                                },
                                "input_summary": { "type": ["string", "null"] },
                                "expected_state": { "type": "string" },
                                "risk": { "type": "string", "enum": ["low", "medium", "high"] }
                            },
                            "required": ["operation", "expected_state", "risk"]
                        }
                    }
                },
                "required": ["goal", "steps"]
            }
        }
    })]
}

fn prepare_confirm(confirm: ConfirmExecution, pending: &mut PendingPlans) -> ConfirmAction {
    if !confirm.approved {
        if let Some(plan) = pending.take(&confirm.request_id, Instant::now()) {
            audit::record("cancelled", "remote", &plan, None);
        }
        return ConfirmAction::Immediate(ExecutionStatus {
            request_id: confirm.request_id,
            state: ExecutionState::Cancelled,
            message: "已取消".to_string(),
            actions_executed: 0,
        });
    }

    let Some(plan) = pending.take(&confirm.request_id, Instant::now()) else {
        return ConfirmAction::Immediate(ExecutionStatus {
            request_id: confirm.request_id,
            state: ExecutionState::Failed,
            message: "PLAN_EXPIRED_OR_UNKNOWN".to_string(),
            actions_executed: 0,
        });
    };

    audit::record("approved", "remote", &plan, None);
    ConfirmAction::Execute {
        request_id: confirm.request_id,
        plan,
    }
}

fn start_remote_execution(
    request_id: String,
    plan: ActionPlan,
    platform: Arc<dyn PlatformService>,
    modeld: Option<SupervisedModeldClient>,
    results: mpsc::UnboundedSender<RemoteExecutionEvent>,
) -> RemoteExecution {
    let deadline = Instant::now() + EXECUTION_TIMEOUT;
    let control = ExecutionControl::new(deadline);
    let task_control = control.clone();
    let task_request_id = request_id.clone();
    let controlled_test = plan
        .actions
        .iter()
        .any(|action| matches!(action, Action::ControlledTestClick { .. }));
    let expected_snapshot = plan.actions.iter().find_map(|action| match action {
        Action::ControlledTestClick { snapshot_id, .. } => Some(snapshot_id.clone()),
        _ => None,
    });
    let task = tokio::spawn(async move {
        let status_request_id = task_request_id.clone();
        if let Some(expected_snapshot) = expected_snapshot {
            let current_snapshot = platform
                .capture_image_now()
                .map(|image| captured_snapshot_id(&image));
            if current_snapshot.as_deref() != Some(expected_snapshot.as_str()) {
                let _ = results.send(RemoteExecutionEvent {
                    request_id: status_request_id,
                    kind: RemoteExecutionEventKind::Complete(RemoteExecutionOutcome::Status(
                        ExecutionStatus {
                            request_id: task_request_id,
                            state: ExecutionState::Failed,
                            message: "SNAPSHOT_EXPIRED".to_string(),
                            actions_executed: 0,
                        },
                    )),
                });
                return;
            }
        }
        let blocking_control = task_control.clone();
        let execution_platform = platform.clone();
        let mut execution = tokio::task::spawn_blocking(move || {
            execute_confirm(task_request_id, plan, execution_platform, blocking_control)
        });
        let outcome = match tokio::time::timeout_at(deadline.into(), &mut execution).await {
            Ok(Ok(_)) if task_control.timed_out() => RemoteExecutionOutcome::Error {
                code: "CONFIRM_TIMEOUT",
                message: "桌面端执行超时，结果可能不确定".to_string(),
            },
            Ok(Ok(mut status)) => {
                if controlled_test && matches!(status.state, ExecutionState::Completed) {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    let verification = match (modeld.as_ref(), platform.capture_image_now()) {
                        (Some(modeld), Some(image)) => {
                            let snapshot_id = captured_snapshot_id(&image);
                            modeld
                                .verify(
                                    &format!("{}:verify", status_request_id),
                                    &snapshot_id,
                                    StateVerificationJob {
                                        image_base64: String::new(),
                                        expected_state:
                                            "The controlled test window visibly shows SAVED"
                                                .to_string(),
                                    },
                                    &image.jpeg_data,
                                )
                                .await
                                .map(|result| result.observed)
                        }
                        (None, _) => Err("模型调度器不可用，无法验证执行结果".to_string()),
                        (_, None) => Err("无法重新截图验证执行结果".to_string()),
                    };
                    match verification {
                        Ok(true) => status.message.push_str("，并已验证 SAVED 后置状态"),
                        Ok(false) => {
                            status.state = ExecutionState::Failed;
                            status.message =
                                "操作已执行，但模型未观察到 SAVED 后置状态".to_string();
                        }
                        Err(error) => {
                            status.state = ExecutionState::Failed;
                            status.message = format!("操作已执行，但后置验证失败: {error}");
                        }
                    }
                }
                RemoteExecutionOutcome::Status(status)
            }
            Ok(Err(error)) => RemoteExecutionOutcome::Error {
                code: "INTERNAL_ERROR",
                message: error.to_string(),
            },
            Err(_) => {
                task_control.cancel();
                let _ = results.send(RemoteExecutionEvent {
                    request_id: status_request_id.clone(),
                    kind: RemoteExecutionEventKind::TimedOut,
                });
                let _ = execution.await;
                let _ = results.send(RemoteExecutionEvent {
                    request_id: status_request_id,
                    kind: RemoteExecutionEventKind::FinishedAfterTimeout,
                });
                return;
            }
        };
        let _ = results.send(RemoteExecutionEvent {
            request_id: status_request_id,
            kind: RemoteExecutionEventKind::Complete(outcome),
        });
    });
    RemoteExecution {
        request_id,
        control,
        task,
    }
}

async fn publish_execution_outcome(
    socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    secure: &mut remote_crypto::SecureChannel,
    request_id: String,
    outcome: RemoteExecutionOutcome,
) -> Result<(), String> {
    match outcome {
        RemoteExecutionOutcome::Status(mut status) => {
            let speech_text = sanitize_speech_text(&status.message);
            let sensitive = speech_text != status.message;
            status.message = speech_text.clone();
            send_secure(
                socket,
                secure,
                &RemoteMessage::ExecutionStatus(status.clone()),
            )
            .await?;
            send_secure(
                socket,
                secure,
                &RemoteMessage::AssistantOutput(AssistantOutput {
                    request_id: Some(request_id),
                    kind: AssistantOutputKind::Result,
                    display_text: speech_text.clone(),
                    speech_text,
                    interrupt: false,
                    sensitive,
                }),
            )
            .await
        }
        RemoteExecutionOutcome::Error { code, message } => {
            let speech_text = sanitize_speech_text(&message);
            let sensitive = speech_text != message;
            send_secure(
                socket,
                secure,
                &RemoteMessage::AssistantOutput(AssistantOutput {
                    request_id: Some(request_id.clone()),
                    kind: AssistantOutputKind::Error,
                    display_text: speech_text.clone(),
                    speech_text,
                    interrupt: true,
                    sensitive,
                }),
            )
            .await?;
            send_remote_error(socket, secure, Some(request_id), code, &message).await
        }
    }
}

fn report_progress(
    sender: Option<&mpsc::UnboundedSender<ProcessingResult>>,
    request_id: &str,
    stage: ProgressStage,
    message: &str,
) {
    if let Some(sender) = sender {
        let _ = sender.send(ProcessingResult {
            request_id: request_id.to_string(),
            outcome: ProcessingOutcome::Progress(stage, message.to_string()),
        });
    }
}

fn sanitize_speech_text(text: &str) -> String {
    let mut redacted = Vec::new();
    for token in text.split_whitespace().take(300) {
        let lower = token.to_ascii_lowercase();
        let looks_like_secret = lower.starts_with("sk-")
            || lower.starts_with("bearer")
            || lower.contains("password=")
            || lower.contains("api_key=")
            || (token.len() >= 24
                && token
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte)));
        let looks_like_path = token.starts_with('/')
            || (token.len() > 3
                && token.as_bytes().get(1) == Some(&b':')
                && matches!(token.as_bytes().get(2), Some(b'\\') | Some(b'/')));
        if looks_like_secret {
            redacted.push("[敏感信息已隐藏]");
        } else if looks_like_path {
            redacted.push("[路径已隐藏]");
        } else {
            redacted.push(token);
        }
    }
    redacted.join(" ")
}

async fn send_assistant_output(
    socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    secure: &mut remote_crypto::SecureChannel,
    request_id: Option<String>,
    kind: AssistantOutputKind,
    text: &str,
    interrupt: bool,
) -> Result<(), String> {
    let speech_text = sanitize_speech_text(text);
    send_secure(
        socket,
        secure,
        &RemoteMessage::AssistantOutput(AssistantOutput {
            request_id,
            kind,
            display_text: speech_text.clone(),
            sensitive: speech_text != text,
            speech_text,
            interrupt,
        }),
    )
    .await
}

fn execute_confirm(
    request_id: String,
    plan: ActionPlan,
    platform: Arc<dyn PlatformService>,
    control: ExecutionControl,
) -> ExecutionStatus {
    match platform.execute_plan_controlled(&plan, true, &control) {
        Ok(result) => {
            audit::record("completed", "remote", &plan, None);
            ExecutionStatus {
                request_id,
                state: ExecutionState::Completed,
                message: format!("执行完成: {} 步", result.actions_executed),
                actions_executed: result.actions_executed,
            }
        }
        Err(error) => {
            audit::record("failed", "remote", &plan, Some(&error.to_string()));
            ExecutionStatus {
                request_id,
                state: ExecutionState::Failed,
                message: error.to_string(),
                actions_executed: 0,
            }
        }
    }
}

async fn send_secure(
    socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    secure: &mut remote_crypto::SecureChannel,
    message: &RemoteMessage,
) -> Result<(), String> {
    for frame in secure.encrypt_message(message)? {
        tokio::time::timeout(SEND_TIMEOUT, socket.send(Message::Binary(frame)))
            .await
            .map_err(|_| "SEND_TIMEOUT".to_string())?
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

async fn send_remote_error(
    socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    secure: &mut remote_crypto::SecureChannel,
    request_id: Option<String>,
    code: &str,
    message: &str,
) -> Result<(), String> {
    let message = sanitize_speech_text(message);
    send_secure(
        socket,
        secure,
        &RemoteMessage::Error(RemoteError {
            request_id,
            code: code.to_string(),
            message,
        }),
    )
    .await
}

async fn send_remote_error_value(
    socket: &mut tokio_tungstenite::WebSocketStream<TcpStream>,
    secure: &mut remote_crypto::SecureChannel,
    error: RemoteError,
) -> Result<(), String> {
    send_secure(socket, secure, &RemoteMessage::Error(error)).await
}

fn remote_error(
    request_id: Option<String>,
    code: impl Into<String>,
    message: impl Into<String>,
) -> RemoteError {
    RemoteError {
        request_id,
        code: code.into(),
        message: message.into(),
    }
}

fn validate_command_request(request: &CommandRequest) -> Result<(), (&'static str, &'static str)> {
    if request.request_id.is_empty() || request.request_id.len() > 64 {
        return Err(("INVALID_REQUEST_ID", "请求 ID 无效"));
    }
    match &request.input {
        CommandInput::Text { text } => {
            if text.trim().is_empty() {
                return Err(("EMPTY_TEXT", "文本不能为空"));
            }
            if text.len() > MAX_TEXT_BYTES || text.chars().count() > MAX_TEXT_CHARS {
                return Err(("TEXT_TOO_LARGE", "文本超过大小限制"));
            }
        }
    }
    Ok(())
}

struct PendingPlan {
    plan: ActionPlan,
    expires_at: Instant,
}

struct PendingPlans {
    plans: HashMap<String, PendingPlan>,
    max_entries: usize,
    ttl: Duration,
}

impl PendingPlans {
    fn new(max_entries: usize, ttl: Duration) -> Self {
        Self {
            plans: HashMap::new(),
            max_entries,
            ttl,
        }
    }

    fn insert(&mut self, request_id: String, plan: ActionPlan, now: Instant) -> Result<(), ()> {
        self.purge_expired(now);
        if !self.plans.contains_key(&request_id) && self.plans.len() >= self.max_entries {
            return Err(());
        }
        self.plans.insert(
            request_id,
            PendingPlan {
                plan,
                expires_at: now + self.ttl,
            },
        );
        Ok(())
    }

    fn take(&mut self, request_id: &str, now: Instant) -> Option<ActionPlan> {
        self.purge_expired(now);
        self.plans.remove(request_id).map(|pending| pending.plan)
    }

    fn purge_expired(&mut self, now: Instant) {
        self.plans.retain(|_, pending| pending.expires_at > now);
    }
}

struct RequestRate {
    attempts: VecDeque<Instant>,
    max_attempts: usize,
    window: Duration,
}

impl RequestRate {
    fn new(max_attempts: usize, window: Duration) -> Self {
        Self {
            attempts: VecDeque::with_capacity(max_attempts),
            max_attempts,
            window,
        }
    }

    fn allow(&mut self, now: Instant) -> bool {
        self.purge(now);
        if self.attempts.len() >= self.max_attempts {
            return false;
        }
        self.attempts.push_back(now);
        true
    }

    fn purge(&mut self, now: Instant) {
        while self
            .attempts
            .front()
            .is_some_and(|attempt| now.duration_since(*attempt) >= self.window)
        {
            self.attempts.pop_front();
        }
    }
}

struct ClientRequestLimiter {
    clients: HashMap<IpAddr, RequestRate>,
    max_attempts: usize,
    window: Duration,
    max_clients: usize,
}

impl ClientRequestLimiter {
    fn new(max_attempts: usize, window: Duration, max_clients: usize) -> Self {
        Self {
            clients: HashMap::new(),
            max_attempts,
            window,
            max_clients,
        }
    }

    fn allow(&mut self, address: IpAddr, now: Instant) -> bool {
        for rate in self.clients.values_mut() {
            rate.purge(now);
        }
        self.clients.retain(|_, rate| !rate.attempts.is_empty());
        if !self.clients.contains_key(&address) && self.clients.len() >= self.max_clients {
            return false;
        }
        self.clients
            .entry(address)
            .or_insert_with(|| RequestRate::new(self.max_attempts, self.window))
            .allow(now)
    }
}

struct PairingLimiter {
    failures: HashMap<IpAddr, VecDeque<Instant>>,
    max_clients: usize,
}

impl Default for PairingLimiter {
    fn default() -> Self {
        Self {
            failures: HashMap::new(),
            max_clients: MAX_TRACKED_PAIRING_CLIENTS,
        }
    }
}

impl PairingLimiter {
    fn allow(&mut self, address: IpAddr, now: Instant) -> bool {
        for failures in self.failures.values_mut() {
            while failures
                .front()
                .is_some_and(|attempt| now.duration_since(*attempt) >= Duration::from_secs(60))
            {
                failures.pop_front();
            }
        }
        self.failures.retain(|_, failures| !failures.is_empty());
        if !self.failures.contains_key(&address) && self.failures.len() >= self.max_clients {
            return false;
        }
        self.failures.entry(address).or_default().len() < MAX_PAIRING_FAILURES_PER_MINUTE
    }

    fn record_failure(&mut self, address: IpAddr, now: Instant) {
        self.failures.entry(address).or_default().push_back(now);
    }

    fn record_success(&mut self, address: IpAddr) {
        self.failures.remove(&address);
    }
}

fn record_pairing_failure(limiter: &Arc<std::sync::Mutex<PairingLimiter>>, address: IpAddr) {
    if let Ok(mut limiter) = limiter.lock() {
        limiter.record_failure(address, Instant::now());
    }
}

fn local_addresses() -> Vec<String> {
    list_afinet_netifas()
        .map(|interfaces| {
            interfaces
                .into_iter()
                .filter_map(|(_, ip)| {
                    if ip.is_ipv4() && !ip.is_loopback() {
                        Some(ip.to_string())
                    } else {
                        None
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn render_qr(uri: &str) -> Result<String, String> {
    let code = QrCode::new(uri.as_bytes()).map_err(|error| error.to_string())?;
    Ok(code.render::<unicode::Dense1x2>().build())
}

struct MdnsRegistration {
    daemon: mdns_sd::ServiceDaemon,
    fullname: String,
}

impl MdnsRegistration {
    fn stop(self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

fn advertise_mdns(pairing: &PairingInfo) -> Option<MdnsRegistration> {
    let daemon = mdns_sd::ServiceDaemon::new().ok()?;
    let properties = [
        ("sid", pairing.session_id.as_str()),
        ("name", pairing.name.as_str()),
    ];
    let info = match mdns_sd::ServiceInfo::new(
        "_ale-my-eyes._tcp.local.",
        &pairing.name,
        &format!("{}.local.", pairing.name.replace(' ', "-")),
        &pairing.host,
        pairing.port,
        &properties[..],
    ) {
        Ok(info) => info,
        Err(_) => {
            let _ = daemon.shutdown();
            return None;
        }
    };
    let fullname = info.get_fullname().to_string();
    if daemon.register(info).is_err() {
        let _ = daemon.shutdown();
        return None;
    }
    Some(MdnsRegistration { daemon, fullname })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ale_core::actions::{Action, MouseButton};
    use ale_core::config::AppConfig;
    use ale_core::secret_store::SecretStore;
    use std::net::{IpAddr, Ipv4Addr};
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct MockPlatform {
        executed: AtomicUsize,
        capture_enabled: bool,
    }

    impl PlatformService for MockPlatform {
        fn capture_image(&self) -> Option<crate::platform::CapturedImage> {
            self.capture_enabled
                .then(|| crate::platform::CapturedImage {
                    jpeg_data: vec![0xff, 0xd8, 0xff, 0xd9],
                    coordinate_space: crate::screen_capture::ScreenCoordinateSpace {
                        image_width: 100,
                        image_height: 100,
                        source_width: 200,
                        source_height: 200,
                        desktop_x: 0,
                        desktop_y: 0,
                        desktop_width: 200,
                        desktop_height: 200,
                        scale_factor: 1.0,
                    },
                })
        }

        fn execute_plan(
            &self,
            plan: &ActionPlan,
            approved: bool,
        ) -> ale_core::Result<crate::platform::ExecutionResult> {
            assert!(approved);
            self.executed.fetch_add(1, Ordering::SeqCst);
            Ok(crate::platform::ExecutionResult {
                actions_executed: plan.actions.len(),
            })
        }

        fn is_automation_ready(&self) -> bool {
            true
        }

        fn set_sensitive_ui_visible(&self, _visible: bool) {}

        fn capabilities(&self) -> crate::platform::PlatformCapabilities {
            crate::platform::PlatformCapabilities {
                image_capture: true,
                automation: true,
                local_microphone: false,
            }
        }
    }

    fn plan() -> ActionPlan {
        let mut plan = ActionPlan::new("test".to_string());
        plan.add_action(Action::Click {
            x: 10.0,
            y: 20.0,
            button: MouseButton::Left,
        });
        plan
    }

    #[derive(Default)]
    struct TestSecretStore(StdMutex<Option<String>>);

    impl TestSecretStore {
        fn with_key(key: &str) -> Self {
            Self(StdMutex::new(Some(key.to_string())))
        }
    }

    impl SecretStore for TestSecretStore {
        fn get_api_key(&self) -> ale_core::Result<Option<String>> {
            Ok(self.0.lock().unwrap().clone())
        }

        fn set_api_key(&self, api_key: &str) -> ale_core::Result<()> {
            *self.0.lock().unwrap() = Some(api_key.to_string());
            Ok(())
        }

        fn delete_api_key(&self) -> ale_core::Result<()> {
            *self.0.lock().unwrap() = None;
            Ok(())
        }
    }

    async fn mock_json_endpoint(body: String) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let header_end = header_end + 4;
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("content-length: ")
                                .or_else(|| line.strip_prefix("Content-Length: "))
                        })
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= header_end + content_length {
                        break;
                    }
                }
            }
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8(request).unwrap()
        });
        (format!("http://{address}"), task)
    }

    async fn mock_chat_endpoint(
        response_text: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        mock_json_endpoint(format!(
            r#"{{"choices":[{{"message":{{"content":"{response_text}"}}}}],"usage":{{"total_tokens":1}}}}"#
        ))
        .await
    }

    async fn mock_hanging_endpoint() -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer).await;
            std::future::pending::<()>().await;
        });
        (format!("http://{address}"), task)
    }

    fn write_test_config(path: &Path, api_url: String, model: &str) -> AppConfig {
        let mut config = AppConfig::default();
        config.model_scheduler.enabled = false;
        config.cloud_api.api_url = api_url;
        config.cloud_api.model = model.to_string();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();
        config
    }

    #[tokio::test]
    async fn remote_context_uses_new_endpoint_model_and_key_after_settings_save() {
        let (old_url, old_request) = mock_chat_endpoint("old-response").await;
        let dir = std::env::temp_dir().join(format!("ale-hot-update-{}", uuid::Uuid::new_v4()));
        let config_path = dir.join("config.json");
        let mut old_config = write_test_config(&config_path, old_url, "old-model");
        old_config.cloud_api.api_key = "old-key".to_string();
        let secret_store = Arc::new(TestSecretStore::with_key("old-key"));
        let engine = AleEngine::new_with_secret_store(&config_path, secret_store.clone())
            .await
            .unwrap();
        let shared_engine = Arc::new(Mutex::new(engine));

        // The remote context is created before Settings is saved and retains this Arc.
        let remote_engine = shared_engine.clone();
        let platform = Arc::new(MockPlatform {
            executed: AtomicUsize::new(0),
            capture_enabled: false,
        });
        let (preview, _) = handle_command(
            remote_engine.clone(),
            platform.clone(),
            "before",
            &CommandInput::Text {
                text: "before".to_string(),
            },
        )
        .await
        .unwrap();
        assert_eq!(preview.response_text, "old-response");
        let old_request = old_request.await.unwrap();
        assert!(old_request.contains(r#""model":"old-model""#));
        assert!(old_request
            .to_ascii_lowercase()
            .contains("authorization: bearer old-key"));

        let (new_url, new_request) = mock_chat_endpoint("new-response").await;
        let mut new_config = old_config;
        new_config.cloud_api.api_url = new_url;
        new_config.cloud_api.model = "new-model".to_string();
        new_config.cloud_api.api_key = "new-key".to_string();
        crate::save_settings(shared_engine.clone(), new_config)
            .await
            .unwrap();

        assert!(Arc::ptr_eq(&shared_engine, &remote_engine));
        let (preview, _) = handle_command(
            remote_engine,
            platform,
            "after",
            &CommandInput::Text {
                text: "after".to_string(),
            },
        )
        .await
        .unwrap();
        assert_eq!(preview.response_text, "new-response");
        let new_request = new_request.await.unwrap();
        assert!(new_request.contains(r#""model":"new-model""#));
        assert!(new_request
            .to_ascii_lowercase()
            .contains("authorization: bearer new-key"));
        assert_eq!(
            secret_store.get_api_key().unwrap().as_deref(),
            Some("new-key")
        );

        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn cancelling_processing_request_prevents_late_preview() {
        let (api_url, endpoint) = mock_hanging_endpoint().await;
        let dir = std::env::temp_dir().join(format!("ale-cancel-{}", uuid::Uuid::new_v4()));
        let config_path = dir.join("config.json");
        write_test_config(&config_path, api_url, "cancel-model");
        let engine = AleEngine::new_with_secret_store(
            &config_path,
            Arc::new(TestSecretStore::with_key("cancel-key")),
        )
        .await
        .unwrap();
        let platform = Arc::new(MockPlatform {
            executed: AtomicUsize::new(0),
            capture_enabled: false,
        });
        let (results, mut receiver) = mpsc::unbounded_channel();
        let request = start_remote_request(
            Arc::new(Mutex::new(engine)),
            platform,
            None,
            Arc::new(Semaphore::new(1)),
            "cancel-request".to_string(),
            RequestPayload::Text("wait".to_string()),
            results,
            false,
            true,
            false,
        )
        .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(request);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(200);
        while let Ok(Some(result)) = tokio::time::timeout_at(deadline, receiver.recv()).await {
            assert!(matches!(result.outcome, ProcessingOutcome::Progress(_, _)));
        }
        endpoint.abort();
    }

    #[tokio::test]
    async fn real_pc_handler_does_not_bypass_disabled_model_scheduler() {
        let action_arguments = serde_json::json!({
            "actions": [{"type": "click", "x": 49.5, "y": 49.5, "button": "left"}],
            "risk_level": "low",
            "explanation": "move to center",
            "requires_confirmation": true
        })
        .to_string();
        let response_body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "ready",
                    "tool_calls": [{
                        "id": "call-1",
                        "function": {
                            "name": "execute_action_plan",
                            "arguments": action_arguments
                        }
                    }]
                }
            }],
            "usage": {"total_tokens": 1}
        })
        .to_string();
        let (api_url, api_request) = mock_json_endpoint(response_body).await;
        let dir = std::env::temp_dir().join(format!("ale-loopback-{}", uuid::Uuid::new_v4()));
        let config_path = dir.join("config.json");
        write_test_config(&config_path, api_url, "loopback-model");
        let secret_store = Arc::new(TestSecretStore::with_key("loopback-key"));
        let engine = AleEngine::new_with_secret_store(&config_path, secret_store)
            .await
            .unwrap();
        let engine = Arc::new(Mutex::new(engine));
        let platform = Arc::new(MockPlatform {
            executed: AtomicUsize::new(0),
            capture_enabled: true,
        });

        let code = "654321";
        let pairing = PairingInfo {
            host: "127.0.0.1".to_string(),
            port: 0,
            session_id: "loopback-session".to_string(),
            code: code.to_string(),
            name: "test-desktop".to_string(),
        };
        let context = ConnectionContext {
            engine,
            pairing,
            platform: platform.clone(),
            request_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_REQUESTS)),
            request_limiter: Arc::new(std::sync::Mutex::new(ClientRequestLimiter::new(
                MAX_REQUESTS_PER_MINUTE,
                Duration::from_secs(60),
                MAX_TRACKED_REQUEST_CLIENTS,
            ))),
            pairing_limiter: Arc::new(std::sync::Mutex::new(PairingLimiter::default())),
            modeld: None,
            scheduler_enabled: false,
            explicit_cloud_mode: true,
            local_planning_available: false,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            handle_connection(stream, peer, context).await
        });

        let (handshake, first) = remote_crypto::test_client_handshake_start(code).unwrap();
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        socket.send(Message::Binary(first)).await.unwrap();
        let reply = socket.next().await.unwrap().unwrap().into_data();
        let mut secure = handshake.finish(&reply).unwrap();

        let hello = loop {
            let frame = socket.next().await.unwrap().unwrap().into_data();
            if let Some(message) = secure
                .decrypt_frame(&frame, remote_crypto::MAX_SECURE_MESSAGE_BYTES)
                .unwrap()
            {
                break message;
            }
        };
        assert!(matches!(hello, RemoteMessage::ServerHello(_)));
        for frame in secure
            .encrypt_message(&RemoteMessage::ClientHello(ClientHello {
                protocol_version: REMOTE_PROTOCOL_VERSION,
                device_name: "test-android".to_string(),
            }))
            .unwrap()
        {
            socket.send(Message::Binary(frame)).await.unwrap();
        }
        for frame in secure
            .encrypt_message(&RemoteMessage::CommandRequest(CommandRequest {
                request_id: "oversized".to_string(),
                input: CommandInput::Text {
                    text: "x".repeat(MAX_TEXT_BYTES + 1),
                },
            }))
            .unwrap()
        {
            socket.send(Message::Binary(frame)).await.unwrap();
        }
        let rejection = loop {
            let frame = socket.next().await.unwrap().unwrap().into_data();
            if let Some(message) = secure
                .decrypt_frame(&frame, remote_crypto::MAX_SECURE_MESSAGE_BYTES)
                .unwrap()
            {
                break message;
            }
        };
        let RemoteMessage::Error(rejection) = rejection else {
            panic!("expected oversized request error");
        };
        assert_eq!(rejection.request_id.as_deref(), Some("oversized"));
        assert_eq!(rejection.code, "TEXT_TOO_LARGE");

        for frame in secure
            .encrypt_message(&RemoteMessage::CommandRequest(CommandRequest {
                request_id: "loopback-request".to_string(),
                input: CommandInput::Text {
                    text: "move to the center".to_string(),
                },
            }))
            .unwrap()
        {
            socket.send(Message::Binary(frame)).await.unwrap();
        }

        let rejection = loop {
            let frame = socket.next().await.unwrap().unwrap().into_data();
            if let Some(message) = secure
                .decrypt_frame(&frame, remote_crypto::MAX_SECURE_MESSAGE_BYTES)
                .unwrap()
            {
                break message;
            }
        };
        let RemoteMessage::Error(rejection) = rejection else {
            panic!("expected scheduler-disabled error");
        };
        assert_eq!(rejection.request_id.as_deref(), Some("loopback-request"));
        assert_eq!(rejection.code, "MODEL_SCHEDULER_DISABLED");
        assert_eq!(platform.executed.load(Ordering::SeqCst), 0);

        socket.close(None).await.unwrap();
        assert!(server.await.unwrap().is_ok());
        api_request.abort();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pending_plans_expire_and_are_isolated_per_session() {
        let now = Instant::now();
        let mut session_a = PendingPlans::new(2, Duration::from_secs(2));
        let mut session_b = PendingPlans::new(2, Duration::from_secs(2));
        session_a
            .insert("request".to_string(), plan(), now)
            .unwrap();

        assert!(session_b.take("request", now).is_none());
        assert!(session_a
            .take("request", now + Duration::from_secs(3))
            .is_none());
    }

    #[test]
    fn assistant_output_redacts_secrets_and_sensitive_paths() {
        let output = sanitize_speech_text(
            "key sk-abcdefghijklmnopqrstuvwxyz path /Users/alice/private.txt ok",
        );
        assert!(!output.contains("sk-abcdefghijklmnopqrstuvwxyz"));
        assert!(!output.contains("/Users/alice/private.txt"));
        assert!(output.contains("[敏感信息已隐藏]"));
        assert!(output.contains("[路径已隐藏]"));
    }

    #[test]
    fn accessibility_candidate_matching_uses_desktop_evidence() {
        let node = crate::platform::AccessibilityNode {
            node_id: "TargetSaveButton".to_string(),
            role: Some("ControlType.Button".to_string()),
            label: Some("SAVE button inside Settings dialog".to_string()),
            bounds: ale_core::model_scheduler::BoundingBox {
                x: 0.6,
                y: 0.7,
                width: 0.1,
                height: 0.08,
            },
        };
        let target = ale_core::model_scheduler::TargetRef {
            node_id: None,
            role: Some("button".to_string()),
            label: Some("SAVE button".to_string()),
            visual_description: None,
        };
        assert!(accessibility_node_matches(&node, &target));
        let unrelated = ale_core::model_scheduler::TargetRef {
            label: Some("CANCEL".to_string()),
            ..target
        };
        assert!(!accessibility_node_matches(&node, &unrelated));
    }

    #[test]
    fn pending_plan_count_is_bounded() {
        let now = Instant::now();
        let mut pending = PendingPlans::new(2, Duration::from_secs(60));
        pending.insert("one".to_string(), plan(), now).unwrap();
        pending.insert("two".to_string(), plan(), now).unwrap();
        assert!(pending.insert("three".to_string(), plan(), now).is_err());
    }

    #[test]
    fn request_rate_stays_bounded_under_stress() {
        let now = Instant::now();
        let address = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2));
        let mut limiter = ClientRequestLimiter::new(30, Duration::from_secs(60), 256);
        let accepted = (0..100_000).filter(|_| limiter.allow(address, now)).count();
        assert_eq!(accepted, 30);
        // The same source remains limited when it opens a new logical connection.
        assert!(!limiter.allow(address, now));
        assert!(limiter.allow(address, now + Duration::from_secs(60)));
    }

    #[test]
    fn request_rate_client_table_is_bounded() {
        let now = Instant::now();
        let mut limiter = ClientRequestLimiter::new(30, Duration::from_secs(60), 2);
        assert!(limiter.allow(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), now));
        assert!(limiter.allow(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2)), now));
        assert!(!limiter.allow(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3)), now));
        assert!(limiter.allow(
            IpAddr::V4(Ipv4Addr::new(10, 0, 0, 3)),
            now + Duration::from_secs(60)
        ));
    }

    #[test]
    fn pairing_failures_are_rate_limited() {
        let now = Instant::now();
        let address = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 2));
        let mut limiter = PairingLimiter::default();
        for _ in 0..MAX_PAIRING_FAILURES_PER_MINUTE {
            assert!(limiter.allow(address, now));
            limiter.record_failure(address, now);
        }
        assert!(!limiter.allow(address, now));
        assert!(limiter.allow(address, now + Duration::from_secs(60)));
    }

    #[test]
    fn pairing_failure_client_table_is_bounded_under_stress() {
        let now = Instant::now();
        let mut limiter = PairingLimiter {
            failures: HashMap::new(),
            max_clients: 256,
        };
        for index in 0..100_000_u32 {
            let address = IpAddr::V4(Ipv4Addr::from(index));
            if limiter.allow(address, now) {
                limiter.record_failure(address, now);
            }
        }
        assert_eq!(limiter.failures.len(), 256);
        assert!(!limiter.allow(IpAddr::V4(Ipv4Addr::from(u32::MAX)), now));
        assert!(limiter.allow(
            IpAddr::V4(Ipv4Addr::from(u32::MAX)),
            now + Duration::from_secs(60)
        ));
    }

    #[test]
    fn rejects_oversized_text_before_inference() {
        let oversized_text = CommandRequest {
            request_id: "text".to_string(),
            input: CommandInput::Text {
                text: "x".repeat(MAX_TEXT_BYTES + 1),
            },
        };
        assert_eq!(
            validate_command_request(&oversized_text).unwrap_err().0,
            "TEXT_TOO_LARGE"
        );
    }

    #[test]
    fn audio_assembler_validates_sequence_and_hash_before_wav() {
        let pcm = vec![0_u8; 4_800];
        let mut assembler = AudioAssembler::new(AudioStart {
            request_id: "audio".to_string(),
            format: AudioFormat::PcmS16Le,
            sample_rate_hz: 48_000,
            channels: 1,
        })
        .unwrap();
        assembler
            .push(AudioChunk {
                request_id: "audio".to_string(),
                sequence: 0,
                pcm_base64: base64::engine::general_purpose::STANDARD.encode(&pcm),
            })
            .unwrap();
        let wav = assembler
            .finish(&AudioEnd {
                request_id: "audio".to_string(),
                chunk_count: 1,
                total_frames: (pcm.len() / 2) as u64,
                sha256: format!("{:x}", Sha256::digest(&pcm)),
            })
            .unwrap();
        let reader = hound::WavReader::new(Cursor::new(wav)).unwrap();
        assert_eq!(reader.spec().sample_rate, 48_000);
        assert_eq!(reader.duration(), (pcm.len() / 2) as u32);
    }

    #[test]
    fn audio_assembler_rejects_bad_sequence_format_and_limits() {
        assert!(AudioAssembler::new(AudioStart {
            request_id: "audio".to_string(),
            format: AudioFormat::PcmS16Le,
            sample_rate_hz: 48_000,
            channels: 2,
        })
        .is_err());

        let mut assembler = AudioAssembler::new(AudioStart {
            request_id: "audio".to_string(),
            format: AudioFormat::PcmS16Le,
            sample_rate_hz: MIN_SAMPLE_RATE_HZ,
            channels: 1,
        })
        .unwrap();
        let error = assembler
            .push(AudioChunk {
                request_id: "audio".to_string(),
                sequence: 1,
                pcm_base64: base64::engine::general_purpose::STANDARD.encode([0_u8; 2]),
            })
            .unwrap_err();
        assert_eq!(error.code, "INVALID_AUDIO_SEQUENCE");

        let mut assembler = AudioAssembler::new(AudioStart {
            request_id: "audio".to_string(),
            format: AudioFormat::PcmS16Le,
            sample_rate_hz: MIN_SAMPLE_RATE_HZ,
            channels: 1,
        })
        .unwrap();
        let error = assembler
            .push(AudioChunk {
                request_id: "audio".to_string(),
                sequence: 0,
                pcm_base64: base64::engine::general_purpose::STANDARD.encode(vec![
                    0_u8;
                    MAX_AUDIO_CHUNK_BYTES
                        + 2
                ]),
            })
            .unwrap_err();
        assert_eq!(error.code, "INVALID_AUDIO_CHUNK");

        let mut assembler = AudioAssembler::new(AudioStart {
            request_id: "audio".to_string(),
            format: AudioFormat::PcmS16Le,
            sample_rate_hz: MIN_SAMPLE_RATE_HZ,
            channels: 1,
        })
        .unwrap();
        assembler.pcm = vec![0; MIN_SAMPLE_RATE_HZ as usize * 2 * MAX_RECORDING_SECONDS as usize];
        let error = assembler
            .push(AudioChunk {
                request_id: "audio".to_string(),
                sequence: 0,
                pcm_base64: base64::engine::general_purpose::STANDARD.encode([0_u8; 2]),
            })
            .unwrap_err();
        assert_eq!(error.code, "AUDIO_TOO_LARGE");
    }

    #[tokio::test]
    async fn vlm_coordinates_to_confirmation_to_automation_chain() {
        let model_output = r#"{
            "actions": [{"type":"click","x":479.5,"y":269.5,"button":"left"}],
            "risk_level":"low",
            "explanation":"click center",
            "requires_confirmation":false
        }"#;
        let parsed = parse_action_plan_arguments(model_output).unwrap();
        let coordinate_space = crate::screen_capture::ScreenCoordinateSpace {
            image_width: 960,
            image_height: 540,
            source_width: 3840,
            source_height: 2160,
            desktop_x: -1920,
            desktop_y: 0,
            desktop_width: 1920,
            desktop_height: 1080,
            scale_factor: 2.0,
        };
        let mapped = coordinate_space.map_plan(parsed).unwrap();
        let Action::Click { x, y, .. } = mapped.actions[0] else {
            panic!("expected click");
        };
        assert!((x - -960.5).abs() < 0.001);
        assert!((y - 539.5).abs() < 0.001);
        assert!(mapped.requires_confirmation);

        let mut pending = PendingPlans::new(2, Duration::from_secs(60));
        pending
            .insert("request".to_string(), mapped, Instant::now())
            .unwrap();
        let platform = Arc::new(MockPlatform {
            executed: AtomicUsize::new(0),
            capture_enabled: false,
        });
        let action = prepare_confirm(
            ConfirmExecution {
                request_id: "request".to_string(),
                approved: true,
            },
            &mut pending,
        );
        let ConfirmAction::Execute { request_id, plan } = action else {
            panic!("expected executable confirmation");
        };
        let status = execute_confirm(
            request_id,
            plan,
            platform.clone(),
            ExecutionControl::new(Instant::now() + Duration::from_secs(1)),
        );
        assert_eq!(status.state, ExecutionState::Completed);
        assert_eq!(platform.executed.load(Ordering::SeqCst), 1);
    }

    struct ControlledSlowPlatform;

    impl PlatformService for ControlledSlowPlatform {
        fn capture_image(&self) -> Option<crate::platform::CapturedImage> {
            None
        }

        fn execute_plan(
            &self,
            _plan: &ActionPlan,
            _approved: bool,
        ) -> ale_core::Result<crate::platform::ExecutionResult> {
            unreachable!("remote execution must use the controlled entrypoint")
        }

        fn execute_plan_controlled(
            &self,
            _plan: &ActionPlan,
            _approved: bool,
            control: &ExecutionControl,
        ) -> ale_core::Result<crate::platform::ExecutionResult> {
            loop {
                control.check()?;
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn is_automation_ready(&self) -> bool {
            true
        }

        fn set_sensitive_ui_visible(&self, _visible: bool) {}

        fn capabilities(&self) -> crate::platform::PlatformCapabilities {
            crate::platform::PlatformCapabilities {
                image_capture: false,
                automation: true,
                local_microphone: false,
            }
        }
    }

    #[tokio::test]
    async fn remote_execution_timeout_is_bounded_and_cooperative() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let request = start_remote_execution(
            "slow".to_string(),
            plan(),
            Arc::new(ControlledSlowPlatform),
            None,
            sender,
        );
        let result = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.request_id, "slow");
        assert!(matches!(result.kind, RemoteExecutionEventKind::TimedOut));
        let finished = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            finished.kind,
            RemoteExecutionEventKind::FinishedAfterTimeout
        ));
        drop(request);
    }

    #[tokio::test]
    async fn controlled_execution_rejects_a_stale_snapshot_before_input() {
        let platform = Arc::new(MockPlatform {
            executed: AtomicUsize::new(0),
            capture_enabled: true,
        });
        let mut controlled = ActionPlan::new("controlled".to_string());
        controlled.add_action(Action::ControlledTestClick {
            x: 50.0,
            y: 50.0,
            window_title: "ALE MODEL RUNTIME CONTROLLED TEST".to_string(),
            target_name: "SAVE button inside Settings dialog".to_string(),
            snapshot_id: "stale-snapshot".to_string(),
        });
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let request = start_remote_execution(
            "stale".to_string(),
            controlled,
            platform.clone(),
            None,
            sender,
        );
        let result = receiver.recv().await.unwrap();
        let RemoteExecutionEventKind::Complete(RemoteExecutionOutcome::Status(status)) =
            result.kind
        else {
            panic!("expected stale snapshot status");
        };
        assert_eq!(status.state, ExecutionState::Failed);
        assert_eq!(status.message, "SNAPSHOT_EXPIRED");
        assert_eq!(status.actions_executed, 0);
        assert_eq!(platform.executed.load(Ordering::SeqCst), 0);
        drop(request);
    }
}
