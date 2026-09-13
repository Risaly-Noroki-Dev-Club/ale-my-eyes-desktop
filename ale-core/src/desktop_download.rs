//! Native downloads for the same pinned models as scripts/download-models.bat.
use anyhow::{bail, Context, Result};
use futures::StreamExt;
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

pub const NAMES: [&str; 3] = ["SenseVoiceSmall", "Qwen2.5-VL-7B-Instruct", "ShowUI-2B"];
const SENSE: &str = "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17";
const ARCHIVE_SHA: &str = "7d1efa2138a65b0b488df37f8b89e3d91a60676e416f515b952358d83dfd347e";

#[derive(Clone, Default, Debug)]
pub struct Progress {
    pub downloaded: u64,
    pub total: u64,
    pub file: String,
    pub phase: &'static str,
    pub phase_completed: u64,
    pub phase_total: u64,
}

fn safe_relative(name: &str) -> Result<PathBuf> {
    let path = Path::new(name);
    if name.is_empty()
        || name.contains(['\\', ':'])
        || !path.components().all(|c| matches!(c, Component::Normal(_)))
    {
        bail!("Invalid model file path");
    }
    Ok(path.to_owned())
}

/// Cancellation drops pending HTTP requests; disk work never runs on the UI thread.
pub async fn download(
    index: usize,
    root: PathBuf,
    cancel: Arc<AtomicBool>,
    report: Arc<dyn Fn(Progress) + Send + Sync>,
) -> Result<()> {
    struct CancelOnDrop(Arc<AtomicBool>);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let guard = CancelOnDrop(cancel.clone());
    // The service owns extraction and commit even if its UI waiter disappears.
    let worker = tokio::spawn(download_owned(index, root, cancel, report));
    let result = worker.await.context("download worker failed")?;
    drop(guard);
    result
}

