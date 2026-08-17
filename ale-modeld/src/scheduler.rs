use ale_core::model_ipc::{
    IpcEnvelope, IpcReply, IpcReplyStatus, IpcRequestKind, MODEL_IPC_VERSION,
};
use ale_core::model_scheduler::{ModelCapability, RouteDecision, RouteTarget};
use ale_core::model_scheduler::{
    ModelJob, ModelRuntimeConfig, RemoteEndpointConfig, RemoteEndpointRole, RemotePlanningJob,
    RemotePlanningResult, RemoteProviderSet, SchedulerHealth, SpeechRecognitionJob,
    SpeechRecognitionResult,
};
use base64::Engine;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct CircuitState {
    consecutive_failures: u32,
    open_until: Option<Instant>,
}

#[derive(Default)]
pub struct ModelScheduler {
    remote: Mutex<Option<RemoteProviderSet>>,
    models: Mutex<Option<ModelRuntimeConfig>>,
    primary_circuit: Mutex<CircuitState>,
    sensevoice: Arc<crate::sensevoice::SenseVoiceAdapter>,
}

impl ModelScheduler {
    pub fn maintenance(&self) {
        self.sensevoice.unload_if_idle();
    }

    pub async fn handle(&self, request: IpcEnvelope) -> IpcReply {
        if request.protocol_version != MODEL_IPC_VERSION {
            return error_reply(
                request.request_id,
                "PROTOCOL_MISMATCH",
                "model IPC version mismatch",
            );
        }
        let kind = IpcRequestKind::try_from(request.kind).ok();
        match kind {
            Some(IpcRequestKind::Health) => {
                let mut available_capabilities = Vec::new();
                if self
                    .remote
                    .lock()
                    .expect("remote provider lock poisoned")
                    .is_some()
                {
                    available_capabilities.push(ModelCapability::RemotePlanning);
                }
                if self
                    .models
                    .lock()
                    .expect("model config lock poisoned")
                    .as_ref()
                    .is_some_and(crate::sensevoice::SenseVoiceAdapter::available)
                {
                    available_capabilities.push(ModelCapability::SpeechRecognition);
                }
                ok_json(
                    request.request_id,
                    &SchedulerHealth {
                        service: "ale-modeld".to_string(),
                        protocol_version: MODEL_IPC_VERSION,
                        local_vlm_gpu_only: true,
                        gpus: crate::gpu::probe(),
                        available_capabilities,
                    },
                )
            }
            Some(IpcRequestKind::Schedule) => self.schedule(request).await,
            Some(IpcRequestKind::Cancel) | Some(IpcRequestKind::Shutdown) => {
                ok_json(request.request_id, &serde_json::json!({"accepted": true}))
            }
            Some(IpcRequestKind::ConfigureRemote) => self.configure_remote(request),
            Some(IpcRequestKind::ConfigureModels) => self.configure_models(request),
            Some(IpcRequestKind::Authenticate) | None => error_reply(
                request.request_id,
                "INVALID_REQUEST",
                "unsupported modeld request",
            ),
        }
    }

    async fn schedule(&self, request: IpcEnvelope) -> IpcReply {
        let job: ModelJob = match serde_json::from_slice(&request.payload) {
            Ok(job) => job,
            Err(error) => {
                return error_reply(request.request_id, "INVALID_JOB", &error.to_string())
            }
        };
        if job.request_id != request.request_id {
            return error_reply(
                request.request_id,
                "REQUEST_ID_MISMATCH",
                "model job request ID does not match IPC envelope",
            );
        }
        let now = chrono::Utc::now().timestamp_millis();
        if job.deadline_unix_ms <= now {
            return error_reply(
                request.request_id,
                "DEADLINE_EXCEEDED",
                "model job deadline has elapsed",
            );
        }

        let remaining = Duration::from_millis((job.deadline_unix_ms - now) as u64);
        let stage_timeout = remaining.min(ale_core::model_scheduler::MODEL_STAGE_TIMEOUT);
        let request_id = request.request_id;
        match tokio::time::timeout(stage_timeout, self.run_job(request_id.clone(), job)).await {
            Ok(reply) => reply,
            Err(_) => error_reply(
                request_id,
                "DEADLINE_EXCEEDED",
                "model stage exceeded its deadline",
            ),
        }
    }

