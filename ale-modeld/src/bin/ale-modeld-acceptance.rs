use ale_core::actions::RiskLevel;
use ale_core::model_ipc::{
    read_message, write_message, IpcEnvelope, IpcReply, IpcReplyStatus, IpcRequestKind,
    MODEL_IPC_VERSION,
};
use ale_core::model_scheduler::{
    BoundingBox, CancelModelJob, GroundingJob, GroundingResult, JobPrivacy, LocalPlanningJob,
    LocalPlanningResult, ModelCapability, ModelJob, ModelRuntimeConfig, SchedulerHealth,
    SchedulerPriority,
};
use base64::Engine;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;

#[cfg(unix)]
type LocalStream = tokio::net::UnixStream;
#[cfg(windows)]
type LocalStream = tokio::net::windows::named_pipe::NamedPipeClient;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().any(|arg| arg == "--asr-only") {
        return asr_acceptance().await;
    }
    let args = Arguments::parse()?;
    std::fs::create_dir_all(&args.report_dir)?;
    let endpoint = endpoint();
    let token = uuid::Uuid::new_v4().as_bytes().repeat(2);
    let mut child = tokio::process::Command::new(&args.modeld)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let bootstrap = json!({
        "endpoint": endpoint,
        "token_base64": base64::engine::general_purpose::STANDARD.encode(&token),
    });
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("modeld stdin is unavailable"))?;
    stdin.write_all(format!("{bootstrap}\n").as_bytes()).await?;
    stdin.shutdown().await?;
    let mut stream = connect(&endpoint).await?;

    let mut results = Vec::<Value>::new();
    let auth = call_raw(&mut stream, "auth", IpcRequestKind::Authenticate, token).await?;
    record_reply(&mut results, "IPC-AUTH", Instant::now(), &auth, true);

    let runtime = runtime_config(&args.models_dir);
    let configured = call_json(
        &mut stream,
        "configure-models",
        IpcRequestKind::ConfigureModels,
        &runtime,
    )
    .await?;
    record_reply(
        &mut results,
        "IPC-CONFIGURE",
        Instant::now(),
        &configured,
        true,
    );

    let health_started = Instant::now();
    let health_reply =
        call_json(&mut stream, "health", IpcRequestKind::Health, &Value::Null).await?;
    let health: Option<SchedulerHealth> = decode_ok(&health_reply).ok();
    let health_ok = health.as_ref().is_some_and(|health| {
        health.gpus.iter().any(|gpu| {
            gpu.backend == ale_core::model_scheduler::GpuBackend::Amd && gpu.supports_large_models()
        }) && health
            .available_capabilities
            .contains(&ModelCapability::LocalPlanning)
            && health
                .available_capabilities
                .contains(&ModelCapability::ElementGrounding)
            && health.hot_worker.is_none()
    });
    record_reply(
        &mut results,
        "IPC-HEALTH",
        health_started,
        &health_reply,
        health_ok,
    );

    let image_path = args.fixtures_dir.join("unique-target-100.png");
    let image = std::fs::read(&image_path)?;
    let image_base64 = base64::engine::general_purpose::STANDARD.encode(&image);
    let fixtures: Value =
        serde_json::from_slice(&std::fs::read(args.fixtures_dir.join("expected.json"))?)?;
    let bbox = &fixtures["fixtures"]["unique_100"]["bbox_normalized"];
    let bounds = BoundingBox {
        x: number(bbox, 0)?,
        y: number(bbox, 1)?,
        width: number(bbox, 2)? - number(bbox, 0)?,
        height: number(bbox, 3)? - number(bbox, 1)?,
    };

    let mismatch_job = model_job(
        "different-id",
        ModelCapability::LocalPlanning,
        "snapshot-mismatch",
        RiskLevel::Medium,
        json!({}),
        unix_millis() + 30_000,
    );
    let mismatch_started = Instant::now();
    let mismatch = call_json_with_envelope_id(
        &mut stream,
        "mismatch-envelope",
        IpcRequestKind::Schedule,
        &mismatch_job,
    )
    .await?;
    record_error(
        &mut results,
        "IPC-REQUEST-ID",
        mismatch_started,
        &mismatch,
        "REQUEST_ID_MISMATCH",
    );

    let expired_job = model_job(
        "expired",
        ModelCapability::LocalPlanning,
        "snapshot-expired",
        RiskLevel::Medium,
        json!({}),
        unix_millis() - 1,
    );
    let expired_started = Instant::now();
    let expired = call_json_with_envelope_id(
        &mut stream,
        "expired",
        IpcRequestKind::Schedule,
        &expired_job,
    )
    .await?;
    record_error(
        &mut results,
        "IPC-DEADLINE",
        expired_started,
        &expired,
        "DEADLINE_EXCEEDED",
    );

    let plan_payload = LocalPlanningJob {
        question: "Activate the DOWNLOAD MODELS button using one semantic step.".to_string(),
        image_base64: image_base64.clone(),
        application_id: Some("ALE MODEL RUNTIME TEST".to_string()),
    };
    let plan_job = model_job(
        "local-plan",
        ModelCapability::LocalPlanning,
        "snapshot-acceptance",
        RiskLevel::Medium,
        serde_json::to_value(plan_payload)?,
        unix_millis() + 90_000,
    );
    let plan_started = Instant::now();
    let plan_reply = call_json_with_envelope_id(
        &mut stream,
        "local-plan",
        IpcRequestKind::Schedule,
        &plan_job,
    )
    .await?;
    let plan_result = decode_ok::<LocalPlanningResult>(&plan_reply).ok();
    let plan_ok = plan_result.as_ref().is_some_and(|result| {
        result.snapshot_id == "snapshot-acceptance"
            && !result.plan.steps.is_empty()
            && result.plan.steps.len() <= 5
            && result.plan.has_observable_postconditions()
    });
    record_reply(
        &mut results,
        "MODELD-QWEN",
        plan_started,
        &plan_reply,
        plan_ok,
    );
    if let Some(result) = plan_result {
        results.last_mut().expect("plan result was just recorded")["evidence"] =
            serde_json::to_value(result)?;
    }

    let cold_health_reply = call_json(
        &mut stream,
        "health-qwen-cold",
        IpcRequestKind::Health,
        &Value::Null,
    )
    .await?;
    let cold_health: SchedulerHealth = decode_ok(&cold_health_reply)?;
    let cold_worker = cold_health.hot_worker.clone();
    let cold_pid = cold_worker.as_ref().map(|worker| worker.process_id);
    results.last_mut().expect("Qwen result was just recorded")["worker_after"] =
        serde_json::to_value(&cold_worker)?;

    let warm_job = model_job(
        "local-plan-warm",
        ModelCapability::LocalPlanning,
        "snapshot-acceptance",
        RiskLevel::Medium,
        serde_json::to_value(LocalPlanningJob {
            question: "Activate the DOWNLOAD MODELS button using one semantic step.".to_string(),
            image_base64: image_base64.clone(),
            application_id: Some("ALE MODEL RUNTIME TEST".to_string()),
        })?,
        unix_millis() + 90_000,
    );
    let warm_started = Instant::now();
    let warm_reply = call_json_with_envelope_id(
        &mut stream,
        "local-plan-warm",
        IpcRequestKind::Schedule,
        &warm_job,
    )
    .await?;
    let warm_result = decode_ok::<LocalPlanningResult>(&warm_reply).ok();
    let warm_health_reply = call_json(
        &mut stream,
        "health-qwen-warm",
        IpcRequestKind::Health,
        &Value::Null,
    )
    .await?;
    let warm_health: SchedulerHealth = decode_ok(&warm_health_reply)?;
    let warm_worker = warm_health.hot_worker.clone();
    let warm_ok = warm_result.as_ref().is_some_and(|result| {
        result.snapshot_id == "snapshot-acceptance"
            && !result.plan.steps.is_empty()
            && result.plan.has_observable_postconditions()
    }) && cold_pid.is_some()
        && warm_worker
            .as_ref()
            .is_some_and(|worker| Some(worker.process_id) == cold_pid && !worker.active);
    record_reply(
        &mut results,
        "MODELD-QWEN-WARM-REUSE",
        warm_started,
        &warm_reply,
        warm_ok,
    );
    results
        .last_mut()
        .expect("warm Qwen result was just recorded")["worker_after"] =
        serde_json::to_value(&warm_worker)?;

    for id in ["MODELD-SHOWUI"] {
        let request_id = id.to_ascii_lowercase();
        let grounding = GroundingJob {
            image_base64: image_base64.clone(),
            target: ale_core::model_scheduler::TargetRef {
                node_id: Some("DownloadModelsButton".to_string()),
                role: Some("button".to_string()),
                label: Some("DOWNLOAD MODELS".to_string()),
                visual_description: None,
            },
            image_width: 1280,
            image_height: 720,
            candidate_bounds: vec![bounds.clone()],
        };
        let job = model_job(
            &request_id,
            ModelCapability::ElementGrounding,
            "snapshot-acceptance",
            RiskLevel::Medium,
            serde_json::to_value(grounding)?,
            unix_millis() + 30_000,
        );
        let started = Instant::now();
        let reply =
            call_json_with_envelope_id(&mut stream, &request_id, IpcRequestKind::Schedule, &job)
                .await?;
        let grounding_result = decode_ok::<GroundingResult>(&reply).ok();
        let grounding_valid = grounding_result.as_ref().is_some_and(|result| {
            result.snapshot_id == "snapshot-acceptance" && result.selected.is_some()
        });
        let grounding_health_reply = call_json(
            &mut stream,
            "health-showui",
            IpcRequestKind::Health,
            &Value::Null,
        )
        .await?;
        let grounding_health: SchedulerHealth = decode_ok(&grounding_health_reply)?;
        let grounding_worker = grounding_health.hot_worker.clone();
        let passed = grounding_valid
            && cold_pid.is_some()
            && grounding_worker.as_ref().is_some_and(|worker| {
                worker.model_id == "ShowUI-2B-Q4_K_M"
                    && Some(worker.process_id) != cold_pid
                    && !worker.active
            });
        record_reply(&mut results, id, started, &reply, passed);
        if let Some(result) = grounding_result {
            results
                .last_mut()
                .expect("grounding result was just recorded")["evidence"] =
                serde_json::to_value(result)?;
        }
        results
            .last_mut()
            .expect("grounding result was just recorded")["worker_after"] =
            serde_json::to_value(&grounding_worker)?;
    }

    println!("waiting for the 120-second hot-worker idle eviction",);
    let idle_started = Instant::now();
    tokio::time::sleep(ale_core::model_scheduler::MODEL_IDLE_TTL + Duration::from_secs(6)).await;
    let idle_health_reply = call_json(
        &mut stream,
        "health-idle",
        IpcRequestKind::Health,
        &Value::Null,
    )
    .await?;
    let idle_health: SchedulerHealth = decode_ok(&idle_health_reply)?;
    results.push(json!({
        "id": "MODELD-IDLE-EVICTION",
        "passed": idle_health.hot_worker.is_none(),
        "duration_seconds": elapsed(idle_started),
        "idle_ttl_seconds": ale_core::model_scheduler::MODEL_IDLE_TTL.as_secs(),
        "worker_after": idle_health.hot_worker,
    }));

    let cancel_id = "cancelled-model-job";
    let cancel_job = model_job(
        cancel_id,
        ModelCapability::LocalPlanning,
        "snapshot-cancel",
        RiskLevel::Medium,
        serde_json::to_value(LocalPlanningJob {
            question: "Describe every visible element in detail before planning.".to_string(),
            image_base64,
            application_id: None,
        })?,
        unix_millis() + 90_000,
    );
    write_envelope(
        &mut stream,
        cancel_id,
        IpcRequestKind::Schedule,
        serde_json::to_vec(&cancel_job)?,
    )
    .await?;
    // Give the cold worker time to spawn so cancellation exercises process teardown,
    // rather than only removing a job that has not yet been polled.
    tokio::time::sleep(Duration::from_secs(2)).await;
    write_envelope(
        &mut stream,
        "cancel-command",
        IpcRequestKind::Cancel,
        serde_json::to_vec(&CancelModelJob {
            target_request_id: cancel_id.to_string(),
        })?,
    )
    .await?;
    let cancel_started = Instant::now();
    let first: IpcReply = read_message(&mut stream).await?;
    let second: IpcReply = read_message(&mut stream).await?;
    let cancellation_ok = [&first, &second]
        .iter()
        .any(|reply| reply.request_id == cancel_id && reply.error_code == "CANCELLED")
        && [&first, &second].iter().any(|reply| {
            reply.request_id == "cancel-command" && reply.status == IpcReplyStatus::Ok as i32
        });
    let cancelled_health_reply = call_json(
        &mut stream,
        "health-after-cancel",
        IpcRequestKind::Health,
        &Value::Null,
    )
    .await?;
    let cancelled_health: SchedulerHealth = decode_ok(&cancelled_health_reply)?;
    let cancellation_ok = cancellation_ok && cancelled_health.hot_worker.is_none();
    results.push(json!({
        "id": "IPC-CANCEL",
        "passed": cancellation_ok,
        "duration_seconds": elapsed(cancel_started),
        "error_code": if cancellation_ok { Value::Null } else { json!("CANCEL_NOT_ACKNOWLEDGED") },
        "worker_after": cancelled_health.hot_worker,
    }));

    let shutdown = call_json(
        &mut stream,
        "shutdown",
        IpcRequestKind::Shutdown,
        &Value::Null,
    )
    .await?;
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
    let passed = results.iter().all(|result| result["passed"] == true);
    let report = json!({
        "schema_version": 1,
        "passed": passed,
        "no_input_executed": true,
        "health": health,
        "tests": results,
        "shutdown_acknowledged": shutdown.status == IpcReplyStatus::Ok as i32,
    });
    std::fs::write(
        args.report_dir.join("modeld-acceptance.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!(
        "ale-modeld zero-input acceptance: {}",
        if passed { "PASS" } else { "FAIL" }
    );
    if !passed {
        std::process::exit(2);
    }
    Ok(())
}

