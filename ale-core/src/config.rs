use crate::secret_store::{SecretStore, SystemSecretStore};
use crate::{AleError, Result};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub(crate) fn atomic_config_write(path: &Path, content: &[u8]) -> Result<()> {
    crate::diagnostics::record(
        "atomic_file_write_start",
        &[("bytes", content.len() as u64)],
    );
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(content)?;
        crate::diagnostics::record("atomic_file_sync_start", &[]);
        file.sync_all()?;
        crate::diagnostics::record("atomic_file_sync_done", &[]);
        drop(file);
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    crate::diagnostics::record(
        "atomic_file_write_done",
        &[("success", result.is_ok() as u64)],
    );
    result
}

/// 云端API配置。
///
/// `provider` 只是 UI 预设名：已知预设会通过 [`CloudApiConfig::effective_wire_api`]
/// 映射到唯一的传输协议；`custom`（及其它/空字符串）使用显式 `wire_api`。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CloudApiConfig {
    pub wire_api: crate::model_api::WireApi,
    pub provider: String,
    #[serde(skip_serializing, default)]
    pub api_key: String,
    pub api_url: String,
    pub model: String,
    pub max_tokens: usize,
    pub timeout: u32,
}

impl CloudApiConfig {
    /// 已知 provider 预设对应的传输协议。
    pub fn preset_wire_api(provider: &str) -> Option<crate::model_api::WireApi> {
        match provider.trim().to_ascii_lowercase().as_str() {
            "openai" => Some(crate::model_api::WireApi::OpenaiChatCompletions),
            "anthropic" => Some(crate::model_api::WireApi::AnthropicMessages),
            "google" => Some(crate::model_api::WireApi::GoogleGenerateContent),
            _ => None,
        }
    }

    /// 真正用于网络传输的协议。
    ///
    /// 预设 provider 始终映射到固定协议；`custom`/其它/空 provider 使用显式
    /// `wire_api` 字段（默认 OpenAI Chat Completions）。
    pub fn effective_wire_api(&self) -> crate::model_api::WireApi {
        Self::preset_wire_api(&self.provider).unwrap_or(self.wire_api)
    }
}

impl Default for CloudApiConfig {
    fn default() -> Self {
        Self {
            wire_api: Default::default(),
            provider: "openai".to_string(),
            api_key: String::new(),
            api_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o".to_string(),
            max_tokens: 1024,
            timeout: 30,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct RemoteRoutingConfig {
    pub backup: Option<CloudApiConfig>,
    pub backup_enabled: bool,
    pub backup_pre_authorized: bool,
    pub circuit_failure_threshold: u32,
    pub circuit_open_seconds: u32,
}

impl Default for RemoteRoutingConfig {
    fn default() -> Self {
        Self {
            backup: None,
            backup_enabled: false,
            backup_pre_authorized: false,
            circuit_failure_threshold: 3,
            circuit_open_seconds: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelSchedulerConfig {
    pub enabled: bool,
    pub require_download_consent: bool,
    pub local_vlm_gpu_only: bool,
    pub idle_ttl_seconds: u32,
    pub minimum_default_vram_bytes: u64,
    pub minimum_large_vram_bytes: u64,
    pub qwen_model: String,
    pub qwen_large_model: String,
    pub grounding_model: String,
    pub full_screenshot_requires_confirmation: bool,
}

impl Default for ModelSchedulerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            require_download_consent: true,
            local_vlm_gpu_only: true,
            idle_ttl_seconds: 120,
            minimum_default_vram_bytes: crate::model_scheduler::MIN_DEFAULT_VRAM_BYTES,
            minimum_large_vram_bytes: crate::model_scheduler::MIN_LARGE_VRAM_BYTES,
            // 下载器/GGUF 准备脚本只交付 7B；默认档也使用 7B，避免默认配置指向
            // 任何获取路径都不存在的 3B 模型（DC-04）。
            qwen_model: "Qwen2.5-VL-7B-Instruct".to_string(),
            qwen_large_model: "Qwen2.5-VL-7B-Instruct".to_string(),
            grounding_model: "ShowUI-2B".to_string(),
            full_screenshot_requires_confirmation: true,
        }
    }
}

/// 模型配置（本地模型只由 `ale-modeld` 管理，这里只保留模型目录）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelsConfig {
    pub models_dir: String,
}

impl Default for ModelsConfig {
    fn default() -> Self {
        Self {
            models_dir: "models".to_string(),
        }
    }
}

/// 推理配置（本地推理由 `ale-modeld` 提供，这里只保留模式）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct InferenceConfig {
    pub mode: String, // "local", "cloud", "adaptive"
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self {
            mode: "adaptive".to_string(),
        }
    }
}

/// 界面配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub language: String,
    pub theme: String,
    pub font_size: u32,
    pub high_contrast: bool,
    pub screen_reader: bool,
    pub auto_speak: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            language: "zh-CN".to_string(),
            theme: "system".to_string(),
            font_size: 16,
            high_contrast: false,
            screen_reader: true,
            auto_speak: true,
        }
    }
}

/// 应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TranscriptionConfig {
    pub enabled: bool,
    pub endpoint: CloudApiConfig,
}
impl Default for TranscriptionConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: CloudApiConfig {
                model: "whisper-1".into(),
                ..Default::default()
            },
        }
    }
}

/// 已验证任务（harness）配置：默认开启，但只在平台无障碍接口可用时生效。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifiedTasksConfig {
    pub enabled: bool,
    /// 宿主显式启用“观察驱动重规划”（映射到 `Task.allow_observation_steps`）。
    /// 默认 `false`：固定步骤的生产流程保持严格模式；只有能够处理空后置条件
    /// `Invoke` 并基于新观察重新决策的宿主才应打开。
    pub allow_observation_steps: bool,
}

