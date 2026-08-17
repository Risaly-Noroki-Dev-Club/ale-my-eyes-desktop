use crate::downloader::{InstalledModelPackage, ModelDownloader, ModelInfo, ModelInstallConsent};
use crate::inference::{DevicePerformance, NetworkStatus};
use crate::model_scheduler::ModelManifest;
use crate::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// 模型使用策略
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ModelStrategy {
    /// 仅使用本地模型
    LocalOnly,
    /// 仅使用云端
    CloudOnly,
    /// 智能选择（根据网络和设备）
    Smart,
    /// 用户自定义
    Custom(Vec<String>),
}

/// 模型配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    pub strategy: ModelStrategy,
    pub auto_download: bool,
    pub max_download_size: u64,    // 字节
    pub preferred_quality: String, // "low", "balanced", "high"
    pub offline_mode: bool,
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            strategy: ModelStrategy::Smart,
            auto_download: false,
            max_download_size: 500 * 1024 * 1024, // 500MB
            preferred_quality: "balanced".to_string(),
            offline_mode: false,
        }
    }
}

/// 模型状态
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelStatus {
    pub model_id: String,
    pub downloaded: bool,
    pub path: Option<PathBuf>,
    pub size: u64,
    pub last_used: Option<chrono::DateTime<chrono::Utc>>,
    pub use_count: u32,
}

/// 智能模型管理器
pub struct SmartModelManager {
    downloader: ModelDownloader,
    config: ModelConfig,
    model_status: HashMap<String, ModelStatus>,
    device_performance: DevicePerformance,
    network_status: NetworkStatus,
    cloud_api: Option<Box<dyn crate::cloud::CloudApi>>,
}

impl SmartModelManager {
    pub fn new(
        models_dir: &Path,
        config: ModelConfig,
        device_performance: DevicePerformance,
        network_status: NetworkStatus,
    ) -> Self {
        Self {
            downloader: ModelDownloader::new(models_dir),
            config,
            model_status: HashMap::new(),
            device_performance,
            network_status,
            cloud_api: None,
        }
    }

    /// 设置云端API
    pub fn set_cloud_api(&mut self, api: Box<dyn crate::cloud::CloudApi>) {
        self.cloud_api = Some(api);
    }

    /// 更新设备性能
    pub fn update_device_performance(&mut self, performance: DevicePerformance) {
        self.device_performance = performance;
    }

    /// 更新网络状态
    pub fn update_network_status(&mut self, status: NetworkStatus) {
        self.network_status = status;
    }

    /// 更新配置
    pub fn update_config(&mut self, config: ModelConfig) {
        self.config = config;
    }

    /// 获取模型状态
    pub fn get_model_status(&self, model_id: &str) -> Option<&ModelStatus> {
        self.model_status.get(model_id)
    }

    /// 更新模型状态
    pub fn update_model_status(&mut self, model_id: &str, status: ModelStatus) {
        self.model_status.insert(model_id.to_string(), status);
    }

    /// 检查模型是否可用
    pub fn is_model_available(&self, model_id: &str) -> bool {
        // 检查本地是否已下载
        if self.downloader.is_model_downloaded(model_id) {
            return true;
        }

        // 检查云端是否可用
        if self.cloud_api.is_some() {
            return true;
        }

        false
    }

    /// 获取最佳模型（根据策略）
    pub fn get_best_model(&self, purpose: &str) -> Option<String> {
        match &self.config.strategy {
            ModelStrategy::LocalOnly => {
                // 只使用本地模型
                self.get_best_local_model(purpose)
            }
            ModelStrategy::CloudOnly => {
                // 只使用云端
                if self.cloud_api.is_some() {
                    Some("cloud".to_string())
                } else {
                    None
                }
            }
            ModelStrategy::Smart => {
                // 智能选择
                self.get_smart_model(purpose)
            }
            ModelStrategy::Custom(models) => {
                // 用户自定义
                models.first().cloned()
            }
        }
    }

