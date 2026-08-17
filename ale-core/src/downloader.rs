use crate::model_scheduler::{ModelArtifact, ModelManifest};
use crate::{AleError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::Mutex;

/// 模型信息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    pub size: u64, // 字节
    pub repo: String,
    pub filename: String,
    pub quantization: Option<String>,
    pub purpose: String,
    pub recommended_for: String,
}

/// 下载进度
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DownloadProgress {
    pub model_id: String,
    pub total_bytes: u64,
    pub downloaded_bytes: u64,
    pub progress: f32, // 0.0 - 1.0
    pub speed: f32,    // 字节/秒
    pub eta: u32,      // 预计剩余秒数
}

/// 进度回调函数类型
pub type ProgressCallback = Box<dyn Fn(DownloadProgress) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInstallConsent {
    pub package_id: String,
    pub license: String,
    pub download_size_bytes: u64,
    pub required_disk_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstalledModelPackage {
    pub package_id: String,
    pub directory: PathBuf,
    pub artifacts: Vec<PathBuf>,
}

/// 模型下载器
pub struct ModelDownloader {
    models_dir: PathBuf,
    progress_callback: Option<ProgressCallback>,
    client: reqwest::Client,
    known_models: Vec<ModelInfo>,
}

impl ModelDownloader {
    pub fn new(models_dir: &Path) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            models_dir: models_dir.to_path_buf(),
            progress_callback: None,
            client,
            known_models: Self::default_known_models(),
        }
    }

    /// 设置进度回调
    pub fn set_progress_callback(&mut self, callback: ProgressCallback) {
        self.progress_callback = Some(callback);
    }

    pub fn package_consent(
        manifest: &ModelManifest,
        package_id: &str,
    ) -> Result<ModelInstallConsent> {
        let package = manifest.package(package_id)?;
        Ok(ModelInstallConsent {
            package_id: package.id.clone(),
            license: package.license.clone(),
            download_size_bytes: package.download_size_bytes()?,
            required_disk_bytes: package.required_disk_bytes()?,
        })
    }

    pub async fn install_package(
        &self,
        manifest: &ModelManifest,
        consent: &ModelInstallConsent,
    ) -> Result<InstalledModelPackage> {
        let package = manifest.package(&consent.package_id)?;
        let expected = Self::package_consent(manifest, &package.id)?;
        if !package.requires_explicit_consent || consent != &expected {
            return Err(AleError::ConfigError(
                "model download consent does not match the pinned package metadata".to_string(),
            ));
        }

        let package_dir = self.models_dir.join(&package.id);
        std::fs::create_dir_all(&package_dir)?;
        let mut installed = Vec::with_capacity(package.artifacts.len());
        for artifact in &package.artifacts {
            let target = package_dir.join(&artifact.filename);
            if target.is_file() {
                verify_artifact(&target, artifact)?;
            } else {
                self.download_pinned_artifact(&package.id, artifact, &target)
                    .await?;
            }
            installed.push(target);
        }
        Ok(InstalledModelPackage {
            package_id: package.id.clone(),
            directory: package_dir,
            artifacts: installed,
        })
    }

    pub fn verify_package(
        &self,
        manifest: &ModelManifest,
        package_id: &str,
    ) -> Result<InstalledModelPackage> {
        let package = manifest.package(package_id)?;
        let directory = self.models_dir.join(&package.id);
        let artifacts = package
            .artifacts
            .iter()
            .map(|artifact| {
                let path = directory.join(&artifact.filename);
                verify_artifact(&path, artifact)?;
                Ok(path)
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(InstalledModelPackage {
            package_id: package.id.clone(),
            directory,
            artifacts,
        })
    }

    async fn download_pinned_artifact(
        &self,
        package_id: &str,
        artifact: &ModelArtifact,
        target: &Path,
    ) -> Result<()> {
        let response = self
            .client
            .get(&artifact.url)
            .send()
            .await
            .map_err(|error| {
                AleError::Other(anyhow::anyhow!("Download request failed: {error}"))
            })?;
        if !response.status().is_success() {
            return Err(AleError::Other(anyhow::anyhow!(
                "Download failed with status: {}",
                response.status()
            )));
        }
        if response
            .content_length()
            .is_some_and(|length| length != artifact.size_bytes)
        {
            return Err(AleError::Other(anyhow::anyhow!(
                "Pinned artifact size does not match Content-Length"
            )));
        }

        let temp = target.with_extension(format!("{}.partial", uuid::Uuid::new_v4()));
        let result = async {
            let mut file = std::fs::File::create(&temp)?;
            let mut hasher = Sha256::new();
            let mut downloaded = 0_u64;
            let started = std::time::Instant::now();
            let mut stream = response.bytes_stream();
            use futures::StreamExt;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk
                    .map_err(|error| AleError::Other(anyhow::anyhow!("Download error: {error}")))?;
                downloaded = downloaded.checked_add(chunk.len() as u64).ok_or_else(|| {
                    AleError::Other(anyhow::anyhow!("Downloaded artifact size overflow"))
                })?;
                if downloaded > artifact.size_bytes {
                    return Err(AleError::Other(anyhow::anyhow!(
                        "Downloaded artifact exceeds pinned size"
                    )));
                }
                file.write_all(&chunk)?;
                hasher.update(&chunk);
                self.report_package_progress(package_id, artifact.size_bytes, downloaded, started);
            }
            file.sync_all()?;
            if downloaded != artifact.size_bytes {
                return Err(AleError::Other(anyhow::anyhow!(
                    "Downloaded artifact is shorter than pinned size"
                )));
            }
            let actual = format!("{:x}", hasher.finalize());
            if !actual.eq_ignore_ascii_case(&artifact.sha256) {
                return Err(AleError::Other(anyhow::anyhow!(
                    "Downloaded artifact SHA-256 mismatch"
                )));
            }
            std::fs::rename(&temp, target)?;
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }

    fn report_package_progress(
        &self,
        package_id: &str,
        total_bytes: u64,
        downloaded_bytes: u64,
        started: std::time::Instant,
    ) {
        let elapsed = started.elapsed().as_secs_f32();
        let speed = if elapsed > 0.0 {
            downloaded_bytes as f32 / elapsed
        } else {
            0.0
        };
        let remaining = total_bytes.saturating_sub(downloaded_bytes);
        let eta = if speed > 0.0 {
            (remaining as f32 / speed) as u32
        } else {
            0
        };
        if let Some(callback) = &self.progress_callback {
            callback(DownloadProgress {
                model_id: package_id.to_string(),
                total_bytes,
                downloaded_bytes,
                progress: downloaded_bytes as f32 / total_bytes as f32,
                speed,
                eta,
            });
        }
    }

    /// 默认的已知模型列表
    fn default_known_models() -> Vec<ModelInfo> {
        vec![
            ModelInfo {
                id: "whisper-tiny".to_string(),
                name: "Whisper Tiny".to_string(),
                description: "轻量级语音识别模型".to_string(),
                size: 75 * 1024 * 1024, // 75MB
                repo: "ggml-org/whisper.cpp".to_string(),
                filename: "ggml-tiny.bin".to_string(),
                quantization: Some("q4_0".to_string()),
                purpose: "基础语音识别".to_string(),
                recommended_for: "低性能设备".to_string(),
            },
            ModelInfo {
                id: "whisper-small".to_string(),
                name: "Whisper Small".to_string(),
                description: "中等质量语音识别模型".to_string(),
                size: 244 * 1024 * 1024, // 244MB
                repo: "ggml-org/whisper.cpp".to_string(),
                filename: "ggml-small.bin".to_string(),
                quantization: Some("q4_0".to_string()),
                purpose: "高质量语音识别".to_string(),
                recommended_for: "中端设备".to_string(),
            },
            ModelInfo {
                id: "whisper-large-v3".to_string(),
                name: "Whisper Large V3".to_string(),
                description: "最高质量语音识别模型".to_string(),
                size: 1500 * 1024 * 1024, // 1.5GB
                repo: "ggml-org/whisper.cpp".to_string(),
                filename: "ggml-large-v3.bin".to_string(),
                quantization: Some("q4_0".to_string()),
                purpose: "专业级语音识别".to_string(),
                recommended_for: "高端设备".to_string(),
            },
            ModelInfo {
                id: "piper-zh_CN".to_string(),
                name: "Piper 中文语音".to_string(),
                description: "轻量级中文语音合成".to_string(),
                size: 50 * 1024 * 1024, // 50MB
                repo: "rhasspy/piper".to_string(),
                filename: "zh_CN-huayan-medium.onnx".to_string(),
                quantization: None,
                purpose: "中文语音合成".to_string(),
                recommended_for: "所有设备".to_string(),
            },
            ModelInfo {
                id: "piper-en_US".to_string(),
                name: "Piper 英文语音".to_string(),
                description: "轻量级英文语音合成".to_string(),
                size: 50 * 1024 * 1024, // 50MB
                repo: "rhasspy/piper".to_string(),
                filename: "en_US-amy-medium.onnx".to_string(),
                quantization: None,
                purpose: "英文语音合成".to_string(),
                recommended_for: "所有设备".to_string(),
            },
        ]
    }

    /// 获取所有可用模型
    pub fn available_models(&self) -> &[ModelInfo] {
        &self.known_models
    }

    /// 根据ID获取模型信息
    pub fn get_model_info(&self, model_id: &str) -> Option<&ModelInfo> {
        self.known_models.iter().find(|m| m.id == model_id)
    }

    /// 检查模型是否已下载
    pub fn is_model_downloaded(&self, model_id: &str) -> bool {
        if let Some(model) = self.get_model_info(model_id) {
            let path = self.models_dir.join(&model.filename);
            path.exists()
        } else {
            false
        }
    }

    /// 获取模型文件路径
    pub fn get_model_path(&self, model_id: &str) -> Option<PathBuf> {
        if let Some(model) = self.get_model_info(model_id) {
            let path = self.models_dir.join(&model.filename);
            if path.exists() {
                Some(path)
            } else {
                None
            }
        } else {
            None
        }
    }

    /// 下载模型
    pub async fn download_model(&self, model_id: &str) -> Result<PathBuf> {
        let model = self
            .get_model_info(model_id)
            .ok_or_else(|| AleError::Other(anyhow::anyhow!("Unknown model: {}", model_id)))?
            .clone();

        // 检查是否已下载
        let target_path = self.models_dir.join(&model.filename);
        if target_path.exists() {
            return Ok(target_path);
        }

        // 确保目录存在
        std::fs::create_dir_all(&self.models_dir)?;

        // 构建下载URL
        let url = format!(
            "https://huggingface.co/{}/resolve/main/{}",
            model.repo, model.filename
        );

        // 开始下载
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| AleError::Other(anyhow::anyhow!("Download request failed: {}", e)))?;

        if !response.status().is_success() {
            return Err(AleError::Other(anyhow::anyhow!(
                "Download failed with status: {}",
                response.status()
            )));
        }

        // 获取文件大小
        let total_size = response.content_length().unwrap_or(model.size);

        // 创建临时文件
        let temp_path = target_path.with_extension("tmp");
        let mut file = std::fs::File::create(&temp_path)?;

        // 下载并写入文件
        let mut downloaded: u64 = 0;
        let start_time = std::time::Instant::now();
        let mut stream = response.bytes_stream();

        use futures::StreamExt;
        use std::io::Write;

        while let Some(chunk) = stream.next().await {
            let chunk =
                chunk.map_err(|e| AleError::Other(anyhow::anyhow!("Download error: {}", e)))?;
            file.write_all(&chunk)?;

            downloaded += chunk.len() as u64;

            // 计算进度
            let progress = downloaded as f32 / total_size as f32;
            let elapsed = start_time.elapsed().as_secs_f32();
            let speed = if elapsed > 0.0 {
                downloaded as f32 / elapsed
            } else {
                0.0
            };
            let remaining_bytes = total_size - downloaded;
            let eta = if speed > 0.0 {
                (remaining_bytes as f32 / speed) as u32
            } else {
                0
            };

            // 调用进度回调
            if let Some(callback) = &self.progress_callback {
                callback(DownloadProgress {
                    model_id: model_id.to_string(),
                    total_bytes: total_size,
                    downloaded_bytes: downloaded,
                    progress,
                    speed,
                    eta,
                });
            }
        }

        // 重命名临时文件
        std::fs::rename(&temp_path, &target_path)?;

        Ok(target_path)
    }

    /// 删除模型
    pub fn delete_model(&self, model_id: &str) -> Result<()> {
        if let Some(model) = self.get_model_info(model_id) {
            let path = self.models_dir.join(&model.filename);
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
        }
        Ok(())
    }

    /// 获取已下载模型列表
    pub fn downloaded_models(&self) -> Vec<&ModelInfo> {
        self.known_models
            .iter()
            .filter(|m| self.is_model_downloaded(&m.id))
            .collect()
    }

    /// 获取推荐模型（根据设备性能）
    pub fn recommended_models(
        &self,
        device_performance: &crate::inference::DevicePerformance,
    ) -> Vec<&ModelInfo> {
        match device_performance {
            crate::inference::DevicePerformance::Low => self
                .known_models
                .iter()
                .filter(|m| m.recommended_for == "低性能设备" || m.recommended_for == "所有设备")
                .collect(),
            crate::inference::DevicePerformance::Medium => self
                .known_models
                .iter()
                .filter(|m| m.recommended_for == "中端设备" || m.recommended_for == "所有设备")
                .collect(),
            crate::inference::DevicePerformance::High => self
                .known_models
                .iter()
                .filter(|m| m.recommended_for == "高端设备" || m.recommended_for == "所有设备")
                .collect(),
        }
    }

    /// 自动下载推荐模型
    pub async fn download_recommended_models(
        &self,
        device_performance: &crate::inference::DevicePerformance,
    ) -> Result<Vec<PathBuf>> {
        let recommended = self.recommended_models(device_performance);
        let mut paths = Vec::new();

        for model in recommended {
            if !self.is_model_downloaded(&model.id) {
                let path = self.download_model(&model.id).await?;
                paths.push(path);
            }
        }

        Ok(paths)
    }
}