    async fn run_job(&self, request_id: String, job: ModelJob) -> IpcReply {
        if job.capability == ModelCapability::RemotePlanning {
            if !job.privacy.allow_remote {
                return error_reply(
                    request_id,
                    "REMOTE_NOT_AUTHORIZED",
                    "remote inference was not authorized",
                );
            }
            return self.remote_plan(request_id, job).await;
        }
        if job.capability == ModelCapability::SpeechRecognition {
            return self.speech_recognition(request_id, job).await;
        }
        let decision = RouteDecision {
            target: RouteTarget::UserDecisionRequired,
            reasons: vec![ale_core::model_scheduler::EscalationReason::LocalModelUnavailable],
            requires_confirmation: true,
        };
        let mut reply = ok_json(request_id, &decision);
        if decision.target == RouteTarget::UserDecisionRequired {
            reply.status = IpcReplyStatus::DecisionRequired as i32;
        }
        reply
    }

    fn configure_remote(&self, request: IpcEnvelope) -> IpcReply {
        let providers: RemoteProviderSet = match serde_json::from_slice(&request.payload) {
            Ok(value) => value,
            Err(error) => {
                return error_reply(
                    request.request_id,
                    "INVALID_REMOTE_CONFIG",
                    &error.to_string(),
                )
            }
        };
        if providers.primary.api_key.trim().is_empty() {
            return error_reply(
                request.request_id,
                "MISSING_API_KEY",
                "primary API key is required",
            );
        }
        if providers.backup_enabled
            && (!providers.backup_pre_authorized || providers.backup.is_none())
        {
            return error_reply(
                request.request_id,
                "BACKUP_NOT_AUTHORIZED",
                "enabled backup endpoint requires configuration and pre-authorization",
            );
        }
        *self.remote.lock().expect("remote provider lock poisoned") = Some(providers);
        ok_json(request.request_id, &serde_json::json!({"configured": true}))
    }

    fn configure_models(&self, request: IpcEnvelope) -> IpcReply {
        let config: ModelRuntimeConfig = match serde_json::from_slice(&request.payload) {
            Ok(value) => value,
            Err(error) => {
                return error_reply(
                    request.request_id,
                    "INVALID_MODEL_CONFIG",
                    &error.to_string(),
                )
            }
        };
        *self.models.lock().expect("model config lock poisoned") = Some(config);
        ok_json(request.request_id, &serde_json::json!({"configured": true}))
    }

    async fn speech_recognition(&self, request_id: String, job: ModelJob) -> IpcReply {
        let speech: SpeechRecognitionJob = match serde_json::from_value(job.payload) {
            Ok(value) => value,
            Err(error) => return error_reply(request_id, "INVALID_ASR_JOB", &error.to_string()),
        };
        let wav = match base64::engine::general_purpose::STANDARD.decode(&speech.wav_base64) {
            Ok(value) => value,
            Err(error) => return error_reply(request_id, "INVALID_AUDIO", &error.to_string()),
        };
        let runtime = self
            .models
            .lock()
            .expect("model config lock poisoned")
            .clone();
        let adapter = self.sensevoice.clone();
        let local_wav = wav.clone();
        let local_result = match runtime {
            Some(config) => {
                tokio::task::spawn_blocking(move || adapter.transcribe_wav(&config, &local_wav))
                    .await
                    .map_err(|error| format!("SenseVoice task failed: {error}"))
                    .and_then(|result| result)
            }
            None => Err("local model runtime is not configured".to_string()),
        };
        if let Ok(text) = local_result {
            return ok_json(
                request_id,
                &SpeechRecognitionResult {
                    text,
                    model_id: "SenseVoiceSmall".to_string(),
                    used_remote: false,
                    failover_notice: None,
                },
            );
        }
        if !speech.allow_remote || !job.privacy.allow_remote {
            let mut reply = error_reply(
                request_id,
                "LOCAL_ASR_UNAVAILABLE",
                "SenseVoiceSmall is unavailable and remote ASR was not authorized",
            );
            reply.status = IpcReplyStatus::DecisionRequired as i32;
            return reply;
        }
        self.remote_transcribe(request_id, &wav).await
    }