impl Default for VerifiedTasksConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_observation_steps: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    pub transcription: TranscriptionConfig,
    pub cloud_api: CloudApiConfig,
    pub remote_routing: RemoteRoutingConfig,
    pub model_scheduler: ModelSchedulerConfig,
    pub models: ModelsConfig,
    pub inference: InferenceConfig,
    pub verified_tasks: VerifiedTasksConfig,
    pub ui: UiConfig,
}

/// 配置管理器
#[derive(Clone)]
pub struct ConfigManager {
    config_path: PathBuf,
    config: AppConfig,
    secret_store: Arc<dyn SecretStore>,
    /// 凭据后端不可用等非致命问题；配置仍已加载，宿主可展示给用户。
    credential_warnings: Vec<String>,
}

impl ConfigManager {
    pub fn new(config_path: &Path) -> Self {
        Self::with_secret_store(config_path, Arc::new(SystemSecretStore))
    }

    pub fn with_secret_store(config_path: &Path, secret_store: Arc<dyn SecretStore>) -> Self {
        Self {
            config_path: config_path.to_path_buf(),
            config: AppConfig::default(),
            secret_store,
            credential_warnings: Vec::new(),
        }
    }

    /// 凭据后端不可用等非致命警告；加载成功后仍然可用（只读命令可运行）。
    pub fn credential_warnings(&self) -> &[String] {
        &self.credential_warnings
    }

    fn push_credential_warning(&mut self, message: impl Into<String>) {
        let message = message.into();
        crate::diagnostics::record("credential_backend_unavailable", &[]);
        tracing::warn!("{message}");
        self.credential_warnings.push(message);
    }

    /// custom/未知 provider 必须显式给出 `wire_api`（DC-07）。
    ///
    /// 只有在配置文件中缺省该字段时才发现“未显式”，因此校验发生在原始 JSON 上；
    /// 进程内构造的配置总会序列化出 `wire_api`，不在此强制。
    fn require_explicit_wire_api(raw: &serde_json::Value) -> Result<()> {
        fn check(endpoint: Option<&serde_json::Value>, label: &str) -> Result<()> {
            let Some(endpoint) = endpoint.filter(|value| value.is_object()) else {
                return Ok(());
            };
            let Some(provider) = endpoint.get("provider").and_then(serde_json::Value::as_str)
            else {
                return Ok(());
            };
            if CloudApiConfig::preset_wire_api(provider).is_none()
                && endpoint.get("wire_api").is_none()
            {
                return Err(AleError::ConfigError(format!(
                    "{label} uses custom/unknown provider '{}' and must set wire_api explicitly",
                    provider.trim()
                )));
            }
            Ok(())
        }

        check(raw.get("cloud_api"), "cloud_api")?;
        check(
            raw.get("remote_routing")
                .and_then(|value| value.get("backup")),
            "remote_routing.backup",
        )?;
        check(
            raw.get("transcription")
                .and_then(|value| value.get("endpoint")),
            "transcription.endpoint",
        )?;
        Ok(())
    }

    /// 加载配置
    pub fn load(&mut self) -> Result<()> {
        crate::diagnostics::record("config_load_start", &[]);
        self.credential_warnings.clear();
        if !self.config_path.exists() {
            // 如果配置文件不存在，创建默认配置
            self.save()?;
            return Ok(());
        }

        let content = std::fs::read_to_string(&self.config_path)?;
        let raw: serde_json::Value = serde_json::from_str(&content)?;
        Self::require_explicit_wire_api(&raw)?;
        let legacy_transcription = raw.get("transcription").is_none();
        let mut config: AppConfig = serde_json::from_str(&content)?;
        // headless/容器/无 D-Bus 环境没有系统凭据库：配置仍应加载成功，只读
        // 命令继续可用；凭据读取降级为警告（DC-03）。
        let backend_available = self.secret_store.backend_available();
        if backend_available {
            if config.cloud_api.api_key.trim().is_empty() {
                config.cloud_api.api_key = self.secret_store.get_api_key()?.unwrap_or_default();
            }
            if let Some(backup) = config.remote_routing.backup.as_mut() {
                if backup.api_key.trim().is_empty() {
                    backup.api_key = self.secret_store.get_backup_api_key()?.unwrap_or_default();
                }
            }
            // 只有启用的转写功能才需要独立凭据；关闭时不应因为凭据后端而失败。
            if config.transcription.enabled && config.transcription.endpoint.api_key.is_empty() {
                config.transcription.endpoint.api_key = self
                    .secret_store
                    .get_transcription_api_key()?
                    .unwrap_or_default();
            }
        }
        if legacy_transcription {
            config.transcription = TranscriptionConfig {
                enabled: !config.cloud_api.api_key.is_empty(),
                endpoint: CloudApiConfig {
                    model: "whisper-1".into(),
                    provider: "openai".into(),
                    wire_api: Default::default(),
                    ..config.cloud_api.clone()
                },
            };
        }
        if !backend_available {
            self.push_credential_warning(
                "系统凭据库不可用（例如 headless/无 D-Bus），已跳过凭据读取；配置仍可使用，云端凭据需要在可用的凭据后端中配置",
            );
        }
        Self::normalize_legacy_wire_api(&mut config, &raw);
        self.update_config(config)
    }