struct Arguments {
    modeld: PathBuf,
    models_dir: PathBuf,
    fixtures_dir: PathBuf,
    report_dir: PathBuf,
}

impl Arguments {
    fn parse() -> anyhow::Result<Self> {
        let mut values = std::env::args().skip(1);
        let mut get = |expected: &str| -> anyhow::Result<PathBuf> {
            let flag = values
                .next()
                .ok_or_else(|| anyhow::anyhow!("missing {expected}"))?;
            if flag != expected {
                anyhow::bail!("expected {expected}, got {flag}");
            }
            values
                .next()
                .map(PathBuf::from)
                .ok_or_else(|| anyhow::anyhow!("missing value for {expected}"))
        };
        let arguments = Self {
            modeld: get("--modeld")?,
            models_dir: get("--models-dir")?,
            fixtures_dir: get("--fixtures-dir")?,
            report_dir: get("--report-dir")?,
        };
        if values.next().is_some() {
            anyhow::bail!("unexpected extra arguments");
        }
        Ok(arguments)
    }
}

fn runtime_config(models_dir: &Path) -> ModelRuntimeConfig {
    let runtime = models_dir.join(".runtime");
    let gguf = runtime.join("gguf");
    let text = |path: PathBuf| Some(path.to_string_lossy().into_owned());
    ModelRuntimeConfig {
        models_dir: models_dir.to_string_lossy().into_owned(),
        sensevoice_model: models_dir
            .join("SenseVoiceSmall/model.int8.onnx")
            .to_string_lossy()
            .into_owned(),
        sensevoice_tokens: models_dir
            .join("SenseVoiceSmall/tokens.txt")
            .to_string_lossy()
            .into_owned(),
        llama_server: text(runtime.join("tools/llama-b10472-vulkan/llama-server.exe")),
        qwen_model: text(gguf.join("Qwen2.5-VL-7B-Instruct/model-q4_k_m.gguf")),
        qwen_mmproj: text(gguf.join("Qwen2.5-VL-7B-Instruct/mmproj-model-f16.gguf")),
        showui_model: text(gguf.join("ShowUI-2B/model-q4_k_m.gguf")),
        showui_mmproj: text(gguf.join("ShowUI-2B/mmproj-model-f16.gguf")),
        capability_manifest: text(runtime.join("runtime-capabilities.json")),
    }
}