fn verify_artifact(path: &Path, artifact: &ModelArtifact) -> Result<()> {
    let metadata = std::fs::metadata(path).map_err(|error| {
        AleError::Other(anyhow::anyhow!(
            "Pinned model artifact is unavailable at {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() || metadata.len() != artifact.size_bytes {
        return Err(AleError::Other(anyhow::anyhow!(
            "Pinned model artifact size mismatch at {}",
            path.display()
        )));
    }
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(&artifact.sha256) {
        return Err(AleError::Other(anyhow::anyhow!(
            "Pinned model artifact SHA-256 mismatch at {}",
            path.display()
        )));
    }
    Ok(())
}

/// 模型下载管理器（带缓存和并发控制）
pub struct ModelDownloadManager {
    downloader: Arc<Mutex<ModelDownloader>>,
    max_concurrent_downloads: usize,
}

impl ModelDownloadManager {
    pub fn new(models_dir: &Path, max_concurrent: usize) -> Self {
        Self {
            downloader: Arc::new(Mutex::new(ModelDownloader::new(models_dir))),
            max_concurrent_downloads: max_concurrent,
        }
    }

    /// 批量下载模型
    pub async fn download_models(&self, model_ids: &[&str]) -> Result<Vec<PathBuf>> {
        let downloader = self.downloader.lock().await;
        let mut paths = Vec::new();

        for model_id in model_ids {
            let path = downloader.download_model(model_id).await?;
            paths.push(path);
        }

        Ok(paths)
    }

    /// 并发下载模型（限制并发数）
    pub async fn download_models_concurrent(&self, model_ids: &[&str]) -> Result<Vec<PathBuf>> {
        let downloader = self.downloader.clone();
        let mut handles = Vec::new();

        for chunk in model_ids.chunks(self.max_concurrent_downloads) {
            let downloader = downloader.clone();
            let chunk: Vec<String> = chunk.iter().map(|s| s.to_string()).collect();

            let handle = tokio::spawn(async move {
                let downloader = downloader.lock().await;
                let mut paths = Vec::new();

                for model_id in chunk {
                    let path = downloader.download_model(&model_id).await?;
                    paths.push(path);
                }

                Ok::<Vec<PathBuf>, AleError>(paths)
            });

            handles.push(handle);
        }

        let mut all_paths = Vec::new();
        for handle in handles {
            let paths = handle
                .await
                .map_err(|e| AleError::Other(anyhow::anyhow!("Task join error: {}", e)))??;
            all_paths.extend(paths);
        }

        Ok(all_paths)
    }
}

#[cfg(test)]
mod package_tests {
    use super::*;
    use crate::model_scheduler::{ModelCapability, ModelPackage};

    fn manifest(bytes: &[u8]) -> ModelManifest {
        ModelManifest {
            schema_version: 1,
            packages: vec![ModelPackage {
                id: "test-model".to_string(),
                display_name: "Test Model".to_string(),
                license: "Apache-2.0".to_string(),
                capabilities: vec![ModelCapability::StateSummary],
                minimum_vram_bytes: 0,
                requires_explicit_consent: true,
                artifacts: vec![ModelArtifact {
                    filename: "model.bin".to_string(),
                    url: "https://example.invalid/model.bin".to_string(),
                    revision: "0123456789abcdef".to_string(),
                    sha256: format!("{:x}", Sha256::digest(bytes)),
                    size_bytes: bytes.len() as u64,
                }],
            }],
        }
    }

    #[test]
    fn consent_is_bound_to_license_and_sizes() {
        let manifest = manifest(b"model");
        let consent = ModelDownloader::package_consent(&manifest, "test-model").unwrap();
        assert_eq!(consent.license, "Apache-2.0");
        assert_eq!(consent.download_size_bytes, 5);
        assert_eq!(consent.required_disk_bytes, 10);
    }

    #[test]
    fn installed_package_is_reverified_before_use() {
        let root = std::env::temp_dir().join(format!("ale-model-{}", uuid::Uuid::new_v4()));
        let package_dir = root.join("test-model");
        std::fs::create_dir_all(&package_dir).unwrap();
        std::fs::write(package_dir.join("model.bin"), b"model").unwrap();
        let downloader = ModelDownloader::new(&root);
        assert!(downloader
            .verify_package(&manifest(b"model"), "test-model")
            .is_ok());
        std::fs::write(package_dir.join("model.bin"), b"tampered").unwrap();
        assert!(downloader
            .verify_package(&manifest(b"model"), "test-model")
            .is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
