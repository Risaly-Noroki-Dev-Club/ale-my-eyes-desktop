use ale_core::child_process::ManagedAsyncChild;
use ale_core::model_ipc::{
    read_message, write_message, IpcEnvelope, IpcReply, IpcRequestKind, MODEL_IPC_VERSION,
};
use ale_core::model_scheduler::LocalModelState;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(Default)]
pub struct SenseVoiceAdapter {
    worker: tokio::sync::Mutex<Option<AsrWorker>>,
    busy: AtomicBool,
    retired: std::sync::Mutex<Vec<AsrWorker>>,
}
struct AsrWorker {
    process: ManagedAsyncChild,
    input: tokio::process::ChildStdin,
    output: tokio::process::ChildStdout,
    model: String,
    tokens: String,
    last_used: Instant,
}
struct ActiveAsr<'a> {
    adapter: &'a SenseVoiceAdapter,
    worker: Option<AsrWorker>,
}
impl Drop for ActiveAsr<'_> {
    fn drop(&mut self) {
        if let Some(mut worker) = self.worker.take() {
            let _ = worker.process.start_kill();
            self.adapter
                .retired
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(worker);
            ale_core::diagnostics::record("asr_cancelled", &[]);
        }
    }
}
struct Busy<'a>(&'a AtomicBool);
impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
#[derive(serde::Serialize, serde::Deserialize)]
struct AsrRequest {
    config: ModelRuntimeConfig,
    wav: Vec<u8>,
}
impl SenseVoiceAdapter {
    pub fn supported() -> bool {
        cfg!(all(
            feature = "sensevoice",
            not(all(target_os = "windows", target_env = "gnu"))
        ))
    }
    pub fn available(config: &ModelRuntimeConfig) -> bool {
        NativeAdapter::available(config)
    }
    pub fn state(&self) -> LocalModelState {
        if self.busy.load(Ordering::Acquire) {
            return LocalModelState::Busy;
        }
        match self.worker.try_lock() {
            Ok(slot) if slot.is_some() => LocalModelState::Ready,
            _ => LocalModelState::Stopped,
        }
    }
    pub fn unload_if_idle(&self) {
        // Maintenance never waits for native work or a contested lock. An
        // unconfirmed exit remains owned here and prevents the next job.
        let Ok(mut retired) = self.retired.try_lock() else {
            return;
        };
        retired.retain_mut(|worker| !matches!(worker.process.try_wait(), Ok(Some(_))));
        if let Ok(mut slot) = self.worker.try_lock() {
            if slot
                .as_ref()
                .is_some_and(|worker| worker.last_used.elapsed() >= MODEL_IDLE_TTL)
            {
                if let Some(mut worker) = slot.take() {
                    let _ = worker.process.start_kill();
                    retired.push(worker);
                    ale_core::diagnostics::record("asr_idle_unload", &[]);
                }
            }
        }
    }
    async fn reap_cancelled(&self) -> Result<(), String> {
        loop {
            let worker = self.retired.lock().unwrap_or_else(|e| e.into_inner()).pop();
            let Some(worker) = worker else { return Ok(()) };
            // Dropping this future during recovery also retains ownership.
            let mut pending = ActiveAsr {
                adapter: self,
                worker: Some(worker),
            };
            if pending
                .worker
                .as_mut()
                .unwrap()
                .process
                .kill_tree_and_wait(Duration::from_secs(2))
                .await
                .is_err()
            {
                ale_core::diagnostics::record("asr_reap_unconfirmed", &[]);
                return Err("ASR_WORKER_EXIT_UNCONFIRMED".into());
            }
            pending.worker.take();
        }
    }
    pub async fn shutdown(&self) {
        let _ = self.reap_cancelled().await;
        if let Some(worker) = self.worker.lock().await.take() {
            let mut pending = ActiveAsr {
                adapter: self,
                worker: Some(worker),
            };
            if pending
                .worker
                .as_mut()
                .unwrap()
                .process
                .kill_tree_and_wait(Duration::from_secs(2))
                .await
                .is_ok()
            {
                pending.worker.take();
            }
        }
    }
    pub async fn transcribe_wav(
        &self,
        config: &ModelRuntimeConfig,
        wav: &[u8],
    ) -> Result<String, String> {
        if !Self::supported() {
            return Err("LOCAL_ASR_UNAVAILABLE".into());
        }
        let mut slot = self.worker.try_lock().map_err(|_| "ASR_BUSY")?;
        self.reap_cancelled().await?;
        self.busy.store(true, Ordering::Release);
        let _busy = Busy(&self.busy);
        let mut existing = slot.take();
        let reuse = existing.as_mut().is_some_and(|worker| {
            worker.model == config.sensevoice_model
                && worker.tokens == config.sensevoice_tokens
                && matches!(worker.process.try_wait(), Ok(None))
        });
        let worker = match existing {
            Some(worker) if reuse => worker,
            old => {
                if let Some(old) = old {
                    let mut pending = ActiveAsr {
                        adapter: self,
                        worker: Some(old),
                    };
                    pending
                        .worker
                        .as_mut()
                        .unwrap()
                        .process
                        .kill_tree_and_wait(Duration::from_secs(2))
                        .await
                        .map_err(|_| "ASR_WORKER_EXIT_UNCONFIRMED")?;
                    pending.worker.take();
                }
                let mut command = tokio::process::Command::new(
                    std::env::current_exe().map_err(|_| "ASR_EXECUTABLE")?,
                );
                command
                    .arg("--sensevoice-worker")
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null());
                let mut process =
                    ManagedAsyncChild::spawn(&mut command).map_err(|_| "ASR_START_FAILED")?;
                AsrWorker {
                    input: process.child.stdin.take().ok_or("ASR_STDIN")?,
                    output: process.child.stdout.take().ok_or("ASR_STDOUT")?,
                    process,
                    model: config.sensevoice_model.clone(),
                    tokens: config.sensevoice_tokens.clone(),
                    last_used: Instant::now(),
                }
            }
        };
        let mut active = ActiveAsr {
            adapter: self,
            worker: Some(worker),
        };
        let worker = active.worker.as_mut().unwrap();
        let request = IpcEnvelope {
            protocol_version: MODEL_IPC_VERSION,
            request_id: uuid::Uuid::new_v4().to_string(),
            kind: IpcRequestKind::Schedule as i32,
            payload: serde_json::to_vec(&AsrRequest {
                config: config.clone(),
                wav: wav.to_vec(),
            })
            .map_err(|_| "ASR_ENCODE")?,
        };
        let deadline = ale_core::model_api::deadline()
            .min(tokio::time::Instant::now() + ale_core::model_scheduler::MODEL_STAGE_TIMEOUT);
        let result = tokio::time::timeout_at(deadline, async {
            write_message(&mut worker.input, &request)
                .await
                .map_err(|_| "ASR_WRITE")?;
            let reply: IpcReply = read_message(&mut worker.output)
                .await
                .map_err(|_| "ASR_READ")?;
            if reply.request_id != request.request_id || reply.protocol_version != MODEL_IPC_VERSION
            {
                return Err("ASR_REPLY_MISMATCH");
            }
            if reply.status != 0 {
                return Err("ASR_TRANSCRIBE_FAILED");
            }
            String::from_utf8(reply.payload).map_err(|_| "ASR_REPLY_ENCODING")
        })
        .await
        .map_err(|_| "ASR_TIMEOUT")
        .and_then(|r| r);
        match result {
            Ok(text) => {
                worker.last_used = Instant::now();
                *slot = active.worker.take();
                Ok(text)
            }
            Err(code) => {
                if worker
                    .process
                    .kill_tree_and_wait(Duration::from_secs(2))
                    .await
                    .is_ok()
                {
                    active.worker.take();
                }
                ale_core::diagnostics::record("asr_job_failed", &[]);
                tracing::warn!(event = "asr_job_failed", code);
                Err(code.into())
            }
        }
    }
}

