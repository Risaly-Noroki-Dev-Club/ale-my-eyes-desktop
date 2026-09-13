use crate::{AleError, Result};
use async_trait::async_trait;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

const WHISPER_SAMPLE_RATE: u32 = 16000;

/// Speech recognition trait
#[async_trait]
pub trait SpeechRecognizer: Send + Sync {
    async fn transcribe(&self, audio_data: &[u8]) -> Result<String>;
    fn supported_languages(&self) -> Vec<String>;
    fn model_info(&self) -> crate::ModelInfo;
}

/// Whisper speech recognizer backed by whisper-rs (whisper.cpp FFI)
#[derive(Clone)]
pub struct WhisperRecognizer {
    model_path: std::path::PathBuf,
    ctx: Option<std::sync::Arc<WhisperContext>>,
    language: Option<String>,
    n_threads: i32,
    use_beam_search: bool,
    beam_size: i32,
    initial_prompt: Option<String>,
    temperature: f32,
    is_first_utterance: std::sync::Arc<AtomicBool>,
    slot: std::sync::Arc<tokio::sync::Semaphore>,
}

impl WhisperRecognizer {
    pub async fn new(model_path: &Path) -> Result<Self> {
        if !model_path.exists() {
            return Err(AleError::AsrError(format!(
                "Model file not found: {}",
                model_path.display()
            )));
        }

        Ok(Self {
            model_path: model_path.to_path_buf(),
            ctx: None,
            language: None,
            n_threads: num_cpus().min(4) as i32,
            use_beam_search: false,
            beam_size: 3,
            initial_prompt: None,
            temperature: 0.0,
            is_first_utterance: std::sync::Arc::new(AtomicBool::new(true)),
            slot: crate::blocking_worker::slot(),
        })
    }

    pub fn with_language(mut self, lang: Option<String>) -> Self {
        self.language = lang;
        self
    }

    pub fn with_threads(mut self, n: i32) -> Self {
        self.n_threads = n;
        self
    }

    pub fn with_beam_search(mut self, enabled: bool, beam_size: i32) -> Self {
        self.use_beam_search = enabled;
        self.beam_size = beam_size.clamp(1, 10);
        self
    }

    pub fn with_initial_prompt(mut self, prompt: Option<String>) -> Self {
        self.initial_prompt = prompt;
        self
    }

    pub fn with_temperature(mut self, temp: f32) -> Self {
        self.temperature = temp.clamp(0.0, 1.0);
        self
    }

    pub fn load_model(&mut self) -> Result<()> {
        if self.ctx.is_some() {
            return Ok(());
        }

        let ctx = WhisperContext::new_with_params(
            self.model_path
                .to_str()
                .ok_or_else(|| AleError::AsrError("Model path is not valid UTF-8".to_string()))?,
            WhisperContextParameters::default(),
        )
        .map_err(|e| AleError::AsrError(format!("Failed to load whisper model: {e}")))?;

        self.ctx = Some(std::sync::Arc::new(ctx));
        Ok(())
    }

    pub async fn load_model_async(&mut self) -> Result<()> {
        let mut owned = self.clone();
        self.ctx = crate::blocking_worker::run(self.slot.clone(), None, move |_| {
            owned.load_model()?;
            Ok(owned.ctx)
        })
        .await?;
        Ok(())
    }

    pub fn is_loaded(&self) -> bool {
        self.ctx.is_some()
    }

