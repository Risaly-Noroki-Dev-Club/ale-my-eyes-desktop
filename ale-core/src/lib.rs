pub mod actions;
pub mod child_process;
pub mod cloud;
pub mod config;
pub mod context;
pub mod desktop_download;
pub mod diagnostics;
pub mod error;
pub mod harness;
pub mod model_api;
pub mod model_ipc;
pub mod model_probe;
pub mod model_scheduler;
pub mod remote;
pub mod secret_store;
pub mod types;

pub use error::{AleError, Result};
pub use types::*;

use cloud::{CloudApi as _, RemoteCloudApi};
use std::path::Path;

/// 主要的Ale, My Eyes!引擎。
///
/// 本地推理（ASR / VLM / LLM）由 `ale-modeld` 进程负责；本引擎只处理配置、
/// 云端模型调用、上下文与远程协议所需的辅助逻辑。
pub struct AleEngine {
    config_manager: config::ConfigManager,
    cloud_api: Option<RemoteCloudApi>,
    transcription_api: Option<RemoteCloudApi>,
    context_manager: context::ContextManager,
}

impl AleEngine {
    pub async fn new(config_path: &Path) -> Result<Self> {
        Self::new_with_secret_store(
            config_path,
            std::sync::Arc::new(secret_store::SystemSecretStore),
        )
        .await
    }

    /// Create an engine with an explicit credential store.
    ///
    /// This keeps integration tests and embedded hosts isolated from the user's
    /// system keychain while exercising the same configuration load path.
    pub async fn new_with_secret_store(
        config_path: &Path,
        secret_store: std::sync::Arc<dyn secret_store::SecretStore>,
    ) -> Result<Self> {
        let config_path = config_path.to_path_buf();
        let config_manager = tokio::task::spawn_blocking(move || {
            let mut manager = config::ConfigManager::with_secret_store(&config_path, secret_store);
            manager.load()?;
            Ok::<_, AleError>(manager)
        })
        .await
        .map_err(|_| AleError::ConfigError("Configuration worker failed".into()))??;

        let cloud_api = Self::build_cloud_api(&config_manager.config().cloud_api);
        let transcription_api =
            Self::build_transcription_api(&config_manager.config().transcription);

        Ok(Self {
            config_manager,
            cloud_api,
            transcription_api,
            context_manager: context::ContextManager::new(4000),
        })
    }

    /// 把应用配置转换成云调用配置：provider 预设优先决定传输协议。
    pub fn cloud_config_from_app(config: &config::CloudApiConfig) -> cloud::CloudConfig {
        cloud::CloudConfig {
            wire_api: config.effective_wire_api(),
            api_key: config.api_key.clone(),
            api_url: config.api_url.clone(),
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            timeout: std::time::Duration::from_secs(config.timeout as u64),
        }
    }

    fn build_cloud_api(config: &config::CloudApiConfig) -> Option<RemoteCloudApi> {
        (!config.api_key.trim().is_empty())
            .then(|| RemoteCloudApi::new(Self::cloud_config_from_app(config)))
    }

    fn build_transcription_api(config: &config::TranscriptionConfig) -> Option<RemoteCloudApi> {
        // headless 无凭据后端时 key 为空：不构造端点，转写按未初始化明确报错。
        (config.enabled && !config.endpoint.api_key.trim().is_empty())
            .then(|| RemoteCloudApi::new(Self::cloud_config_from_app(&config.endpoint)))
    }

    fn cloud(&self) -> Result<&RemoteCloudApi> {
        self.cloud_api
            .as_ref()
            .ok_or(AleError::NotInitialized("Cloud API"))
    }

    /// 云端调用前统一的 API key 前置校验，给出明确错误而不是 `Not initialized`。
    fn require_cloud_api_key(&self) -> Result<()> {
        if self
            .config_manager
            .config()
            .cloud_api
            .api_key
            .trim()
            .is_empty()
        {
            return Err(AleError::ConfigError("API key is required".to_string()));
        }
        Ok(())
    }

    /// 凭据后端不可用等非致命配置警告（headless/无 D-Bus 环境）。
    pub fn credential_warnings(&self) -> &[String] {
        self.config_manager.credential_warnings()
    }

    fn vision_question_with_context(&self, question: &str) -> String {
        let messages = self.context_manager.build_messages(question);
        let context = messages
            .into_iter()
            .map(|message| format!("[{}]\n{}", message.role, message.content))
            .collect::<Vec<_>>()
            .join("\n\n");

        format!(
            "请结合随附图片和以下上下文回答。若用户要求执行操作，请使用可用工具生成操作计划。\n\n{}",
            context
        )
    }

    pub fn prepare_vision_question(&self, question: &str) -> String {
        self.vision_question_with_context(question)
    }

    /// 语音识别（使用独立转写端点，不共享主对话凭据）
    pub async fn transcribe(&self, audio_data: &[u8]) -> Result<String> {
        let api = self
            .transcription_api
            .as_ref()
            .ok_or(AleError::NotInitialized("Independent transcription API"))?;
        let response =
            model_api::retry(model_api::deadline(), 1, || api.transcribe(audio_data)).await?;
        Ok(response.content)
    }

