use crate::{AleError, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

#[cfg(feature = "local-inference")]
use crate::asr::SpeechRecognizer;
#[cfg(feature = "local-inference")]
use crate::vlm::VisionModel;
#[cfg(feature = "local-inference")]
use std::sync::Arc;

/// 设备性能等级
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum DevicePerformance {
    /// 低端设备 (联发科/赛扬, 4GB内存)
    Low,
    /// 中端设备 (i5/Ryzen5, 8GB内存)
    Medium,
    /// 高端设备 (i7/Ryzen7, 16GB+内存)
    High,
}

/// 网络状态
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum NetworkStatus {
    /// 离线
    Offline,
    /// 弱网 (高延迟/低带宽)
    Weak,
    /// 正常网络
    Normal,
    /// 高速网络
    Fast,
}

/// 推理模式
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum InferenceMode {
    /// 仅本地推理
    LocalOnly,
    /// 仅云端推理
    CloudOnly,
    /// 自适应 (根据设备和网络自动选择)
    Adaptive,
}

/// 任务复杂度
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub enum TaskComplexity {
    /// 简单任务 (基础语音识别、简单描述)
    Simple,
    /// 中等任务 (多语言识别、详细描述)
    Medium,
    /// 复杂任务 (复杂推理、高质量生成)
    Complex,
}

/// 推理配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceConfig {
    pub mode: InferenceMode,
    pub device_performance: DevicePerformance,
    pub network_status: NetworkStatus,
    pub prefer_cloud: bool,
    pub timeout: Duration,
}

impl Default for InferenceConfig {
    fn default() -> Self {
        Self {
            mode: InferenceMode::CloudOnly,
            device_performance: DevicePerformance::Medium,
            network_status: NetworkStatus::Normal,
            prefer_cloud: true,
            timeout: Duration::from_secs(30),
        }
    }
}

/// 推理结果
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResult<T> {
    pub data: T,
    pub mode_used: InferenceMode,
    pub latency: Duration,
    pub tokens_used: Option<usize>,
}

/// 自适应推理引擎
pub struct AdaptiveInference {
    config: InferenceConfig,
    cloud_api: Option<Box<dyn crate::cloud::CloudApi>>,
    #[cfg(feature = "local-inference")]
    local_asr: Option<crate::asr::WhisperRecognizer>,
    #[cfg(feature = "local-inference")]
    local_vlm: Option<Arc<dyn VisionModel>>,
}

impl AdaptiveInference {
    pub fn new(config: InferenceConfig) -> Self {
        Self {
            config,
            cloud_api: None,
            #[cfg(feature = "local-inference")]
            local_asr: None,
            #[cfg(feature = "local-inference")]
            local_vlm: None,
        }
    }

    /// 设置云端API
    pub fn set_cloud_api(&mut self, api: Box<dyn crate::cloud::CloudApi>) {
        self.cloud_api = Some(api);
    }

    /// Remove the configured cloud provider when credentials are cleared.
    pub fn clear_cloud_api(&mut self) {
        self.cloud_api = None;
    }

    /// 设置本地 ASR 模型
    #[cfg(feature = "local-inference")]
    pub fn set_local_asr(&mut self, recognizer: crate::asr::WhisperRecognizer) {
        self.local_asr = Some(recognizer);
    }

    /// Sets an explicitly loaded local image-description model.
    #[cfg(feature = "local-inference")]
    pub fn set_local_vlm(&mut self, model: Arc<dyn VisionModel>) {
        self.local_vlm = Some(model);
    }

    #[cfg(feature = "local-inference")]
    pub fn local_vlm_available(&self) -> bool {
        self.local_vlm.is_some()
    }

    /// 检测设备性能
    pub async fn detect_device_performance() -> DevicePerformance {
        // 检测 CPU 核心数
        let cpu_cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);

        // 检测内存大小（macOS/Linux）
        let total_memory_gb = Self::detect_memory_gb();

        // 检测是否为 Apple Silicon (M1/M2/M3)
        let is_apple_silicon = cfg!(target_os = "macos") && cfg!(target_arch = "aarch64");