    fn run_inference(
        &self,
        ctx: &WhisperContext,
        samples: &[f32],
        cancel: std::sync::Arc<AtomicBool>,
    ) -> Result<String> {
        let mut state = ctx
            .create_state()
            .map_err(|e| AleError::AsrError(format!("Failed to create whisper state: {e}")))?;

        // 默认 Greedy（快），仅在明确启用时用 BeamSearch（慢但更准）
        let strategy = if self.use_beam_search {
            SamplingStrategy::BeamSearch {
                beam_size: self.beam_size,
                patience: -1.0,
            }
        } else {
            SamplingStrategy::Greedy { best_of: 1 }
        };

        let mut params = FullParams::new(strategy);
        unsafe extern "C" fn should_abort(data: *mut std::ffi::c_void) -> bool {
            // The Arc below retains this AtomicBool throughout the synchronous full() call.
            unsafe { (&*data.cast::<AtomicBool>()).load(Ordering::Relaxed) }
        }
        // Use the raw callback with a retained typed allocation. whisper-rs 0.16's
        // safe adapter erases its closure to a trait object then casts it back to F.
        unsafe {
            params.set_abort_callback(Some(should_abort));
            params.set_abort_callback_user_data(std::sync::Arc::as_ptr(&cancel).cast_mut().cast());
        }
        params.set_n_threads(self.n_threads);
        params.set_print_special(false);
        params.set_print_progress(false);
        params.set_print_realtime(false);
        params.set_print_timestamps(false);
        params.set_suppress_blank(true);
        params.set_suppress_nst(true);
        params.set_temperature(self.temperature);

        let is_first = self.is_first_utterance.swap(false, Ordering::Relaxed);
        params.set_no_context(is_first);

        if let Some(ref prompt) = self.initial_prompt {
            if !prompt.is_empty() {
                params.set_initial_prompt(prompt);
            }
        }

        if let Some(ref lang) = self.language {
            params.set_language(Some(lang));
        } else {
            params.set_language(Some("auto"));
        }

        state
            .full(params, samples)
            .map_err(|e| AleError::AsrError(format!("Whisper inference failed: {e}")))?;

        let num_segments = state.full_n_segments();

        let mut text = String::new();
        for i in 0..num_segments {
            if let Some(segment) = state.get_segment(i) {
                if let Ok(segment_text) = segment.to_str() {
                    text.push_str(segment_text);
                }
            }
        }

        Ok(text.trim().to_string())
    }
}

impl WhisperRecognizer {
    fn transcribe_blocking(
        &self,
        audio_data: &[u8],
        cancel: std::sync::Arc<AtomicBool>,
    ) -> Result<String> {
        let mut samples = parse_audio_to_f32_mono(audio_data, WHISPER_SAMPLE_RATE)?;

        if samples.is_empty() {
            return Err(AleError::AsrError("No audio samples found".to_string()));
        }

        // 轻量预处理：先 RMS 归一化再噪声门限
        // 先 normalize 让弱语音达到可听范围，再 gate 消除归一化后仍低于阈值的底噪
        normalize_rms(&mut samples, 0.1);
        apply_noise_gate(&mut samples, 0.01);

        let ctx = self.ctx.as_ref().ok_or_else(|| {
            AleError::AsrError("Whisper model not loaded, call load_model() first".to_string())
        })?;

        crate::blocking_worker::check(&cancel)?;
        self.run_inference(ctx, &samples, cancel)
    }
}

#[async_trait]
impl SpeechRecognizer for WhisperRecognizer {
    async fn transcribe(&self, audio_data: &[u8]) -> Result<String> {
        let owned = self.clone();
        let audio = audio_data.to_vec();
        crate::blocking_worker::run(self.slot.clone(), None, move |cancel| {
            owned.transcribe_blocking(&audio, cancel)
        })
        .await
    }

    fn supported_languages(&self) -> Vec<String> {
        vec![
            "auto".into(),
            "en".into(),
            "zh".into(),
            "ja".into(),
            "ko".into(),
            "fr".into(),
            "de".into(),
            "es".into(),
            "ru".into(),
            "pt".into(),
            "it".into(),
            "nl".into(),
            "pl".into(),
            "ar".into(),
            "tr".into(),
            "vi".into(),
            "th".into(),
        ]
    }

    fn model_info(&self) -> crate::ModelInfo {
        crate::ModelInfo {
            name: "whisper".to_string(),
            version: format!("cpp-{}", whisper_rs::WHISPER_CPP_VERSION),
            device: "cpu".to_string(),
            loaded: self.ctx.is_some(),
        }
    }
}

// ── Audio parsing ──────────────────────────────────────────────

