use ale_core::model_scheduler::{
    BoundingBox, GroundingCandidate, GroundingJob, GroundingModel, GroundingResult,
    LocalPlanningJob, LocalPlanningResult, ModelRuntimeConfig, SemanticPlan, StateVerificationJob,
    StateVerificationResult,
};
use base64::Engine;
use serde::Deserialize;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use tokio::process::Command;
use tokio::sync::Semaphore;

pub struct LlamaAdapter {
    gpu_gate: Arc<Semaphore>,
    failed_models: Mutex<HashSet<RuntimeModel>>,
}

impl Default for LlamaAdapter {
    fn default() -> Self {
        Self {
            gpu_gate: Arc::new(Semaphore::new(1)),
            failed_models: Mutex::new(HashSet::new()),
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
    uitars: UiTarsAcceptance,
}

#[derive(Debug, Default, Deserialize)]
struct ModelAcceptance {
    ready: bool,
}

#[derive(Debug, Default, Deserialize)]
struct UiTarsAcceptance {
    ready: bool,
    selected_profile: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum RuntimeModel {
    Qwen,
    ShowUi,
    UiTars,
}

struct TemporaryImage(PathBuf);

impl Drop for TemporaryImage {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl LlamaAdapter {
    pub fn capabilities(
        &self,
        config: &ModelRuntimeConfig,
    ) -> Vec<ale_core::model_scheduler::ModelCapability> {
        use ale_core::model_scheduler::ModelCapability;
        let accepted = load_capabilities(config);
        if !accepted.amd_vulkan_device_detected || !path_is_file(config.llama_cli.as_deref()) {
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
        let runtime_model = match job.model {
            GroundingModel::ShowUi => RuntimeModel::ShowUi,
            GroundingModel::UiTars => RuntimeModel::UiTars,
        };
        ensure_capability(config, runtime_model)?;
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
        let prompt = grounding_prompt(
            config,
            runtime_model,
            target,
            job.image_width,
            job.image_height,
        );
        let output = self
            .run_model(config, runtime_model, &image, &prompt, 128)
            .await?;
        let raw = extract_coordinate(&output)
            .ok_or_else(|| "grounding model did not return a coordinate".to_string())?;
        let point = normalize_coordinate(raw, job.image_width, job.image_height)
            .ok_or_else(|| "grounding coordinate is outside the screenshot".to_string())?;
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
                        "model point [{:.6},{:.6}] is inside a desktop candidate; geometric center confidence {:.6}",
                        point.0, point.1, confidence
                    ),
                }
            })
            .collect::<Vec<_>>();
        let selected = (candidates.len() == 1).then(|| candidates[0].clone());
        Ok(GroundingResult {
            snapshot_id: snapshot_id.to_string(),
            model_id: match runtime_model {
                RuntimeModel::ShowUi => "ShowUI-2B-Q4_K_M",
                RuntimeModel::UiTars => "UI-TARS-1.5-7B-Q4_K_M",
                RuntimeModel::Qwen => unreachable!(),
            }
            .to_string(),
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
        let cli = config
            .llama_cli
            .as_deref()
            .ok_or_else(|| "llama.cpp is not configured".to_string())?;
        let (model_path, mmproj_path) = model_files(config, model)
            .ok_or_else(|| "model GGUF files are not configured".to_string())?;
        let image_path =
            std::env::temp_dir().join(format!("ale-modeld-{}.jpg", uuid::Uuid::new_v4()));
        tokio::fs::write(&image_path, image)
            .await
            .map_err(|error| format!("write temporary model image: {error}"))?;
        let image_guard = TemporaryImage(image_path.clone());
        let mut command = Command::new(cli);
        command
            .args(["-m", model_path, "--mmproj", mmproj_path, "--image"])
            .arg(&image_path)
            .args([
                "-p",
                prompt,
                "-n",
                &tokens.to_string(),
                "-c",
                "4096",
                "-ngl",
                "all",
                "--temp",
                "0",
                "--top-k",
                "1",
                "--seed",
                "42",
                "--image-max-tokens",
                "1024",
                "--verbose",
                "--no-display-prompt",
                "--single-turn",
            ])
            .kill_on_drop(true);
        let output = command
            .output()
            .await
            .map_err(|error| format!("start llama.cpp: {error}"))?;
        drop(image_guard);
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let lower = stderr.to_ascii_lowercase();
            if lower.contains("out of memory")
                || lower.contains("failed to allocate")
                || lower.contains("device memory")
            {
                self.failed_models
                    .lock()
                    .expect("failed model lock poisoned")
                    .insert(model);
            }
            return Err(format!("llama.cpp exited with {}", output.status));
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        if !gpu_offload_complete(&stderr) {
            return Err("llama.cpp did not report complete GPU layer offload".to_string());
        }
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if stdout.is_empty() {
            return Err("llama.cpp returned an empty response".to_string());
        }
        Ok(stdout)
    }
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
            RuntimeModel::UiTars => accepted.models.uitars.ready,
        };
    if !ready {
        return Err("model has not passed the strict local runtime acceptance test".to_string());
    }
    if !path_is_file(config.llama_cli.as_deref()) || model_files(config, model).is_none() {
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
        RuntimeModel::UiTars => (
            config.uitars_model.as_deref()?,
            config.uitars_mmproj.as_deref()?,
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

fn grounding_prompt(
    config: &ModelRuntimeConfig,
    model: RuntimeModel,
    target: &str,
    width: u32,
    height: u32,
) -> String {
    match model {
        RuntimeModel::ShowUi => format!(
            "Based on the screenshot of the page, I give a text description and you give its corresponding \
             location. The coordinate represents a clickable location [x, y] for an element, which is a \
             relative coordinate on the screenshot, scaled from 0 to 1. {target}"
        ),
        RuntimeModel::UiTars => {
            let profile = load_capabilities(config).models.uitars.selected_profile;
            match profile.as_deref() {
                Some("normalized_center") => format!(
                    "Locate {target}. Return only its clickable center as [x, y], using relative coordinates from 0 to 1."
                ),
                Some("action_position") => format!(
                    "Task: click {target}. Return only {{'action':'CLICK','value':null,'position':[x,y]}}. \
                     Position is the clickable center in relative coordinates from 0 to 1."
                ),
                _ => format!(
                    "Locate {target}. The screenshot is exactly {width} by {height} pixels. Return only the \
                     absolute pixel coordinate [x, y] at the center of the target, strictly inside its visible bounds."
                ),
            }
        }
        RuntimeModel::Qwen => unreachable!(),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn accepted_runtime() -> (tempfile::TempDir, ModelRuntimeConfig) {
        let root = tempfile::tempdir().unwrap();
        for name in [
            "llama-cli",
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
            r#"{"amd_vulkan_device_detected":true,"models":{"qwen":{"ready":true},"showui":{"ready":true},"uitars":{"ready":false,"selected_profile":null}}}"#,
        )
        .unwrap();
        let path = |name: &str| Some(root.path().join(name).to_string_lossy().into_owned());
        let runtime = ModelRuntimeConfig {
            models_dir: root.path().to_string_lossy().into_owned(),
            sensevoice_model: String::new(),
            sensevoice_tokens: String::new(),
            llama_cli: path("llama-cli"),
            qwen_model: path("qwen.gguf"),
            qwen_mmproj: path("qwen-mmproj.gguf"),
            showui_model: path("showui.gguf"),
            showui_mmproj: path("showui-mmproj.gguf"),
            uitars_model: None,
            uitars_mmproj: None,
            capability_manifest: Some(capabilities.to_string_lossy().into_owned()),
        };
        (root, runtime)
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