    /// 旧配置没有显式 `wire_api` 时，用 provider 预设的协议补齐。
    fn normalize_legacy_wire_api(config: &mut AppConfig, raw: &serde_json::Value) {
        fn missing(endpoint: Option<&serde_json::Value>) -> bool {
            endpoint.and_then(|value| value.get("wire_api")).is_none()
        }

        if missing(raw.get("cloud_api")) {
            config.cloud_api.wire_api = config.cloud_api.effective_wire_api();
        }
        if missing(
            raw.get("remote_routing")
                .and_then(|value| value.get("backup")),
        ) {
            if let Some(backup) = config.remote_routing.backup.as_mut() {
                backup.wire_api = backup.effective_wire_api();
            }
        }
        if missing(
            raw.get("transcription")
                .and_then(|value| value.get("endpoint")),
        ) {
            config.transcription.endpoint.wire_api =
                config.transcription.endpoint.effective_wire_api();
        }
    }

    /// 保存配置
    pub fn save(&self) -> Result<()> {
        ConfigValidator::validate_model_endpoints(&self.config)?;
        // 确保目录存在
        if let Some(parent) = self.config_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let content = serde_json::to_string_pretty(&self.config)?;
        atomic_config_write(&self.config_path, content.as_bytes())?;
        Ok(())
    }

    /// 获取配置
    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    /// 更新配置
    pub fn update_config(&mut self, config: AppConfig) -> Result<()> {
        ConfigValidator::validate_model_endpoints(&config)?;
        // 凭据后端不可用时仍允许保存配置本体（只读/无凭据场景可用），
        // 只是凭据无法落盘并给出警告（DC-03）。
        let backend_available = self.secret_store.backend_available();
        crate::diagnostics::record("credentials_read_start", &[]);
        let old_keys = if backend_available {
            (
                self.secret_store.get_api_key()?,
                self.secret_store.get_backup_api_key()?,
                self.secret_store.get_transcription_api_key()?,
            )
        } else {
            (None, None, None)
        };
        crate::diagnostics::record("credentials_read_done", &[]);
        let mut degraded = false;
        let result = (|| -> Result<()> {
            if backend_available {
                crate::diagnostics::record("credentials_write_start", &[]);
                if config.cloud_api.api_key.trim().is_empty() {
                    self.secret_store.delete_api_key()?;
                } else {
                    self.secret_store.set_api_key(&config.cloud_api.api_key)?;
                }
                match config.remote_routing.backup.as_ref() {
                    Some(backup) if !backup.api_key.trim().is_empty() => {
                        self.secret_store.set_backup_api_key(&backup.api_key)?;
                    }
                    _ => self.secret_store.delete_backup_api_key()?,
                }
                if config.transcription.endpoint.api_key.is_empty() {
                    self.secret_store.delete_transcription_api_key()?;
                } else {
                    self.secret_store
                        .set_transcription_api_key(&config.transcription.endpoint.api_key)?;
                }
                crate::diagnostics::record("credentials_write_done", &[]);
            } else {
                degraded = true;
            }
            atomic_config_write(&self.config_path, &serde_json::to_vec_pretty(&config)?)?;
            Ok(())
        })();
        if let Err(error) = result {
            if backend_available {
                crate::diagnostics::record("credentials_rollback_start", &[]);
                let rollback = [
                    match old_keys.0 {
                        Some(key) => self.secret_store.set_api_key(&key),
                        None => self.secret_store.delete_api_key(),
                    },
                    match old_keys.1 {
                        Some(key) => self.secret_store.set_backup_api_key(&key),
                        None => self.secret_store.delete_backup_api_key(),
                    },
                    match old_keys.2 {
                        Some(key) => self.secret_store.set_transcription_api_key(&key),
                        None => self.secret_store.delete_transcription_api_key(),
                    },
                ];
                crate::diagnostics::record(
                    "credentials_rollback_done",
                    &[("success", rollback.iter().all(Result::is_ok) as u64)],
                );
                if rollback.iter().any(Result::is_err) {
                    return Err(AleError::ConfigError("Save failed; credential rollback also failed. Re-enter credentials before retrying.".into()));
                }
            }
            return Err(error);
        }
        if degraded {
            self.push_credential_warning(
                "系统凭据库不可用，API Key 未写入系统凭据库；本次会话内存中的配置仍生效",
            );
        }
        self.config = config;
        crate::diagnostics::record("config_commit_done", &[]);
        Ok(())
    }

    /// 获取配置文件路径
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }
}

/// 配置工厂
pub struct ConfigFactory;

impl ConfigFactory {
    /// 创建默认配置管理器
    pub fn create_default() -> ConfigManager {
        let config_dir = dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ale-my-eyes");

        let config_path = config_dir.join("config.json");
        ConfigManager::new(&config_path)
    }
}

/// 配置验证器
pub struct ConfigValidator;

impl ConfigValidator {
    pub fn validate_model_endpoints(config: &AppConfig) -> Result<()> {
        // 主规划端点始终启用；备用/转写只在启用时才校验，避免关闭功能后
        // 残留的非法 URL 阻止整个配置保存（DC-14）。
        Self::validate_endpoint(&config.cloud_api)?;
        if config.remote_routing.backup_enabled {
            let backup = config
                .remote_routing
                .backup
                .as_ref()
                .ok_or_else(|| AleError::ConfigError("Backup endpoint is required".into()))?;
            Self::validate_endpoint(backup)?;
        }
        if config.transcription.enabled {
            let endpoint = &config.transcription.endpoint;
            Self::validate_endpoint(endpoint)?;
            // 凭据是否存在属于运行时问题：headless 无 keyring 时加载会跳过凭据读取，
            // 不能因此让整个配置加载/保存失败（DC-03）。缺失凭据由 transcribe 明确报错。
            if !endpoint.api_key.trim().is_empty() {
                Self::validate_cloud_api(endpoint)?;
            }
            // `/audio/transcriptions` 只存在于 OpenAI Chat Completions 风格
            // 端点；Responses-only 端点会 404（DC-05）。
            if !matches!(
                endpoint.effective_wire_api(),
                crate::model_api::WireApi::OpenaiChatCompletions
            ) {
                return Err(AleError::ConfigError(
                    "Transcription requires an OpenAI Chat Completions endpoint (/audio/transcriptions)".into(),
                ));
            }
        }
        if !config.cloud_api.api_key.trim().is_empty() {
            Self::validate_cloud_api(&config.cloud_api)?;
        }
        if config.remote_routing.backup_enabled && !config.remote_routing.backup_pre_authorized {
            return Err(AleError::ConfigError(
                "Backup endpoint requires explicit authorization".into(),
            ));
        }
        if config.remote_routing.backup_enabled {
            let backup = config
                .remote_routing
                .backup
                .as_ref()
                .ok_or_else(|| AleError::ConfigError("Backup endpoint is required".into()))?;
            // 与主端点一致：凭据缺失是运行时问题，不阻止 headless 加载/保存。
            if !backup.api_key.trim().is_empty() {
                Self::validate_cloud_api(backup)?;
            }
        }
        Ok(())
    }