        // 保守策略：只有明确是强设备才升级，否则默认 Low
        // 这样确保在未知设备上使用 whisper-tiny（75MB，最快）
        if is_apple_silicon && total_memory_gb >= 16.0 {
            DevicePerformance::High
        } else if cpu_cores >= 4 && total_memory_gb >= 8.0 {
            DevicePerformance::Medium
        } else {
            DevicePerformance::Low
        }
    }

    /// 检测系统内存大小 (GB)
    fn detect_memory_gb() -> f64 {
        #[cfg(target_os = "macos")]
        {
            if let Ok(output) = std::process::Command::new("sysctl")
                .args(["-n", "hw.memsize"])
                .output()
            {
                if let Ok(s) = String::from_utf8(output.stdout) {
                    if let Ok(bytes) = s.trim().parse::<u64>() {
                        return bytes as f64 / (1024.0 * 1024.0 * 1024.0);
                    }
                }
            }
        }
        #[cfg(target_os = "linux")]
        {
            if let Ok(content) = std::fs::read_to_string("/proc/meminfo") {
                for line in content.lines() {
                    if line.starts_with("MemTotal:") {
                        if let Some(kb_str) = line.split_whitespace().nth(1) {
                            if let Ok(kb) = kb_str.parse::<u64>() {
                                return kb as f64 / (1024.0 * 1024.0);
                            }
                        }
                    }
                }
            }
        }
        // 无法检测时保守估计为 4GB
        4.0
    }

    /// 检测网络状态
    pub async fn detect_network_status() -> NetworkStatus {
        // 这里可以添加实际的网络状态检测逻辑
        // 例如：ping测试、带宽测试等

        // 简化实现：假设正常网络
        NetworkStatus::Normal
    }

    /// 选择推理模式
    fn select_inference_mode(&self, task_complexity: TaskComplexity) -> InferenceMode {
        match self.config.mode {
            InferenceMode::LocalOnly => InferenceMode::LocalOnly,
            InferenceMode::CloudOnly => InferenceMode::CloudOnly,
            InferenceMode::Adaptive => {
                // 根据任务复杂度、设备性能、网络状态选择
                match task_complexity {
                    TaskComplexity::Simple => {
                        // 简单任务优先使用云端
                        if self.cloud_api.is_some() {
                            InferenceMode::CloudOnly
                        } else {
                            InferenceMode::LocalOnly
                        }
                    }
                    TaskComplexity::Medium => {
                        // 中等任务根据网络状态选择
                        match self.config.network_status {
                            NetworkStatus::Offline => InferenceMode::LocalOnly,
                            NetworkStatus::Weak => InferenceMode::LocalOnly,
                            _ => {
                                if self.config.prefer_cloud {
                                    InferenceMode::CloudOnly
                                } else {
                                    InferenceMode::LocalOnly
                                }
                            }
                        }
                    }
                    TaskComplexity::Complex => {
                        // 复杂任务优先使用云端
                        match self.config.network_status {
                            NetworkStatus::Offline => {
                                // 离线时只能使用本地
                                InferenceMode::LocalOnly
                            }
                            _ => InferenceMode::CloudOnly,
                        }
                    }
                }
            }
        }
    }

    /// 语音识别推理
    pub async fn transcribe(&self, audio_data: &[u8]) -> Result<InferenceResult<String>> {
        let start_time = Instant::now();

        // 选择推理模式
        let mode = self.select_inference_mode(TaskComplexity::Simple);

        let result = match mode {
            InferenceMode::LocalOnly => {
                #[cfg(feature = "local-inference")]
                {
                    let asr = self
                        .local_asr
                        .as_ref()
                        .ok_or_else(|| AleError::NotInitialized("Local ASR model"))?;
                    asr.transcribe(audio_data).await?
                }
                #[cfg(not(feature = "local-inference"))]
                {
                    return Err(AleError::Other(anyhow::anyhow!(
                        "Local inference not available (feature not enabled)"
                    )));
                }
            }
            InferenceMode::CloudOnly | InferenceMode::Adaptive => {
                // 云端推理
                let cloud_api = self
                    .cloud_api
                    .as_ref()
                    .ok_or(AleError::NotInitialized("Cloud API"))?;

                let response = cloud_api.transcribe(audio_data).await?;
                response.content
            }
        };

        let latency = start_time.elapsed();

        Ok(InferenceResult {
            data: result,
            mode_used: mode,
            latency,
            tokens_used: Some(0),
        })
    }

    /// 图像描述推理
    pub async fn describe_image(&self, image_data: &[u8]) -> Result<InferenceResult<String>> {
        let start_time = Instant::now();
        let mode = self.select_inference_mode(TaskComplexity::Complex);

        match mode {
            InferenceMode::LocalOnly => {
                #[cfg(feature = "local-inference")]
                {
                    let model = self
                        .local_vlm
                        .as_ref()
                        .ok_or_else(|| AleError::NotInitialized("Local image-description model"))?;
                    let description = model.describe_image(image_data).await?;
                    Ok(InferenceResult {
                        data: description,
                        mode_used: InferenceMode::LocalOnly,
                        latency: start_time.elapsed(),
                        tokens_used: None,
                    })
                }
                #[cfg(not(feature = "local-inference"))]
                {
                    Err(AleError::Other(anyhow::anyhow!(
                        "Local inference not available (feature not enabled)"
                    )))
                }
            }
            InferenceMode::CloudOnly | InferenceMode::Adaptive => {
                let cloud_api = self
                    .cloud_api
                    .as_ref()
                    .ok_or(AleError::NotInitialized("Cloud API"))?;

                let response = cloud_api.vision(image_data, "请描述这张图片的内容").await?;
                Ok(InferenceResult {
                    data: response.content,
                    mode_used: mode,
                    latency: start_time.elapsed(),
                    tokens_used: Some(response.tokens_used),
                })
            }
        }
    }

    /// 视觉问答推理（支持自定义问题 + Function Calling）
    pub async fn ask_about_image(
        &self,
        image_data: &[u8],
        question: &str,
        tools: Option<Vec<serde_json::Value>>,
    ) -> Result<InferenceResult<crate::cloud::VisionResponse>> {
        let start_time = Instant::now();
        let mode = self.select_inference_mode(TaskComplexity::Complex);

        match mode {
            InferenceMode::LocalOnly => Err(AleError::Other(anyhow::anyhow!(
                "本地视觉模型仅支持图像描述；问答和自动化计划需要云端模型"
            ))),
            InferenceMode::CloudOnly | InferenceMode::Adaptive => {
                let cloud_api = self
                    .cloud_api
                    .as_ref()
                    .ok_or(AleError::NotInitialized("Cloud API"))?;

                let resp = cloud_api.vision_ask(image_data, question, tools).await?;
                Ok(InferenceResult {
                    data: resp.clone(),
                    mode_used: mode,
                    latency: start_time.elapsed(),
                    tokens_used: Some(resp.tokens_used),
                })
            }
        }
    }

    /// 文本生成推理
    pub async fn generate(&self, prompt: &str) -> Result<InferenceResult<String>> {
        let start_time = Instant::now();
        let mode = self.select_inference_mode(TaskComplexity::Medium);

        match mode {
            InferenceMode::LocalOnly => Err(AleError::Other(anyhow::anyhow!(
                "Local inference not available"
            ))),
            InferenceMode::CloudOnly | InferenceMode::Adaptive => {
                let cloud_api = self
                    .cloud_api
                    .as_ref()
                    .ok_or(AleError::NotInitialized("Cloud API"))?;

                let messages = vec![crate::cloud::CloudMessage {
                    role: "user".to_string(),
                    content: prompt.to_string(),
                }];

                let response = cloud_api.chat(messages).await?;
                Ok(InferenceResult {
                    data: response.content,
                    mode_used: mode,
                    latency: start_time.elapsed(),
                    tokens_used: Some(response.tokens_used),
                })
            }
        }
    }
}

/// 语音识别推理
#[async_trait]
pub trait AsrInference: Send + Sync {
    async fn transcribe(&self, audio_data: &[u8]) -> Result<InferenceResult<String>>;
}

/// 图像描述推理
#[async_trait]
pub trait VlmInference: Send + Sync {
    async fn describe_image(&self, image_data: &[u8]) -> Result<InferenceResult<String>>;
}

/// 文本生成推理
#[async_trait]
pub trait LlmInference: Send + Sync {
    async fn generate(&self, prompt: &str) -> Result<InferenceResult<String>>;
}
