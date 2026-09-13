use crate::{AleError, Result};
use async_trait::async_trait;
use std::path::Path;
use std::sync::Mutex;

const DEFAULT_IMG_SIZE: usize = 224;
const MEAN: [f32; 3] = [0.485, 0.456, 0.406];
const STD: [f32; 3] = [0.229, 0.224, 0.225];

/// 视觉语言模型trait
#[async_trait]
pub trait VisionModel: Send + Sync {
    /// 描述图像内容
    async fn describe_image(&self, image_data: &[u8]) -> Result<String>;

    /// 获取模型信息
    fn model_info(&self) -> crate::ModelInfo;
}

/// 基于ONNX的VLM模型
#[derive(Clone)]
pub struct OnnxVlm {
    model_path: std::path::PathBuf,
    session: Option<std::sync::Arc<Mutex<ort::session::Session>>>,
    slot: std::sync::Arc<tokio::sync::Semaphore>,
    img_size: usize,
}

impl OnnxVlm {
    pub async fn new(model_path: &Path) -> Result<Self> {
        let model_path = model_path.to_path_buf();

        if !model_path.exists() {
            return Err(AleError::VlmError(format!(
                "Model file not found: {}",
                model_path.display()
            )));
        }

        Ok(Self {
            model_path,
            session: None,
            slot: crate::blocking_worker::slot(),
            img_size: DEFAULT_IMG_SIZE,
        })
    }

    pub fn with_img_size(mut self, size: usize) -> Self {
        self.img_size = size;
        self
    }

    pub fn load_model(&mut self) -> Result<()> {
        if self.session.is_some() {
            return Ok(());
        }

        let session = ort::session::Session::builder()
            .map_err(|e| AleError::VlmError(format!("Failed to create session builder: {}", e)))?
            .commit_from_file(&self.model_path)
            .map_err(|e| AleError::VlmError(format!("Failed to load ONNX model: {}", e)))?;

        self.img_size = infer_input_size(&session).unwrap_or(self.img_size);
        tracing::info!(
            "VLM model loaded: {} (input size: {}x{})",
            self.model_path.display(),
            self.img_size,
            self.img_size
        );

        self.session = Some(std::sync::Arc::new(Mutex::new(session)));
        Ok(())
    }

    pub async fn load_model_async(&mut self) -> Result<()> {
        let mut owned = self.clone();
        let (session, size) = crate::blocking_worker::run(self.slot.clone(), None, move |_| {
            owned.load_model()?;
            Ok((owned.session, owned.img_size))
        })
        .await?;
        self.session = session;
        self.img_size = size;
        Ok(())
    }

    pub fn is_loaded(&self) -> bool {
        self.session.is_some()
    }
}

impl OnnxVlm {
    fn describe_blocking(
        &self,
        image_data: &[u8],
        options: &ort::session::RunOptions,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<String> {
        let session = self.session.as_ref().ok_or_else(|| {
            AleError::VlmError("VLM model not loaded, call load_model() first".to_string())
        })?;

        let mut session = session
            .lock()
            .map_err(|e| AleError::VlmError(format!("Session lock poisoned: {}", e)))?;

        // 1. 解码图像并 resize
        let img = image::load_from_memory(image_data)
            .map_err(|e| AleError::VlmError(format!("Failed to decode image: {}", e)))?
            .resize_exact(
                self.img_size as u32,
                self.img_size as u32,
                image::imageops::FilterType::Triangle,
            )
            .to_rgb8();

        // 2. 归一化为 NCHW flat 数据
        crate::blocking_worker::check(cancel)?;
        let data = image_to_nchw_f32(&img, self.img_size);
        let shape = vec![1usize, 3, self.img_size, self.img_size];

        // 3. 获取输入名称
        let input_name = session
            .inputs()
            .first()
            .map(|i| i.name().to_string())
            .unwrap_or_else(|| "pixel_values".to_string());

        // 4. 构造 ort Tensor 并运行推理
        let ort_tensor = ort::value::Tensor::from_array((shape, data))
            .map_err(|e| AleError::VlmError(format!("Failed to create ort tensor: {}", e)))?;

        let outputs = session
            .run_with_options(ort::inputs![input_name.as_str() => ort_tensor], options)
            .map_err(|e| AleError::VlmError(format!("ONNX inference failed: {}", e)))?;

        // 5. 解码输出
        let text = decode_output(&outputs)?;
        if text.is_empty() {
            return Err(AleError::VlmError(
                "Model output could not be decoded; a compatible tokenizer/model is required"
                    .into(),
            ));
        }
        Ok(text)
    }
}

#[async_trait]
impl VisionModel for OnnxVlm {
    async fn describe_image(&self, image_data: &[u8]) -> Result<String> {
        if self.session.is_none() {
            return Err(AleError::NotInitialized("Local image-description model"));
        }
        let options = std::sync::Arc::new(
            ort::session::RunOptions::new()
                .map_err(|_| AleError::VlmError("Cannot create inference options".into()))?,
        );
        let abort = options.clone();
        let owned = self.clone();
        let image = image_data.to_vec();
        crate::blocking_worker::run(
            self.slot.clone(),
            Some(std::sync::Arc::new(move || {
                let _ = abort.terminate();
            })),
            move |cancel| owned.describe_blocking(&image, &options, &cancel),
        )
        .await
    }

