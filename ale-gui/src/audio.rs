use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::{Arc, Mutex as StdMutex};

pub const NORMALIZED_SAMPLE_RATE: u32 = 16_000;
const MAX_BUFFERED_SAMPLES: usize = NORMALIZED_SAMPLE_RATE as usize * 60;

#[cfg(target_os = "android")]
use oboe::{AudioInputCallback, AudioInputStreamSafe, AudioStream, DataCallbackResult, Mono};

pub struct Recorder {
    #[cfg(not(target_os = "android"))]
    stream: cpal::Stream,
    capture: Arc<StdMutex<CaptureState>>,
}

impl Recorder {
    pub fn start() -> Result<Self, String> {
        #[cfg(not(target_os = "android"))]
        {
            Self::start_desktop()
        }
        #[cfg(target_os = "android")]
        {
            Self::start_android()
        }
    }

    pub fn into_wav_bytes(self) -> Result<Vec<u8>, String> {
        let samples = self
            .capture
            .lock()
            .map_err(|_| "读取录音缓存失败".to_string())?
            .buffer
            .all_samples();

        if samples.is_empty() {
            return Err("没有录到音频".to_string());
        }

        #[cfg(not(target_os = "android"))]
        {
            drop(self.stream);
        }

        encode_wav(&samples)
    }

    /// Return normalized samples appended after an absolute sequence cursor.
    pub fn samples_since(&self, cursor: &mut u64) -> Vec<f32> {
        let Ok(capture) = self.capture.lock() else {
            return Vec::new();
        };
        capture.buffer.samples_since(cursor)
    }

    #[cfg(not(target_os = "android"))]
    fn start_desktop() -> Result<Self, String> {
        use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

        let host = cpal::default_host();
        let device = host
            .default_input_device()
            .ok_or_else(|| "没有找到可用麦克风".to_string())?;
        let supported_config = device
            .default_input_config()
            .map_err(|error| format!("获取麦克风配置失败: {error}"))?;
        let sample_format = supported_config.sample_format();
        let config: cpal::StreamConfig = supported_config.into();
        let sample_rate = config.sample_rate.0;
        let channels = config.channels;
        let capture = Arc::new(StdMutex::new(CaptureState::new(sample_rate, channels)));

        let stream = match sample_format {
            cpal::SampleFormat::F32 => build_input_stream::<f32>(&device, &config, capture.clone()),
            cpal::SampleFormat::I16 => build_input_stream::<i16>(&device, &config, capture.clone()),
            cpal::SampleFormat::U16 => build_input_stream::<u16>(&device, &config, capture.clone()),
            other => Err(format!("不支持的麦克风采样格式: {other:?}")),
        }?;

        stream
            .play()
            .map_err(|error| format!("启动录音失败: {error}"))?;

        Ok(Self { stream, capture })
    }

    #[cfg(target_os = "android")]
    fn start_android() -> Result<Self, String> {
        use oboe::{AudioStreamBuilder, PerformanceMode, SharingMode};

        let capture = Arc::new(StdMutex::new(CaptureState::new(48_000, 1)));
        let capture_clone = capture.clone();

        let mut stream = AudioStreamBuilder::default()
            .set_input()
            .set_performance_mode(PerformanceMode::LowLatency)
            .set_sharing_mode(SharingMode::Shared)
            .set_format::<f32>()
            .set_channel_count::<Mono>()
            .set_callback(RecorderCallback {
                capture: capture_clone,
            })
            .open_stream()
            .map_err(|error| format!("打开音频流失败: {error:?}"))?;

        stream
            .start()
            .map_err(|error| format!("启动录音失败: {error:?}"))?;

        // Leak the stream so it keeps running until stop
        let stream_ref = Box::new(stream);
        std::mem::forget(stream_ref);

        Ok(Self { capture })
    }
}

fn encode_wav(samples: &[f32]) -> Result<Vec<u8>, String> {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: NORMALIZED_SAMPLE_RATE,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };

    let mut cursor = Cursor::new(Vec::new());
    {
        let mut writer = hound::WavWriter::new(&mut cursor, spec)
            .map_err(|error| format!("创建 WAV 失败: {error}"))?;
        for sample in samples {
            let sample = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
            writer
                .write_sample(sample)
                .map_err(|error| format!("写入 WAV 失败: {error}"))?;
        }
        writer
            .finalize()
            .map_err(|error| format!("完成 WAV 失败: {error}"))?;
    }
    Ok(cursor.into_inner())
}