    /// 获取最佳本地模型
    fn get_best_local_model(&self, purpose: &str) -> Option<String> {
        let available_models = self.downloader.available_models();

        // 根据用途筛选模型
        let matching_models: Vec<&ModelInfo> = available_models
            .iter()
            .filter(|m| m.purpose.contains(purpose))
            .filter(|m| self.downloader.is_model_downloaded(&m.id))
            .collect();

        // 根据设备性能选择最佳模型
        match self.device_performance {
            DevicePerformance::Low => {
                // 选择最小的模型
                matching_models
                    .iter()
                    .min_by_key(|m| m.size)
                    .map(|m| m.id.clone())
            }
            DevicePerformance::Medium => {
                // 选择中等大小的模型
                matching_models
                    .iter()
                    .find(|m| m.recommended_for == "中端设备" || m.recommended_for == "所有设备")
                    .map(|m| m.id.clone())
                    .or_else(|| matching_models.first().map(|m| m.id.clone()))
            }
            DevicePerformance::High => {
                // 选择最大的模型
                matching_models
                    .iter()
                    .max_by_key(|m| m.size)
                    .map(|m| m.id.clone())
            }
        }
    }

    /// 智能选择模型
    fn get_smart_model(&self, purpose: &str) -> Option<String> {
        // 如果离线模式，只能使用本地模型
        if self.config.offline_mode {
            return self.get_best_local_model(purpose);
        }

        // 根据网络状态选择
        match self.network_status {
            NetworkStatus::Offline => {
                // 离线，使用本地模型
                self.get_best_local_model(purpose)
            }
            NetworkStatus::Weak => {
                // 弱网，优先使用本地模型
                if let Some(local_model) = self.get_best_local_model(purpose) {
                    Some(local_model)
                } else if self.cloud_api.is_some() {
                    Some("cloud".to_string())
                } else {
                    None
                }
            }
            NetworkStatus::Normal | NetworkStatus::Fast => {
                // 网络良好，根据任务复杂度选择
                if purpose == "语音识别" || purpose == "语音合成" {
                    // 简单任务，优先使用本地模型（低延迟）
                    if let Some(local_model) = self.get_best_local_model(purpose) {
                        Some(local_model)
                    } else if self.cloud_api.is_some() {
                        Some("cloud".to_string())
                    } else {
                        None
                    }
                } else {
                    // 复杂任务，使用云端（更高质量）
                    if self.cloud_api.is_some() {
                        Some("cloud".to_string())
                    } else {
                        self.get_best_local_model(purpose)
                    }
                }
            }
        }
    }

    /// 自动下载推荐模型
    pub async fn auto_download_models(&mut self) -> Result<Vec<PathBuf>> {
        if !self.config.auto_download {
            return Ok(Vec::new());
        }

        // 先收集推荐模型的ID
        let recommended_ids: Vec<String> = self
            .downloader
            .recommended_models(&self.device_performance)
            .iter()
            .map(|m| m.id.clone())
            .collect();

        let mut downloaded = Vec::new();

        for model_id in recommended_ids {
            // 检查是否已下载
            if self.downloader.is_model_downloaded(&model_id) {
                continue;
            }

            // 获取模型信息
            let model_info = match self.downloader.get_model_info(&model_id) {
                Some(info) => info.clone(),
                None => continue,
            };

            // 检查大小限制
            if model_info.size > self.config.max_download_size {
                continue;
            }

            // 下载模型
            let path = self.downloader.download_model(&model_id).await?;
            downloaded.push(path.clone());

            // 更新状态
            self.update_model_status(
                &model_id,
                ModelStatus {
                    model_id: model_id.clone(),
                    downloaded: true,
                    path: Some(path),
                    size: model_info.size,
                    last_used: None,
                    use_count: 0,
                },
            );
        }

        Ok(downloaded)
    }

    /// 下载指定模型
    pub async fn download_model(&mut self, model_id: &str) -> Result<PathBuf> {
        let path = self.downloader.download_model(model_id).await?;

        // 更新状态
        if let Some(model) = self.downloader.get_model_info(model_id) {
            self.update_model_status(
                model_id,
                ModelStatus {
                    model_id: model_id.to_string(),
                    downloaded: true,
                    path: Some(path.clone()),
                    size: model.size,
                    last_used: None,
                    use_count: 0,
                },
            );
        }

        Ok(path)
    }

    pub fn package_install_consent(
        &self,
        manifest: &ModelManifest,
        package_id: &str,
    ) -> Result<ModelInstallConsent> {
        ModelDownloader::package_consent(manifest, package_id)
    }

