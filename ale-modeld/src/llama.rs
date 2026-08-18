use ale_core::model_scheduler::{
    BoundingBox, GroundingCandidate, GroundingJob, GroundingResult, LocalPlanningJob,
    LocalPlanningResult, ModelRuntimeConfig, SemanticPlan, StateVerificationJob,
    StateVerificationResult, MODEL_IDLE_TTL, MODEL_START_TIMEOUT,
};
use base64::Engine;
use serde::Deserialize;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex as AsyncMutex, Semaphore};

const MAX_STARTUP_LOG_BYTES: usize = 256 * 1024;
const WORKER_STOP_TIMEOUT: Duration = Duration::from_secs(5);

pub struct LlamaAdapter {
    gpu_gate: Arc<Semaphore>,
    hot_worker: AsyncMutex<Option<Arc<HotWorker>>>,
    failed_models: Mutex<HashSet<RuntimeModel>>,
    client: reqwest::Client,
}

impl Default for LlamaAdapter {
    fn default() -> Self {
        Self {
            gpu_gate: Arc::new(Semaphore::new(1)),
            hot_worker: AsyncMutex::new(None),
            failed_models: Mutex::new(HashSet::new()),
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .build()
                .expect("build loopback llama.cpp client"),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct CapabilityManifest {
    amd_vulkan_device_detected: bool,
    models: CapabilityModels,
}

#[derive(Debug, Default, Deserialize)]
struct CapabilityModels {
    qwen: ModelAcceptance,
    showui: ModelAcceptance,
}

#[derive(Debug, Default, Deserialize)]
struct ModelAcceptance {
    ready: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RuntimeModel {
    Qwen,
    ShowUi,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct WorkerKey {
    model: RuntimeModel,
    server: PathBuf,
    model_path: PathBuf,
    mmproj_path: PathBuf,
}

struct WorkerProcess {
    child: Mutex<Child>,
    terminated: AtomicBool,
}

impl WorkerProcess {
    fn is_running(&self) -> bool {
        !self.terminated.load(Ordering::Acquire) && !self.has_exited()
    }

    fn has_exited(&self) -> bool {
        self.child
            .lock()
            .expect("llama.cpp child lock poisoned")
            .try_wait()
            .is_ok_and(|status| status.is_some())
    }

    fn terminate(&self) {
        if !self.terminated.swap(true, Ordering::AcqRel) {
            let _ = self
                .child
                .lock()
                .expect("llama.cpp child lock poisoned")
                .start_kill();
        }
    }

    fn pid(&self) -> Option<u32> {
        self.child
            .lock()
            .expect("llama.cpp child lock poisoned")
            .id()
    }
}

struct HotWorker {
    key: WorkerKey,
    endpoint: String,
    process: Arc<WorkerProcess>,
    logs: Arc<Mutex<String>>,
    last_used: Mutex<Instant>,
    active: AtomicBool,
}

impl HotWorker {
    fn can_reuse(&self, requested: &WorkerKey) -> bool {
        worker_can_reuse(&self.key, requested, self.process.is_running())
    }

    fn mark_active(&self) {
        self.active.store(true, Ordering::Release);
    }

    fn finish_request(&self) {
        *self.last_used.lock().expect("last-used lock poisoned") = Instant::now();
        self.active.store(false, Ordering::Release);
    }

    fn is_idle_expired(&self) -> bool {
        !self.active.load(Ordering::Acquire)
            && idle_worker_expired(
                self.last_used
                    .lock()
                    .expect("last-used lock poisoned")
                    .elapsed(),
            )
    }

    fn terminate(&self) {
        self.process.terminate();
    }

    fn logs(&self) -> String {
        self.logs
            .lock()
            .expect("llama.cpp log lock poisoned")
            .clone()
    }
}

impl Drop for HotWorker {
    fn drop(&mut self) {
        self.process.terminate();
    }
}

struct ActiveRequest {
    worker: Arc<HotWorker>,
    armed: bool,
}

impl ActiveRequest {
    fn new(worker: Arc<HotWorker>) -> Self {
        worker.mark_active();
        Self {
            worker,
            armed: true,
        }
    }

    fn finish(mut self) {
        self.worker.finish_request();
        self.armed = false;
    }
}

impl Drop for ActiveRequest {
    fn drop(&mut self) {
        if self.armed {
            self.worker.active.store(false, Ordering::Release);
            self.worker.terminate();
        }
    }
}

impl LlamaAdapter {
    pub fn capabilities(
        &self,
        config: &ModelRuntimeConfig,
    ) -> Vec<ale_core::model_scheduler::ModelCapability> {
        use ale_core::model_scheduler::ModelCapability;
        let accepted = load_capabilities(config);
        if !accepted.amd_vulkan_device_detected || !path_is_file(config.llama_server.as_deref()) {
            return Vec::new();
        }
        let mut capabilities = Vec::new();
        let failed = self
            .failed_models
            .lock()
            .expect("failed model lock poisoned");
        if accepted.models.qwen.ready
            && model_files(config, RuntimeModel::Qwen).is_some()
            && !failed.contains(&RuntimeModel::Qwen)
        {
            capabilities.extend([
                ModelCapability::StateSummary,
                ModelCapability::LocalPlanning,
                ModelCapability::StateVerification,
            ]);
        }
        if accepted.models.showui.ready
            && model_files(config, RuntimeModel::ShowUi).is_some()
            && !failed.contains(&RuntimeModel::ShowUi)
        {
            capabilities.push(ModelCapability::ElementGrounding);
        }
        capabilities
    }

    pub fn maintenance(&self) {
        let Ok(mut slot) = self.hot_worker.try_lock() else {
            return;
        };
        if slot.as_ref().is_some_and(|worker| worker.is_idle_expired()) {
            if let Some(worker) = slot.take() {
                tracing::info!(
                    model = ?worker.key.model,
                    pid = ?worker.process.pid(),
                    "unloading idle llama.cpp worker"
                );
                worker.terminate();
            }
        }
    }

    pub fn reconfigure(&self) {
        self.failed_models
            .lock()
            .expect("failed model lock poisoned")
            .clear();
        if let Ok(slot) = self.hot_worker.try_lock() {
            if let Some(worker) = slot.as_ref() {
                worker.terminate();
            }
        }
    }

    pub fn worker_health(&self) -> Option<ale_core::model_scheduler::ModelWorkerHealth> {
        let slot = self.hot_worker.try_lock().ok()?;
        let worker = slot.as_ref()?;
        if !worker.process.is_running() {
            return None;
        }
        let idle_millis = worker
            .last_used
            .lock()
            .expect("last-used lock poisoned")
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        Some(ale_core::model_scheduler::ModelWorkerHealth {
            model_id: match worker.key.model {
                RuntimeModel::Qwen => "Qwen2.5-VL-7B-Instruct-Q4_K_M",
                RuntimeModel::ShowUi => "ShowUI-2B-Q4_K_M",
            }
            .to_string(),
            process_id: worker.process.pid()?,
            active: worker.active.load(Ordering::Acquire),
            idle_millis,
        })
    }

    pub async fn local_plan(
        &self,
        config: &ModelRuntimeConfig,
        snapshot_id: &str,
        job: LocalPlanningJob,
    ) -> Result<LocalPlanningResult, String> {
        ensure_capability(config, RuntimeModel::Qwen)?;
        let image = decode_image(&job.image_base64)?;
        let prompt = format!(
            "Analyze this desktop screenshot and the request. Return only JSON matching \
             {{\"goal\":string,\"application_id\":string|null,\"steps\":[{{\"operation\":string,\
             \"target\":{{\"node_id\":null,\"role\":string|null,\"label\":string|null,\
             \"visual_description\":string|null}},\"input_summary\":string|null,\
             \"expected_state\":string,\"risk\":\"low\"}}]}}. Do not include coordinates or bounding boxes. \
             Use no more than five steps and give every step an observable expected_state. Request: {}",
            job.question
        );
        let output = self
            .run_model(config, RuntimeModel::Qwen, &image, &prompt, 384)
            .await?;
        let value =
            extract_json(&output).ok_or_else(|| "Qwen did not return valid JSON".to_string())?;
        let plan: SemanticPlan =
            serde_json::from_value(value).map_err(|error| error.to_string())?;
        let plan = ale_core::model_scheduler::validate_semantic_plan(plan)
            .map_err(|error| error.to_string())?;
        if plan.steps.len() > ale_core::model_scheduler::MAX_LOCAL_PLAN_STEPS {
            return Err("Qwen local plan exceeds five steps".to_string());
        }
        Ok(LocalPlanningResult {
            plan,
            model_id: "Qwen2.5-VL-7B-Instruct-Q4_K_M".to_string(),
            snapshot_id: snapshot_id.to_string(),
        })
    }

    pub async fn ground(
        &self,
        config: &ModelRuntimeConfig,
        snapshot_id: &str,
        job: GroundingJob,
    ) -> Result<GroundingResult, String> {
        ensure_capability(config, RuntimeModel::ShowUi)?;
        if job.image_width == 0 || job.image_height == 0 {
            return Err("grounding image dimensions are invalid".to_string());
        }
        let image = decode_image(&job.image_base64)?;
        let target = job
            .target
            .label
            .as_deref()
            .or(job.target.visual_description.as_deref())
            .ok_or_else(|| "grounding target has no label or visual description".to_string())?;
        let output = self
            .run_model(
                config,
                RuntimeModel::ShowUi,
                &image,
                &grounding_prompt(target),
                128,
            )
            .await?;
        let raw = extract_coordinate(&output)
            .ok_or_else(|| "ShowUI did not return a coordinate".to_string())?;
        let point = normalize_coordinate(raw, job.image_width, job.image_height)
            .ok_or_else(|| "ShowUI coordinate is outside the screenshot".to_string())?;
        let candidates = job
            .candidate_bounds
            .into_iter()
            .filter(|bounds| bounds.is_normalized() && point_in_bounds(point, bounds))
            .map(|bounds| {
                let confidence = geometric_grounding_confidence(point, &bounds);
                GroundingCandidate {
                    bounds,
                    click_x: point.0,
                    click_y: point.1,
                    confidence,
                    evidence: format!(
                        "ShowUI point [{:.6},{:.6}] is inside a desktop candidate; geometric center confidence {:.6}",
                        point.0, point.1, confidence
                    ),
                }
            })
            .collect::<Vec<_>>();
        let selected = (candidates.len() == 1).then(|| candidates[0].clone());
        Ok(GroundingResult {
            snapshot_id: snapshot_id.to_string(),
            model_id: "ShowUI-2B-Q4_K_M".to_string(),
            selected,
            candidates,
        })
    }

    pub async fn verify(
        &self,
        config: &ModelRuntimeConfig,
        snapshot_id: &str,
        job: StateVerificationJob,
    ) -> Result<StateVerificationResult, String> {
        ensure_capability(config, RuntimeModel::Qwen)?;
        let image = decode_image(&job.image_base64)?;
        let prompt = format!(
            "Inspect the screenshot and decide whether this expected state is visibly true: {}. \
             Return only JSON {{\"observed\":true|false,\"summary\":string}}. Do not propose actions.",
            job.expected_state
        );
        let output = self
            .run_model(config, RuntimeModel::Qwen, &image, &prompt, 128)
            .await?;
        #[derive(Deserialize)]
        struct Verification {
            observed: bool,
            summary: String,
        }
        let value = extract_json(&output)
            .ok_or_else(|| "Qwen did not return verification JSON".to_string())?;
        let result: Verification =
            serde_json::from_value(value).map_err(|error| error.to_string())?;
        Ok(StateVerificationResult {
            observed: result.observed,
            summary: result.summary,
            model_id: "Qwen2.5-VL-7B-Instruct-Q4_K_M".to_string(),
            snapshot_id: snapshot_id.to_string(),
        })
    }

    async fn run_model(
        &self,
        config: &ModelRuntimeConfig,
        model: RuntimeModel,
        image: &[u8],
        prompt: &str,
        tokens: usize,
    ) -> Result<String, String> {
        let _permit = self
            .gpu_gate
            .acquire()
            .await
            .map_err(|_| "GPU scheduler is closed".to_string())?;
        if self
            .failed_models
            .lock()
            .expect("failed model lock poisoned")
            .contains(&model)
        {
            return Err(format!("{model:?} was disabled after a previous GPU OOM"));
        }

        let key = worker_key(config, model)?;
        let worker = self.ensure_worker(key).await?;
        let request = ActiveRequest::new(worker.clone());
        let image_url = format!(
            "data:{};base64,{}",
            image_mime(image),
            base64::engine::general_purpose::STANDARD.encode(image)
        );
        let response = self
            .client
            .post(format!("{}/v1/chat/completions", worker.endpoint))
            .json(&serde_json::json!({
                "model": "local",
                "messages": [{
                    "role": "user",
                    "content": [
                        {"type": "image_url", "image_url": {"url": image_url}},
                        {"type": "text", "text": prompt}
                    ]
                }],
                "max_tokens": tokens,
                "temperature": 0,
                "top_k": 1,
                "seed": 42,
                "stream": false
            }))
            .send()
            .await
            .map_err(|error| format!("llama.cpp request failed: {error}"))?;
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|error| format!("read llama.cpp response: {error}"))?;
        if !status.is_success() {
            let evidence = format!("{}\n{}", worker.logs(), body);
            self.disable_after_oom(model, &evidence);
            return Err(format!(
                "llama.cpp returned HTTP {}: {}",
                status,
                bounded_text(&body, 2048)
            ));
        }
        let output = parse_completion_content(&body)?;
        if output.trim().is_empty() {
            return Err("llama.cpp returned an empty response".to_string());
        }
        request.finish();
        Ok(output)
    }

    async fn ensure_worker(&self, key: WorkerKey) -> Result<Arc<HotWorker>, String> {
        let mut slot = self.hot_worker.lock().await;
        if let Some(worker) = slot.as_ref() {
            if worker.can_reuse(&key) {
                tracing::debug!(
                    model = ?key.model,
                    pid = ?worker.process.pid(),
                    "reusing hot llama.cpp worker"
                );
                return Ok(worker.clone());
            }
        }

        if let Some(worker) = slot.take() {
            tracing::info!(
                old_model = ?worker.key.model,
                new_model = ?key.model,
                pid = ?worker.process.pid(),
                "evicting llama.cpp worker for model switch"
            );
            worker.terminate();
            if !wait_for_exit(&worker.process, WORKER_STOP_TIMEOUT).await {
                return Err(
                    "previous llama-server worker did not exit after termination".to_string(),
                );
            }
        }

        let worker = match self.start_worker(key.clone()).await {
            Ok(worker) => worker,
            Err(error) => {
                self.disable_after_oom(key.model, &error);
                return Err(error);
            }
        };
        *slot = Some(worker.clone());
        Ok(worker)
    }

    async fn start_worker(&self, key: WorkerKey) -> Result<Arc<HotWorker>, String> {
        let address = reserve_loopback_address()?;
        let mut command = Command::new(&key.server);
        command
            .arg("-m")
            .arg(&key.model_path)
            .arg("--mmproj")
            .arg(&key.mmproj_path)
            .args([
                "--host",
                "127.0.0.1",
                "--port",
                &address.port().to_string(),
                "-c",
                "4096",
                "-ngl",
                "all",
                "--parallel",
                "1",
                "--image-max-tokens",
                "1024",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000);

        let mut child = command
            .spawn()
            .map_err(|error| format!("start llama-server: {error}"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "llama-server stdout is unavailable".to_string())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "llama-server stderr is unavailable".to_string())?;
        let logs = Arc::new(Mutex::new(String::new()));
        capture_logs(stdout, logs.clone());
        capture_logs(stderr, logs.clone());
        let worker = Arc::new(HotWorker {
            key,
            endpoint: format!("http://{address}"),
            process: Arc::new(WorkerProcess {
                child: Mutex::new(child),
                terminated: AtomicBool::new(false),
            }),
            logs,
            last_used: Mutex::new(Instant::now()),
            active: AtomicBool::new(false),
        });

        let ready = tokio::time::timeout(MODEL_START_TIMEOUT, self.wait_until_ready(&worker)).await;
        match ready {
            Ok(Ok(())) => {
                tracing::info!(
                    model = ?worker.key.model,
                    pid = ?worker.process.pid(),
                    endpoint = %worker.endpoint,
                    "llama.cpp worker is ready"
                );
                Ok(worker)
            }
            Ok(Err(error)) => {
                worker.terminate();
                Err(error)
            }
            Err(_) => {
                let logs = worker.logs();
                worker.terminate();
                Err(format!(
                    "llama-server did not become ready within {} seconds: {}",
                    MODEL_START_TIMEOUT.as_secs(),
                    bounded_text(&logs, 4096)
                ))
            }
        }
    }

    async fn wait_until_ready(&self, worker: &HotWorker) -> Result<(), String> {
        loop {
            if !worker.process.is_running() {
                return Err(format!(
                    "llama-server exited during startup: {}",
                    bounded_text(&worker.logs(), 4096)
                ));
            }
            let healthy = self
                .client
                .get(format!("{}/health", worker.endpoint))
                .timeout(Duration::from_secs(2))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success());
            let logs = worker.logs();
            if healthy && gpu_offload_complete(&logs) {
                return Ok(());
            }
            if is_oom(&logs) {
                return Err(format!(
                    "llama-server exhausted GPU memory during startup: {}",
                    bounded_text(&logs, 4096)
                ));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn disable_after_oom(&self, model: RuntimeModel, evidence: &str) {
        if is_oom(evidence) {
            self.failed_models
                .lock()
                .expect("failed model lock poisoned")
                .insert(model);
        }
    }
}

fn capture_logs<R>(mut reader: R, logs: Arc<Mutex<String>>)
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer = [0_u8; 4096];
        loop {
            let count = match reader.read(&mut buffer).await {
                Ok(0) | Err(_) => return,
                Ok(count) => count,
            };
            let text = String::from_utf8_lossy(&buffer[..count]);
            let mut output = logs.lock().expect("llama.cpp log lock poisoned");
            output.push_str(&text);
            if output.len() > MAX_STARTUP_LOG_BYTES {
                let remove = output.len() - MAX_STARTUP_LOG_BYTES;
                let boundary = output
                    .char_indices()
                    .map(|(index, _)| index)
                    .find(|index| *index >= remove)
                    .unwrap_or(remove);
                output.drain(..boundary);
            }
        }
    });
}

async fn wait_for_exit(process: &WorkerProcess, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if process.has_exited() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    process.has_exited()
}

fn reserve_loopback_address() -> Result<SocketAddr, String> {
    let listener = std::net::TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .map_err(|error| format!("reserve llama-server loopback port: {error}"))?;
    listener
        .local_addr()
        .map_err(|error| format!("read llama-server loopback port: {error}"))
}

fn worker_key(config: &ModelRuntimeConfig, model: RuntimeModel) -> Result<WorkerKey, String> {
    let server = config
        .llama_server
        .as_deref()
        .filter(|path| Path::new(path).is_file())
        .ok_or_else(|| "llama-server is not configured or is missing".to_string())?;
    let (model_path, mmproj_path) = model_files(config, model)
        .ok_or_else(|| "model GGUF files are not configured or are missing".to_string())?;
    Ok(WorkerKey {
        model,
        server: PathBuf::from(server),
        model_path: PathBuf::from(model_path),
        mmproj_path: PathBuf::from(mmproj_path),
    })
}

fn worker_can_reuse(current: &WorkerKey, requested: &WorkerKey, running: bool) -> bool {
    running && current == requested
}

fn idle_worker_expired(elapsed: Duration) -> bool {
    elapsed >= MODEL_IDLE_TTL
}

fn load_capabilities(config: &ModelRuntimeConfig) -> CapabilityManifest {
    config
        .capability_manifest
        .as_deref()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn ensure_capability(config: &ModelRuntimeConfig, model: RuntimeModel) -> Result<(), String> {
    let accepted = load_capabilities(config);
    let ready = accepted.amd_vulkan_device_detected
        && match model {
            RuntimeModel::Qwen => accepted.models.qwen.ready,
            RuntimeModel::ShowUi => accepted.models.showui.ready,
        };
    if !ready {
        return Err("model has not passed the strict local runtime acceptance test".to_string());
    }
    if !path_is_file(config.llama_server.as_deref()) || model_files(config, model).is_none() {
        return Err("accepted model runtime files are missing".to_string());
    }
    Ok(())
}

fn model_files(config: &ModelRuntimeConfig, model: RuntimeModel) -> Option<(&str, &str)> {
    let (model, mmproj) = match model {
        RuntimeModel::Qwen => (
            config.qwen_model.as_deref()?,
            config.qwen_mmproj.as_deref()?,
        ),
        RuntimeModel::ShowUi => (
            config.showui_model.as_deref()?,
            config.showui_mmproj.as_deref()?,
        ),
    };
    (Path::new(model).is_file() && Path::new(mmproj).is_file()).then_some((model, mmproj))
}

fn path_is_file(path: Option<&str>) -> bool {
    path.is_some_and(|path| Path::new(path).is_file())
}

fn decode_image(image: &str) -> Result<Vec<u8>, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(image)
        .map_err(|error| format!("invalid image payload: {error}"))?;
    if bytes.is_empty() || bytes.len() > 16 * 1024 * 1024 {
        return Err("image payload is empty or exceeds 16 MiB".to_string());
    }
    Ok(bytes)
}

fn image_mime(image: &[u8]) -> &'static str {
    if image.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if image.starts_with(b"RIFF") && image.get(8..12) == Some(b"WEBP") {
        "image/webp"
    } else {
        "image/jpeg"
    }
}

fn grounding_prompt(target: &str) -> String {
    format!(
        "Based on the screenshot of the page, I give a text description and you give its corresponding \
         location. The coordinate represents a clickable location [x, y] for an element, which is a \
         relative coordinate on the screenshot, scaled from 0 to 1. {target}"
    )
}

fn extract_json(text: &str) -> Option<serde_json::Value> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    serde_json::from_str(&text[start..=end]).ok()
}

fn extract_coordinate(text: &str) -> Option<(f32, f32)> {
    for (open, close) in [('[', ']'), ('(', ')')] {
        let mut remainder = text;
        let mut found = None;
        while let Some(start) = remainder.find(open) {
            remainder = &remainder[start + 1..];
            let Some(end) = remainder.find(close) else {
                break;
            };
            let pair = &remainder[..end];
            let mut values = pair.split(',').map(str::trim);
            if let (Some(x), Some(y), None) = (values.next(), values.next(), values.next()) {
                if let (Ok(x), Ok(y)) = (x.parse::<f32>(), y.parse::<f32>()) {
                    found = Some((x, y));
                }
            }
            remainder = &remainder[end + 1..];
        }
        if found.is_some() {
            return found;
        }
    }
    None
}

fn normalize_coordinate(point: (f32, f32), width: u32, height: u32) -> Option<(f32, f32)> {
    let point = if point.0 <= 1.0 && point.1 <= 1.0 {
        point
    } else {
        (point.0 / width as f32, point.1 / height as f32)
    };
    (point.0.is_finite()
        && point.1.is_finite()
        && (0.0..=1.0).contains(&point.0)
        && (0.0..=1.0).contains(&point.1))
    .then_some(point)
}

fn point_in_bounds(point: (f32, f32), bounds: &BoundingBox) -> bool {
    point.0 >= bounds.x
        && point.1 >= bounds.y
        && point.0 <= bounds.x + bounds.width
        && point.1 <= bounds.y + bounds.height
}

fn geometric_grounding_confidence(point: (f32, f32), bounds: &BoundingBox) -> f32 {
    let center_x = bounds.x + bounds.width / 2.0;
    let center_y = bounds.y + bounds.height / 2.0;
    let error = ((point.0 - center_x).powi(2) + (point.1 - center_y).powi(2)).sqrt();
    let diagonal = (bounds.width.powi(2) + bounds.height.powi(2)).sqrt();
    if diagonal <= f32::EPSILON {
        return 0.0;
    }
    (1.0 - error / diagonal).clamp(0.0, 1.0)
}

fn gpu_offload_complete(text: &str) -> bool {
    text.lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        let Some(rest) = lower.split("offloaded ").nth(1) else {
            return false;
        };
        let Some(counts) = rest.split(" layers").next() else {
            return false;
        };
        let Some((offloaded, total)) = counts.split_once('/') else {
            return false;
        };
        offloaded
            .trim()
            .parse::<u32>()
            .ok()
            .zip(total.trim().parse::<u32>().ok())
            .is_some_and(|(offloaded, total)| offloaded > 0 && offloaded == total)
    })
}