/// Parse raw audio bytes (WAV or raw PCM) into f32 mono samples at the target sample rate.
pub fn parse_audio_to_f32_mono(audio_data: &[u8], target_rate: u32) -> Result<Vec<f32>> {
    if audio_data.len() < 44 {
        return Err(AleError::AsrError("Audio data too short".to_string()));
    }

    // Check for WAV header
    if &audio_data[0..4] == b"RIFF" && &audio_data[8..12] == b"WAVE" {
        parse_wav(audio_data, target_rate)
    } else {
        // Assume raw 16-bit PCM mono at target rate
        Ok(pcm16_to_f32(audio_data))
    }
}

fn parse_wav(data: &[u8], target_rate: u32) -> Result<Vec<f32>> {
    let num_channels = u16::from_le_bytes([data[22], data[23]]) as u32;
    let sample_rate = u32::from_le_bytes([data[24], data[25], data[26], data[27]]);
    let bits_per_sample = u16::from_le_bytes([data[34], data[35]]) as u32;
    if sample_rate == 0 || target_rate == 0 || num_channels == 0 {
        return Err(AleError::AsrError(
            "Invalid WAV sample rate or channels".into(),
        ));
    }

    // Find data chunk
    let mut offset = 36;
    while offset + 8 <= data.len() {
        let chunk_id = &data[offset..offset + 4];
        let chunk_size = u32::from_le_bytes([
            data[offset + 4],
            data[offset + 5],
            data[offset + 6],
            data[offset + 7],
        ]) as usize;

        if chunk_id == b"data" {
            let audio_start = offset + 8;
            let audio_end = audio_start + chunk_size.min(data.len() - audio_start);
            let raw_audio = &data[audio_start..audio_end];

            let samples = match bits_per_sample {
                16 => pcm16_to_f32(raw_audio),
                32 => pcm32f_to_f32(raw_audio),
                _ => {
                    return Err(AleError::AsrError(format!(
                        "Unsupported WAV bit depth: {bits_per_sample}"
                    )))
                }
            };

            let mono = if num_channels >= 2 {
                stereo_to_mono(&samples)
            } else {
                samples
            };

            if sample_rate != target_rate {
                return Ok(resample(&mono, sample_rate, target_rate));
            }
            return Ok(mono);
        }

        offset += 8 + chunk_size;
        if chunk_size % 2 != 0 {
            offset += 1;
        }
    }

    Err(AleError::AsrError("WAV data chunk not found".to_string()))
}

fn pcm16_to_f32(data: &[u8]) -> Vec<f32> {
    data.chunks_exact(2)
        .map(|chunk| {
            let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
            sample as f32 / 32768.0
        })
        .collect()
}

