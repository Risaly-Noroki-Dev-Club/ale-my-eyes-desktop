use ale_core::model_ipc::{
    IpcEnvelope, IpcReply, IpcReplyStatus, IpcRequestKind, MODEL_IPC_VERSION,
};
use ale_core::model_scheduler::{
    GroundingJob, LocalPlanningJob, ModelJob, ModelRuntimeConfig, RemoteEndpointConfig,
    RemoteEndpointRole, RemotePlanningJob, RemotePlanningResult, RemoteProviderSet,
    SchedulerHealth, SpeechRecognitionJob, SpeechRecognitionResult, StateVerificationJob,
};
use ale_core::model_scheduler::{ModelCapability, RouteDecision, RouteTarget};
use base64::Engine;
use serde::Serialize;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

tokio::task_local! { static PROVIDERS: Option<RemoteProviderSet>; }

#[derive(Default)]
struct CircuitState {
    revision: u64,
    consecutive_failures: u32,
    open_until: Option<Instant>,
}

#[derive(Default)]
pub struct ModelScheduler {
    remote: Mutex<Option<RemoteProviderSet>>,
    models: Mutex<Option<ModelRuntimeConfig>>,
    primary_circuit: Mutex<CircuitState>,
    sensevoice: Arc<crate::sensevoice::SenseVoiceAdapter>,
    llama: Arc<crate::llama::LlamaAdapter>,
}