#[cfg(target_os = "android")]
struct RecorderCallback {
    capture: Arc<StdMutex<CaptureState>>,
}

#[cfg(target_os = "android")]
impl AudioInputCallback for RecorderCallback {
    type FrameType = (f32, Mono);

    fn on_audio_ready(
        &mut self,
        _stream: &mut dyn AudioInputStreamSafe,
        frames: &[f32],
    ) -> DataCallbackResult {
        if let Ok(mut capture) = self.capture.lock() {
            capture.push_interleaved(frames);
        }
        DataCallbackResult::Continue
    }
}

#[cfg(not(target_os = "android"))]
fn build_input_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    capture: Arc<StdMutex<CaptureState>>,
) -> Result<cpal::Stream, String>
where
    T: cpal::Sample + cpal::SizedSample + Send + 'static,
    f32: FromSample<T>,
{
    use cpal::traits::DeviceTrait;

    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                if let Ok(mut capture) = capture.lock() {
                    let converted = data
                        .iter()
                        .copied()
                        .map(f32::from_sample)
                        .collect::<Vec<_>>();
                    capture.push_interleaved(&converted);
                }
            },
            move |error| {
                eprintln!("录音流错误: {error}");
            },
            None,
        )
        .map_err(|error| format!("创建录音流失败: {error}"))
}

#[derive(Debug)]
struct CaptureState {
    normalizer: InputNormalizer,
    buffer: SampleBuffer,
}

impl CaptureState {
    fn new(input_sample_rate: u32, channels: u16) -> Self {
        Self {
            normalizer: InputNormalizer::new(input_sample_rate, channels),
            buffer: SampleBuffer::new(MAX_BUFFERED_SAMPLES),
        }
    }

    fn push_interleaved(&mut self, samples: &[f32]) {
        let normalized = self.normalizer.push_interleaved(samples);
        self.buffer.push(&normalized);
    }
}

#[derive(Debug)]
struct InputNormalizer {
    channels: usize,
    step: f64,
    source_position: f64,
    pending_mono: Vec<f32>,
}

impl InputNormalizer {
    fn new(input_sample_rate: u32, channels: u16) -> Self {
        Self {
            channels: channels.max(1) as usize,
            step: input_sample_rate.max(1) as f64 / NORMALIZED_SAMPLE_RATE as f64,
            source_position: 0.0,
            pending_mono: Vec::new(),
        }
    }

    fn push_interleaved(&mut self, input: &[f32]) -> Vec<f32> {
        self.pending_mono.extend(
            input
                .chunks_exact(self.channels)
                .map(|frame| frame.iter().sum::<f32>() / self.channels as f32),
        );

        let mut output = Vec::with_capacity(
            ((self.pending_mono.len() as f64 - self.source_position).max(0.0) / self.step).ceil()
                as usize,
        );
        while self.source_position + 1.0 < self.pending_mono.len() as f64 {
            let left = self.source_position.floor() as usize;
            let fraction = (self.source_position - left as f64) as f32;
            let sample =
                self.pending_mono[left] * (1.0 - fraction) + self.pending_mono[left + 1] * fraction;
            output.push(sample);
            self.source_position += self.step;
        }

        let consumed =
            (self.source_position.floor() as usize).min(self.pending_mono.len().saturating_sub(1));
        if consumed > 0 {
            self.pending_mono.drain(..consumed);
            self.source_position -= consumed as f64;
        }
        output
    }
}

#[derive(Debug)]
struct SampleBuffer {
    samples: VecDeque<f32>,
    start_sequence: u64,
    next_sequence: u64,
    capacity: usize,
}

impl SampleBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            samples: VecDeque::with_capacity(capacity),
            start_sequence: 0,
            next_sequence: 0,
            capacity,
        }
    }

    fn push(&mut self, samples: &[f32]) {
        self.next_sequence = self.next_sequence.saturating_add(samples.len() as u64);
        self.samples.extend(samples.iter().copied());
        let excess = self.samples.len().saturating_sub(self.capacity);
        if excess > 0 {
            self.samples.drain(..excess);
            self.start_sequence = self.start_sequence.saturating_add(excess as u64);
        }
    }

    fn samples_since(&self, cursor: &mut u64) -> Vec<f32> {
        *cursor = (*cursor).clamp(self.start_sequence, self.next_sequence);
        let offset = (*cursor - self.start_sequence) as usize;
        let samples = self.samples.iter().skip(offset).copied().collect();
        *cursor = self.next_sequence;
        samples
    }

    fn all_samples(&self) -> Vec<f32> {
        self.samples.iter().copied().collect()
    }
}