    pub async fn install_pinned_package(
        &self,
        manifest: &ModelManifest,
        consent: &ModelInstallConsent,
    ) -> Result<InstalledModelPackage> {
        self.downloader.install_package(manifest, consent).await
    }

    pub fn verify_pinned_package(
        &self,
        manifest: &ModelManifest,
        package_id: &str,
    ) -> Result<InstalledModelPackage> {
        self.downloader.verify_package(manifest, package_id)
    }

    /// 删除模型
    pub fn delete_model(&mut self, model_id: &str) -> Result<()> {
        self.downloader.delete_model(model_id)?;

        // 更新状态
        if let Some(status) = self.model_status.get_mut(model_id) {
            status.downloaded = false;
            status.path = None;
        }

        Ok(())
    }

    /// 获取已下载模型列表
    pub fn downloaded_models(&self) -> Vec<&ModelInfo> {
        self.downloader.downloaded_models()
    }

    /// 获取所有可用模型
    pub fn available_models(&self) -> &[ModelInfo] {
        self.downloader.available_models()
    }

    /// 获取推荐模型
    pub fn recommended_models(&self) -> Vec<&ModelInfo> {
        self.downloader.recommended_models(&self.device_performance)
    }

    /// 使用模型（更新使用统计）
    pub fn use_model(&mut self, model_id: &str) {
        if let Some(status) = self.model_status.get_mut(model_id) {
            status.last_used = Some(chrono::Utc::now());
            status.use_count += 1;
        }
    }

    /// 获取模型路径
    pub fn get_model_path(&self, model_id: &str) -> Option<PathBuf> {
        self.downloader.get_model_path(model_id)
    }

    /// 获取模型信息
    pub fn get_model_info(&self, model_id: &str) -> Option<&ModelInfo> {
        self.downloader.get_model_info(model_id)
    }

    /// 获取配置
    pub fn config(&self) -> &ModelConfig {
        &self.config
    }

    /// 获取设备性能
    pub fn device_performance(&self) -> &DevicePerformance {
        &self.device_performance
    }

    /// 获取网络状态
    pub fn network_status(&self) -> &NetworkStatus {
        &self.network_status
    }
}

/// 模型管理器工厂
pub struct ModelManagerFactory;

impl ModelManagerFactory {
    /// 创建默认的模型管理器
    pub fn create_default(models_dir: &Path) -> SmartModelManager {
        let config = ModelConfig::default();
        let device_performance = DevicePerformance::Medium;
        let network_status = NetworkStatus::Normal;

        SmartModelManager::new(models_dir, config, device_performance, network_status)
    }

    /// 根据设备性能创建模型管理器
    pub fn create_for_device(
        models_dir: &Path,
        device_performance: DevicePerformance,
        network_status: NetworkStatus,
    ) -> SmartModelManager {
        let config = match device_performance {
            DevicePerformance::Low => ModelConfig {
                strategy: ModelStrategy::Smart,
                auto_download: false,
                max_download_size: 200 * 1024 * 1024, // 200MB
                preferred_quality: "low".to_string(),
                offline_mode: false,
            },
            DevicePerformance::Medium => ModelConfig {
                strategy: ModelStrategy::Smart,
                auto_download: false,
                max_download_size: 500 * 1024 * 1024, // 500MB
                preferred_quality: "balanced".to_string(),
                offline_mode: false,
            },
            DevicePerformance::High => ModelConfig {
                strategy: ModelStrategy::Smart,
                auto_download: false,
                max_download_size: 2 * 1024 * 1024 * 1024, // 2GB
                preferred_quality: "high".to_string(),
                offline_mode: false,
            },
        };

        SmartModelManager::new(models_dir, config, device_performance, network_status)
    }

    /// 创建离线模式管理器
    pub fn create_offline(models_dir: &Path) -> SmartModelManager {
        let config = ModelConfig {
            strategy: ModelStrategy::LocalOnly,
            auto_download: false,
            max_download_size: 2 * 1024 * 1024 * 1024, // 2GB
            preferred_quality: "high".to_string(),
            offline_mode: true,
        };

        SmartModelManager::new(
            models_dir,
            config,
            DevicePerformance::Medium,
            NetworkStatus::Offline,
        )
    }
}