fn is_oom(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("out of memory")
        || lower.contains("failed to allocate")
        || lower.contains("device memory")
        || lower.contains("vk_error_out_of_device_memory")
}

fn bounded_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

fn parse_completion_content(body: &str) -> Result<String, String> {
    #[derive(Deserialize)]
    struct Completion {
        choices: Vec<Choice>,
    }
    #[derive(Deserialize)]
    struct Choice {
        message: Message,
    }
    #[derive(Deserialize)]
    struct Message {
        content: serde_json::Value,
    }

    let completion: Completion = serde_json::from_str(body)
        .map_err(|error| format!("invalid llama.cpp completion JSON: {error}"))?;
    let content = completion
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| "llama.cpp completion has no choices".to_string())?
        .message
        .content;
    if let Some(text) = content.as_str() {
        return Ok(text.to_string());
    }
    if let Some(parts) = content.as_array() {
        let text = parts
            .iter()
            .filter_map(|part| part.get("text").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join("");
        if !text.is_empty() {
            return Ok(text);
        }
    }
    Err("llama.cpp completion content is not text".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn accepted_runtime() -> (tempfile::TempDir, ModelRuntimeConfig) {
        let root = tempfile::tempdir().unwrap();
        for name in [
            "llama-server",
            "qwen.gguf",
            "qwen-mmproj.gguf",
            "showui.gguf",
            "showui-mmproj.gguf",
        ] {
            std::fs::write(root.path().join(name), b"test").unwrap();
        }
        let capabilities = root.path().join("capabilities.json");
        std::fs::write(
            &capabilities,
            r#"{"amd_vulkan_device_detected":true,"models":{"qwen":{"ready":true},"showui":{"ready":true}}}"#,
        )
        .unwrap();
        let path = |name: &str| Some(root.path().join(name).to_string_lossy().into_owned());
        let runtime = ModelRuntimeConfig {
            models_dir: root.path().to_string_lossy().into_owned(),
            sensevoice_model: String::new(),
            sensevoice_tokens: String::new(),
            llama_server: path("llama-server"),
            qwen_model: path("qwen.gguf"),
            qwen_mmproj: path("qwen-mmproj.gguf"),
            showui_model: path("showui.gguf"),
            showui_mmproj: path("showui-mmproj.gguf"),
            capability_manifest: Some(capabilities.to_string_lossy().into_owned()),
        };
        (root, runtime)
    }

    fn key(model: RuntimeModel, model_path: &str) -> WorkerKey {
        WorkerKey {
            model,
            server: PathBuf::from("llama-server"),
            model_path: PathBuf::from(model_path),
            mmproj_path: PathBuf::from(format!("{model_path}.mmproj")),
        }
    }

    #[test]
    fn same_model_worker_is_reused_only_while_running() {
        let qwen = key(RuntimeModel::Qwen, "qwen.gguf");
        assert!(worker_can_reuse(&qwen, &qwen, true));
        assert!(!worker_can_reuse(&qwen, &qwen, false));
    }

    #[test]
    fn model_switch_evicts_the_current_worker() {
        let qwen = key(RuntimeModel::Qwen, "qwen.gguf");
        let showui = key(RuntimeModel::ShowUi, "showui.gguf");
        assert!(!worker_can_reuse(&qwen, &showui, true));
    }

    #[test]
    fn idle_worker_expires_at_120_seconds() {
        assert!(!idle_worker_expired(Duration::from_secs(119)));
        assert!(idle_worker_expired(Duration::from_secs(120)));
    }

    #[test]
    fn parses_openai_compatible_completion_content() {
        let body = r#"{"choices":[{"message":{"content":"[0.8, 0.7]"}}]}"#;
        assert_eq!(parse_completion_content(body).unwrap(), "[0.8, 0.7]");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_an_active_request_terminates_its_worker() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "sleep 30"])
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let child = command.spawn().unwrap();
        let worker = Arc::new(HotWorker {
            key: key(RuntimeModel::Qwen, "qwen.gguf"),
            endpoint: "http://127.0.0.1:1".to_string(),
            process: Arc::new(WorkerProcess {
                child: Mutex::new(child),
                terminated: AtomicBool::new(false),
            }),
            logs: Arc::new(Mutex::new(String::new())),
            last_used: Mutex::new(Instant::now()),
            active: AtomicBool::new(false),
        });
        let request = ActiveRequest::new(worker.clone());
        assert!(worker.active.load(Ordering::Acquire));
        drop(request);
        assert!(worker.process.terminated.load(Ordering::Acquire));
        assert!(wait_for_exit(&worker.process, Duration::from_secs(2)).await);
    }

    #[test]
    fn extracts_parenthesized_and_normalized_coordinates() {
        assert_eq!(extract_coordinate("answer (820,515)"), Some((820.0, 515.0)));
        assert_eq!(
            normalize_coordinate((820.0, 515.0), 1280, 720),
            Some((0.640625, 0.7152778))
        );
    }

    #[test]
    fn requires_complete_gpu_offload() {
        assert!(gpu_offload_complete("offloaded 29/29 layers to GPU"));
        assert!(!gpu_offload_complete("offloaded 12/29 layers to GPU"));
    }

    #[test]
    fn grounding_confidence_is_desktop_derived_and_center_sensitive() {
        let bounds = BoundingBox {
            x: 0.2,
            y: 0.3,
            width: 0.4,
            height: 0.2,
        };
        assert_eq!(geometric_grounding_confidence((0.4, 0.4), &bounds), 1.0);
        assert!(geometric_grounding_confidence((0.2, 0.3), &bounds) <= 0.5);
    }

    #[test]
    fn publishes_only_models_with_acceptance_evidence_and_files() {
        let (_root, runtime) = accepted_runtime();
        let capabilities = LlamaAdapter::default().capabilities(&runtime);
        assert!(capabilities.contains(&ale_core::model_scheduler::ModelCapability::LocalPlanning));
        assert!(
            capabilities.contains(&ale_core::model_scheduler::ModelCapability::ElementGrounding)
        );
        assert!(
            !capabilities.contains(&ale_core::model_scheduler::ModelCapability::SpeechRecognition)
        );
    }
}