    /// 单个端点的传输层校验：URL、provider/wire_api 预设一致性、超时与长度。
    fn validate_endpoint(endpoint: &CloudApiConfig) -> Result<()> {
        Self::validate_cloud_api_transport(&endpoint.api_url)?;
        Self::validate_cloud_api_preset(endpoint)?;
        if endpoint.timeout == 0 || endpoint.timeout > 80 || endpoint.max_tokens == 0 {
            return Err(AleError::ConfigError(
                "Timeout must be 1-80 seconds and token limit must be positive".into(),
            ));
        }
        Ok(())
    }

    /// 已知 provider 预设必须使用其固定协议；要自定义协议请把 provider 设为 `custom`。
    pub fn validate_cloud_api_preset(config: &CloudApiConfig) -> Result<()> {
        if let Some(expected) = CloudApiConfig::preset_wire_api(&config.provider) {
            if config.wire_api != expected {
                return Err(AleError::ConfigError(format!(
                    "Provider preset '{}' requires wire_api '{}' (got '{}'); choose provider 'custom' to use a different protocol",
                    config.provider.trim(),
                    expected.id(),
                    config.wire_api.id()
                )));
            }
        }
        Ok(())
    }

    pub fn validate_cloud_api_transport(api_url: &str) -> Result<()> {
        let parsed = url::Url::parse(api_url)
            .map_err(|error| AleError::ConfigError(format!("Invalid API URL: {error}")))?;
        if parsed.host().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
        {
            return Err(AleError::ConfigError(
                "Use a base API URL without credentials, query parameters, or fragment".into(),
            ));
        }
        match parsed.scheme() {
            "https" => Ok(()),
            "http" => {
                let loopback = match parsed.host() {
                    Some(url::Host::Domain(host)) => host.eq_ignore_ascii_case("localhost"),
                    Some(url::Host::Ipv4(address)) => address.is_loopback(),
                    Some(url::Host::Ipv6(address)) => address.is_loopback(),
                    None => false,
                };
                if loopback {
                    Ok(())
                } else {
                    Err(AleError::ConfigError(
                        "HTTP API URLs are allowed only for localhost or loopback addresses"
                            .to_string(),
                    ))
                }
            }
            _ => Err(AleError::ConfigError(
                "API URL must use HTTPS (HTTP is limited to loopback)".to_string(),
            )),
        }
    }