fn pcm32f_to_f32(data: &[u8]) -> Vec<f32> {
    data.chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

fn stereo_to_mono(samples: &[f32]) -> Vec<f32> {
    samples
        .chunks_exact(2)
        .map(|pair| (pair[0] + pair[1]) / 2.0)
        .collect()
}

fn resample(samples: &[f32], from_rate: u32, to_rate: u32) -> Vec<f32> {
    if from_rate == to_rate {
        return samples.to_vec();
    }

    let ratio = from_rate as f64 / to_rate as f64;
    let output_len = (samples.len() as f64 / ratio) as usize;
    let mut output = Vec::with_capacity(output_len);

    for i in 0..output_len {
        let src_idx = i as f64 * ratio;
        let idx = src_idx as usize;
        let frac = src_idx - idx as f64;

        let sample = if idx + 1 < samples.len() {
            samples[idx] as f64 * (1.0 - frac) + samples[idx + 1] as f64 * frac
        } else if idx < samples.len() {
            samples[idx] as f64
        } else {
            0.0
        };

        output.push(sample as f32);
    }

    output
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}

// ── 轻量音频预处理 ────────────────────────────────────────────

/// 噪声门限：低于 threshold 的样本直接置零
/// 开销：单次遍历 O(n)，消除底噪对 Whisper 解码的干扰
fn apply_noise_gate(samples: &mut [f32], threshold: f32) {
    for s in samples.iter_mut() {
        if s.abs() < threshold {
            *s = 0.0;
        }
    }
}

/// RMS 归一化：将音频整体能量缩放到目标 RMS 级别
/// 开销：两次遍历 O(n)，让弱语音达到 Whisper 期望的音量范围
fn normalize_rms(samples: &mut [f32], target_rms: f32) {
    if samples.is_empty() {
        return;
    }
    // 计算 RMS
    let sum_sq: f64 = samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    let rms = (sum_sq / samples.len() as f64).sqrt() as f32;

    if rms < 1e-6 {
        return; // 全静音，不处理
    }

    let gain = target_rms / rms;

    // 限制增益范围，避免放大底噪
    let gain = gain.clamp(0.5, 20.0);

    for s in samples.iter_mut() {
        *s = (*s * gain).clamp(-1.0, 1.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stereo_to_mono() {
        let stereo = vec![0.5, -0.5, 1.0, -1.0];
        let mono = stereo_to_mono(&stereo);
        assert_eq!(mono.len(), 2);
        assert!((mono[0] - 0.0).abs() < 1e-6);
        assert!((mono[1] - 0.0).abs() < 1e-6);
    }

    #[test]
    fn test_pcm16_to_f32() {
        let bytes = (i16::MAX as i16).to_le_bytes();
        let samples = pcm16_to_f32(&bytes);
        assert!((samples[0] - 32767.0 / 32768.0).abs() < 1e-6);
    }

    #[test]
    fn test_resample_same_rate() {
        let samples = vec![1.0, 2.0, 3.0];
        let result = resample(&samples, 16000, 16000);
        assert_eq!(result, samples);
    }

    #[test]
    fn test_resample_downsample() {
        let samples: Vec<f32> = (0..48000).map(|i| (i as f32).sin()).collect();
        let result = resample(&samples, 48000, 16000);
        assert_eq!(result.len(), 16000);
    }

    #[test]
    fn test_noise_gate_silences_low_amplitude() {
        let mut samples = vec![0.001, 0.5, -0.002, -0.8, 0.009];
        apply_noise_gate(&mut samples, 0.01);
        assert_eq!(samples[0], 0.0);
        assert_eq!(samples[2], 0.0);
        assert_eq!(samples[4], 0.0);
        assert!((samples[1] - 0.5).abs() < 1e-6);
        assert!((samples[3] - (-0.8)).abs() < 1e-6);
    }

    #[test]
    fn test_normalize_rms_boosts_weak_signal() {
        let mut samples = vec![0.001; 1000]; // 非常弱的信号
        normalize_rms(&mut samples, 0.1);
        // 归一化后应该有显著提升
        let new_rms: f32 =
            (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
        // The configured 20x gain cap intentionally prevents amplifying this input to 0.1.
        assert!(
            (new_rms - 0.02).abs() < 1e-5,
            "RMS should respect the 20x gain cap, got {}",
            new_rms
        );
    }

    #[test]
    fn test_normalize_rms_does_not_amplify_silence() {
        let mut samples = vec![0.0; 1000];
        normalize_rms(&mut samples, 0.1);
        assert!(samples.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn test_weak_voice_preprocess_does_not_zero_speech() {
        // Simulate weak voice: samples around 0.005, below noise gate threshold 0.01
        let mut samples: Vec<f32> = (0..1000).map(|i| 0.005 * (i as f32 * 0.1).sin()).collect();
        let rms_before: f32 =
            (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
        assert!(
            rms_before < 0.01,
            "precondition: signal below gate threshold"
        );

        // Correct order: normalize first, then gate
        normalize_rms(&mut samples, 0.1);
        apply_noise_gate(&mut samples, 0.01);

        let rms_after: f32 =
            (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
        assert!(
            rms_after > 0.05,
            "weak voice should survive normalization then gating, got RMS {}",
            rms_after
        );
    }

    #[test]
    fn test_weak_voice_preprocess_wrong_order_kills_signal() {
        // Demonstrate that the old order (gate then normalize) kills weak voice
        let mut samples: Vec<f32> = (0..1000).map(|i| 0.005 * (i as f32 * 0.1).sin()).collect();

        // Wrong order: gate first
        apply_noise_gate(&mut samples, 0.01);
        normalize_rms(&mut samples, 0.01);

        let rms_after: f32 =
            (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
        assert!(
            rms_after < 1e-6,
            "old order should zero the signal, got RMS {}",
            rms_after
        );
    }
}