fn model_job(
    request_id: &str,
    capability: ModelCapability,
    snapshot_id: &str,
    risk_ceiling: RiskLevel,
    payload: Value,
    deadline_unix_ms: i64,
) -> ModelJob {
    ModelJob {
        runtime_snapshot: None,
        remote_snapshot: None,
        request_id: request_id.to_string(),
        capability,
        priority: SchedulerPriority::InteractiveRequest,
        deadline_unix_ms,
        risk_ceiling,
        snapshot_id: Some(snapshot_id.to_string()),
        privacy: JobPrivacy::default(),
        payload,
    }
}

async fn call_json<T: serde::Serialize>(
    stream: &mut LocalStream,
    request_id: &str,
    kind: IpcRequestKind,
    payload: &T,
) -> anyhow::Result<IpcReply> {
    call_json_with_envelope_id(stream, request_id, kind, payload).await
}

async fn call_json_with_envelope_id<T: serde::Serialize>(
    stream: &mut LocalStream,
    request_id: &str,
    kind: IpcRequestKind,
    payload: &T,
) -> anyhow::Result<IpcReply> {
    call_raw(stream, request_id, kind, serde_json::to_vec(payload)?).await
}

async fn call_raw(
    stream: &mut LocalStream,
    request_id: &str,
    kind: IpcRequestKind,
    payload: Vec<u8>,
) -> anyhow::Result<IpcReply> {
    write_envelope(stream, request_id, kind, payload).await?;
    Ok(tokio::time::timeout(Duration::from_secs(90), read_message(stream)).await??)
}

