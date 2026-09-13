use crate::{AleError, Result};
use async_trait::async_trait;
use std::sync::Mutex;

/// 语音合成trait
#[async_trait]
pub trait TextToSpeech: Send + Sync {
    /// 合成语音
    ///
    /// 对于 SystemTts，音频会直接通过系统扬声器播放。
    /// 返回的 Vec<u8> 在系统 TTS 场景下为空，调用方应检查长度判断是否需要云端 TTS 回退。
    async fn synthesize(&self, text: &str) -> Result<Vec<u8>>;

    /// 流式合成语音
    async fn synthesize_stream(&self, text: &str) -> Result<Box<dyn tokio::io::AsyncRead + Unpin>>;

    /// 获取可用语音列表
    fn available_voices(&self) -> Vec<String>;

    /// 获取模型信息
    fn model_info(&self) -> crate::ModelInfo;
}

/// 系统TTS引擎
///
/// 使用操作系统内置的 TTS 引擎：
/// - macOS: AVSpeechSynthesizer (via tts crate)
/// - Windows: SAPI
/// - Linux: speech-dispatcher
///
/// `speak()` 通过系统扬声器直接播放音频，不返回原始音频字节。
pub struct SystemTts {
    sender: std::sync::mpsc::SyncSender<SpeechJob>,
    slot: std::sync::Arc<tokio::sync::Semaphore>,
    voices: std::sync::Arc<Mutex<Vec<String>>>,
    loaded: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

struct SpeechJob {
    text: String,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _permit: tokio::sync::OwnedSemaphorePermit,
    reply: tokio::sync::oneshot::Sender<Result<Vec<u8>>>,
}
struct CancelSpeech(std::sync::Arc<std::sync::atomic::AtomicBool>);
impl Drop for CancelSpeech {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

impl SystemTts {
    pub async fn new(voice: Option<&str>) -> Result<Self> {
        let voice = voice.map(str::to_owned);
        let (sender, receiver) = std::sync::mpsc::sync_channel::<SpeechJob>(1);
        let voices = std::sync::Arc::new(Mutex::new(vec!["default".into()]));
        let loaded = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_voices = voices.clone();
        let worker_loaded = loaded.clone();
        // The OS engine is created, used and dropped on the same dedicated thread.
        std::thread::Builder::new()
            .name("ale-system-tts".into())
            .spawn(move || {
                let mut engine: Option<tts::Tts> = None;
                while let Ok(job) = receiver.recv() {
                    let result = (|| -> Result<Vec<u8>> {
                        use std::sync::atomic::Ordering;
                        if job.cancel.load(Ordering::Relaxed) {
                            return Err(AleError::TtsError("Speech cancelled".into()));
                        }
                        if engine.is_none() {
                            let mut created = tts::Tts::default().map_err(|_| {
                                AleError::TtsError("Cannot initialize system speech".into())
                            })?;
                            if let Ok(available) = created.voices() {
                                if let Some(selected) =
                                    available.iter().find(|v| Some(v.name()) == voice)
                                {
                                    created.set_voice(selected).map_err(|_| {
                                        AleError::TtsError("Cannot select speech voice".into())
                                    })?;
                                }
                                if let Ok(mut cache) = worker_voices.lock() {
                                    *cache = available.iter().map(|v| v.name()).collect();
                                }
                            }
                            engine = Some(created);
                            worker_loaded.store(true, Ordering::Relaxed);
                        }
                        let current = engine.as_mut().unwrap();
                        current
                            .speak(&job.text, true)
                            .map_err(|_| AleError::TtsError("Cannot start system speech".into()))?;
                        loop {
                            if job.cancel.load(Ordering::Relaxed) {
                                let _ = current.stop();
                                return Err(AleError::TtsError("Speech cancelled".into()));
                            }
                            if !current.is_speaking().map_err(|_| {
                                AleError::TtsError("Cannot query system speech".into())
                            })? {
                                break;
                            }
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                        Ok(Vec::new())
                    })();
                    let _ = job.reply.send(result);
                }
            })
            .map_err(|_| AleError::TtsError("Cannot start speech worker".into()))?;
        Ok(Self {
            sender,
            slot: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
            voices,
            loaded,
        })
    }
}

#[async_trait]
impl TextToSpeech for SystemTts {
    async fn synthesize(&self, text: &str) -> Result<Vec<u8>> {
        let permit = self
            .slot
            .clone()
            .try_acquire_owned()
            .map_err(|_| AleError::TtsError("Speech worker is busy".into()))?;
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let guard = CancelSpeech(cancel.clone());
        let (reply, response) = tokio::sync::oneshot::channel();
        self.sender
            .try_send(SpeechJob {
                text: text.into(),
                cancel,
                _permit: permit,
                reply,
            })
            .map_err(|_| AleError::TtsError("Speech worker unavailable".into()))?;
        let deadline = crate::model_api::deadline()
            .min(tokio::time::Instant::now() + std::time::Duration::from_secs(80));
        let result = tokio::time::timeout_at(deadline, response)
            .await
            .map_err(|_| AleError::TtsError("Speech timed out; cancellation requested".into()))?
            .map_err(|_| AleError::TtsError("Speech worker failed".into()))?;
        drop(guard);
        result
    }

    async fn synthesize_stream(&self, text: &str) -> Result<Box<dyn tokio::io::AsyncRead + Unpin>> {
        self.synthesize(text).await?;
        Ok(Box::new(tokio::io::empty()))
    }

    /// Cached voices, populated by the speech worker after first initialization.
    fn available_voices(&self) -> Vec<String> {
        self.voices.lock().map(|v| v.clone()).unwrap_or_default()
    }

    fn model_info(&self) -> crate::ModelInfo {
        crate::ModelInfo {
            name: "system-tts".into(),
            version: "1.0".into(),
            device: "system".into(),
            loaded: self.loaded.load(std::sync::atomic::Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_system_tts_new() {
        let tts = SystemTts::new(None).await;
        assert!(tts.is_ok());
    }

    #[tokio::test]
    async fn test_system_tts_with_voice() {
        let tts = SystemTts::new(Some("female")).await;
        assert!(tts.is_ok());
    }

    #[tokio::test]
    async fn test_model_info_not_loaded() {
        let tts = SystemTts::new(None).await.unwrap();
        let info = tts.model_info();
        assert_eq!(info.name, "system-tts");
        assert!(!info.loaded);
    }
}