    /// 检查云端 API 是否可用。
    pub async fn test_cloud_api(&self) -> Result<bool> {
        self.require_cloud_api_key()?;
        self.cloud()?.health_check().await
    }

    /// 纯文本问答（无屏幕截图或相机画面时使用）。
    pub async fn ask_text(&self, question: &str) -> Result<cloud::CloudResponse> {
        self.require_cloud_api_key()?;
        let messages = self.context_manager.build_messages(question);
        self.cloud()?.chat(messages).await
    }

    /// 图像描述（云端视觉模型）。
    pub async fn describe_image(&self, image_data: &[u8]) -> Result<String> {
        // 与 ask_text/test_cloud_api 一致：未配置 key 时给出明确错误（DC-12）。
        self.require_cloud_api_key()?;
        let response = self
            .cloud()?
            .vision(image_data, "请描述这张图片的内容")
            .await?;
        Ok(response.content)
    }

    /// 视觉问答（带工具调用支持）
    pub async fn ask_about_image_with_tools(
        &self,
        image_data: &[u8],
        question: &str,
        tools: Vec<serde_json::Value>,
    ) -> Result<cloud::VisionResponse> {
        self.require_cloud_api_key()?;
        let question = self.vision_question_with_context(question);
        self.cloud()?
            .vision_ask(image_data, &question, Some(tools))
            .await
    }

    /// 获取配置
    pub fn config(&self) -> &config::AppConfig {
        self.config_manager.config()
    }

    /// 更新配置
    pub fn update_config(&mut self, config: config::AppConfig) -> Result<()> {
        self.config_manager.update_config(config)?;
        self.apply_saved_config();
        Ok(())
    }

    /// Must be awaited to completion by the owning background transaction task.
    /// Dropping a started transaction cannot cancel OS credential writes.
    pub async fn update_config_async(&mut self, config: config::AppConfig) -> Result<()> {
        let mut manager = self.config_manager.clone();
        manager = tokio::task::spawn_blocking(move || {
            manager.update_config(config)?;
            Ok::<_, AleError>(manager)
        })
        .await
        .map_err(|_| AleError::ConfigError("Configuration worker failed".into()))??;
        self.config_manager = manager;
        self.apply_saved_config();
        Ok(())
    }

    fn apply_saved_config(&mut self) {
        let transcription_api =
            Self::build_transcription_api(&self.config_manager.config().transcription);
        let cloud_api = Self::build_cloud_api(&self.config_manager.config().cloud_api);
        self.transcription_api = transcription_api;
        self.cloud_api = cloud_api;
    }

    /// 检查引擎状态
    pub async fn status(&self) -> EngineStatus {
        EngineStatus {
            cloud_ready: self.cloud_api.is_some(),
        }
    }
}

/// 引擎工厂
pub struct AleEngineFactory;

impl AleEngineFactory {
    /// 创建默认引擎
    pub async fn create_default() -> Result<AleEngine> {
        let config_path = resolve_default_config_path(|| {
            config::ConfigFactory::create_default()
                .config_path()
                .to_path_buf()
        })
        .await?;
        AleEngine::new(&config_path).await
    }
}