async fn write_envelope(
    stream: &mut LocalStream,
    request_id: &str,
    kind: IpcRequestKind,
    payload: Vec<u8>,
) -> anyhow::Result<()> {
    write_message(
        stream,
        &IpcEnvelope {
            protocol_version: MODEL_IPC_VERSION,
            request_id: request_id.to_string(),
            kind: kind as i32,
            payload,
        },
    )
    .await?;
    Ok(())
}

fn decode_ok<T: serde::de::DeserializeOwned>(reply: &IpcReply) -> anyhow::Result<T> {
    if reply.status != IpcReplyStatus::Ok as i32 {
        anyhow::bail!("{}: {}", reply.error_code, reply.error_message);
    }
    Ok(serde_json::from_slice(&reply.payload)?)
}

fn record_reply(
    results: &mut Vec<Value>,
    id: &str,
    started: Instant,
    reply: &IpcReply,
    passed: bool,
) {
    results.push(json!({
        "id": id,
        "passed": passed,
        "duration_seconds": elapsed(started),
        "status": reply.status,
        "error_code": reply.error_code,
        "error_message": reply.error_message,
    }));
}

fn record_error(
    results: &mut Vec<Value>,
    id: &str,
    started: Instant,
    reply: &IpcReply,
    expected: &str,
) {
    record_reply(results, id, started, reply, reply.error_code == expected);
}