async fn download_owned(
    index: usize,
    root: PathBuf,
    cancel: Arc<AtomicBool>,
    report: Arc<dyn Fn(Progress) + Send + Sync>,
) -> Result<()> {
    if index >= NAMES.len() {
        bail!("Unknown model");
    }
    tokio::fs::create_dir_all(&root).await?;
    let lock = tokio::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(format!(".ale-install-{index}.lock")))
        .await?
        .into_std()
        .await;
    lock.try_lock()
        .map_err(|_| anyhow::anyhow!("MODEL_INSTALL_ALREADY_ACTIVE"))?;
    let _destination_lease = lock;
    if tokio::fs::try_exists(root.join(NAMES[index])).await? {
        bail!("模型目录已存在，请选择其他下载目录以保留已有文件 / Destination already exists");
    }
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .read_timeout(Duration::from_secs(60))
        .build()?;
    tokio::fs::create_dir_all(&root).await?;
    let stage = root.join(format!(".ale-download-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir(&stage).await?;
    report(Progress {
        phase: "connecting",
        ..Default::default()
    });
    let work = async {
        if index == 0 {
            let archive = stage.join("sense.tar.bz2");
            let url = format!("https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/{SENSE}.tar.bz2");
            fetch(
                &client,
                &url,
                &archive,
                Some(ARCHIVE_SHA),
                &cancel,
                &report,
                &mut Progress::default(),
            )
            .await?;
            let extraction = stage.clone();
            let flag = cancel.clone();
            let extraction_report = report.clone();
            // The extraction worker is joined before its staging directory is cleaned up.
            tokio::task::spawn_blocking(move || -> Result<()> {
                let decoder = bzip2::read::BzDecoder::new(std::fs::File::open(archive)?);
                let mut archive = tar::Archive::new(CancellableReader {
                    inner: decoder,
                    cancel: flag.clone(),
                });
                for entry in archive.entries()? {
                    if flag.load(Ordering::Relaxed) {
                        bail!("Download cancelled");
                    }
                    let mut entry = entry?;
                    let path = entry.path()?.into_owned();
                    if !entry.header().entry_type().is_file() {
                        continue;
                    }
                    let Some(name) = path.strip_prefix(SENSE).ok().and_then(|p| p.to_str()) else {
                        continue;
                    };
                    if !["model.int8.onnx", "tokens.txt", "LICENSE", "README.md"].contains(&name) {
                        continue;
                    }
                    let total = entry.size();
                    let mut file = std::fs::File::create(extraction.join(name))?;
                    copy_chunks(
                        &mut entry,
                        &mut file,
                        &flag,
                        &extraction_report,
                        "extracting",
                        name,
                        total,
                    )?;
                    file.sync_all()?;
                }
                for (file, expected) in [
                    (
                        "model.int8.onnx",
                        "c71f0ce00bec95b07744e116345e33d8cbbe08cef896382cf907bf4b51a2cd51",
                    ),
                    (
                        "tokens.txt",
                        "f449eb28dc567533d7fa59be34e2abca8784f771850c78a47fb731a31429a1dc",
                    ),
                ] {
                    let mut input = std::fs::File::open(extraction.join(file))?;
                    let mut hash = Sha256::new();
                    let total = input.metadata()?.len();
                    copy_chunks(
                        &mut input,
                        &mut hash,
                        &flag,
                        &extraction_report,
                        "verifying",
                        file,
                        total,
                    )?;
                    if format!("{:x}", hash.finalize()) != expected {
                        bail!("Model checksum mismatch");
                    }
                }
                Ok(())
            })
            .await??;
            tokio::fs::remove_file(stage.join("sense.tar.bz2")).await?;
        } else {
            let (repo, revision) = if index == 1 {
                (
                    "Qwen/Qwen2.5-VL-7B-Instruct",
                    "cc594898137f460bfe9f0759e9844b3ce807cfb5",
                )
            } else {
                (
                    "showlab/ShowUI-2B",
                    "cabec4fcc48d15ffd3efe0b33ea9bc7d41509d60",
                )
            };
            let metadata: serde_json::Value = tokio::select! {
                result = async {
                    client.get(format!("https://huggingface.co/api/models/{repo}/revision/{revision}?blobs=true"))
                        .send().await?.error_for_status()?.json::<serde_json::Value>().await
                } => result?,
                _ = cancelled(&cancel) => bail!("下载已取消 / Download cancelled"),
            };
            let files = metadata["siblings"]
                .as_array()
                .context("Missing model file list")?;
            if files.is_empty() {
                bail!("Empty model file list");
            }
            let mut progress = Progress {
                total: files.iter().filter_map(|f| f["size"].as_u64()).sum(),
                ..Default::default()
            };
            for file in files {
                if cancel.load(Ordering::Relaxed) {
                    bail!("Download cancelled");
                }
                let name = file["rfilename"]
                    .as_str()
                    .context("Missing model filename")?;
                let target = stage.join(safe_relative(name)?);
                let mut url = reqwest::Url::parse(&format!(
                    "https://huggingface.co/{repo}/resolve/{revision}/"
                ))?;
                url.path_segments_mut()
                    .map_err(|_| anyhow::anyhow!("Invalid download URL"))?
                    .pop_if_empty()
                    .extend(name.split('/'));
                fetch(
                    &client,
                    url.as_str(),
                    &target,
                    file["lfs"]["sha256"].as_str(),
                    &cancel,
                    &report,
                    &mut progress,
                )
                .await?;
            }
        }
        if cancel.load(Ordering::Relaxed) {
            bail!("Download cancelled");
        }
        report(Progress {
            phase: "committing",
            ..Default::default()
        });
        // Never replace an existing installation or user-owned directory.
        let destination = root.join(NAMES[index]);
        if tokio::fs::try_exists(&destination).await? {
            bail!("Destination already exists; choose a different download directory to preserve existing files");
        }
        tokio::fs::rename(&stage, destination).await?;
        Ok(())
    };
    // Do not drop the extraction future: it owns a blocking worker. Cancellation
    // is checked between entries; HTTP is interrupted promptly by fetch below.
    let result = work.await;
    if cancel.load(Ordering::Relaxed) {
        report(Progress {
            phase: "cancelling",
            ..Default::default()
        });
    }
    let _ = tokio::fs::remove_dir_all(&stage).await;
    result
}

struct CancellableReader<R> {
    inner: R,
    cancel: Arc<AtomicBool>,
}
impl<R: std::io::Read> std::io::Read for CancellableReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.load(Ordering::Relaxed) {
            return Err(std::io::Error::other("Download cancelled"));
        }
        self.inner.read(buffer)
    }
}

fn copy_chunks(
    input: &mut impl std::io::Read,
    output: &mut impl std::io::Write,
    cancel: &AtomicBool,
    report: &Arc<dyn Fn(Progress) + Send + Sync>,
    phase: &'static str,
    file: &str,
    total: u64,
) -> Result<()> {
    let mut buffer = [0u8; 64 * 1024];
    let mut done = 0u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            bail!("Download cancelled");
        }
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        output.write_all(&buffer[..count])?;
        done += count as u64;
        report(Progress {
            phase,
            file: file.into(),
            phase_completed: done,
            phase_total: total,
            ..Default::default()
        });
    }
    Ok(())
}