    async fn remote_transcribe(&self, request_id: String, wav: &[u8]) -> IpcReply {
        let providers = match self
            .remote
            .lock()
            .expect("remote provider lock poisoned")
            .clone()
        {
            Some(value) => value,
            None => {
                return error_reply(
                    request_id,
                    "REMOTE_NOT_CONFIGURED",
                    "remote provider is not configured",
                )
            }
        };
        match call_remote_transcribe(&providers.primary, wav).await {
            Ok(text) => ok_json(
                request_id,
                &SpeechRecognitionResult {
                    text,
                    model_id: "remote-asr".to_string(),
                    used_remote: true,
                    failover_notice: None,
                },
            ),
            Err(primary_error)
                if is_transient_remote_error(&primary_error)
                    && providers.backup_enabled
                    && providers.backup_pre_authorized =>
            {
                if let Some(backup) = &providers.backup {
                    match call_remote_transcribe(backup, wav).await {
                        Ok(text) => ok_json(
                            request_id,
                            &SpeechRecognitionResult {
                                text,
                                model_id: "remote-asr-backup".to_string(),
                                used_remote: true,
                                failover_notice: Some(
                                    "主模型不可用，语音识别已切换到备用端点".to_string(),
                                ),
                            },
                        ),
                        Err(error) => {
                            error_reply(request_id, "REMOTE_ASR_FAILED", &error.to_string())
                        }
                    }
                } else {
                    error_reply(request_id, "REMOTE_ASR_FAILED", &primary_error.to_string())
                }
            }
            Err(error) => error_reply(request_id, "REMOTE_ASR_FAILED", &error.to_string()),
        }
    }

    async fn remote_plan(&self, request_id: String, job: ModelJob) -> IpcReply {
        let planning: RemotePlanningJob = match serde_json::from_value(job.payload) {
            Ok(value) => value,
            Err(error) => return error_reply(request_id, "INVALID_REMOTE_JOB", &error.to_string()),
        };
        if planning.image_base64.is_some() && !job.privacy.allow_full_screenshot {
            return error_reply(
                request_id,
                "SCREENSHOT_NOT_AUTHORIZED",
                "full screenshot payload was not authorized",
            );
        }
        let providers = match self
            .remote
            .lock()
            .expect("remote provider lock poisoned")
            .clone()
        {
            Some(value) => value,
            None => {
                return error_reply(
                    request_id,
                    "REMOTE_NOT_CONFIGURED",
                    "remote provider is not configured",
                )
            }
        };

        let primary_open = self
            .primary_circuit
            .lock()
            .expect("circuit lock poisoned")
            .open_until
            .is_some_and(|until| until > Instant::now());
        if !primary_open {
            match call_remote(&providers.primary, &planning).await {
                Ok(response) => {
                    let mut circuit = self.primary_circuit.lock().expect("circuit lock poisoned");
                    circuit.consecutive_failures = 0;
                    circuit.open_until = None;
                    return ok_json(
                        request_id,
                        &RemotePlanningResult {
                            response,
                            endpoint: RemoteEndpointRole::Primary,
                            failover_notice: None,
                        },
                    );
                }
                Err(primary_error) => {
                    if !is_transient_remote_error(&primary_error) {
                        return error_reply(
                            request_id,
                            "PRIMARY_REMOTE_REJECTED",
                            &primary_error.to_string(),
                        );
                    }
                    let mut circuit = self.primary_circuit.lock().expect("circuit lock poisoned");
                    circuit.consecutive_failures = circuit.consecutive_failures.saturating_add(1);
                    if circuit.consecutive_failures >= providers.circuit_failure_threshold.max(1) {
                        circuit.open_until = Some(
                            Instant::now()
                                + Duration::from_secs(providers.circuit_open_seconds.max(1) as u64),
                        );
                    }
                    tracing::warn!("primary remote model failed with a transient error");
                }
            }
        }

        if providers.backup_enabled && providers.backup_pre_authorized {
            if let Some(backup) = &providers.backup {
                return match call_remote(backup, &planning).await {
                    Ok(response) => ok_json(
                        request_id,
                        &RemotePlanningResult {
                            response,
                            endpoint: RemoteEndpointRole::Backup,
                            failover_notice: Some(
                                "主模型不可用，已切换到预先授权的备用模型".to_string(),
                            ),
                        },
                    ),
                    Err(error) => error_reply(request_id, "REMOTE_FAILED", &error.to_string()),
                };
            }
        }
        error_reply(
            request_id,
            "PRIMARY_REMOTE_FAILED",
            "primary remote model failed and no authorized backup is available",
        )
    }
}