trait FromSample<T> {
    fn from_sample(sample: T) -> f32;
}

impl FromSample<f32> for f32 {
    fn from_sample(sample: f32) -> f32 {
        sample
    }
}

impl FromSample<i16> for f32 {
    fn from_sample(sample: i16) -> f32 {
        sample as f32 / i16::MAX as f32
    }
}

impl FromSample<u16> for f32 {
    fn from_sample(sample: u16) -> f32 {
        (sample as f32 / u16::MAX as f32) * 2.0 - 1.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signal(frames: usize, channels: usize) -> Vec<f32> {
        (0..frames)
            .flat_map(|frame| {
                let value = ((frame % 100) as f32 / 50.0) - 1.0;
                std::iter::repeat_n(value, channels)
            })
            .collect()
    }

    #[test]
    fn normalizes_48khz_stereo_to_16khz_mono() {
        let mut normalizer = InputNormalizer::new(48_000, 2);
        let output = normalizer.push_interleaved(&signal(48_003, 2));
        assert_eq!(output.len(), 16_001);
    }

    #[test]
    fn normalizes_44_1khz_mono_to_16khz_time_scale() {
        let mut normalizer = InputNormalizer::new(44_100, 1);
        let output = normalizer.push_interleaved(&signal(44_103, 1));
        assert!((output.len() as isize - 16_001).abs() <= 1);
    }

    #[test]
    fn downmixes_stereo_before_resampling() {
        let mut normalizer = InputNormalizer::new(16_000, 2);
        let output = normalizer.push_interleaved(&[1.0, -1.0, 0.5, 0.5, 0.0, 0.0]);
        assert_eq!(output, vec![0.0, 0.5]);
    }

    #[test]
    fn absolute_cursor_survives_buffer_trimming() {
        let mut buffer = SampleBuffer::new(8);
        let mut cursor = 0;
        for batch in 0..10_000 {
            buffer.push(&[batch as f32; 4]);
            let samples = buffer.samples_since(&mut cursor);
            assert_eq!(samples, vec![batch as f32; 4]);
        }
        assert_eq!(cursor, 40_000);
    }

    #[test]
    fn lagging_cursor_returns_only_retained_samples() {
        let mut buffer = SampleBuffer::new(4);
        buffer.push(&[1.0, 2.0, 3.0]);
        buffer.push(&[4.0, 5.0, 6.0]);
        let mut cursor = 0;
        assert_eq!(buffer.samples_since(&mut cursor), vec![3.0, 4.0, 5.0, 6.0]);
        assert_eq!(cursor, 6);
    }

    #[test]
    fn microphone_format_to_wav_and_vad_chain_detects_speech_end() {
        use ale_core::vad::{VadConfig, VadState, VoiceActivityDetector};

        let mut capture = CaptureState::new(48_000, 2);
        let mut input = vec![0.0; 48_000 / 5 * 2];
        input.extend(std::iter::repeat_n(0.5, 48_000 / 2 * 2));
        input.extend(std::iter::repeat_n(0.0, 48_000 * 2));
        capture.push_interleaved(&input);
        let normalized = capture.buffer.all_samples();
        let wav = encode_wav(&normalized).unwrap();

        let reader = hound::WavReader::new(Cursor::new(wav)).unwrap();
        assert_eq!(reader.spec().sample_rate, NORMALIZED_SAMPLE_RATE);
        assert_eq!(reader.spec().channels, 1);
        let samples = reader
            .into_samples::<i16>()
            .map(|sample| sample.unwrap() as f32 / i16::MAX as f32)
            .collect::<Vec<_>>();

        let config = VadConfig {
            energy_threshold: 0.01,
            speech_start_frames: 2,
            silence_end_frames: 4,
            sample_rate: NORMALIZED_SAMPLE_RATE,
            frame_size: 320,
        };
        let mut vad = VoiceActivityDetector::new(config);
        let states = samples
            .chunks_exact(320)
            .map(|frame| vad.process_frame(frame))
            .collect::<Vec<_>>();
        assert!(states.contains(&VadState::Speaking));
        assert!(states.contains(&VadState::SpeechEnded));
    }
}