fn number(value: &Value, index: usize) -> anyhow::Result<f32> {
    value[index]
        .as_f64()
        .map(|number| number as f32)
        .ok_or_else(|| anyhow::anyhow!("fixture bbox field {index} is invalid"))
}

fn elapsed(started: Instant) -> f64 {
    (started.elapsed().as_secs_f64() * 1000.0).round() / 1000.0
}

fn unix_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn endpoint() -> String {
    #[cfg(windows)]
    {
        format!(r"\\.\pipe\ale-modeld-acceptance-{}", uuid::Uuid::new_v4())
    }
    #[cfg(unix)]
    {
        std::env::temp_dir()
            .join(format!(
                "ale-modeld-acceptance-{}.sock",
                uuid::Uuid::new_v4()
            ))
            .to_string_lossy()
            .into_owned()
    }
}

#[cfg(unix)]
async fn connect(endpoint: &str) -> anyhow::Result<LocalStream> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        match tokio::net::UnixStream::connect(endpoint).await {
            Ok(stream) => return Ok(stream),
            Err(error) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if !matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) {
                    return Err(error.into());
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

#[cfg(windows)]
async fn connect(endpoint: &str) -> anyhow::Result<LocalStream> {
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
                    return Err(error.into());
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
}

async fn asr_acceptance() -> anyhow::Result<()> {
    use ale_core::child_process::ManagedAsyncChild;
    use ale_core::model_scheduler::{SpeechRecognitionJob, SpeechRecognitionResult};
    let mut args = std::collections::HashMap::new();
    let mut values = std::env::args().skip(1);
    while let Some(flag) = values.next() {
        if flag == "--asr-only" {
            continue;
        }
        args.insert(
            flag,
            values
                .next()
                .ok_or_else(|| anyhow::anyhow!("missing argument value"))?,
        );
    }
    let required = |key: &str| {
        args.get(key)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("missing {key}"))
    };
    let report_dir = PathBuf::from(required("--report-dir")?);
    std::fs::create_dir_all(&report_dir)?;
    let expected = required("--expected")?;
    if expected.trim().is_empty() {
        anyhow::bail!("--expected must contain a known transcript word");
    }
    let audio = PathBuf::from(required("--audio")?);
    let modeld = required("--modeld")?;
    let models_dir = PathBuf::from(required("--models-dir")?);
    let mut results = Vec::new();
    let result: anyhow::Result<()> = async {
        let wav=std::fs::read(audio)?;
        let runtime=runtime_config(&models_dir);
        if !Path::new(&runtime.sensevoice_model).is_file() || !Path::new(&runtime.sensevoice_tokens).is_file() {anyhow::bail!("ASR_MODEL_FILES_MISSING");}
        let mut command=tokio::process::Command::new(modeld);
        command.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        let mut child=ManagedAsyncChild::spawn(&mut command).map_err(|_|anyhow::anyhow!("MODELD_START_FAILED_OR_DEPENDENCY_MISSING"))?;
        let endpoint=endpoint();let token=uuid::Uuid::new_v4().as_bytes().repeat(2);
        let bootstrap=json!({"endpoint":endpoint,"token_base64":base64::engine::general_purpose::STANDARD.encode(&token)});
        let mut input=child.child.stdin.take().ok_or_else(||anyhow::anyhow!("MODELD_BOOTSTRAP_PIPE"))?;
        input.write_all(format!("{bootstrap}\n").as_bytes()).await?;input.shutdown().await?;
        let mut stream=tokio::time::timeout(Duration::from_secs(10),connect(&endpoint)).await.map_err(|_|anyhow::anyhow!("MODELD_START_TIMEOUT_OR_DEPENDENCY_MISSING"))??;
        call_raw(&mut stream,"auth",IpcRequestKind::Authenticate,token).await?;
        let configured=call_json(&mut stream,"configure",IpcRequestKind::ConfigureModels,&runtime).await?;
        if configured.status!=0 {anyhow::bail!("ASR_CONFIGURATION_FAILED");}
        let health:SchedulerHealth=decode_ok(&call_json(&mut stream,"health",IpcRequestKind::Health,&Value::Null).await?)?;
        if !health.diagnostics.as_ref().is_some_and(|d|d.local_asr_supported) {anyhow::bail!("ASR_UNSUPPORTED_BUILD_TARGET");}
        let job=|id:&str,wav:&[u8]|ModelJob {runtime_snapshot:Some(runtime.clone()),remote_snapshot:None,request_id:id.into(),capability:ModelCapability::SpeechRecognition,priority:SchedulerPriority::InteractiveRequest,deadline_unix_ms:unix_millis()+85_000,risk_ceiling:RiskLevel::Low,snapshot_id:None,privacy:JobPrivacy::default(),payload:serde_json::to_value(SpeechRecognitionJob {wav_base64:base64::engine::general_purpose::STANDARD.encode(wav),allow_remote:false}).unwrap()};
        let recognized:SpeechRecognitionResult=decode_ok(&call_json_with_envelope_id(&mut stream,"asr-first",IpcRequestKind::Schedule,&job("asr-first",&wav)).await?)?;
        let text_ok=!recognized.text.trim().is_empty() && recognized.text.to_lowercase().contains(&expected.to_lowercase()) && !recognized.used_remote;
        results.push(json!({"case":"local_transcription","passed":text_ok}));
        if !text_ok {anyhow::bail!("ASR_TRANSCRIPT_EXPECTATION_FAILED");}
        let long_wav=repeat_audio_for_cancel(&wav)?;
        write_envelope(&mut stream,"asr-cancel-target",IpcRequestKind::Schedule,serde_json::to_vec(&job("asr-cancel-target",&long_wav))?).await?;
        tokio::time::sleep(Duration::from_millis(50)).await;
        write_envelope(&mut stream,"cancel",IpcRequestKind::Cancel,serde_json::to_vec(&CancelModelJob {target_request_id:"asr-cancel-target".into()})?).await?;
        let mut accepted=false;let mut cancelled=false;
        for _ in 0..2 {
            let reply:IpcReply=tokio::time::timeout(Duration::from_secs(5),read_message(&mut stream)).await??;
            if reply.request_id=="cancel" {accepted=decode_ok::<Value>(&reply)?.get("accepted").and_then(Value::as_bool)==Some(true);}
            if reply.request_id=="asr-cancel-target" {cancelled=reply.error_code=="CANCELLED";}
        }
        results.push(json!({"case":"cancel_active_asr","passed":accepted && cancelled}));
        if !accepted || !cancelled {anyhow::bail!("ASR_CANCELLATION_NOT_OBSERVED");}
        let retry:SpeechRecognitionResult=decode_ok(&call_json_with_envelope_id(&mut stream,"asr-retry",IpcRequestKind::Schedule,&job("asr-retry",&wav)).await?)?;
        let retry_ok=retry.text.to_lowercase().contains(&expected.to_lowercase()) && !retry.used_remote;
        results.push(json!({"case":"retry_after_cancel","passed":retry_ok}));
        child.kill_tree_and_wait(Duration::from_secs(2)).await?;
        if !retry_ok {anyhow::bail!("ASR_RETRY_FAILED");}Ok(())
    }.await;
    let report = json!({"mode":"asr_only","passed":result.is_ok(),"results":results,"failure":result.as_ref().err().map(|error|error.to_string())});
    std::fs::write(
        report_dir.join("asr-acceptance.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    result
}

fn repeat_audio_for_cancel(wav: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mut reader = hound::WavReader::new(std::io::Cursor::new(wav))?;
    let spec = reader.spec();
    if spec.channels != 1
        || spec.bits_per_sample != 16
        || spec.sample_rate == 0
        || spec.sample_format != hound::SampleFormat::Int
    {
        anyhow::bail!("ASR_FIXTURE_REQUIRES_MONO_PCM16");
    }
    let samples = reader.samples::<i16>().collect::<Result<Vec<_>, _>>()?;
    if samples.is_empty() {
        anyhow::bail!("ASR_FIXTURE_EMPTY");
    }
    let mut cursor = std::io::Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec)?;
        for index in 0..u64::from(spec.sample_rate) * 60 {
            writer.write_sample(samples[index as usize % samples.len()])?;
        }
        writer.finalize()?;
    }
    Ok(cursor.into_inner())
}
