use crate::actions::RiskLevel;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const LOCAL_LOW_RISK_THRESHOLD: f32 = 0.90;
pub const LOCAL_MEDIUM_RISK_THRESHOLD: f32 = 0.97;
pub const MAX_LOCAL_PLAN_STEPS: usize = 5;
pub const MIN_DEFAULT_VRAM_BYTES: u64 = 8 * 1024 * 1024 * 1024;
pub const MIN_LARGE_VRAM_BYTES: u64 = 16 * 1024 * 1024 * 1024;
pub const MODEL_IDLE_TTL: Duration = Duration::from_secs(120);
pub const MODEL_START_TIMEOUT: Duration = Duration::from_secs(45);
pub const MODEL_STAGE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCapability {
    SpeechRecognition,
    StateSummary,
    LocalPlanning,
    RemotePlanning,
    ElementGrounding,
    StateVerification,
    SpeechSynthesis,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchedulerPriority {
    DownloadMaintenance,
    ModelWarmup,
    StateVerification,
    InteractiveRequest,
    ConfirmedExecution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalModelState {
    Stopped,
    Starting,
    Ready,
    Busy,
    Idle,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteModelState {
    Ready,
    Busy,
    RateLimited,
    CircuitOpen,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoundingBox {
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

impl BoundingBox {
    pub fn is_normalized(&self) -> bool {
        self.x.is_finite()
            && self.y.is_finite()
            && self.width.is_finite()
            && self.height.is_finite()
            && self.x >= 0.0
            && self.y >= 0.0
            && self.width > 0.0
            && self.height > 0.0
            && self.x + self.width <= 1.0
            && self.y + self.height <= 1.0
    }

    pub fn iou(&self, other: &Self) -> f32 {
        let left = self.x.max(other.x);
        let top = self.y.max(other.y);
        let right = (self.x + self.width).min(other.x + other.width);
        let bottom = (self.y + self.height).min(other.y + other.height);
        let intersection = (right - left).max(0.0) * (bottom - top).max(0.0);
        let union = self.width * self.height + other.width * other.height - intersection;
        if union <= f32::EPSILON {
            0.0
        } else {
            intersection / union
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TargetRef {
    pub node_id: Option<String>,
    pub role: Option<String>,
    pub label: Option<String>,
    pub visual_description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticStep {
    pub operation: String,
    pub target: Option<TargetRef>,
    pub input_summary: Option<String>,
    pub expected_state: String,
    pub risk: RiskLevel,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SemanticPlan {
    pub goal: String,
    pub application_id: Option<String>,
    pub steps: Vec<SemanticStep>,
}

impl SemanticPlan {
    pub fn maximum_risk(&self) -> RiskLevel {
        self.steps
            .iter()
            .map(|step| step.risk)
            .max()
            .unwrap_or(RiskLevel::Low)
    }

    pub fn has_observable_postconditions(&self) -> bool {
        !self.steps.is_empty()
            && self
                .steps
                .iter()
                .all(|step| !step.expected_state.trim().is_empty())
    }

    pub fn describe_steps(&self) -> Vec<String> {
        self.steps
            .iter()
            .map(|step| {
                let target = step
                    .target
                    .as_ref()
                    .and_then(|target| {
                        target
                            .label
                            .as_deref()
                            .or(target.visual_description.as_deref())
                    })
                    .unwrap_or("当前界面");
                format!(
                    "{}: {}，预期 {}",
                    step.operation, target, step.expected_state
                )
            })
            .collect()
    }
}

pub fn validate_semantic_plan(mut plan: SemanticPlan) -> crate::Result<SemanticPlan> {
    if plan.goal.trim().is_empty() || plan.steps.is_empty() {
        return Err(crate::AleError::ConfigError(
            "semantic plan must have a goal and at least one step".to_string(),
        ));
    }
    if plan.steps.len() > 20 {
        return Err(crate::AleError::ConfigError(
            "semantic plan exceeds 20 steps".to_string(),
        ));
    }
    for step in &mut plan.steps {
        if step.operation.trim().is_empty() || step.expected_state.trim().is_empty() {
            return Err(crate::AleError::ConfigError(
                "every semantic step requires an operation and observable expected state"
                    .to_string(),
            ));
        }
        step.risk = semantic_operation_risk(&step.operation, step.input_summary.as_deref());
    }
    Ok(plan)
}

pub fn semantic_operation_risk(operation: &str, input: Option<&str>) -> RiskLevel {
    let operation = operation.to_ascii_lowercase();
    let input = input.unwrap_or_default().to_ascii_lowercase();
    let high_risk_terms = [
        "delete",
        "remove",
        "close",
        "payment",
        "purchase",
        "send",
        "publish",
        "install",
        "permission",
        "security",
        "account",
        "credential",
        "password",
        "system_setting",
        "file_operation",
        "删除",
        "付款",
        "购买",
        "发送",
        "发布",
        "安装",
        "权限",
        "密码",
    ];
    if high_risk_terms
        .iter()
        .any(|term| operation.contains(term) || input.contains(term))
    {
        return RiskLevel::High;
    }
    let medium_risk_terms = [
        "click", "type", "key", "open", "select", "toggle", "submit", "点击", "输入", "打开",
        "选择", "切换",
    ];
    if medium_risk_terms
        .iter()
        .any(|term| operation.contains(term))
    {
        RiskLevel::Medium
    } else {
        RiskLevel::Low
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroundingCandidate {
    pub bounds: BoundingBox,
    pub click_x: f32,
    pub click_y: f32,
    pub confidence: f32,
    pub evidence: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroundingResult {
    pub snapshot_id: String,
    pub model_id: String,
    pub selected: Option<GroundingCandidate>,
    pub candidates: Vec<GroundingCandidate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ConfidenceEvidence {
    pub target_uniqueness: f32,
    pub grounding_confidence: f32,
    pub evidence_match: f32,
    pub postcondition_coverage: f32,
}

impl ConfidenceEvidence {
    pub fn conservative_score(self) -> f32 {
        [
            self.target_uniqueness,
            self.grounding_confidence,
            self.evidence_match,
            self.postcondition_coverage,
        ]
        .into_iter()
        .map(|value| value.clamp(0.0, 1.0))
        .fold(1.0, f32::min)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationReason {
    HighRisk,
    CrossApplication,
    TooManySteps,
    SensitiveOperation,
    MissingPostcondition,
    StaleSnapshot,
    InsufficientEvidence,
    LocalModelUnavailable,
    GpuUnavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteTarget {
    LocalQwen,
    RemotePrimary,
    RemoteBackup,
    UserDecisionRequired,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteDecision {
    pub target: RouteTarget,
    pub reasons: Vec<EscalationReason>,
    pub requires_confirmation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelJob {
    pub request_id: String,
    pub capability: ModelCapability,
    pub priority: SchedulerPriority,
    pub deadline_unix_ms: i64,
    pub risk_ceiling: RiskLevel,
    pub snapshot_id: Option<String>,
    pub privacy: JobPrivacy,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobPrivacy {
    pub allow_remote: bool,
    pub allow_full_screenshot: bool,
    pub allow_sensitive_content: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelModelJob {
    pub target_request_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteEndpointConfig {
    pub provider: String,
    pub api_key: String,
    pub api_url: String,
    pub model: String,
    pub max_tokens: usize,
    pub timeout_seconds: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteProviderSet {
    pub primary: RemoteEndpointConfig,
    pub backup: Option<RemoteEndpointConfig>,
    pub backup_enabled: bool,
    pub backup_pre_authorized: bool,
    pub circuit_failure_threshold: u32,
    pub circuit_open_seconds: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemotePlanningJob {
    pub question: String,
    pub image_base64: Option<String>,
    pub tools: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemotePlanningResult {
    pub response: crate::cloud::VisionResponse,
    pub endpoint: RemoteEndpointRole,
    pub failover_notice: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteEndpointRole {
    Primary,
    Backup,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerHealth {
    pub service: String,
    pub protocol_version: u32,
    pub local_vlm_gpu_only: bool,
    pub gpus: Vec<GpuDevice>,
    pub available_capabilities: Vec<ModelCapability>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GpuBackend {
    Nvidia,
    Amd,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuDevice {
    pub id: String,
    pub name: String,
    pub backend: GpuBackend,
    pub total_vram_bytes: u64,
    pub available_vram_bytes: u64,
}

impl GpuDevice {
    pub fn supports_default_models(&self) -> bool {
        self.available_vram_bytes >= MIN_DEFAULT_VRAM_BYTES
    }

    pub fn supports_large_models(&self) -> bool {
        self.available_vram_bytes >= MIN_LARGE_VRAM_BYTES
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRuntimeConfig {
    pub models_dir: String,
    pub sensevoice_model: String,
    pub sensevoice_tokens: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeechRecognitionJob {
    pub wav_base64: String,
    pub allow_remote: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpeechRecognitionResult {
    pub text: String,
    pub model_id: String,
    pub used_remote: bool,
    pub failover_notice: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelArtifact {
    pub filename: String,
    pub url: String,
    pub revision: String,
    pub sha256: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelPackage {
    pub id: String,
    pub display_name: String,
    pub license: String,
    pub capabilities: Vec<ModelCapability>,
    pub minimum_vram_bytes: u64,
    pub requires_explicit_consent: bool,
    pub artifacts: Vec<ModelArtifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelManifest {
    pub schema_version: u32,
    pub packages: Vec<ModelPackage>,
}

impl ModelManifest {
    pub fn validate(&self) -> crate::Result<()> {
        if self.schema_version != 1 || self.packages.is_empty() {
            return Err(crate::AleError::ConfigError(
                "unsupported or empty model manifest".to_string(),
            ));
        }
        let mut ids = std::collections::HashSet::new();
        for package in &self.packages {
            let package_path = std::path::Path::new(&package.id);
            if package.id.trim().is_empty()
                || package_path.is_absolute()
                || package_path.components().count() != 1
                || matches!(package.id.as_str(), "." | "..")
                || package.display_name.trim().is_empty()
                || package.license.trim().is_empty()
                || package.capabilities.is_empty()
                || package.artifacts.is_empty()
                || !ids.insert(package.id.as_str())
            {
                return Err(crate::AleError::ConfigError(
                    "invalid or duplicate model package".to_string(),
                ));
            }
            if !package.requires_explicit_consent {
                return Err(crate::AleError::ConfigError(
                    "large model packages must require explicit download consent".to_string(),
                ));
            }
            for artifact in &package.artifacts {
                let filename = std::path::Path::new(&artifact.filename);
                if artifact.size_bytes == 0
                    || filename.is_absolute()
                    || filename.components().count() != 1
                    || artifact.revision.is_empty()
                    || matches!(artifact.revision.as_str(), "main" | "master" | "latest")
                    || artifact.sha256.len() != 64
                    || !artifact.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
                    || !artifact.url.starts_with("https://")
                {
                    return Err(crate::AleError::ConfigError(format!(
                        "invalid pinned artifact for model {}",
                        package.id
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn package(&self, package_id: &str) -> crate::Result<&ModelPackage> {
        self.validate()?;
        self.packages
            .iter()
            .find(|package| package.id == package_id)
            .ok_or_else(|| {
                crate::AleError::ConfigError(format!(
                    "model package is not present in the pinned manifest: {package_id}"
                ))
            })
    }
}

impl ModelPackage {
    pub fn download_size_bytes(&self) -> crate::Result<u64> {
        self.artifacts.iter().try_fold(0_u64, |total, artifact| {
            total.checked_add(artifact.size_bytes).ok_or_else(|| {
                crate::AleError::ConfigError(format!(
                    "model package size overflows for {}",
                    self.id
                ))
            })
        })
    }

    pub fn required_disk_bytes(&self) -> crate::Result<u64> {
        // Installation uses a temporary file beside the final file, so reserve two copies.
        self.download_size_bytes()?.checked_mul(2).ok_or_else(|| {
            crate::AleError::ConfigError(format!(
                "model package disk requirement overflows for {}",
                self.id
            ))
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanningEvidence {
    pub maximum_risk: RiskLevel,
    pub step_count: usize,
    pub single_application: bool,
    pub sensitive_operation: bool,
    pub observable_postconditions: bool,
    pub snapshot_fresh: bool,
    pub local_model_available: bool,
    pub gpu_available: bool,
    pub confidence: ConfidenceEvidence,
}

pub fn route_planning(evidence: &PlanningEvidence) -> RouteDecision {
    let mut reasons = Vec::new();
    if evidence.maximum_risk == RiskLevel::High {
        reasons.push(EscalationReason::HighRisk);
    }
    if !evidence.single_application {
        reasons.push(EscalationReason::CrossApplication);
    }
    if evidence.step_count > MAX_LOCAL_PLAN_STEPS {
        reasons.push(EscalationReason::TooManySteps);
    }
    if evidence.sensitive_operation {
        reasons.push(EscalationReason::SensitiveOperation);
    }
    if !evidence.observable_postconditions {
        reasons.push(EscalationReason::MissingPostcondition);
    }
    if !evidence.snapshot_fresh {
        reasons.push(EscalationReason::StaleSnapshot);
    }
    if !evidence.local_model_available {
        reasons.push(EscalationReason::LocalModelUnavailable);
    }
    if !evidence.gpu_available {
        reasons.push(EscalationReason::GpuUnavailable);
    }

    let threshold = match evidence.maximum_risk {
        RiskLevel::Low => LOCAL_LOW_RISK_THRESHOLD,
        RiskLevel::Medium => LOCAL_MEDIUM_RISK_THRESHOLD,
        RiskLevel::High => 1.0,
    };
    if evidence.confidence.conservative_score() < threshold {
        reasons.push(EscalationReason::InsufficientEvidence);
    }

    if reasons.is_empty() {
        return RouteDecision {
            target: RouteTarget::LocalQwen,
            reasons,
            requires_confirmation: evidence.maximum_risk >= RiskLevel::Medium,
        };
    }

    RouteDecision {
        target: if reasons.contains(&EscalationReason::StaleSnapshot) {
            RouteTarget::Reject
        } else {
            RouteTarget::UserDecisionRequired
        },
        reasons,
        requires_confirmation: true,
    }
}

pub fn grounding_models_agree(first: &GroundingCandidate, second: &GroundingCandidate) -> bool {
    let dx = first.click_x - second.click_x;
    let dy = first.click_y - second.click_y;
    first.bounds.iou(&second.bounds) >= 0.70 && (dx * dx + dy * dy).sqrt() <= 0.02
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(risk: RiskLevel, score: f32) -> PlanningEvidence {
        PlanningEvidence {
            maximum_risk: risk,
            step_count: 2,
            single_application: true,
            sensitive_operation: false,
            observable_postconditions: true,
            snapshot_fresh: true,
            local_model_available: true,
            gpu_available: true,
            confidence: ConfidenceEvidence {
                target_uniqueness: score,
                grounding_confidence: score,
                evidence_match: score,
                postcondition_coverage: score,
            },
        }
    }

    #[test]
    fn local_threshold_depends_on_risk() {
        assert_eq!(
            route_planning(&evidence(RiskLevel::Low, 0.90)).target,
            RouteTarget::LocalQwen
        );
        assert_eq!(
            route_planning(&evidence(RiskLevel::Medium, 0.96)).target,
            RouteTarget::UserDecisionRequired
        );
        assert_eq!(
            route_planning(&evidence(RiskLevel::Medium, 0.97)).target,
            RouteTarget::LocalQwen
        );
    }

    #[test]
    fn high_risk_never_routes_locally() {
        let decision = route_planning(&evidence(RiskLevel::High, 1.0));
        assert_eq!(decision.target, RouteTarget::UserDecisionRequired);
        assert!(decision.reasons.contains(&EscalationReason::HighRisk));
    }

    #[test]
    fn missing_gpu_requires_user_decision_instead_of_cpu_vlm_fallback() {
        let mut evidence = evidence(RiskLevel::Low, 1.0);
        evidence.gpu_available = false;
        let decision = route_planning(&evidence);
        assert_eq!(decision.target, RouteTarget::UserDecisionRequired);
        assert!(decision.reasons.contains(&EscalationReason::GpuUnavailable));
    }

    #[test]
    fn stale_snapshot_is_rejected_even_with_perfect_model_scores() {
        let mut evidence = evidence(RiskLevel::Low, 1.0);
        evidence.snapshot_fresh = false;
        let decision = route_planning(&evidence);
        assert_eq!(decision.target, RouteTarget::Reject);
        assert!(decision.reasons.contains(&EscalationReason::StaleSnapshot));
    }

    #[test]
    fn conservative_confidence_uses_weakest_signal() {
        let score = ConfidenceEvidence {
            target_uniqueness: 0.99,
            grounding_confidence: 0.98,
            evidence_match: 0.72,
            postcondition_coverage: 1.0,
        }
        .conservative_score();
        assert_eq!(score, 0.72);
    }

    #[test]
    fn dual_grounders_require_overlap_and_nearby_points() {
        let first = GroundingCandidate {
            bounds: BoundingBox {
                x: 0.1,
                y: 0.1,
                width: 0.2,
                height: 0.2,
            },
            click_x: 0.2,
            click_y: 0.2,
            confidence: 0.99,
            evidence: String::new(),
        };
        let mut second = first.clone();
        second.bounds.x = 0.11;
        second.click_x = 0.21;
        assert!(grounding_models_agree(&first, &second));
        second.click_x = 0.24;
        assert!(!grounding_models_agree(&first, &second));
    }

    #[test]
    fn semantic_risk_is_recomputed_from_operation() {
        let plan = validate_semantic_plan(SemanticPlan {
            goal: "remove a file".to_string(),
            application_id: Some("files".to_string()),
            steps: vec![SemanticStep {
                operation: "delete".to_string(),
                target: None,
                input_summary: None,
                expected_state: "file is absent".to_string(),
                risk: RiskLevel::Low,
            }],
        })
        .unwrap();
        assert_eq!(plan.maximum_risk(), RiskLevel::High);
    }

    #[test]
    fn model_manifest_rejects_moving_revisions_and_unverified_files() {
        let manifest = ModelManifest {
            schema_version: 1,
            packages: vec![ModelPackage {
                id: "model".to_string(),
                display_name: "Model".to_string(),
                license: "Apache-2.0".to_string(),
                capabilities: vec![ModelCapability::StateSummary],
                minimum_vram_bytes: MIN_DEFAULT_VRAM_BYTES,
                requires_explicit_consent: true,
                artifacts: vec![ModelArtifact {
                    filename: "model.gguf".to_string(),
                    url: "https://example.invalid/model.gguf".to_string(),
                    revision: "main".to_string(),
                    sha256: "0".repeat(64),
                    size_bytes: 1,
                }],
            }],
        };
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn model_manifest_rejects_package_path_traversal() {
        let manifest = ModelManifest {
            schema_version: 1,
            packages: vec![ModelPackage {
                id: "../model".to_string(),
                display_name: "Model".to_string(),
                license: "Apache-2.0".to_string(),
                capabilities: vec![ModelCapability::StateSummary],
                minimum_vram_bytes: MIN_DEFAULT_VRAM_BYTES,
                requires_explicit_consent: true,
                artifacts: vec![ModelArtifact {
                    filename: "model.gguf".to_string(),
                    url: "https://example.invalid/model.gguf".to_string(),
                    revision: "0123456789abcdef".to_string(),
                    sha256: "0".repeat(64),
                    size_bytes: 1,
                }],
            }],
        };
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn model_package_reports_download_and_atomic_install_disk_sizes() {
        let package = ModelPackage {
            id: "model".to_string(),
            display_name: "Model".to_string(),
            license: "Apache-2.0".to_string(),
            capabilities: vec![ModelCapability::StateSummary],
            minimum_vram_bytes: MIN_DEFAULT_VRAM_BYTES,
            requires_explicit_consent: true,
            artifacts: vec![
                ModelArtifact {
                    filename: "a".to_string(),
                    url: "https://example.invalid/a".to_string(),
                    revision: "0123456789abcdef".to_string(),
                    sha256: "0".repeat(64),
                    size_bytes: 3,
                },
                ModelArtifact {
                    filename: "b".to_string(),
                    url: "https://example.invalid/b".to_string(),
                    revision: "0123456789abcdef".to_string(),
                    sha256: "1".repeat(64),
                    size_bytes: 5,
                },
            ],
        };
        assert_eq!(package.download_size_bytes().unwrap(), 8);
        assert_eq!(package.required_disk_bytes().unwrap(), 16);
    }
}