async fn call_remote(
    endpoint: &RemoteEndpointConfig,
    job: &RemotePlanningJob,
) -> ale_core::Result<ale_core::cloud::VisionResponse> {
    use ale_core::cloud::{
        CloudApiFactory, CloudConfig, CloudMessage, CloudProvider, VisionResponse,
    };
    let provider = match endpoint.provider.to_ascii_lowercase().as_str() {
        "openai" => CloudProvider::OpenAI,
        "anthropic" => CloudProvider::Anthropic,
        "google" => CloudProvider::Google,
        "azure" => CloudProvider::Azure,
        other => CloudProvider::Custom(other.to_string()),
    };
    let api = CloudApiFactory::create(CloudConfig {
        provider,
        api_key: endpoint.api_key.clone(),
        api_url: endpoint.api_url.clone(),
        model: endpoint.model.clone(),
        max_tokens: endpoint.max_tokens,
        timeout: Duration::from_secs(endpoint.timeout_seconds.max(1) as u64),
        retry_count: 0,
    });
    if let Some(image_base64) = &job.image_base64 {
        let image = base64::engine::general_purpose::STANDARD
            .decode(image_base64)
            .map_err(|error| {
                ale_core::AleError::CloudApiError(format!("invalid image payload: {error}"))
            })?;
        api.vision_ask(&image, &job.question, job.tools.clone())
            .await
    } else {
        let response = api
            .chat(vec![CloudMessage {
                role: "user".to_string(),
                content: job.question.clone(),
            }])
            .await?;
        Ok(VisionResponse {
            content: response.content,
            tool_calls: None,
            tokens_used: response.tokens_used,
            model: response.model,
        })
    }
}

async fn call_remote_transcribe(
    endpoint: &RemoteEndpointConfig,
    wav: &[u8],
) -> ale_core::Result<String> {
    let api = remote_api(endpoint);
    api.transcribe(wav).await.map(|response| response.content)
}

fn remote_api(endpoint: &RemoteEndpointConfig) -> Box<dyn ale_core::cloud::CloudApi> {
    use ale_core::cloud::{CloudApiFactory, CloudConfig, CloudProvider};
    let provider = match endpoint.provider.to_ascii_lowercase().as_str() {
        "openai" => CloudProvider::OpenAI,
        "anthropic" => CloudProvider::Anthropic,
        "google" => CloudProvider::Google,
        "azure" => CloudProvider::Azure,
        other => CloudProvider::Custom(other.to_string()),
    };
    CloudApiFactory::create(CloudConfig {
        provider,
        api_key: endpoint.api_key.clone(),
        api_url: endpoint.api_url.clone(),
        model: endpoint.model.clone(),
        max_tokens: endpoint.max_tokens,
        timeout: Duration::from_secs(endpoint.timeout_seconds.max(1) as u64),
        retry_count: 0,
    })
}

fn is_transient_remote_error(error: &ale_core::AleError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("request failed")
        || message.contains("timed out")
        || message.contains("timeout")
        || message.contains("429 too many requests")
        || (500..=599).any(|status| message.contains(&format!("{status} ")))
}

pub(crate) fn ok_json(request_id: String, value: &impl Serialize) -> IpcReply {
    match serde_json::to_vec(value) {
        Ok(payload) => IpcReply {
            protocol_version: MODEL_IPC_VERSION,
            request_id,
            status: IpcReplyStatus::Ok as i32,
            payload,
            error_code: String::new(),
            error_message: String::new(),
        },
        Err(error) => error_reply(request_id, "SERIALIZATION_FAILED", &error.to_string()),
    }
}