    /// 验证云端API配置
    pub fn validate_cloud_api(config: &CloudApiConfig) -> Result<()> {
        if config.api_key.is_empty() {
            return Err(AleError::ConfigError("API key is required".to_string()));
        }

        if config.api_url.is_empty() {
            return Err(AleError::ConfigError("API URL is required".to_string()));
        }

        Self::validate_cloud_api_transport(&config.api_url)?;

        if config.model.is_empty() {
            return Err(AleError::ConfigError("Model name is required".to_string()));
        }

        if config.timeout == 0 || config.timeout > 80 {
            return Err(AleError::ConfigError(
                "Timeout must be between 1 and 80 seconds".to_string(),
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct TestSecretStore(
        Mutex<Option<String>>,
        Mutex<Option<String>>,
        Mutex<Option<String>>,
    );

    impl SecretStore for TestSecretStore {
        fn get_backup_api_key(&self) -> Result<Option<String>> {
            Ok(self.1.lock().unwrap().clone())
        }
        fn set_backup_api_key(&self, key: &str) -> Result<()> {
            *self.1.lock().unwrap() = Some(key.into());
            Ok(())
        }
        fn delete_backup_api_key(&self) -> Result<()> {
            *self.1.lock().unwrap() = None;
            Ok(())
        }
        fn get_transcription_api_key(&self) -> Result<Option<String>> {
            Ok(self.2.lock().unwrap().clone())
        }
        fn set_transcription_api_key(&self, key: &str) -> Result<()> {
            *self.2.lock().unwrap() = Some(key.into());
            Ok(())
        }
        fn delete_transcription_api_key(&self) -> Result<()> {
            *self.2.lock().unwrap() = None;
            Ok(())
        }
        fn get_api_key(&self) -> Result<Option<String>> {
            Ok(self.0.lock().unwrap().clone())
        }

        fn set_api_key(&self, api_key: &str) -> Result<()> {
            *self.0.lock().unwrap() = Some(api_key.to_string());
            Ok(())
        }

        fn delete_api_key(&self) -> Result<()> {
            *self.0.lock().unwrap() = None;
            Ok(())
        }
    }

    fn temp_dir(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!("ale-{label}-{}", uuid::Uuid::new_v4()))
    }

    /// 模拟 headless/无 D-Bus 环境：凭据后端不可用，任何读写都失败。
    #[derive(Default)]
    struct UnavailableSecretStore {
        reads: std::sync::atomic::AtomicUsize,
        writes: std::sync::atomic::AtomicUsize,
    }

    impl UnavailableSecretStore {
        fn fail<T>(&self, counter: &std::sync::atomic::AtomicUsize) -> Result<T> {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(AleError::ConfigError(
                "Platform failure: no D-Bus session".into(),
            ))
        }
    }

    impl SecretStore for UnavailableSecretStore {
        fn backend_available(&self) -> bool {
            false
        }
        fn get_transcription_api_key(&self) -> Result<Option<String>> {
            self.fail(&self.reads)
        }
        fn set_transcription_api_key(&self, _key: &str) -> Result<()> {
            self.fail(&self.writes)
        }
        fn delete_transcription_api_key(&self) -> Result<()> {
            self.fail(&self.writes)
        }
        fn get_api_key(&self) -> Result<Option<String>> {
            self.fail(&self.reads)
        }
        fn set_api_key(&self, _api_key: &str) -> Result<()> {
            self.fail(&self.writes)
        }
        fn delete_api_key(&self) -> Result<()> {
            self.fail(&self.writes)
        }
        fn get_backup_api_key(&self) -> Result<Option<String>> {
            self.fail(&self.reads)
        }
        fn set_backup_api_key(&self, _api_key: &str) -> Result<()> {
            self.fail(&self.writes)
        }
        fn delete_backup_api_key(&self) -> Result<()> {
            self.fail(&self.writes)
        }
    }

    #[test]
    fn headless_config_load_and_save_degrade_without_credential_backend() {
        let dir = temp_dir("headless");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{
  "cloud_api": {"provider": "openai", "api_url": "https://api.openai.com/v1", "model": "gpt-4o", "max_tokens": 1024, "timeout": 30},
  "transcription": {"enabled": true, "endpoint": {"provider": "openai", "api_url": "https://api.openai.com/v1", "model": "whisper-1", "max_tokens": 1024, "timeout": 30}}
}"#,
        )
        .unwrap();
        let store = Arc::new(UnavailableSecretStore::default());
        let mut manager = ConfigManager::with_secret_store(&path, store.clone());
        // 配置加载不再因为凭据后端不可用而失败（只读 CLI 命令可运行）。
        manager.load().unwrap();
        assert_eq!(
            store.reads.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "unavailable backend must not be queried"
        );
        assert!(!manager.credential_warnings().is_empty());
        assert!(manager.config().cloud_api.api_key.is_empty());

        let mut updated = manager.config().clone();
        updated.ui.font_size = 18;
        manager.update_config(updated).unwrap();
        assert_eq!(
            store.writes.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "unavailable backend must not be written"
        );
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("\"font_size\": 18"));

        let mut reloaded = ConfigManager::with_secret_store(&path, store.clone());
        reloaded.load().unwrap();
        assert_eq!(reloaded.config().ui.font_size, 18);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn transcription_keyring_is_only_read_when_enabled() {
        let dir = temp_dir("transcription-gate");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let config_json = |enabled: bool| {
            format!(
                r#"{{
  "cloud_api": {{"provider": "openai", "api_url": "https://api.openai.com/v1", "model": "gpt-4o", "max_tokens": 1024, "timeout": 30}},
  "transcription": {{"enabled": {enabled}, "endpoint": {{"provider": "openai", "api_url": "https://api.openai.com/v1", "model": "whisper-1", "max_tokens": 1024, "timeout": 30}}}}
}}"#
            )
        };
        std::fs::write(&path, config_json(false)).unwrap();
        let store = Arc::new(TestSecretStore::default());
        let mut manager = ConfigManager::with_secret_store(&path, store.clone());
        manager.load().unwrap();
        assert!(manager.config().transcription.endpoint.api_key.is_empty());

        // 关闭转写时既不读取、也不因加载而删除已有凭据；设置后再开启才会读取。
        store.set_transcription_api_key("asr-key").unwrap();
        std::fs::write(&path, config_json(true)).unwrap();
        let mut enabled = ConfigManager::with_secret_store(&path, store);
        enabled.load().unwrap();
        assert_eq!(enabled.config().transcription.endpoint.api_key, "asr-key");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_default_config() {
        let config = AppConfig::default();
        assert_eq!(config.cloud_api.provider, "openai");
        assert_eq!(config.cloud_api.api_url, "https://api.openai.com/v1");
        assert_eq!(config.cloud_api.model, "gpt-4o");
        assert_eq!(config.cloud_api.max_tokens, 1024);
        assert_eq!(config.cloud_api.timeout, 30);
        assert_eq!(config.ui.language, "zh-CN");
        assert_eq!(config.ui.font_size, 16);
        assert!(!config.ui.high_contrast);
        assert!(config.verified_tasks.enabled);
        assert_eq!(config.models.models_dir, "models");
    }

    #[test]
    fn transcription_migrates_once_and_credentials_remain_independent() {
        let dir = temp_dir("migration");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"cloud_api":{"api_key":"legacy-key","api_url":"https://example.test/v1","model":"vision"}}"#).unwrap();
        let store = Arc::new(TestSecretStore::default());
        let mut manager = ConfigManager::with_secret_store(&path, store.clone());
        manager.load().unwrap();
        assert!(manager.config().transcription.enabled);
        assert_eq!(manager.config().transcription.endpoint.model, "whisper-1");
        assert_eq!(manager.config().transcription.endpoint.provider, "openai");
        assert_eq!(
            store.get_transcription_api_key().unwrap().as_deref(),
            Some("legacy-key")
        );
        let mut updated = manager.config().clone();
        updated.cloud_api.api_key = "primary-secret-unique".into();
        updated.cloud_api.api_url = "https://new-primary.test/v1".into();
        updated.cloud_api.provider = "anthropic".into();
        updated.cloud_api.wire_api = crate::model_api::WireApi::AnthropicMessages;
        updated.remote_routing.backup = Some(CloudApiConfig {
            api_key: "backup-key".into(),
            provider: "custom".into(),
            wire_api: crate::model_api::WireApi::OpenaiResponses,
            ..Default::default()
        });
        manager.update_config(updated).unwrap();
        let mut reloaded = ConfigManager::with_secret_store(&path, store.clone());
        reloaded.load().unwrap();
        assert_eq!(reloaded.config().cloud_api.api_key, "primary-secret-unique");
        assert_eq!(
            reloaded.config().transcription.endpoint.api_key,
            "legacy-key"
        );
        assert_eq!(
            reloaded.config().transcription.endpoint.api_url,
            "https://example.test/v1"
        );
        assert_eq!(
            reloaded
                .config()
                .remote_routing
                .backup
                .as_ref()
                .unwrap()
                .api_key,
            "backup-key"
        );
        let contents = std::fs::read_to_string(&path).unwrap();
        for key in ["legacy-key", "primary-secret-unique", "backup-key"] {
            assert!(!contents.contains(key));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_atomic_save_rolls_back_all_credentials() {
        let dir = temp_dir("rollback");
        let path = dir.join("config.json");
        let store = Arc::new(TestSecretStore::default());
        let mut manager = ConfigManager::with_secret_store(&path, store.clone());
        let mut original = AppConfig::default();
        original.cloud_api.api_key = "primary-old".into();
        original.remote_routing.backup = Some(CloudApiConfig {
            api_key: "backup-old".into(),
            ..Default::default()
        });
        original.transcription.endpoint.api_key = "asr-old".into();
        manager.update_config(original.clone()).unwrap();
        let saved = std::fs::read(&path).unwrap();
        manager.config_path = dir.join("blocked");
        std::fs::create_dir_all(&manager.config_path).unwrap();
        let mut changed = original;
        changed.cloud_api.api_key = "primary-new".into();
        changed.remote_routing.backup.as_mut().unwrap().api_key = "backup-new".into();
        changed.transcription.endpoint.api_key = "asr-new".into();
        assert!(manager.update_config(changed).is_err());
        assert_eq!(manager.config().cloud_api.api_key, "primary-old");
        assert_eq!(store.get_api_key().unwrap().as_deref(), Some("primary-old"));
        assert_eq!(
            store.get_backup_api_key().unwrap().as_deref(),
            Some("backup-old")
        );
        assert_eq!(
            store.get_transcription_api_key().unwrap().as_deref(),
            Some("asr-old")
        );
        assert_eq!(std::fs::read(path).unwrap(), saved);
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            2,
            "temporary file leaked"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn model_endpoint_policy_requires_valid_timeouts_and_backup_consent() {
        let mut config = AppConfig::default();
        assert!(!config.transcription.enabled);
        assert!(!config.remote_routing.backup_enabled);
        config.cloud_api.timeout = 81;
        assert!(ConfigValidator::validate_model_endpoints(&config).is_err());
        config.cloud_api.timeout = 60;
        config.remote_routing.backup_enabled = true;
        config.remote_routing.backup = Some(CloudApiConfig {
            api_key: "test".into(),
            ..Default::default()
        });
        assert!(ConfigValidator::validate_model_endpoints(&config).is_err());
        config.remote_routing.backup_pre_authorized = true;
        assert!(ConfigValidator::validate_model_endpoints(&config).is_ok());
    }

    #[test]
    fn test_config_serialization_roundtrip() {
        let config = AppConfig::default();
        let json = serde_json::to_string_pretty(&config).unwrap();
        let restored: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.cloud_api.provider, config.cloud_api.provider);
        assert_eq!(restored.cloud_api.api_url, config.cloud_api.api_url);
        assert_eq!(restored.ui.language, config.ui.language);
        assert_eq!(
            restored.verified_tasks.enabled,
            config.verified_tasks.enabled
        );
    }

    #[test]
    fn verified_tasks_default_to_enabled_for_old_configs() {
        let config: AppConfig =
            serde_json::from_str(r#"{"cloud_api":{"api_key":"sk-test"}}"#).unwrap();
        assert!(config.verified_tasks.enabled);
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"verified_tasks\""));
        assert!(json.contains("\"enabled\":true"));

        let disabled: AppConfig =
            serde_json::from_str(r#"{"verified_tasks":{"enabled":false}}"#).unwrap();
        assert!(!disabled.verified_tasks.enabled);
        let restored: AppConfig =
            serde_json::from_str(&serde_json::to_string(&disabled).unwrap()).unwrap();
        assert!(!restored.verified_tasks.enabled);
    }

    #[test]
    fn provider_presets_map_to_effective_wire_api() {
        for (provider, expected) in [
            ("openai", crate::model_api::WireApi::OpenaiChatCompletions),
            ("Anthropic", crate::model_api::WireApi::AnthropicMessages),
            (" google ", crate::model_api::WireApi::GoogleGenerateContent),
        ] {
            let config = CloudApiConfig {
                provider: provider.into(),
                wire_api: crate::model_api::WireApi::OpenaiResponses,
                ..Default::default()
            };
            assert_eq!(config.effective_wire_api(), expected);
        }
    }

    #[test]
    fn custom_provider_keeps_explicit_wire_api() {
        for provider in ["custom", "azure", "", "my-provider"] {
            let config = CloudApiConfig {
                provider: provider.into(),
                wire_api: crate::model_api::WireApi::GoogleGenerateContent,
                ..Default::default()
            };
            assert_eq!(
                config.effective_wire_api(),
                crate::model_api::WireApi::GoogleGenerateContent
            );
        }
    }

    #[test]
    fn preset_wire_api_conflict_is_rejected() {
        let mut config = AppConfig::default();
        config.cloud_api.provider = "anthropic".into();
        config.cloud_api.wire_api = crate::model_api::WireApi::OpenaiChatCompletions;
        let error = ConfigValidator::validate_model_endpoints(&config)
            .unwrap_err()
            .to_string();
        assert!(error.contains("anthropic_messages"), "{error}");

        config.cloud_api.provider = "custom".into();
        assert!(ConfigValidator::validate_model_endpoints(&config).is_ok());
    }

    #[test]
    fn backup_endpoint_preset_conflict_is_rejected_only_when_enabled() {
        let mut config = AppConfig::default();
        config.remote_routing.backup = Some(CloudApiConfig {
            provider: "google".into(),
            wire_api: crate::model_api::WireApi::OpenaiChatCompletions,
            api_key: "test".into(),
            ..Default::default()
        });
        // 备用端点关闭时，残留的冲突配置不应阻止保存（DC-14）。
        assert!(ConfigValidator::validate_model_endpoints(&config).is_ok());
        config.remote_routing.backup_enabled = true;
        config.remote_routing.backup_pre_authorized = true;
        assert!(ConfigValidator::validate_model_endpoints(&config).is_err());
    }

    #[test]
    fn disabled_endpoints_are_not_validated() {
        let mut config = AppConfig::default();
        config.transcription.enabled = false;
        config.transcription.endpoint.api_url = "http://public.example.com/v1".into();
        config.transcription.endpoint.provider = "anthropic".into();
        config.transcription.endpoint.wire_api = crate::model_api::WireApi::OpenaiChatCompletions;
        config.remote_routing.backup = Some(CloudApiConfig {
            api_url: "not-a-url".into(),
            ..Default::default()
        });
        assert!(ConfigValidator::validate_model_endpoints(&config).is_ok());

        config.transcription.enabled = true;
        assert!(ConfigValidator::validate_model_endpoints(&config).is_err());
    }

    #[test]
    fn transcription_requires_openai_chat_completions_endpoint() {
        let mut config = AppConfig::default();
        config.transcription.enabled = true;
        config.transcription.endpoint.api_key = "sk-asr".into();
        config.transcription.endpoint.provider = "custom".into();
        config.transcription.endpoint.wire_api = crate::model_api::WireApi::OpenaiResponses;
        let error = ConfigValidator::validate_model_endpoints(&config)
            .unwrap_err()
            .to_string();
        assert!(error.contains("audio/transcriptions"), "{error}");
        config.transcription.endpoint.wire_api = crate::model_api::WireApi::OpenaiChatCompletions;
        assert!(ConfigValidator::validate_model_endpoints(&config).is_ok());
    }

    #[test]
    fn default_qwen_model_has_a_delivered_download_path() {
        assert_eq!(
            AppConfig::default().model_scheduler.qwen_model,
            "Qwen2.5-VL-7B-Instruct"
        );
    }

    #[test]
    fn verified_tasks_observation_steps_are_opt_in() {
        let config = AppConfig::default();
        assert!(config.verified_tasks.enabled);
        assert!(!config.verified_tasks.allow_observation_steps);
        let restored: AppConfig = serde_json::from_str(
            r#"{"verified_tasks":{"enabled":true,"allow_observation_steps":true}}"#,
        )
        .unwrap();
        assert!(restored.verified_tasks.allow_observation_steps);
        let legacy: AppConfig =
            serde_json::from_str(r#"{"verified_tasks":{"enabled":true}}"#).unwrap();
        assert!(!legacy.verified_tasks.allow_observation_steps);
    }

    #[test]
    fn custom_provider_requires_explicit_wire_api_in_config_file() {
        let dir = temp_dir("explicit-custom");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"cloud_api":{"provider":"my-provider","api_key":"sk","api_url":"https://example.test/v1","model":"m","max_tokens":1024,"timeout":30}}"#,
        )
        .unwrap();
        let mut manager =
            ConfigManager::with_secret_store(&path, Arc::new(TestSecretStore::default()));
        let error = manager.load().unwrap_err().to_string();
        assert!(error.contains("wire_api"), "{error}");

        std::fs::write(
            &path,
            r#"{"cloud_api":{"provider":"my-provider","wire_api":"openai_responses","api_key":"sk","api_url":"https://example.test/v1","model":"m","max_tokens":1024,"timeout":30}}"#,
        )
        .unwrap();
        manager.load().unwrap();
        assert_eq!(
            manager.config().cloud_api.wire_api,
            crate::model_api::WireApi::OpenaiResponses
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_config_without_wire_api_is_normalized_from_provider() {
        let dir = temp_dir("normalize");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{
  "cloud_api": {"provider": "anthropic", "api_key": "sk-test", "api_url": "https://api.anthropic.test/v1", "model": "claude", "max_tokens": 1024, "timeout": 30},
  "inference": {"mode": "adaptive"},
  "ui": {"language": "zh-CN"}
}"#,
        )
        .unwrap();
        let mut manager =
            ConfigManager::with_secret_store(&path, Arc::new(TestSecretStore::default()));
        manager.load().unwrap();
        assert_eq!(
            manager.config().cloud_api.wire_api,
            crate::model_api::WireApi::AnthropicMessages
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn explicit_conflicting_wire_api_is_not_silently_normalized() {
        let dir = temp_dir("explicit-conflict");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{
  "cloud_api": {"provider": "anthropic", "wire_api": "openai_chat_completions", "api_key": "sk-test", "api_url": "https://api.anthropic.test/v1", "model": "claude", "max_tokens": 1024, "timeout": 30}
}"#,
        )
        .unwrap();
        let mut manager =
            ConfigManager::with_secret_store(&path, Arc::new(TestSecretStore::default()));
        assert!(manager.load().is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_validate_cloud_api_missing_key() {
        let config = CloudApiConfig {
            api_key: String::new(),
            ..Default::default()
        };
        assert!(ConfigValidator::validate_cloud_api(&config).is_err());
    }

    #[test]
    fn test_validate_cloud_api_bad_url() {
        let config = CloudApiConfig {
            api_key: "sk-test".to_string(),
            api_url: "not-a-url".to_string(),
            ..Default::default()
        };
        assert!(ConfigValidator::validate_cloud_api(&config).is_err());
    }

    #[test]
    fn test_validate_cloud_api_valid() {
        let config = CloudApiConfig {
            api_key: "sk-test".to_string(),
            api_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o".to_string(),
            ..Default::default()
        };
        assert!(ConfigValidator::validate_cloud_api(&config).is_ok());
    }

    #[test]
    fn test_validate_cloud_api_rejects_public_http() {
        for url in [
            "http://example.com/v1",
            "http://localhost.example.com/v1",
            "http://192.168.1.20:8080/v1",
            "http://10.0.0.2/v1",
        ] {
            assert!(
                ConfigValidator::validate_cloud_api_transport(url).is_err(),
                "public HTTP URL should be rejected: {url}"
            );
        }
    }

    #[test]
    fn test_validate_cloud_api_allows_loopback_http() {
        for url in [
            "http://localhost:8080/v1",
            "http://127.0.0.1:8080/v1",
            "http://127.0.0.2:8080/v1",
            "http://[::1]:8080/v1",
            "http://[0:0:0:0:0:0:0:1]:8080/v1",
        ] {
            assert!(
                ConfigValidator::validate_cloud_api_transport(url).is_ok(),
                "loopback HTTP URL should be allowed: {url}"
            );
        }
    }

    #[test]
    fn config_save_rejects_public_http_after_direct_section_update() {
        let dir = temp_dir("config-http");
        let path = dir.join("config.json");
        let mut manager =
            ConfigManager::with_secret_store(&path, Arc::new(TestSecretStore::default()));
        manager.config.cloud_api.api_url = "http://example.com/v1".to_string();

        assert!(manager.save().is_err());
        assert!(!path.exists());
    }

    #[test]
    fn test_config_manager_load_creates_default() {
        let path = std::path::PathBuf::from("/tmp/ale-my-eyes-test-unit/config.json");
        let _ = std::fs::remove_file(&path);
        let mut manager = ConfigManager::new(&path);
        manager.load().unwrap();
        assert_eq!(manager.config().cloud_api.provider, "openai");
        assert!(manager.config().verified_tasks.enabled);
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_config_manager_loads_legacy_ui_without_auto_speak() {
        let dir = std::path::PathBuf::from("/tmp/ale-my-eyes-test-legacy-ui");
        let path = dir.join("config.json");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            r#"{
  "cloud_api": {
    "provider": "openai",
    "api_key": "sk-test",
    "api_url": "https://api.openai.com/v1",
    "model": "gpt-4o",
    "max_tokens": 1024,
    "timeout": 30
  },
  "models": {
    "auto_download": true,
    "max_download_size": 524288000,
    "preferred_quality": "balanced",
    "offline_mode": false,
    "models_dir": "models"
  },
  "inference": {
    "mode": "adaptive",
    "prefer_cloud": true,
    "timeout": 30,
    "fallback_to_local": true
  },
  "audio": {
    "sample_rate": 16000,
    "channels": 1,
    "buffer_size": 4096,
    "voice": "default",
    "speed": 1.0
  },
  "ui": {
    "language": "zh-CN",
    "theme": "system",
    "font_size": 16,
    "high_contrast": false,
    "screen_reader": true
  }
}"#,
        )
        .unwrap();

        let secret_store = Arc::new(TestSecretStore::default());
        let mut manager = ConfigManager::with_secret_store(&path, secret_store.clone());
        manager.load().unwrap();

        assert!(manager.config().ui.auto_speak);
        assert_eq!(manager.config().models.models_dir, "models");
        assert_eq!(manager.config().inference.mode, "adaptive");
        assert_eq!(
            secret_store.get_api_key().unwrap().as_deref(),
            Some("sk-test")
        );
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("auto_speak"));
        assert!(!saved.contains("sk-test"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_config_missing_sections_use_defaults() {
        let config: AppConfig =
            serde_json::from_str(r#"{"cloud_api":{"api_key":"sk-test"}}"#).unwrap();

        assert_eq!(config.cloud_api.api_key, "sk-test");
        assert_eq!(config.cloud_api.provider, "openai");
        assert_eq!(config.inference.mode, "adaptive");
        assert!(config.ui.auto_speak);
        assert!(config.verified_tasks.enabled);
    }

    #[test]
    fn config_file_io_stress_preserves_latest_value_and_redacts_secret() {
        let dir = temp_dir("config-stress");
        let path = dir.join("config.json");
        let secret_store = Arc::new(TestSecretStore::default());
        let mut manager = ConfigManager::with_secret_store(&path, secret_store.clone());

        for iteration in 0..1_000 {
            let mut config = manager.config().clone();
            config.cloud_api.api_key = "stress-secret".to_string();
            config.cloud_api.model = format!("model-{iteration}");
            manager.update_config(config).unwrap();

            let mut reloaded = ConfigManager::with_secret_store(&path, secret_store.clone());
            reloaded.load().unwrap();
            assert_eq!(
                reloaded.config().cloud_api.model,
                format!("model-{iteration}")
            );
            assert_eq!(reloaded.config().cloud_api.api_key, "stress-secret");
            manager = reloaded;
        }

        let serialized = std::fs::read_to_string(&path).unwrap();
        assert!(!serialized.contains("stress-secret"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