impl ModelScheduler {
    pub fn maintenance(&self) {
        self.sensevoice.unload_if_idle();
        self.llama.maintenance();
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
                    .as_ref()
                    .is_some_and(|providers| !providers.primary.api_key.trim().is_empty())
                {
                    available_capabilities.push(ModelCapability::RemotePlanning);
                }
                let runtime = self
                    .models
                    .lock()
                    .expect("model config lock poisoned")
                    .clone();
                if let Some(runtime) = runtime.as_ref() {
                    if crate::sensevoice::SenseVoiceAdapter::available(runtime) {
                        available_capabilities.push(ModelCapability::SpeechRecognition);
                    }
                    available_capabilities.extend(self.llama.capabilities(runtime));
                    available_capabilities.sort_by_key(|capability| *capability as u8);
                    available_capabilities.dedup();
                }
                ok_json(
                    request.request_id,
                    &SchedulerHealth {
                        service: "ale-modeld".to_string(),
                        protocol_version: MODEL_IPC_VERSION,
                        local_vlm_gpu_only: true,
                        gpus: crate::gpu::probe_with_runtime(runtime.as_ref()),
                        available_capabilities,
                        hot_worker: self.llama.worker_health(),
                        sensevoice_state: Some(self.sensevoice.state()),
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
        let stage_timeout = if matches!(
            job.capability,
            ModelCapability::RemotePlanning | ModelCapability::SpeechRecognition
        ) {
            remaining
        } else {
            remaining.min(ale_core::model_scheduler::MODEL_STAGE_TIMEOUT)
        };
        let providers = job
            .remote_snapshot
            .clone()
            .or_else(|| self.remote.lock().unwrap().clone());
        let deadline = tokio::time::Instant::now() + stage_timeout;
        let request_id = request.request_id;
        match tokio::time::timeout_at(
            deadline,
            ale_core::model_api::DEADLINE.scope(
                deadline,
                PROVIDERS.scope(providers, self.run_job(request_id.clone(), job)),
            ),
        )
        .await
        {
            Ok(reply) if tokio::time::Instant::now() < deadline => reply,
            _ => error_reply(
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
        if matches!(
            job.capability,
            ModelCapability::StateSummary
                | ModelCapability::LocalPlanning
                | ModelCapability::ElementGrounding
                | ModelCapability::StateVerification
        ) && !self.local_capability_available(job.capability)
        {
            return decision_required(request_id);
        }
        if matches!(
            job.capability,
            ModelCapability::StateSummary | ModelCapability::LocalPlanning
        ) {
            return self.local_plan(request_id, job).await;
        }
        if job.capability == ModelCapability::ElementGrounding {
            return self.ground(request_id, job).await;
        }
        if job.capability == ModelCapability::StateVerification {
            return self.verify(request_id, job).await;
        }
        decision_required(request_id)
    }

    fn local_capability_available(&self, capability: ModelCapability) -> bool {
        self.models
            .lock()
            .expect("model config lock poisoned")
            .as_ref()
            .is_some_and(|runtime| self.llama.capabilities(runtime).contains(&capability))
    }

    fn runtime(&self) -> Result<ModelRuntimeConfig, &'static str> {
        self.models
            .lock()
            .expect("model config lock poisoned")
            .clone()
            .ok_or("local model runtime is not configured")
    }

    async fn local_plan(&self, request_id: String, job: ModelJob) -> IpcReply {
        let snapshot_id = match job.snapshot_id.as_deref() {
            Some(value) if !value.trim().is_empty() => value.to_string(),
            _ => {
                return error_reply(
                    request_id,
                    "SNAPSHOT_REQUIRED",
                    "local planning requires a snapshot ID",
                )
            }
        };
        let risk_ceiling = job.risk_ceiling;
        let planning: LocalPlanningJob = match serde_json::from_value(job.payload) {
            Ok(value) => value,
            Err(error) => return error_reply(request_id, "INVALID_LOCAL_JOB", &error.to_string()),
        };
        let runtime = match self.runtime() {
            Ok(value) => value,
            Err(error) => return error_reply(request_id, "LOCAL_MODEL_UNAVAILABLE", error),
        };
        match self
            .llama
            .local_plan(&runtime, &snapshot_id, planning)
            .await
        {
            Ok(result) if result.plan.maximum_risk() <= risk_ceiling => {
                ok_json(request_id, &result)
            }
            Ok(_) => error_reply(
                request_id,
                "RISK_CEILING_EXCEEDED",
                "desktop risk recomputation exceeded the model job ceiling",
            ),
            Err(error) => error_reply(request_id, "LOCAL_PLANNING_FAILED", &error),
        }
    }

    async fn ground(&self, request_id: String, job: ModelJob) -> IpcReply {
        let snapshot_id = match job.snapshot_id.as_deref() {
            Some(value) if !value.trim().is_empty() => value.to_string(),
            _ => {
                return error_reply(
                    request_id,
                    "SNAPSHOT_REQUIRED",
                    "grounding requires a snapshot ID",
                )
            }
        };
        let grounding: GroundingJob = match serde_json::from_value(job.payload) {
            Ok(value) => value,
            Err(error) => {
                return error_reply(request_id, "INVALID_GROUNDING_JOB", &error.to_string())
            }
        };
        let runtime = match self.runtime() {
            Ok(value) => value,
            Err(error) => return error_reply(request_id, "LOCAL_MODEL_UNAVAILABLE", error),
        };
        match self.llama.ground(&runtime, &snapshot_id, grounding).await {
            Ok(result) => ok_json(request_id, &result),
            Err(error) => error_reply(request_id, "GROUNDING_FAILED", &error),
        }
    }

    async fn verify(&self, request_id: String, job: ModelJob) -> IpcReply {
        let snapshot_id = match job.snapshot_id.as_deref() {
            Some(value) if !value.trim().is_empty() => value.to_string(),
            _ => {
                return error_reply(
                    request_id,
                    "SNAPSHOT_REQUIRED",
                    "verification requires a snapshot ID",
                )
            }
        };
        let verification: StateVerificationJob = match serde_json::from_value(job.payload) {
            Ok(value) => value,
            Err(error) => {
                return error_reply(request_id, "INVALID_VERIFICATION_JOB", &error.to_string())
            }
        };
        let runtime = match self.runtime() {
            Ok(value) => value,
            Err(error) => return error_reply(request_id, "LOCAL_MODEL_UNAVAILABLE", error),
        };
        match self
            .llama
            .verify(&runtime, &snapshot_id, verification)
            .await
        {
            Ok(result) => ok_json(request_id, &result),
            Err(error) => error_reply(request_id, "VERIFICATION_FAILED", &error),
        }
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
        if providers.backup_enabled
            && (!providers.backup_pre_authorized || providers.backup.is_none())
        {
            return error_reply(
                request.request_id,
                "BACKUP_NOT_AUTHORIZED",
                "enabled backup endpoint requires configuration and pre-authorization",
            );
        }
        *self.primary_circuit.lock().unwrap() = CircuitState {
            revision: providers.revision,
            ..Default::default()
        };
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
        self.llama.reconfigure();
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
            Some(config) => tokio::time::timeout(
                ale_core::model_scheduler::MODEL_STAGE_TIMEOUT,
                tokio::task::spawn_blocking(move || adapter.transcribe_wav(&config, &local_wav)),
            )
            .await
            .map_err(|_| "SenseVoice stage timed out".to_string())
            .and_then(|result| result.map_err(|error| format!("SenseVoice task failed: {error}")))
            .and_then(|result| result),
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
        let providers = match PROVIDERS
            .try_with(Clone::clone)
            .ok()
            .flatten()
            .or_else(|| self.remote.lock().unwrap().clone())
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
        let Some(endpoint) = providers.transcription else {
            return error_reply(
                request_id,
                "REMOTE_ASR_NOT_CONFIGURED",
                "Configure an independent transcription endpoint",
            );
        };
        match ale_core::model_api::retry(ale_core::model_api::deadline(), 1, || {
            call_remote_transcribe(&endpoint, wav)
        })
        .await
        {
            Ok(text) => ok_json(
                request_id,
                &SpeechRecognitionResult {
                    text,
                    model_id: endpoint.model,
                    used_remote: true,
                    failover_notice: None,
                },
            ),
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
        let providers = match PROVIDERS
            .try_with(Clone::clone)
            .ok()
            .flatten()
            .or_else(|| self.remote.lock().unwrap().clone())
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

        let primary_open = {
            let circuit = self.primary_circuit.lock().expect("circuit lock poisoned");
            circuit.revision == providers.revision
                && circuit
                    .open_until
                    .is_some_and(|until| until > Instant::now())
        };
        let deadline = ale_core::model_api::deadline();
        let mut failure_message = "Primary endpoint circuit is temporarily open".to_string();
        let has_backup = providers.backup_enabled
            && providers.backup_pre_authorized
            && providers.backup.is_some();
        let primary_deadline = if has_backup {
            tokio::time::Instant::now()
                + deadline.saturating_duration_since(tokio::time::Instant::now()) / 2
        } else {
            deadline
        };
        if !primary_open {
            match ale_core::model_api::retry(
                primary_deadline,
                if has_backup { 0 } else { 1 },
                || call_remote(&providers.primary, &planning),
            )
            .await
            {
                Ok(response) => {
                    let mut circuit = self.primary_circuit.lock().expect("circuit lock poisoned");
                    if circuit.revision == providers.revision {
                        circuit.consecutive_failures = 0;
                        circuit.open_until = None;
                    }
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
                    failure_message = primary_error.to_string();
                    if !is_transient_remote_error(&primary_error) {
                        return error_reply(
                            request_id,
                            "PRIMARY_REMOTE_REJECTED",
                            &primary_error.to_string(),
                        );
                    }
                    let mut circuit = self.primary_circuit.lock().expect("circuit lock poisoned");
                    if circuit.revision == providers.revision {
                        circuit.consecutive_failures =
                            circuit.consecutive_failures.saturating_add(1);
                        if circuit.consecutive_failures
                            >= providers.circuit_failure_threshold.max(1)
                        {
                            circuit.open_until = Some(
                                Instant::now()
                                    + Duration::from_secs(
                                        providers.circuit_open_seconds.max(1) as u64
                                    ),
                            );
                        }
                    }
                    tracing::warn!("primary remote model failed with a transient error");
                }
            }
        }

        if providers.backup_enabled && providers.backup_pre_authorized {
            if let Some(backup) = &providers.backup {
                return match ale_core::model_api::retry(deadline, 0, || {
                    call_remote(backup, &planning)
                })
                .await
                {
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
        error_reply(request_id, "PRIMARY_REMOTE_FAILED", &failure_message)
    }
}

async fn call_remote(
    endpoint: &RemoteEndpointConfig,
    job: &RemotePlanningJob,
) -> ale_core::Result<ale_core::cloud::VisionResponse> {
    use ale_core::cloud::{CloudApiFactory, CloudConfig, CloudProvider, VisionResponse};
    let provider = match endpoint.provider.to_ascii_lowercase().as_str() {
        "openai" => CloudProvider::OpenAI,
        "anthropic" => CloudProvider::Anthropic,
        "google" => CloudProvider::Google,
        "azure" => CloudProvider::Azure,
        other => CloudProvider::Custom(other.to_string()),
    };
    let api = CloudApiFactory::create(CloudConfig {
        wire_api: endpoint.wire_api,
        provider,
        api_key: endpoint.api_key.clone(),
        api_url: endpoint.api_url.clone(),
        model: endpoint.model.clone(),
        max_tokens: endpoint.max_tokens,
        timeout: Duration::from_secs(endpoint.timeout_seconds.max(1) as u64),
        retry_count: 0,
    });
    let image = job
        .image_base64
        .as_ref()
        .map(|data| base64::engine::general_purpose::STANDARD.decode(data))
        .transpose()
        .map_err(|_| {
            ale_core::model_api::ModelCallError::new(
                ale_core::model_api::ErrorKind::InvalidRequest,
                "Invalid image payload",
            )
        })?;
    let response = api
        .generate(ale_core::model_api::ModelRequest {
            image,
            tools: job.tools.clone().unwrap_or_default(),
            ..ale_core::model_api::ModelRequest::text(&job.question)
        })
        .await?;
    Ok(VisionResponse {
        content: response.content,
        tool_calls: (!response.tool_calls.is_empty()).then_some(response.tool_calls),
        tokens_used: response.tokens_used,
        model: response.model,
    })
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
        wire_api: endpoint.wire_api,
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
    matches!(error, ale_core::AleError::ModelCall(error) if error.transient())
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

fn decision_required(request_id: String) -> IpcReply {
    let decision = RouteDecision {
        target: RouteTarget::UserDecisionRequired,
        reasons: vec![ale_core::model_scheduler::EscalationReason::LocalModelUnavailable],
        requires_confirmation: true,
    };
    let mut reply = ok_json(request_id, &decision);
    reply.status = IpcReplyStatus::DecisionRequired as i32;
    reply
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
            wire_api: Default::default(),
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
            remote_snapshot: None,
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

    fn providers(primary: RemoteEndpointConfig, revision: u64) -> RemoteProviderSet {
        RemoteProviderSet {
            primary,
            backup: None,
            transcription: None,
            revision,
            backup_enabled: false,
            backup_pre_authorized: false,
            circuit_failure_threshold: 1,
            circuit_open_seconds: 60,
        }
    }

    fn request_with_snapshot(
        providers: RemoteProviderSet,
        tools: Option<Vec<serde_json::Value>>,
    ) -> IpcEnvelope {
        IpcEnvelope {
            protocol_version: MODEL_IPC_VERSION,
            request_id: "pinned".into(),
            kind: IpcRequestKind::Schedule as i32,
            payload: serde_json::to_vec(&ModelJob {
                request_id: "pinned".into(),
                remote_snapshot: Some(providers),
                capability: ModelCapability::RemotePlanning,
                priority: ale_core::model_scheduler::SchedulerPriority::InteractiveRequest,
                deadline_unix_ms: chrono::Utc::now().timestamp_millis() + 65_000,
                risk_ceiling: ale_core::actions::RiskLevel::High,
                snapshot_id: None,
                privacy: ale_core::model_scheduler::JobPrivacy {
                    allow_remote: true,
                    ..Default::default()
                },
                payload: serde_json::to_value(RemotePlanningJob {
                    question: "text-only plan".into(),
                    image_base64: None,
                    tools,
                })
                .unwrap(),
            })
            .unwrap(),
        }
    }

    #[tokio::test]
    async fn text_only_planning_preserves_tools_and_uses_pinned_config() {
        let old=mock_endpoint("200 OK",r#"{"choices":[{"finish_reason":"tool_calls","message":{"tool_calls":[{"id":"test","function":{"name":"probe","arguments":"{\"value\":\"old\"}"}}]}}]}"#).await;
        let scheduler = ModelScheduler::default();
        let mut no_primary = endpoint("http://127.0.0.1:1".into());
        no_primary.api_key.clear();
        let configured = scheduler.configure_remote(IpcEnvelope {
            protocol_version: MODEL_IPC_VERSION,
            request_id: "config".into(),
            kind: IpcRequestKind::ConfigureRemote as i32,
            payload: serde_json::to_vec(&providers(no_primary, 2)).unwrap(),
        });
        assert_eq!(configured.status, IpcReplyStatus::Ok as i32);
        let health = scheduler
            .handle(IpcEnvelope {
                protocol_version: MODEL_IPC_VERSION,
                request_id: "health".into(),
                kind: IpcRequestKind::Health as i32,
                payload: vec![],
            })
            .await;
        let health: SchedulerHealth = serde_json::from_slice(&health.payload).unwrap();
        assert!(!health
            .available_capabilities
            .contains(&ModelCapability::RemotePlanning));
        let tools = vec![
            serde_json::json!({"type":"function","function":{"name":"probe","parameters":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"]}}}),
        ];
        let reply = scheduler
            .handle(request_with_snapshot(
                providers(endpoint(old), 1),
                Some(tools),
            ))
            .await;
        assert_eq!(
            reply.status,
            IpcReplyStatus::Ok as i32,
            "{}",
            reply.error_message
        );
        let result: RemotePlanningResult = serde_json::from_slice(&reply.payload).unwrap();
        assert_eq!(
            result.response.tool_calls.unwrap()[0].function.name,
            "probe"
        );
    }

    #[tokio::test]
    async fn old_request_failure_cannot_trip_new_config_circuit() {
        let old = mock_endpoint("503 Unavailable", "{}").await;
        let scheduler = ModelScheduler::default();
        scheduler.configure_remote(IpcEnvelope {
            protocol_version: MODEL_IPC_VERSION,
            request_id: "config".into(),
            kind: IpcRequestKind::ConfigureRemote as i32,
            payload: serde_json::to_vec(&providers(endpoint("http://127.0.0.1:1".into()), 2))
                .unwrap(),
        });
        let reply = scheduler
            .handle(request_with_snapshot(providers(endpoint(old), 1), None))
            .await;
        assert_eq!(reply.status, IpcReplyStatus::Error as i32);
        let circuit = scheduler.primary_circuit.lock().unwrap();
        assert_eq!(circuit.revision, 2);
        assert_eq!(circuit.consecutive_failures, 0);
        assert!(circuit.open_until.is_none());
    }

    #[tokio::test]
    async fn cloud_stage_accepts_response_after_old_thirty_second_limit() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let responder = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0; 8192];
            assert!(socket.read(&mut buffer).await.unwrap() > 0);
            tokio::time::sleep(Duration::from_secs(31)).await;
            let body =
                r#"{"choices":[{"finish_reason":"stop","message":{"content":"completed"}}]}"#;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let mut primary = endpoint(url);
        primary.timeout_seconds = 60;
        let reply = ModelScheduler::default()
            .handle(request_with_snapshot(providers(primary, 0), None))
            .await;
        assert_eq!(
            reply.status,
            IpcReplyStatus::Ok as i32,
            "{}",
            reply.error_message
        );
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn preauthorized_backup_handles_primary_failure() {
        let primary = mock_endpoint("500 Internal Server Error", "{}").await;
        let backup = mock_endpoint(
            "200 OK",
            r#"{"status":"completed","output":[{"type":"message","content":[{"type":"output_text","text":"backup response"}]}],"usage":{"total_tokens":2}}"#,
        )
        .await;
        let scheduler = ModelScheduler::default();
        let configure = scheduler
            .handle(IpcEnvelope {
                protocol_version: MODEL_IPC_VERSION,
                request_id: "configure".to_string(),
                kind: IpcRequestKind::ConfigureRemote as i32,
                payload: serde_json::to_vec(&RemoteProviderSet {
                    transcription: None,
                    revision: 0,
                    primary: endpoint(primary),
                    backup: Some(RemoteEndpointConfig {
                        wire_api: ale_core::model_api::WireApi::OpenaiResponses,
                        ..endpoint(backup)
                    }),
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
                    remote_snapshot: None,
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
                    transcription: None,
                    revision: 0,
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
                    remote_snapshot: None,
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
                    transcription: None,
                    revision: 0,
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
                    remote_snapshot: None,
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
                    remote_snapshot: None,
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