pub(crate) fn error_reply(request_id: String, code: &str, message: &str) -> IpcReply {
    IpcReply {
        protocol_version: MODEL_IPC_VERSION,
        request_id,
        status: IpcReplyStatus::Error as i32,
        payload: Vec::new(),
        error_code: code.to_string(),
        error_message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn mock_endpoint(status: &str, body: &str) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_string();
        let body = body.to_string();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 16 * 1024];
            let _ = stream.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        format!("http://{address}")
    }

    async fn hanging_endpoint() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 16 * 1024];
            let _ = stream.read(&mut request).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        format!("http://{address}")
    }

    fn endpoint(api_url: String) -> RemoteEndpointConfig {
        RemoteEndpointConfig {
            provider: "openai".to_string(),
            api_key: "test".to_string(),
            api_url,
            model: "test-model".to_string(),
            max_tokens: 64,
            timeout_seconds: 2,
        }
    }

    #[tokio::test]
    async fn unavailable_local_capability_requires_a_decision() {
        let payload = serde_json::to_vec(&ModelJob {
            request_id: "job".to_string(),
            capability: ModelCapability::LocalPlanning,
            priority: ale_core::model_scheduler::SchedulerPriority::InteractiveRequest,
            deadline_unix_ms: chrono::Utc::now().timestamp_millis() + 1_000,
            risk_ceiling: ale_core::actions::RiskLevel::Low,
            snapshot_id: None,
            privacy: ale_core::model_scheduler::JobPrivacy::default(),
            payload: serde_json::Value::Null,
        })
        .unwrap();
        let reply = ModelScheduler::default()
            .handle(IpcEnvelope {
                protocol_version: MODEL_IPC_VERSION,
                request_id: "job".to_string(),
                kind: IpcRequestKind::Schedule as i32,
                payload,
            })
            .await;
        assert_eq!(reply.status, IpcReplyStatus::DecisionRequired as i32);
    }

    #[tokio::test]
    async fn preauthorized_backup_handles_primary_failure() {
        let primary = mock_endpoint("500 Internal Server Error", "{}").await;
        let backup = mock_endpoint(
            "200 OK",
            r#"{"choices":[{"message":{"content":"backup response"}}],"usage":{"total_tokens":2}}"#,
        )
        .await;
        let scheduler = ModelScheduler::default();
        let configure = scheduler
            .handle(IpcEnvelope {
                protocol_version: MODEL_IPC_VERSION,
                request_id: "configure".to_string(),
                kind: IpcRequestKind::ConfigureRemote as i32,
                payload: serde_json::to_vec(&RemoteProviderSet {
                    primary: endpoint(primary),
                    backup: Some(endpoint(backup)),
                    backup_enabled: true,
                    backup_pre_authorized: true,
                    circuit_failure_threshold: 1,
                    circuit_open_seconds: 60,
                })
                .unwrap(),
            })
            .await;
        assert_eq!(configure.status, IpcReplyStatus::Ok as i32);

        let planning = RemotePlanningJob {
            question: "hello".to_string(),
            image_base64: None,
            tools: None,
        };
        let reply = scheduler
            .handle(IpcEnvelope {
                protocol_version: MODEL_IPC_VERSION,
                request_id: "plan".to_string(),
                kind: IpcRequestKind::Schedule as i32,
                payload: serde_json::to_vec(&ModelJob {
                    request_id: "plan".to_string(),
                    capability: ModelCapability::RemotePlanning,
                    priority: ale_core::model_scheduler::SchedulerPriority::InteractiveRequest,
                    deadline_unix_ms: chrono::Utc::now().timestamp_millis() + 2_000,
                    risk_ceiling: ale_core::actions::RiskLevel::High,
                    snapshot_id: None,
                    privacy: ale_core::model_scheduler::JobPrivacy {
                        allow_remote: true,
                        allow_full_screenshot: false,
                        allow_sensitive_content: false,
                    },
                    payload: serde_json::to_value(planning).unwrap(),
                })
                .unwrap(),
            })
            .await;
        assert_eq!(reply.status, IpcReplyStatus::Ok as i32);
        let result: RemotePlanningResult = serde_json::from_slice(&reply.payload).unwrap();
        assert_eq!(result.endpoint, RemoteEndpointRole::Backup);
        assert_eq!(result.response.content, "backup response");
        assert!(result.failover_notice.is_some());
    }

    #[tokio::test]
    async fn authentication_failure_does_not_switch_to_backup() {
        let primary = mock_endpoint("401 Unauthorized", "denied").await;
        let backup = mock_endpoint(
            "200 OK",
            r#"{"choices":[{"message":{"content":"must not run"}}]}"#,
        )
        .await;
        let scheduler = ModelScheduler::default();
        let _ = scheduler
            .handle(IpcEnvelope {
                protocol_version: MODEL_IPC_VERSION,
                request_id: "configure".to_string(),
                kind: IpcRequestKind::ConfigureRemote as i32,
                payload: serde_json::to_vec(&RemoteProviderSet {
                    primary: endpoint(primary),
                    backup: Some(endpoint(backup)),
                    backup_enabled: true,
                    backup_pre_authorized: true,
                    circuit_failure_threshold: 1,
                    circuit_open_seconds: 60,
                })
                .unwrap(),
            })
            .await;
        let reply = scheduler
            .handle(IpcEnvelope {
                protocol_version: MODEL_IPC_VERSION,
                request_id: "plan".to_string(),
                kind: IpcRequestKind::Schedule as i32,
                payload: serde_json::to_vec(&ModelJob {
                    request_id: "plan".to_string(),
                    capability: ModelCapability::RemotePlanning,
                    priority: ale_core::model_scheduler::SchedulerPriority::InteractiveRequest,
                    deadline_unix_ms: chrono::Utc::now().timestamp_millis() + 2_000,
                    risk_ceiling: ale_core::actions::RiskLevel::High,
                    snapshot_id: None,
                    privacy: ale_core::model_scheduler::JobPrivacy {
                        allow_remote: true,
                        allow_full_screenshot: false,
                        allow_sensitive_content: false,
                    },
                    payload: serde_json::to_value(RemotePlanningJob {
                        question: "hello".to_string(),
                        image_base64: None,
                        tools: None,
                    })
                    .unwrap(),
                })
                .unwrap(),
            })
            .await;
        assert_eq!(reply.error_code, "PRIMARY_REMOTE_REJECTED");
    }

    #[tokio::test]
    async fn remote_stage_observes_job_deadline() {
        let scheduler = ModelScheduler::default();
        let configure = scheduler
            .handle(IpcEnvelope {
                protocol_version: MODEL_IPC_VERSION,
                request_id: "configure".to_string(),
                kind: IpcRequestKind::ConfigureRemote as i32,
                payload: serde_json::to_vec(&RemoteProviderSet {
                    primary: endpoint(hanging_endpoint().await),
                    backup: None,
                    backup_enabled: false,
                    backup_pre_authorized: false,
                    circuit_failure_threshold: 3,
                    circuit_open_seconds: 60,
                })
                .unwrap(),
            })
            .await;
        assert_eq!(configure.status, IpcReplyStatus::Ok as i32);

        let reply = scheduler
            .handle(IpcEnvelope {
                protocol_version: MODEL_IPC_VERSION,
                request_id: "deadline".to_string(),
                kind: IpcRequestKind::Schedule as i32,
                payload: serde_json::to_vec(&ModelJob {
                    request_id: "deadline".to_string(),
                    capability: ModelCapability::RemotePlanning,
                    priority: ale_core::model_scheduler::SchedulerPriority::InteractiveRequest,
                    deadline_unix_ms: chrono::Utc::now().timestamp_millis() + 50,
                    risk_ceiling: ale_core::actions::RiskLevel::High,
                    snapshot_id: None,
                    privacy: ale_core::model_scheduler::JobPrivacy {
                        allow_remote: true,
                        allow_full_screenshot: false,
                        allow_sensitive_content: false,
                    },
                    payload: serde_json::to_value(RemotePlanningJob {
                        question: "wait".to_string(),
                        image_base64: None,
                        tools: None,
                    })
                    .unwrap(),
                })
                .unwrap(),
            })
            .await;
        assert_eq!(reply.error_code, "DEADLINE_EXCEEDED");
    }

    #[tokio::test]
    async fn screenshot_payload_requires_matching_privacy_grant() {
        let scheduler = ModelScheduler::default();
        let reply = scheduler
            .handle(IpcEnvelope {
                protocol_version: MODEL_IPC_VERSION,
                request_id: "privacy".to_string(),
                kind: IpcRequestKind::Schedule as i32,
                payload: serde_json::to_vec(&ModelJob {
                    request_id: "privacy".to_string(),
                    capability: ModelCapability::RemotePlanning,
                    priority: ale_core::model_scheduler::SchedulerPriority::InteractiveRequest,
                    deadline_unix_ms: chrono::Utc::now().timestamp_millis() + 1_000,
                    risk_ceiling: ale_core::actions::RiskLevel::High,
                    snapshot_id: Some("snapshot".to_string()),
                    privacy: ale_core::model_scheduler::JobPrivacy {
                        allow_remote: true,
                        allow_full_screenshot: false,
                        allow_sensitive_content: false,
                    },
                    payload: serde_json::to_value(RemotePlanningJob {
                        question: "inspect".to_string(),
                        image_base64: Some("AA==".to_string()),
                        tools: None,
                    })
                    .unwrap(),
                })
                .unwrap(),
            })
            .await;
        assert_eq!(reply.error_code, "SCREENSHOT_NOT_AUTHORIZED");
    }
}
