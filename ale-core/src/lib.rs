pub mod actions;
#[cfg(any(feature = "local-inference", test))]
mod blocking_worker;
pub mod child_process;
pub mod cloud;
pub mod config;
pub mod context;
pub mod desktop_download;
pub mod diagnostics;
pub mod downloader;
pub mod error;
pub mod inference;
pub mod manager;
pub mod memory;
pub mod model_ipc;
pub mod model_scheduler;
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
        Self::new_with_secret_store(config_path, Arc::new(secret_store::SystemSecretStore)).await
    }

    /// Create an engine with an explicit credential store.
    ///
    /// This keeps integration tests and embedded hosts isolated from the user's
    /// system keychain while exercising the same configuration load path.
    pub async fn new_with_secret_store(
        config_path: &Path,
        secret_store: Arc<dyn secret_store::SecretStore>,
    ) -> Result<Self> {
        let config_path = config_path.to_path_buf();
        let (config_manager, memory_store) = tokio::task::spawn_blocking(move || {
            let mut manager = config::ConfigManager::with_secret_store(&config_path, secret_store);
            manager.load()?;
            let memory_path = config_path
                .parent()
                .map(|p| p.join("memory.json"))
                .unwrap_or_else(memory::MemoryStore::default_path);
            let memories = memory::MemoryStore::load_preserving(memory_path);
            Ok::<_, AleError>((manager, memories))
        })
        .await
        .map_err(|_| AleError::ConfigError("Configuration worker failed".into()))??;

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
        inference_engine.configure_transcription(&config_manager.config().transcription);

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
                        if let Err(e) = recognizer.load_model_async().await {
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

    pub fn cloud_config_from_app(config: &config::CloudApiConfig) -> cloud::CloudConfig {
        let provider = match config.provider.to_lowercase().as_str() {
            "anthropic" => cloud::CloudProvider::Anthropic,
            "google" => cloud::CloudProvider::Google,
            "azure" => cloud::CloudProvider::Azure,
            "openai" => cloud::CloudProvider::OpenAI,
            other => cloud::CloudProvider::Custom(other.to_string()),
        };

        cloud::CloudConfig {
            wire_api: config.wire_api,
            provider,
            api_key: config.api_key.clone(),
            api_url: config.api_url.clone(),
            model: config.model.clone(),
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

    pub fn prepare_vision_question(&self, question: &str) -> String {
        self.vision_question_with_context(question)
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
        recognizer.load_model_async().await?;
        self.inference_engine.set_local_asr(recognizer);
        Ok(())
    }

    /// Load a user-provided compatible ONNX image-description model.
    #[cfg(feature = "local-inference")]
    pub async fn load_local_vlm(&mut self, model_path: &Path) -> Result<()> {
        let mut model = vlm::OnnxVlm::new(model_path).await?;
        model.load_model_async().await?;
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
        let added = self.memory_store.add_many(candidates)?;

        if added > 0 {
            self.context_manager
                .replace_memories(self.memory_store.memories().to_vec());
        }

        Ok(added)
    }

    /// 自动下载推荐模型
    pub async fn auto_download_models(&self) -> Result<Vec<std::path::PathBuf>> {
        let ids = self.model_manager.lock().await.automatic_download_ids();
        let mut paths = Vec::new();
        for id in ids {
            paths.push(self.download_model(&id).await?);
        }
        Ok(paths)
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

    pub fn memory_available(&self) -> bool {
        self.memory_store.available()
    }

    fn apply_saved_config(&mut self) {
        self.inference_engine
            .configure_transcription(&self.config_manager.config().transcription);
        let cloud = &self.config_manager.config().cloud_api;
        if cloud.api_key.trim().is_empty() {
            self.inference_engine.clear_cloud_api();
            self.cloud_api = false;
        } else {
            let cloud_config = Self::cloud_config_from_app(cloud);
            self.inference_engine
                .set_cloud_api(cloud::CloudApiFactory::create(cloud_config));
            self.cloud_api = true;
        }
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
        let downloader = self.model_manager.lock().await.download_context();
        let path = downloader.download_model(model_id).await?;
        self.model_manager
            .lock()
            .await
            .record_download(model_id, path.clone());
        Ok(path)
    }

    pub async fn model_package_consent(
        &self,
        manifest: &model_scheduler::ModelManifest,
        package_id: &str,
    ) -> Result<downloader::ModelInstallConsent> {
        let manager = self.model_manager.lock().await;
        manager.package_install_consent(manifest, package_id)
    }

    pub async fn install_model_package(
        &self,
        manifest: &model_scheduler::ModelManifest,
        consent: &downloader::ModelInstallConsent,
    ) -> Result<downloader::InstalledModelPackage> {
        let downloader = self.model_manager.lock().await.download_context();
        downloader.install_package(manifest, consent).await
    }

    pub async fn verify_model_package(
        &self,
        manifest: &model_scheduler::ModelManifest,
        package_id: &str,
    ) -> Result<downloader::InstalledModelPackage> {
        let downloader = self.model_manager.lock().await.download_context();
        downloader.verify_package_async(manifest, package_id).await
    }

    /// 删除模型
    pub async fn delete_model(&self, model_id: &str) -> Result<()> {
        let downloader = self.model_manager.lock().await.download_context();
        let id = model_id.to_owned();
        tokio::task::spawn_blocking(move || downloader.delete_model(&id))
            .await
            .map_err(|_| AleError::ConfigError("Model removal worker failed".into()))??;
        self.model_manager.lock().await.record_delete(model_id);
        Ok(())
    }

    /// 获取已下载模型列表
    pub async fn downloaded_models(&self) -> Vec<downloader::ModelInfo> {
        let downloader = self.model_manager.lock().await.download_context();
        tokio::task::spawn_blocking(move || {
            downloader
                .downloaded_models()
                .into_iter()
                .cloned()
                .collect()
        })
        .await
        .unwrap_or_default()
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
        let config_path = resolve_default_config_path(|| {
            config::ConfigFactory::create_default()
                .config_path()
                .to_path_buf()
        })
        .await?;
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
    async fn slow_credentials_do_not_block_runtime_and_bad_memory_preserves_startup() {
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
        std::fs::write(dir.join("memory.json"), b"{bad").unwrap();
        let engine = AleEngine::new_with_secret_store(&path, Arc::new(SlowStore));
        tokio::pin!(engine);
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(30)) => {},
            _ = &mut engine => panic!("slow credentials unexpectedly completed before heartbeat"),
        }
        let engine = engine.await.unwrap();
        assert!(!engine.memory_available());
        assert_eq!(std::fs::read(dir.join("memory.json")).unwrap(), b"{bad");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn test_cloud_config_from_app_openai() {
        let app_config = config::CloudApiConfig {
            wire_api: model_api::WireApi::OpenaiChatCompletions,
            provider: "openai".to_string(),
            api_key: "sk-test".to_string(),
            api_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o".to_string(),
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
pub mod model_api;
pub mod model_probe;
