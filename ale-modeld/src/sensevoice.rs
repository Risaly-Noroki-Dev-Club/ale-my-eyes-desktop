use ale_core::model_scheduler::{ModelRuntimeConfig, MODEL_IDLE_TTL};
use std::io::Cursor;
use std::sync::Mutex;
use std::time::Instant;

#[derive(Default)]
pub struct SenseVoiceAdapter {
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

impl SenseVoiceAdapter {
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

    pub fn unload_if_idle(&self) {
        if let Ok(mut loaded) = self.loaded.lock() {
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
    if spec.channels != 1
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