/// Native calls run only in this disposable child process.
pub async fn run_worker() -> anyhow::Result<()> {
    let adapter = NativeAdapter::default();
    let mut input = tokio::io::stdin();
    let mut output = tokio::io::stdout();
    loop {
        let request: IpcEnvelope = match read_message(&mut input).await {
            Ok(r) => r,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        let payload: AsrRequest = serde_json::from_slice(&request.payload)?;
        let result = adapter.transcribe_wav(&payload.config, &payload.wav);
        let reply = match result {
            Ok(text) => IpcReply {
                protocol_version: MODEL_IPC_VERSION,
                request_id: request.request_id,
                status: 0,
                payload: text.into_bytes(),
                error_code: String::new(),
                error_message: String::new(),
            },
            Err(_) => crate::scheduler::error_reply(
                request.request_id,
                "ASR_TRANSCRIBE_FAILED",
                "ASR_TRANSCRIBE_FAILED",
            ),
        };
        write_message(&mut output, &reply).await?;
    }
}

use ale_core::model_scheduler::{ModelRuntimeConfig, MODEL_IDLE_TTL};
use std::io::Cursor;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Default)]
struct NativeAdapter {
    loaded: Mutex<Option<LoadedRecognizer>>,
}

struct LoadedRecognizer {
    model_path: String,
    last_used: Instant,
    #[cfg(all(
        feature = "sensevoice",
        not(all(target_os = "windows", target_env = "gnu"))
    ))]
    recognizer: sherpa_rs::sense_voice::SenseVoiceRecognizer,
}

impl NativeAdapter {
    pub fn available(config: &ModelRuntimeConfig) -> bool {
        cfg!(all(
            feature = "sensevoice",
            not(all(target_os = "windows", target_env = "gnu"))
        )) && std::path::Path::new(&config.sensevoice_model).is_file()
            && std::path::Path::new(&config.sensevoice_tokens).is_file()
    }