    fn model_info(&self) -> crate::ModelInfo {
        crate::ModelInfo {
            name: "onnx-vlm".to_string(),
            version: "1.0".to_string(),
            device: "cpu".to_string(),
            loaded: self.session.is_some(),
        }
    }
}

/// 将 RGB 图像转为归一化的 NCHW flat Vec
fn image_to_nchw_f32(img: &image::RgbImage, size: usize) -> Vec<f32> {
    let (width, height) = img.dimensions();
    assert_eq!(width as usize, size);
    assert_eq!(height as usize, size);

    let mut data = vec![0.0f32; 3 * size * size];

    for y in 0..size {
        for x in 0..size {
            let pixel = img.get_pixel(x as u32, y as u32);
            for c in 0..3 {
                let val = pixel[c] as f32 / 255.0;
                data[c * size * size + y * size + x] = (val - MEAN[c]) / STD[c];
            }
        }
    }

    data
}

/// 从 ONNX 模型元数据推断输入图像尺寸
fn infer_input_size(session: &ort::session::Session) -> Option<usize> {
    let input = session.inputs().first()?;
    if let ort::value::ValueType::Tensor { shape, .. } = input.dtype() {
        // shape derefs to &[i64], expect [batch, channels, H, W]
        if shape.len() >= 4 {
            let h = shape[shape.len() - 2];
            if h > 0 {
                return Some(h as usize);
            }
        }
    }
    None
}

/// 解码 ONNX 输出张量为文本
fn decode_output(outputs: &ort::session::SessionOutputs) -> Result<String> {
    let output = &outputs[0];

    // 使用 try_extract_tensor 获取 (&Shape, &[f32])
    let (shape, data) = output
        .try_extract_tensor::<f32>()
        .map_err(|e| AleError::VlmError(format!("Failed to extract output tensor: {}", e)))?;

    let dims: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
    tracing::debug!("VLM output shape: {:?}", dims);

    let vocab_size = *dims.last().unwrap_or(&0);

    if vocab_size == 0 || data.is_empty() {
        return Ok(decode_logits_flat(data));
    }

    let seq_len = data.len() / vocab_size;
    let mut token_ids = Vec::with_capacity(seq_len);

    for i in 0..seq_len {
        let start = i * vocab_size;
        let end = start + vocab_size;
        if end > data.len() {
            break;
        }
        token_ids.push(argmax(&data[start..end]));
    }

    Ok(ids_to_text(&token_ids))
}

fn argmax(values: &[f32]) -> usize {
    values
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn decode_logits_flat(logits: &[f32]) -> String {
    if logits.is_empty() {
        return String::new();
    }
    let ids: Vec<usize> = logits
        .iter()
        .map(|v| (*v as i64).unsigned_abs() as usize)
        .filter(|&id| id > 3 && id < 50000)
        .take(512)
        .collect();

    ids_to_text(&ids)
}

fn ids_to_text(ids: &[usize]) -> String {
    let mut text = String::new();
    let mut byte_buf = Vec::new();

    for &id in ids {
        if id <= 3 || id >= 50256 {
            if !byte_buf.is_empty() {
                text.push_str(&String::from_utf8_lossy(&byte_buf));
                byte_buf.clear();
            }
            continue;
        }

        if id < 256 {
            byte_buf.push(id as u8);
        } else {
            if !byte_buf.is_empty() {
                text.push_str(&String::from_utf8_lossy(&byte_buf));
                byte_buf.clear();
            }
            if let Some(ch) = char::from_u32(id as u32) {
                text.push(ch);
            }
        }
    }

    if !byte_buf.is_empty() {
        text.push_str(&String::from_utf8_lossy(&byte_buf));
    }

    let text = text.trim().to_string();
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_argmax() {
        assert_eq!(argmax(&[0.1, 0.9, 0.3]), 1);
        assert_eq!(argmax(&[0.5]), 0);
        assert_eq!(argmax(&[]), 0);
    }

    #[test]
    fn test_image_to_nchw_f32() {
        let img = image::RgbImage::new(224, 224);
        let data = image_to_nchw_f32(&img, 224);
        assert_eq!(data.len(), 1 * 3 * 224 * 224);
    }

    #[test]
    fn test_ids_to_text_ascii() {
        let text = ids_to_text(&[104, 101, 108, 108, 111]);
        assert_eq!(text, "hello");
    }

    #[test]
    fn test_ids_to_text_skips_special() {
        let text = ids_to_text(&[0, 1, 2, 3]);
        assert!(text.is_empty());
    }

    #[test]
    fn test_decode_logits_flat_empty() {
        assert!(decode_logits_flat(&[]).is_empty());
    }
}