async fn resolve_default_config_path(
    resolve: impl FnOnce() -> std::path::PathBuf + Send + 'static,
) -> Result<std::path::PathBuf> {
    // On Windows the config directory lookup calls SHGetKnownFolderPath. Keep
    // shell/profile work off the async executor, just like credential loading.
    let started = std::time::Instant::now();
    diagnostics::record("config_directory_lookup_started", &[]);
    let result = tokio::task::spawn_blocking(resolve).await;
    diagnostics::record(
        "config_directory_lookup_finished",
        &[
            ("elapsed_ms", started.elapsed().as_millis() as u64),
            ("success", result.is_ok() as u64),
        ],
    );
    result.map_err(|_| AleError::ConfigError("Configuration directory worker failed".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_api::WireApi;
    use std::sync::Arc;

    #[tokio::test(flavor = "current_thread")]
    async fn slow_config_directory_lookup_keeps_runtime_responsive() {
        let runtime_thread = std::thread::current().id();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let lookup = tokio::spawn(resolve_default_config_path(move || {
            assert_ne!(std::thread::current().id(), runtime_thread);
            started_tx.send(()).unwrap();
            release_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("runtime must progress while directory discovery is pending");
            std::path::PathBuf::from("test-config.json")
        }));

        started_rx.await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(!lookup.is_finished());
        release_tx.send(()).unwrap();
        assert_eq!(
            lookup.await.unwrap().unwrap(),
            std::path::PathBuf::from("test-config.json")
        );
    }

    #[tokio::test]
    async fn slow_credentials_do_not_block_runtime() {
        struct SlowStore;
        impl secret_store::SecretStore for SlowStore {
            fn get_api_key(&self) -> Result<Option<String>> {
                std::thread::sleep(std::time::Duration::from_millis(150));
                Ok(None)
            }
            fn set_api_key(&self, _: &str) -> Result<()> {
                Ok(())
            }
            fn delete_api_key(&self) -> Result<()> {
                Ok(())
            }
        }
        let dir = std::env::temp_dir().join(format!("ale-init-worker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&config::AppConfig::default()).unwrap(),
        )
        .unwrap();
        let engine = AleEngine::new_with_secret_store(&path, Arc::new(SlowStore));
        tokio::pin!(engine);
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(30)) => {},
            _ = &mut engine => panic!("slow credentials unexpectedly completed before heartbeat"),
        }
        let engine = engine.await.unwrap();
        assert_eq!(engine.config().cloud_api.provider, "openai");
        assert!(engine.config().verified_tasks.enabled);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// 独立转写凭据不依赖主对话 API key，且使用独立端点。
    #[tokio::test]
    async fn transcription_uses_independent_credentials_without_primary() {
        struct TranscriptionOnlyStore;
        impl secret_store::SecretStore for TranscriptionOnlyStore {
            fn get_api_key(&self) -> Result<Option<String>> {
                Ok(None)
            }
            fn set_api_key(&self, _: &str) -> Result<()> {
                Ok(())
            }
            fn delete_api_key(&self) -> Result<()> {
                Ok(())
            }
            fn get_transcription_api_key(&self) -> Result<Option<String>> {
                Ok(Some("asr-only-key".into()))
            }
            fn set_transcription_api_key(&self, _: &str) -> Result<()> {
                Ok(())
            }
            fn delete_transcription_api_key(&self) -> Result<()> {
                Ok(())
            }
        }
        let (api_url, captured) = crate::model_api::tests::mock_http(
            "200 OK",
            serde_json::json!({"text":"independent"}),
            "",
        )
        .await;
        let dir = std::env::temp_dir().join(format!("ale-transcription-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let mut config = config::AppConfig::default();
        config.transcription = config::TranscriptionConfig {
            enabled: true,
            endpoint: config::CloudApiConfig {
                api_url,
                model: "custom-transcriber".into(),
                ..Default::default()
            },
        };
        std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
        let engine = AleEngine::new_with_secret_store(&path, Arc::new(TranscriptionOnlyStore))
            .await
            .unwrap();
        let text = engine
            .transcribe(include_bytes!("../assets/model-probe.wav"))
            .await
            .unwrap();
        assert_eq!(text, "independent");
        let request = captured.await.unwrap();
        assert!(request.contains("Bearer asr-only-key"));
        assert!(request.contains("custom-transcriber"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cloud_config_from_app_follows_provider_preset() {
        let app_config = config::CloudApiConfig {
            provider: "anthropic".to_string(),
            wire_api: WireApi::OpenaiChatCompletions,
            api_key: "sk-test".to_string(),
            api_url: "https://api.anthropic.com/v1".to_string(),
            model: "claude-sonnet-4-20250514".to_string(),
            max_tokens: 512,
            timeout: 60,
        };
        let cloud_config = AleEngine::cloud_config_from_app(&app_config);
        assert_eq!(cloud_config.wire_api, WireApi::AnthropicMessages);
        assert_eq!(cloud_config.api_key, "sk-test");
        assert_eq!(cloud_config.model, "claude-sonnet-4-20250514");
        assert_eq!(cloud_config.max_tokens, 512);
        assert_eq!(cloud_config.timeout, std::time::Duration::from_secs(60));
    }

    #[test]
    fn cloud_config_from_app_keeps_custom_protocol() {
        let app_config = config::CloudApiConfig {
            provider: "custom".to_string(),
            wire_api: WireApi::GoogleGenerateContent,
            ..Default::default()
        };
        let cloud_config = AleEngine::cloud_config_from_app(&app_config);
        assert_eq!(cloud_config.wire_api, WireApi::GoogleGenerateContent);
    }

    #[test]
    fn test_engine_status_default() {
        let status = EngineStatus { cloud_ready: false };
        assert!(!status.cloud_ready);
    }

    #[tokio::test]
    async fn describe_image_without_api_key_reports_configuration_error() {
        struct EmptyStore;
        impl secret_store::SecretStore for EmptyStore {
            fn get_api_key(&self) -> Result<Option<String>> {
                Ok(None)
            }
            fn set_api_key(&self, _: &str) -> Result<()> {
                Ok(())
            }
            fn delete_api_key(&self) -> Result<()> {
                Ok(())
            }
        }
        let dir = std::env::temp_dir().join(format!("ale-no-key-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            serde_json::to_vec(&config::AppConfig::default()).unwrap(),
        )
        .unwrap();
        let engine = AleEngine::new_with_secret_store(&path, Arc::new(EmptyStore))
            .await
            .unwrap();
        let error = engine.describe_image(b"not-an-image").await.unwrap_err();
        assert!(error.to_string().contains("API key is required"), "{error}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