    pub fn transcribe_wav(
        &self,
        config: &ModelRuntimeConfig,
        wav: &[u8],
    ) -> Result<String, String> {
        if !Self::available(config) {
            return Err("SenseVoiceSmall model is not installed".to_string());
        }
        let (sample_rate, samples) = decode_wav(wav)?;
        let samples = resample_linear(&samples, sample_rate, 16_000);
        let mut loaded = self
            .loaded
            .lock()
            .map_err(|_| "SenseVoice runtime lock poisoned".to_string())?;
        if loaded.as_ref().is_some_and(|current| {
            current.last_used.elapsed() >= MODEL_IDLE_TTL
                || current.model_path != config.sensevoice_model
        }) {
            loaded.take();
        }
        if loaded.is_none() {
            *loaded = Some(load_recognizer(config)?);
        }
        let current = loaded.as_mut().expect("SenseVoice recognizer loaded");
        current.last_used = Instant::now();
        transcribe(current, &samples)
    }

    #[allow(dead_code)]
    pub fn unload_if_idle(&self) {
        if let Ok(mut loaded) = self.loaded.try_lock() {
            if loaded
                .as_ref()
                .is_some_and(|current| current.last_used.elapsed() >= MODEL_IDLE_TTL)
            {
                loaded.take();
            }
        }
    }
}

#[cfg(all(
    feature = "sensevoice",
    not(all(target_os = "windows", target_env = "gnu"))
))]
fn load_recognizer(config: &ModelRuntimeConfig) -> Result<LoadedRecognizer, String> {
    let recognizer = sherpa_rs::sense_voice::SenseVoiceRecognizer::new(
        sherpa_rs::sense_voice::SenseVoiceConfig {
            model: config.sensevoice_model.clone(),
            tokens: config.sensevoice_tokens.clone(),
            language: "auto".to_string(),
            use_itn: true,
            provider: Some("cpu".to_string()),
            num_threads: Some(
                std::thread::available_parallelism()
                    .map(|value| value.get().min(4) as i32)
                    .unwrap_or(1),
            ),
            debug: false,
        },
    )
    .map_err(|error| error.to_string())?;
    Ok(LoadedRecognizer {
        model_path: config.sensevoice_model.clone(),
        last_used: Instant::now(),
        recognizer,
    })
}

#[cfg(not(all(
    feature = "sensevoice",
    not(all(target_os = "windows", target_env = "gnu"))
)))]
fn load_recognizer(_config: &ModelRuntimeConfig) -> Result<LoadedRecognizer, String> {
    Err("ale-modeld has no SenseVoice runtime for this build target".to_string())
}

#[cfg(all(
    feature = "sensevoice",
    not(all(target_os = "windows", target_env = "gnu"))
))]
fn transcribe(recognizer: &mut LoadedRecognizer, samples: &[f32]) -> Result<String, String> {
    let result = recognizer.recognizer.transcribe(16_000, samples);
    let text = result.text.trim().to_string();
    if text.is_empty() {
        Err("SenseVoice returned an empty transcript".to_string())
    } else {
        Ok(text)
    }
}

#[cfg(not(all(
    feature = "sensevoice",
    not(all(target_os = "windows", target_env = "gnu"))
)))]
fn transcribe(_recognizer: &mut LoadedRecognizer, _samples: &[f32]) -> Result<String, String> {
    Err("ale-modeld has no SenseVoice runtime for this build target".to_string())
}

fn decode_wav(wav: &[u8]) -> Result<(u32, Vec<f32>), String> {
    let mut reader = hound::WavReader::new(Cursor::new(wav)).map_err(|error| error.to_string())?;
    let spec = reader.spec();
    if spec.sample_rate == 0
        || spec.channels != 1
        || spec.bits_per_sample != 16
        || spec.sample_format != hound::SampleFormat::Int
    {
        return Err("SenseVoice requires mono PCM S16LE WAV input".to_string());
    }
    let samples = reader
        .samples::<i16>()
        .map(|sample| sample.map(|value| f32::from(value) / 32768.0))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?;
    if samples.is_empty() {
        return Err("audio is empty".to_string());
    }
    Ok((spec.sample_rate, samples))
}

fn resample_linear(samples: &[f32], source_rate: u32, target_rate: u32) -> Vec<f32> {
    if source_rate == target_rate || samples.len() < 2 {
        return samples.to_vec();
    }
    let output_len =
        ((samples.len() as u64 * u64::from(target_rate)) / u64::from(source_rate)).max(1) as usize;
    let scale = source_rate as f64 / target_rate as f64;
    (0..output_len)
        .map(|index| {
            let source = index as f64 * scale;
            let left = source.floor() as usize;
            let right = (left + 1).min(samples.len() - 1);
            let fraction = (source - left as f64) as f32;
            samples[left] * (1.0 - fraction) + samples[right] * fraction
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_preserves_duration() {
        let input = vec![0.25; 48_000];
        let output = resample_linear(&input, 48_000, 16_000);
        assert_eq!(output.len(), 16_000);
        assert!(output
            .iter()
            .all(|sample| (*sample - 0.25).abs() < f32::EPSILON));
    }
}
