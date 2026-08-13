pub mod actions;
pub mod cloud;
pub mod config;
pub mod context;
pub mod downloader;
pub mod error;
pub mod inference;
pub mod manager;
pub mod memory;
pub mod remote;
pub mod secret_store;
pub mod types;
pub mod vad;

// 条件编译模块
#[cfg(feature = "tts")]
pub mod tts;

#[cfg(feature = "local-inference")]
pub mod asr;

#[cfg(feature = "local-inference")]
pub mod vlm;

#[cfg(feature = "local-inference")]
pub mod llm;

pub use error::{AleError, Result};
pub use types::*;

use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

/// 主要的Ale, My Eyes!引擎，整合所有功能
pub struct AleEngine {
    config_manager: config::ConfigManager,
    model_manager: Arc<Mutex<manager::SmartModelManager>>,
    inference_engine: inference::AdaptiveInference,
    cloud_api: bool,
    context_manager: context::ContextManager,
    memory_store: memory::MemoryStore,
    #[cfg(feature = "tts")]
    tts: Option<Box<dyn tts::TextToSpeech>>,
}

impl AleEngine {
    pub async fn new(config_path: &Path) -> Result<Self> {
        // 加载配置
        let mut config_manager = config::ConfigManager::new(config_path);
        config_manager.load()?;

        // 检测设备性能
        let device_performance = inference::AdaptiveInference::detect_device_performance().await;
        let network_status = inference::AdaptiveInference::detect_network_status().await;

        // 创建模型管理器
        let models_dir = Path::new(&config_manager.config().models.models_dir);
        let model_manager = manager::ModelManagerFactory::create_for_device(
            models_dir,
            device_performance,
            network_status,
        );

        // 创建推理引擎
        let inference_config = inference::InferenceConfig {
            mode: match config_manager.config().inference.mode.as_str() {
                "local" => inference::InferenceMode::LocalOnly,
                "cloud" => inference::InferenceMode::CloudOnly,
                _ => inference::InferenceMode::Adaptive,
            },
            device_performance,
            network_status,
            prefer_cloud: config_manager.config().inference.prefer_cloud,
            timeout: std::time::Duration::from_secs(
                config_manager.config().inference.timeout as u64,
            ),
        };

        let mut inference_engine = inference::AdaptiveInference::new(inference_config);
        let mut cloud_ready = false;

        if !config_manager.config().cloud_api.api_key.trim().is_empty() {
            let cloud_config = Self::cloud_config_from_app(&config_manager.config().cloud_api);
            inference_engine.set_cloud_api(cloud::CloudApiFactory::create(cloud_config));
            cloud_ready = true;
        }

        // Try to load local ASR model if local-inference feature is enabled
        #[cfg(feature = "local-inference")]
        {
            let whisper_model_id = match config_manager.config().inference.mode.as_str() {
                "local" | "adaptive" => {
                    // Pick model based on device performance
                    match model_manager.device_performance() {
                        inference::DevicePerformance::Low => "whisper-tiny",
                        inference::DevicePerformance::Medium => "whisper-small",
                        inference::DevicePerformance::High => "whisper-large-v3",
                    }
                }
                _ => "whisper-tiny",
            };

            if let Some(model_path) = model_manager.get_model_path(whisper_model_id) {
                match asr::WhisperRecognizer::new(&model_path).await {
                    Ok(mut recognizer) => {
                        let lang = Self::map_whisper_language(
                            &config_manager.config().ui.language,
                            &config_manager.config().asr.language,
                        );
                        recognizer = recognizer
                            .with_language(lang)
                            .with_beam_search(
                                config_manager.config().asr.sampling_strategy == "beam",
                                config_manager.config().asr.beam_size as i32,
                            )
                            .with_temperature(config_manager.config().asr.temperature)
                            .with_initial_prompt(
                                if config_manager.config().asr.initial_prompt.is_empty() {
                                    None
                                } else {
                                    Some(config_manager.config().asr.initial_prompt.clone())
                                },
                            );
                        if let Err(e) = recognizer.load_model() {
                            tracing::warn!("Failed to load whisper model weights: {}", e);
                        } else {
                            inference_engine.set_local_asr(recognizer);
                            tracing::info!("Local ASR model loaded: {}", whisper_model_id);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to create WhisperRecognizer: {}", e);
                    }
                }
            } else {
                tracing::info!(
                    "No local whisper model found ({}). Local ASR disabled.",
                    whisper_model_id
                );
            }
        }

        let memory_path = config_path
            .parent()
            .map(|parent| parent.join("memory.json"))
            .unwrap_or_else(memory::MemoryStore::default_path);
        let memory_store = memory::MemoryStore::load_or_create(memory_path)?;
        let mut context_manager = context::ContextManager::new(4000);
        context_manager.replace_memories(memory_store.memories().to_vec());

        Ok(Self {
            config_manager,
            model_manager: Arc::new(Mutex::new(model_manager)),
            inference_engine,
            cloud_api: cloud_ready,
            context_manager,
            memory_store,
            #[cfg(feature = "tts")]
            tts: None,
        })
    }

    fn cloud_config_from_app(config: &config::CloudApiConfig) -> cloud::CloudConfig {
        let provider = match config.provider.to_lowercase().as_str() {
            "anthropic" => cloud::CloudProvider::Anthropic,
            "google" => cloud::CloudProvider::Google,
            "azure" => cloud::CloudProvider::Azure,
            "openai" => cloud::CloudProvider::OpenAI,
            other => cloud::CloudProvider::Custom(other.to_string()),
        };

        cloud::CloudConfig {
            provider,
            api_key: config.api_key.clone(),
            api_url: config.api_url.clone(),
            model: config.model.clone(),
            wire_api: config.wire_api.clone(),
            reasoning_effort: config.reasoning_effort.clone(),
            store_responses: config.store_responses,
            max_tokens: config.max_tokens,
            timeout: std::time::Duration::from_secs(config.timeout as u64),
            retry_count: 3,
        }
    }

    fn vision_question_with_context(&self, question: &str) -> String {
        let messages = self.context_manager.build_messages(None, question);
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

    /// 设置云端API
    pub async fn set_cloud_api(&mut self, api: Box<dyn cloud::CloudApi>) -> Result<()> {
        // 更新推理引擎
        self.inference_engine.set_cloud_api(api);
        self.cloud_api = true;

        Ok(())
    }

    /// 加载本地 ASR 模型（下载后调用）
    #[cfg(feature = "local-inference")]
    pub async fn load_local_asr(&mut self, model_id: &str) -> Result<()> {
        let manager = self.model_manager.lock().await;
        let model_path = manager.get_model_path(model_id).ok_or_else(|| {
            AleError::ConfigError(format!("Model '{}' not found or not downloaded", model_id))
        })?;
        drop(manager);

        let lang = Self::map_whisper_language(
            &self.config_manager.config().ui.language,
            &self.config_manager.config().asr.language,
        );

        let mut recognizer = asr::WhisperRecognizer::new(&model_path).await?;
        recognizer = recognizer
            .with_language(lang)
            .with_beam_search(
                self.config_manager.config().asr.sampling_strategy == "beam",
                self.config_manager.config().asr.beam_size as i32,
            )
            .with_temperature(self.config_manager.config().asr.temperature)
            .with_initial_prompt(
                if self.config_manager.config().asr.initial_prompt.is_empty() {
                    None
                } else {
                    Some(self.config_manager.config().asr.initial_prompt.clone())
                },
            );
        recognizer.load_model()?;
        self.inference_engine.set_local_asr(recognizer);
        Ok(())
    }

    /// Load a user-provided compatible ONNX image-description model.
    #[cfg(feature = "local-inference")]
    pub async fn load_local_vlm(&mut self, model_path: &Path) -> Result<()> {
        let mut model = vlm::OnnxVlm::new(model_path).await?;
        model.load_model()?;
        self.inference_engine.set_local_vlm(Arc::new(model));
        Ok(())
    }

    #[cfg(feature = "local-inference")]
    pub fn local_image_description_available(&self) -> bool {
        self.inference_engine.local_vlm_available()
    }

    /// 将 UI 语言代码映射为 Whisper 可识别的语言代码
    #[cfg(feature = "local-inference")]
    fn map_whisper_language(ui_lang: &str, asr_lang: &str) -> Option<String> {
        // ASR 配置中的语言优先
        if !asr_lang.is_empty() {
            return Some(asr_lang.to_string());
        }
        // 否则从 UI 语言映射
        let lang = match ui_lang {
            "zh-CN" | "zh-TW" | "zh-HK" => "zh",
            "en-US" | "en-GB" => "en",
            "ja-JP" => "ja",
            "ko-KR" => "ko",
            "fr-FR" => "fr",
            "de-DE" => "de",
            "es-ES" => "es",
            "ru-RU" => "ru",
            "pt-BR" | "pt-PT" => "pt",
            "it-IT" => "it",
            "nl-NL" => "nl",
            "pl-PL" => "pl",
            "ar-SA" => "ar",
            "tr-TR" => "tr",
            "vi-VN" => "vi",
            "th-TH" => "th",
            _ => ui_lang,
        };
        Some(lang.to_string())
    }

    /// 初始化TTS引擎（如果可用）
    #[cfg(feature = "tts")]
    pub async fn init_tts(&mut self, voice: Option<&str>) -> Result<()> {
        let tts_engine = tts::SystemTts::new(voice).await?;
        self.tts = Some(Box::new(tts_engine));
        Ok(())
    }

    /// 语音识别（通过推理引擎）
    pub async fn transcribe(&self, audio_data: &[u8]) -> Result<String> {
        let result = self.inference_engine.transcribe(audio_data).await?;
        Ok(result.data)
    }

    /// 语音合成（通过推理引擎或本地TTS）
    pub async fn synthesize(&self, text: &str) -> Result<Vec<u8>> {
        // 优先使用本地TTS（如果可用）
        #[cfg(feature = "tts")]
        if let Some(tts) = &self.tts {
            return tts.synthesize(text).await;
        }

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

        let cloud_config = Self::cloud_config_from_app(&self.config_manager.config().cloud_api);
        let cloud_api = cloud::CloudApiFactory::create(cloud_config);
        cloud_api.synthesize(text).await
    }

    /// 检查云端 API 是否可用。
    pub async fn test_cloud_api(&self) -> Result<bool> {
        Self::test_cloud_api_config(&self.config_manager.config().cloud_api).await
    }

    /// Test a cloud configuration without persisting it.
    pub async fn test_cloud_api_config(config: &config::CloudApiConfig) -> Result<bool> {
        config::ConfigValidator::validate_cloud_api(config)?;
        if config.api_key.trim().is_empty() {
            return Err(AleError::ConfigError("API key is required".to_string()));
        }

        let cloud_config = Self::cloud_config_from_app(config);
        let cloud_api = cloud::CloudApiFactory::create(cloud_config);
        cloud_api.health_check().await
    }

    /// 纯文本问答（无屏幕截图或相机画面时使用）。
    pub async fn ask_text(&self, question: &str) -> Result<cloud::CloudResponse> {
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

        let cloud_config = Self::cloud_config_from_app(&self.config_manager.config().cloud_api);
        let cloud_api = cloud::CloudApiFactory::create(cloud_config);
        let messages = self.context_manager.build_messages(None, question);
        cloud_api.chat(messages).await
    }

    /// 图像描述（通过推理引擎）
    pub async fn describe_image(&self, image_data: &[u8]) -> Result<String> {
        let result = self.inference_engine.describe_image(image_data).await?;
        Ok(result.data)
    }

    /// 视觉问答：对图像提问并获取回答
    pub async fn ask_about_image(
        &self,
        image_data: &[u8],
        question: &str,
    ) -> Result<cloud::VisionResponse> {
        let question = self.vision_question_with_context(question);
        let result = self
            .inference_engine
            .ask_about_image(image_data, &question, None)
            .await?;
        Ok(result.data)
    }

    /// 视觉问答并自动学习稳定记忆。
    pub async fn ask_about_image_with_memory(
        &mut self,
        image_data: &[u8],
        question: &str,
    ) -> Result<cloud::VisionResponse> {
        let result = self.ask_about_image(image_data, question).await?;
        let _ = self.learn_from_interaction(question, &result.content)?;
        Ok(result)
    }

    /// 视觉问答（带工具调用支持）
    pub async fn ask_about_image_with_tools(
        &self,
        image_data: &[u8],
        question: &str,
        tools: Vec<serde_json::Value>,
    ) -> Result<cloud::VisionResponse> {
        let question = self.vision_question_with_context(question);
        let result = self
            .inference_engine
            .ask_about_image(image_data, &question, Some(tools))
            .await?;
        Ok(result.data)
    }

    /// 获取上下文管理器的可变引用
    pub fn context_mut(&mut self) -> &mut context::ContextManager {
        &mut self.context_manager
    }

    /// 获取上下文管理器的不可变引用
    pub fn context(&self) -> &context::ContextManager {
        &self.context_manager
    }

    /// 添加并持久化长期记忆。
    pub fn add_memory(&mut self, entry: context::MemoryEntry) -> Result<bool> {
        let added = self.memory_store.add(entry)?;
        if added {
            self.context_manager
                .replace_memories(self.memory_store.memories().to_vec());
        }
        Ok(added)
    }

    /// 搜索持久化长期记忆。
    pub fn search_memories(&self, query: &str, limit: usize) -> Vec<&context::MemoryEntry> {
        self.memory_store.search(query, limit)
    }

    /// 删除并同步长期记忆。
    pub fn delete_memory(&mut self, id: &str) -> Result<bool> {
        let deleted = self.memory_store.delete(id)?;
        if deleted {
            self.context_manager
                .replace_memories(self.memory_store.memories().to_vec());
        }
        Ok(deleted)
    }

    /// 清空持久化长期记忆。
    pub fn clear_memories(&mut self) -> Result<()> {
        self.memory_store.clear()?;
        self.context_manager.replace_memories(Vec::new());
        Ok(())
    }

    /// 获取长期记忆文件路径。
    pub fn memory_path(&self) -> &Path {
        self.memory_store.path()
    }

    /// 从一次交互中自动提取并持久化长期记忆。
    pub fn learn_from_interaction(&mut self, question: &str, answer: &str) -> Result<usize> {
        let candidates = memory::extract_memories(question, answer);
        let mut added = 0usize;

        for entry in candidates {
            if self.memory_store.add(entry)? {
                added += 1;
            }
        }

        if added > 0 {
            self.context_manager
                .replace_memories(self.memory_store.memories().to_vec());
        }

        Ok(added)
    }

    /// 自动下载推荐模型
    pub async fn auto_download_models(&self) -> Result<Vec<std::path::PathBuf>> {
        let mut manager = self.model_manager.lock().await;
        manager.auto_download_models().await
    }

    /// 获取模型状态
    pub async fn get_model_status(&self, model_id: &str) -> Option<manager::ModelStatus> {
        let manager = self.model_manager.lock().await;
        manager.get_model_status(model_id).cloned()
    }

    /// 获取配置
    pub fn config(&self) -> &config::AppConfig {
        self.config_manager.config()
    }

    /// 更新配置
    pub fn update_config(&mut self, config: config::AppConfig) -> Result<()> {
        self.config_manager.update_config(config)
    }

    /// 检查引擎状态
    pub async fn status(&self) -> EngineStatus {
        let cloud_ready = self.cloud_api;

        #[cfg(feature = "tts")]
        let tts_ready = self.tts.is_some();
        #[cfg(not(feature = "tts"))]
        let tts_ready = false;

        EngineStatus {
            cloud_ready,
            tts_ready,
        }
    }

    /// 获取设备性能
    pub async fn device_performance(&self) -> inference::DevicePerformance {
        let manager = self.model_manager.lock().await;
        *manager.device_performance()
    }

    /// 获取网络状态
    pub async fn network_status(&self) -> inference::NetworkStatus {
        let manager = self.model_manager.lock().await;
        *manager.network_status()
    }

    /// 获取推荐模型
    pub async fn recommended_models(&self) -> Vec<downloader::ModelInfo> {
        let manager = self.model_manager.lock().await;
        manager.recommended_models().into_iter().cloned().collect()
    }

    /// 下载指定模型
    pub async fn download_model(&self, model_id: &str) -> Result<std::path::PathBuf> {
        let mut manager = self.model_manager.lock().await;
        manager.download_model(model_id).await
    }

    /// 删除模型
    pub async fn delete_model(&self, model_id: &str) -> Result<()> {
        let mut manager = self.model_manager.lock().await;
        manager.delete_model(model_id)
    }

    /// 获取已下载模型列表
    pub async fn downloaded_models(&self) -> Vec<downloader::ModelInfo> {
        let manager = self.model_manager.lock().await;
        manager.downloaded_models().into_iter().cloned().collect()
    }

    /// 获取所有可用模型
    pub async fn available_models(&self) -> Vec<downloader::ModelInfo> {
        let manager = self.model_manager.lock().await;
        manager.available_models().to_vec()
    }
}

impl Default for AleEngine {
    fn default() -> Self {
        // 这里需要一个默认实现，但实际使用时应该使用new方法
        // 为了编译通过，我们创建一个临时的实现
        let config_manager = config::ConfigManager::new(Path::new("config.json"));
        let model_manager = manager::ModelManagerFactory::create_default(Path::new("models"));
        let inference_engine =
            inference::AdaptiveInference::new(inference::InferenceConfig::default());

        Self {
            config_manager,
            model_manager: Arc::new(Mutex::new(model_manager)),
            inference_engine,
            cloud_api: false,
            context_manager: context::ContextManager::new(4000),
            memory_store: memory::MemoryStore::new(memory::MemoryStore::default_path()),
            #[cfg(feature = "tts")]
            tts: None,
        }
    }
}

/// 引擎工厂
pub struct AleEngineFactory;

impl AleEngineFactory {
    /// 创建默认引擎
    pub async fn create_default() -> Result<AleEngine> {
        let config_path = config::ConfigFactory::create_default()
            .config_path()
            .to_path_buf();
        AleEngine::new(&config_path).await
    }

    /// 创建指定配置的引擎
    pub async fn create_with_config(config_path: &Path) -> Result<AleEngine> {
        AleEngine::new(config_path).await
    }

    /// 创建测试引擎
    pub async fn create_test() -> Result<AleEngine> {
        let config_path = config::ConfigFactory::create_test()
            .config_path()
            .to_path_buf();
        AleEngine::new(&config_path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cloud_config_from_app_openai() {
        let app_config = config::CloudApiConfig {
            provider: "openai".to_string(),
            api_key: "sk-test".to_string(),
            api_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o".to_string(),
            wire_api: "responses".to_string(),
            reasoning_effort: "xhigh".to_string(),
            store_responses: false,
            max_tokens: 512,
            timeout: 60,
        };
        let cloud_config = AleEngine::cloud_config_from_app(&app_config);
        assert!(matches!(
            cloud_config.provider,
            cloud::CloudProvider::OpenAI
        ));
        assert_eq!(cloud_config.api_key, "sk-test");
        assert_eq!(cloud_config.model, "gpt-4o");
        assert_eq!(cloud_config.wire_api, "responses");
        assert_eq!(cloud_config.reasoning_effort, "xhigh");
        assert!(!cloud_config.store_responses);
        assert_eq!(cloud_config.max_tokens, 512);
    }

    #[test]
    fn test_cloud_config_from_app_anthropic() {
        let app_config = config::CloudApiConfig {
            provider: "anthropic".to_string(),
            ..Default::default()
        };
        let cloud_config = AleEngine::cloud_config_from_app(&app_config);
        assert!(matches!(
            cloud_config.provider,
            cloud::CloudProvider::Anthropic
        ));
    }

    #[test]
    fn test_cloud_config_from_app_custom() {
        let app_config = config::CloudApiConfig {
            provider: "my-provider".to_string(),
            ..Default::default()
        };
        let cloud_config = AleEngine::cloud_config_from_app(&app_config);
        if let cloud::CloudProvider::Custom(name) = cloud_config.provider {
            assert_eq!(name, "my-provider");
        } else {
            panic!("Expected Custom provider");
        }
    }

    #[test]
    fn test_engine_status_default() {
        let status = EngineStatus {
            cloud_ready: false,
            tts_ready: false,
        };
        assert!(!status.cloud_ready);
        assert!(!status.tts_ready);
    }
}
