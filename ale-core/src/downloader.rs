use crate::model_scheduler::{ModelArtifact, ModelManifest};
use crate::{AleError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt;

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
#[derive(Clone)]
pub struct ModelDownloader {
    models_dir: PathBuf,
    progress_callback: Option<Arc<dyn Fn(DownloadProgress) + Send + Sync>>,
    client: reqwest::Client,
    known_models: Vec<ModelInfo>,
    active: Arc<std::sync::Mutex<HashSet<PathBuf>>>,
    slots: Arc<tokio::sync::Semaphore>,
    #[cfg(test)]
    test_url: Option<String>,
}

impl ModelDownloader {
    pub fn new(models_dir: &Path) -> Self {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(20))
            .read_timeout(std::time::Duration::from_secs(60))
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self {
            models_dir: models_dir.to_path_buf(),
            progress_callback: None,
            client,
            known_models: Self::default_known_models(),
            active: Arc::new(std::sync::Mutex::new(HashSet::new())),
            slots: Arc::new(tokio::sync::Semaphore::new(3)),
            #[cfg(test)]
            test_url: None,
        }
    }

    /// 设置进度回调
    pub fn set_progress_callback(&mut self, callback: ProgressCallback) {
        self.progress_callback = Some(callback.into());
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

    async fn run_owned<T: Send + 'static>(
        &self,
        run: impl FnOnce(
                Self,
                Arc<AtomicBool>,
            )
                -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T>> + Send>>
            + Send
            + 'static,
    ) -> Result<T> {
        let permit = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| AleError::ConfigError("Model download worker is busy".into()))?;
        let cancel = Arc::new(AtomicBool::new(false));
        let guard = CancelOnDrop(cancel.clone());
        let downloader = self.clone();
        let task = tokio::spawn(async move {
            let _permit = permit;
            run(downloader, cancel).await
        });
        let result = task
            .await
            .map_err(|_| AleError::ConfigError("Model download worker failed".into()))?;
        drop(guard);
        result
    }

    pub async fn install_package(
        &self,
        manifest: &ModelManifest,
        consent: &ModelInstallConsent,
    ) -> Result<InstalledModelPackage> {
        let manifest = manifest.clone();
        let consent = consent.clone();
        self.run_owned(move |downloader, cancel| {
            Box::pin(async move {
                downloader
                    .install_package_inner(&manifest, &consent, cancel)
                    .await
            })
        })
        .await
    }

    fn reserve(&self, target: PathBuf) -> Result<ActiveTarget> {
        let mut active = self
            .active
            .lock()
            .map_err(|_| AleError::ConfigError("Download state unavailable".into()))?;
        if !active.insert(target.clone()) {
            return Err(AleError::ConfigError(
                "A download for this destination is already active".into(),
            ));
        }
        Ok(ActiveTarget {
            active: self.active.clone(),
            target,
        })
    }

    async fn install_package_inner(
        &self,
        manifest: &ModelManifest,
        consent: &ModelInstallConsent,
        cancel: Arc<AtomicBool>,
    ) -> Result<InstalledModelPackage> {
        let package = manifest.package(&consent.package_id)?;
        let expected = Self::package_consent(manifest, &package.id)?;
        if !package.requires_explicit_consent || consent != &expected {
            return Err(AleError::ConfigError(
                "model download consent does not match the pinned package metadata".to_string(),
            ));
        }

        let package_dir = self.models_dir.join(&package.id);
        let _active = self.reserve(package_dir.clone())?;
        tokio::fs::create_dir_all(&package_dir).await?;
        let mut installed = Vec::with_capacity(package.artifacts.len());
        for artifact in &package.artifacts {
            let target = package_dir.join(&artifact.filename);
            check_cancel(&cancel)?;
            if tokio::fs::try_exists(&target).await? {
                let path = target.clone();
                let artifact = artifact.clone();
                let flag = cancel.clone();
                tokio::task::spawn_blocking(move || {
                    verify_artifact_cancellable(&path, &artifact, &flag)
                })
                .await
                .map_err(|_| AleError::ConfigError("Model verification worker failed".into()))??;
            } else {
                self.download_pinned_artifact(&package.id, artifact, &target, &cancel)
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
        cancel: &AtomicBool,
    ) -> Result<()> {
        let response = tokio::select! {
            response = self.client.get(&artifact.url).send() => response,
            _ = cancelled(cancel) => return Err(cancel_error()),
        }
        .map_err(|error| AleError::Other(anyhow::anyhow!("Download request failed: {error}")))?;
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
            let mut file = tokio::fs::File::create(&temp).await?;
            let mut hasher = Sha256::new();
            let mut downloaded = 0_u64;
            let started = std::time::Instant::now();
            let mut stream = response.bytes_stream();
            use futures::StreamExt;
            loop {
                let chunk = tokio::select! { chunk = stream.next() => chunk, _ = cancelled(cancel) => return Err(cancel_error()) };
                let Some(chunk) = chunk else { break };
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
                file.write_all(&chunk).await?;
                hasher.update(&chunk);
                self.report_package_progress(package_id, artifact.size_bytes, downloaded, started);
            }
            file.sync_all().await?;
            drop(file);
            check_cancel(cancel)?;
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
            tokio::fs::rename(&temp, target).await?;
            Ok(())
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temp).await;
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
                progress: if total_bytes == 0 {
                    0.0
                } else {
                    (downloaded_bytes as f32 / total_bytes as f32).min(1.0)
                },
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
        let id = model_id.to_owned();
        self.run_owned(move |downloader, cancel| {
            Box::pin(async move { downloader.download_model_inner(&id, cancel).await })
        })
        .await
    }

    async fn download_model_inner(
        &self,
        model_id: &str,
        cancel: Arc<AtomicBool>,
    ) -> Result<PathBuf> {
        let model = self
            .get_model_info(model_id)
            .ok_or_else(|| AleError::ConfigError("Unknown model".into()))?
            .clone();
        let target = self.models_dir.join(&model.filename);
        let _active = self.reserve(target.clone())?;
        if tokio::fs::try_exists(&target).await? {
            return Ok(target);
        }
        tokio::fs::create_dir_all(&self.models_dir).await?;
        let url = format!(
            "https://huggingface.co/{}/resolve/main/{}",
            model.repo, model.filename
        );
        #[cfg(test)]
        let url = self.test_url.clone().unwrap_or(url);
        let response = tokio::select! {
            response = self.client.get(url).send() => response.map_err(|_| AleError::CloudApiError("Model download connection failed".into()))?,
            _ = cancelled(&cancel) => return Err(cancel_error()),
        };
        if !response.status().is_success() {
            return Err(AleError::CloudApiError(format!(
                "Model download HTTP {}",
                response.status().as_u16()
            )));
        }
        let expected = response.content_length();
        let total = expected.unwrap_or(model.size);
        let temp = target.with_extension(format!("{}.partial", uuid::Uuid::new_v4()));
        let result = async {
            let mut file = tokio::fs::File::create(&temp).await?;
            let started = std::time::Instant::now();
            let mut downloaded = 0u64;
            let mut stream = response.bytes_stream();
            use futures::StreamExt;
            loop {
                let chunk = tokio::select! { chunk = stream.next() => chunk, _ = cancelled(&cancel) => return Err(cancel_error()) };
                let Some(chunk) = chunk else { break };
                let chunk = chunk.map_err(|_| AleError::CloudApiError("Model download interrupted".into()))?;
                file.write_all(&chunk).await?;
                downloaded = downloaded.saturating_add(chunk.len() as u64);
                self.report_package_progress(model_id, total, downloaded, started);
            }
            file.sync_all().await?;
            drop(file);
            check_cancel(&cancel)?;
            if expected.is_some_and(|size| size != downloaded) { return Err(AleError::ConfigError("Model download incomplete".into())); }
            if tokio::fs::try_exists(&target).await? { return Err(AleError::ConfigError("Destination appeared during download; existing model preserved".into())); }
            tokio::fs::rename(&temp, &target).await?;
            Ok(target.clone())
        }.await;
        let _ = tokio::fs::remove_file(&temp).await;
        result
    }

    pub async fn verify_package_async(
        &self,
        manifest: &ModelManifest,
        package_id: &str,
    ) -> Result<InstalledModelPackage> {
        let manifest = manifest.clone();
        let id = package_id.to_owned();
        self.run_owned(move |downloader, cancel| {
            Box::pin(async move {
                tokio::task::spawn_blocking(move || {
                    let package = manifest.package(&id)?;
                    let directory = downloader.models_dir.join(&id);
                    let mut artifacts = Vec::new();
                    for artifact in &package.artifacts {
                        let path = directory.join(&artifact.filename);
                        verify_artifact_cancellable(&path, artifact, &cancel)?;
                        artifacts.push(path);
                    }
                    Ok(InstalledModelPackage {
                        package_id: id,
                        directory,
                        artifacts,
                    })
                })
                .await
                .map_err(|_| AleError::ConfigError("Model verification worker failed".into()))?
            })
        })
        .await
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
    verify_artifact_cancellable(path, artifact, &AtomicBool::new(false))
}

fn verify_artifact_cancellable(
    path: &Path,
    artifact: &ModelArtifact,
    cancel: &AtomicBool,
) -> Result<()> {
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
        check_cancel(cancel)?;
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
    downloader: ModelDownloader,
    max_concurrent_downloads: usize,
}

impl ModelDownloadManager {
    pub fn new(models_dir: &Path, max_concurrent: usize) -> Self {
        let max = max_concurrent.max(1);
        let mut downloader = ModelDownloader::new(models_dir);
        downloader.slots = Arc::new(tokio::sync::Semaphore::new(max));
        Self {
            downloader,
            max_concurrent_downloads: max,
        }
    }

    pub async fn download_models(&self, model_ids: &[&str]) -> Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        for id in model_ids {
            paths.push(self.downloader.download_model(id).await?);
        }
        Ok(paths)
    }

    pub async fn download_models_concurrent(&self, model_ids: &[&str]) -> Result<Vec<PathBuf>> {
        use futures::{stream::FuturesUnordered, StreamExt};
        let cancel = Arc::new(AtomicBool::new(false));
        let guard = CancelOnDrop(cancel.clone());
        let downloader = self.downloader.clone();
        let ids: Vec<String> = model_ids.iter().map(|id| (*id).to_owned()).collect();
        let max = self.max_concurrent_downloads;
        // Supervisor outlives an abandoned waiter and joins every active transfer before exit.
        let task = tokio::spawn(async move {
            let mut next = ids.into_iter().enumerate();
            let mut active = FuturesUnordered::new();
            let mut results = Vec::new();
            let mut failure = None;
            loop {
                while failure.is_none() && !cancel.load(Ordering::Relaxed) && active.len() < max {
                    let Some((index, id)) = next.next() else {
                        break;
                    };
                    let downloader = downloader.clone();
                    let cancel = cancel.clone();
                    active.push(async move {
                        let permit =
                            downloader.slots.clone().try_acquire_owned().map_err(|_| {
                                AleError::ConfigError("Model download worker is busy".into())
                            })?;
                        let result = downloader.download_model_inner(&id, cancel).await;
                        drop(permit);
                        result.map(|path| (index, path))
                    });
                }
                let Some(result) = active.next().await else {
                    break;
                };
                match result {
                    Ok(result) => results.push(result),
                    Err(error) => {
                        if failure.is_none() {
                            failure = Some(error);
                        }
                        cancel.store(true, Ordering::Relaxed);
                    }
                }
            }
            if let Some(error) = failure {
                return Err(error);
            }
            check_cancel(&cancel)?;
            results.sort_by_key(|entry| entry.0);
            Ok(results.into_iter().map(|entry| entry.1).collect())
        });
        let result = task
            .await
            .map_err(|_| AleError::ConfigError("Download supervisor failed".into()))?;
        drop(guard);
        result
    }
}

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
struct ActiveTarget {
    active: Arc<std::sync::Mutex<HashSet<PathBuf>>>,
    target: PathBuf,
}
impl Drop for ActiveTarget {
    fn drop(&mut self) {
        if let Ok(mut active) = self.active.lock() {
            active.remove(&self.target);
        }
    }
}
fn cancel_error() -> AleError {
    crate::model_api::ModelCallError::new(
        crate::model_api::ErrorKind::Cancelled,
        "Model download cancelled",
    )
    .into()
}
fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(cancel_error())
    } else {
        Ok(())
    }
}
async fn cancelled(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Relaxed) {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod package_tests {
    use super::*;
    use crate::model_scheduler::{ModelCapability, ModelPackage};

    #[tokio::test]
    async fn batch_downloads_overlap_and_finish_without_temporary_files() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = std::env::temp_dir().join(format!("ale-parallel-{}", uuid::Uuid::new_v4()));
        let mut manager = ModelDownloadManager::new(&root, 2);
        manager.downloader.test_url = Some(format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move {
            let mut sockets = Vec::new();
            for _ in 0..2 {
                let (mut socket, _) =
                    tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let mut input = [0u8; 4096];
                assert!(socket.read(&mut input).await.unwrap() > 0);
                sockets.push(socket);
            }
            // Neither response is sent until both requests are in flight.
            for mut socket in sockets {
                socket
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nConnection: close\r\n\r\nmodel",
                    )
                    .await
                    .unwrap();
            }
        });
        let paths = manager
            .download_models_concurrent(&["whisper-tiny", "whisper-small"])
            .await
            .unwrap();
        server.await.unwrap();
        assert_eq!(paths.len(), 2);
        for path in paths {
            assert_eq!(std::fs::read(path).unwrap(), b"model");
        }
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
        assert!(manager.downloader.active.lock().unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn dropping_download_waiter_cancels_request_and_releases_destination() {
        use tokio::io::AsyncReadExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = std::env::temp_dir().join(format!("ale-cancel-{}", uuid::Uuid::new_v4()));
        let mut downloader = ModelDownloader::new(&root);
        downloader.test_url = Some(format!("http://{}", listener.local_addr().unwrap()));
        let observed = downloader.clone();
        let task = tokio::spawn(async move { downloader.download_model("whisper-tiny").await });
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = [0u8; 4096];
        assert!(socket.read(&mut bytes).await.unwrap() > 0);
        task.abort();
        let _ = task.await;
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(2), socket.read(&mut bytes))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        for _ in 0..100 {
            if observed.active.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(observed.active.lock().unwrap().is_empty());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        std::fs::remove_dir(root).unwrap();
    }

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