async fn cancelled(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn fetch(
    client: &reqwest::Client,
    url: &str,
    path: &Path,
    sha: Option<&str>,
    cancel: &AtomicBool,
    report: &Arc<dyn Fn(Progress) + Send + Sync>,
    progress: &mut Progress,
) -> Result<()> {
    let work = async {
        let response = client.get(url).send().await?.error_for_status()?;
        let expected = response.content_length();
        if progress.total == 0 {
            progress.total = expected.unwrap_or(0);
        }
        progress.phase = "downloading";
        progress.phase_completed = 0;
        progress.phase_total = expected.unwrap_or(0);
        progress.file = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        tokio::fs::create_dir_all(path.parent().context("Missing parent")?).await?;
        let mut file = tokio::fs::File::create(path).await?;
        let mut hash = Sha256::new();
        let mut received = 0;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            file.write_all(&chunk).await?;
            hash.update(&chunk);
            received += chunk.len() as u64;
            progress.downloaded += chunk.len() as u64;
            progress.phase_completed = received;
            report(progress.clone());
        }
        file.sync_all().await?;
        if expected.is_some_and(|size| size != received) {
            bail!("Incomplete model file");
        }
        if sha.is_some_and(|expected| format!("{:x}", hash.finalize()) != expected) {
            bail!("Model checksum mismatch");
        }
        Ok(())
    };
    tokio::select! {
        result = work => result,
        _ = cancelled(cancel) => bail!("下载已取消 / Download cancelled"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    #[test]
    fn extraction_and_verification_check_cancellation_between_blocks() {
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let report: Arc<dyn Fn(Progress) + Send + Sync> = Arc::new(move |progress| {
            assert_eq!(progress.phase, "verifying");
            assert!(progress.phase_completed > 0);
            flag.store(true, Ordering::Relaxed);
        });
        let source = vec![1u8; 256 * 1024];
        let mut input = std::io::Cursor::new(source);
        let mut output = Vec::new();
        assert!(copy_chunks(
            &mut input,
            &mut output,
            &cancel,
            &report,
            "verifying",
            "model",
            256 * 1024
        )
        .is_err());
        assert_eq!(output.len(), 64 * 1024);
    }

    async fn server(body: &'static [u8], stall: bool) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/model", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            if stall {
                std::future::pending::<()>().await;
            }
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
            socket.write_all(body).await.unwrap();
        });
        (url, task)
    }

    #[tokio::test]
    async fn download_validates_bytes_and_reports_progress() {
        let root = std::env::temp_dir().join(format!("ale-fetch-{}", uuid::Uuid::new_v4()));
        let body = b"test model contents";
        let expected = format!("{:x}", Sha256::digest(body));
        let (url, server) = server(body, false).await;
        let report: Arc<dyn Fn(Progress) + Send + Sync> = Arc::new(|_| {});
        let mut progress = Progress::default();
        fetch(
            &reqwest::Client::new(),
            &url,
            &root.join("model"),
            Some(&expected),
            &AtomicBool::new(false),
            &report,
            &mut progress,
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(progress.downloaded, body.len() as u64);
        assert_eq!(std::fs::read(root.join("model")).unwrap(), body);
        let (url, server) = self::server(body, false).await;
        assert!(fetch(
            &reqwest::Client::new(),
            &url,
            &root.join("bad"),
            Some("wrong"),
            &AtomicBool::new(false),
            &report,
            &mut Progress::default()
        )
        .await
        .unwrap_err()
        .to_string()
        .contains("checksum"));
        server.await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn cancel_interrupts_a_server_that_never_sends_headers() {
        let (url, server) = server(b"", true).await;
        let flag = Arc::new(AtomicBool::new(false));
        let cancel = flag.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel.store(true, Ordering::Relaxed);
        });
        let report: Arc<dyn Fn(Progress) + Send + Sync> = Arc::new(|_| {});
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            fetch(
                &reqwest::Client::new(),
                &url,
                Path::new("unused"),
                None,
                &flag,
                &report,
                &mut Progress::default(),
            ),
        )
        .await
        .unwrap();
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        server.abort();
    }

    #[tokio::test]
    async fn existing_installation_is_not_modified() {
        let root = std::env::temp_dir().join(format!("ale-preserve-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(NAMES[0])).unwrap();
        std::fs::write(root.join(NAMES[0]).join("model"), b"existing").unwrap();
        assert!(download(
            0,
            root.clone(),
            Arc::new(AtomicBool::new(false)),
            Arc::new(|_| {})
        )
        .await
        .is_err());
        assert_eq!(
            std::fs::read(root.join(NAMES[0]).join("model")).unwrap(),
            b"existing"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn model_paths_cannot_escape_destination() {
        for name in [
            "../secret",
            "/tmp/file",
            "C:\\file",
            "a/../../b",
            "a\\b",
            "",
        ] {
            assert!(safe_relative(name).is_err(), "{name}");
        }
        assert_eq!(
            safe_relative("weights/model.safetensors").unwrap(),
            Path::new("weights/model.safetensors")
        );
    }
}
