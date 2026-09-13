use crate::{AleError, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;

/// LLM推理后端
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LlmBackend {
    /// 本地推理（ONNX Runtime）
    Local,
    /// 远程API（OpenAI、Anthropic等）
    Remote,
}

/// LLM配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub backend: LlmBackend,
    pub model_path: Option<String>,
    pub tokenizer_path: Option<String>,
    pub api_url: Option<String>,
    pub api_key: Option<String>,
    pub model_name: Option<String>,
    pub max_tokens: usize,
    pub temperature: f32,
    pub top_p: f32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            backend: LlmBackend::Local,
            model_path: None,
            tokenizer_path: None,
            api_url: None,
            api_key: None,
            model_name: None,
            max_tokens: 512,
            temperature: 0.7,
            top_p: 0.9,
        }
    }
}

/// LLM响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub text: String,
    pub tokens_used: usize,
    pub finish_reason: String,
}

/// LLM trait
#[async_trait]
pub trait LanguageModel: Send + Sync {
    async fn generate(&self, prompt: &str) -> Result<LlmResponse>;
    async fn generate_stream(&self, prompt: &str) -> Result<Box<dyn tokio::io::AsyncRead + Unpin>>;
    fn model_info(&self) -> crate::ModelInfo;
}

/// 本地LLM（基于 ONNX Runtime）
#[derive(Clone)]
pub struct LocalLlm {
    config: LlmConfig,
    session: Option<std::sync::Arc<Mutex<ort::session::Session>>>,
    slot: std::sync::Arc<tokio::sync::Semaphore>,
}

impl LocalLlm {
    pub async fn new(config: LlmConfig) -> Result<Self> {
        Ok(Self {
            config,
            session: None,
            slot: crate::blocking_worker::slot(),
        })
    }

    pub fn load_model(&mut self) -> Result<()> {
        if self.session.is_some() {
            return Ok(());
        }

        let model_path = self
            .config
            .model_path
            .as_ref()
            .ok_or_else(|| AleError::Other(anyhow::anyhow!("Model path not specified")))?;

        if !Path::new(model_path).exists() {
            return Err(AleError::Other(anyhow::anyhow!(
                "Model file not found: {}",
                model_path
            )));
        }

        let session = ort::session::Session::builder()
            .map_err(|e| {
                AleError::Other(anyhow::anyhow!("Failed to create session builder: {}", e))
            })?
            .commit_from_file(model_path)
            .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to load ONNX model: {}", e)))?;

        tracing::info!(
            "Local LLM model loaded: {} (inputs: {})",
            model_path,
            session.inputs().len()
        );

        self.session = Some(std::sync::Arc::new(Mutex::new(session)));
        Ok(())
    }

    pub async fn load_model_async(&mut self) -> Result<()> {
        let mut owned = self.clone();
        self.session = crate::blocking_worker::run(self.slot.clone(), None, move |_| {
            owned.load_model()?;
            Ok(owned.session)
        })
        .await?;
        Ok(())
    }

    pub fn is_loaded(&self) -> bool {
        self.session.is_some()
    }

    fn encode_simple(&self, text: &str) -> Vec<i64> {
        text.chars()
            .filter(|c| !c.is_control())
            .map(|c| c as i64)
            .collect()
    }

    fn sample_token(logits: &[f32], temperature: f32, top_p: f32) -> usize {
        if temperature <= 0.0 || temperature.abs() < f32::EPSILON {
            return argmax(logits);
        }

        let scaled: Vec<f32> = logits.iter().map(|&l| l / temperature).collect();
        let max_val = scaled.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let exp_sum: f32 = scaled.iter().map(|&v| (v - max_val).exp()).sum();
        let mut probs: Vec<f32> = scaled
            .iter()
            .map(|&v| (v - max_val).exp() / exp_sum)
            .collect();

        if top_p < 1.0 {
            let mut indexed: Vec<(usize, f32)> = probs.iter().copied().enumerate().collect();
            indexed.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

            let mut cumsum = 0.0;
            let mut cutoff = indexed.len();
            for (i, (_, p)) in indexed.iter().enumerate() {
                cumsum += p;
                if cumsum >= top_p {
                    cutoff = i + 1;
                    break;
                }
            }

            let mut keep = vec![false; probs.len()];
            for &(idx, _) in &indexed[..cutoff] {
                keep[idx] = true;
            }
            for (i, k) in keep.iter().enumerate() {
                if !k {
                    probs[i] = 0.0;
                }
            }

            let sum: f32 = probs.iter().sum();
            if sum > 0.0 {
                for p in &mut probs {
                    *p /= sum;
                }
            }
        }

        let r: f32 = pseudo_random();
        let mut cumsum = 0.0;
        for (i, &p) in probs.iter().enumerate() {
            cumsum += p;
            if cumsum >= r {
                return i;
            }
        }
        argmax(logits)
    }
}

fn argmax(values: &[f32]) -> usize {
    values
        .iter()
        .enumerate()
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn pseudo_random() -> f32 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    std::time::Instant::now()
        .elapsed()
        .as_nanos()
        .hash(&mut hasher);
    (hasher.finish() % 10000) as f32 / 10000.0
}

impl LocalLlm {
    fn generate_blocking(
        &self,
        prompt: &str,
        options: &ort::session::RunOptions,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<LlmResponse> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| AleError::NotInitialized("Local LLM model"))?;

        let mut session = session
            .lock()
            .map_err(|e| AleError::Other(anyhow::anyhow!("Session lock poisoned: {}", e)))?;

        let mut token_ids: Vec<i64> = self.encode_simple(prompt);
        let prompt_len = token_ids.len();
        let max_tokens = self.config.max_tokens;
        let mut generated = 0;

        for _ in 0..max_tokens {
            crate::blocking_worker::check(cancel)?;
            let seq_len = token_ids.len();

            let input_name = session
                .inputs()
                .first()
                .map(|i| i.name().to_string())
                .unwrap_or_else(|| "input_ids".to_string());

            let ort_tensor =
                ort::value::Tensor::from_array((vec![1usize, seq_len], token_ids.clone()))
                    .map_err(|e| {
                        AleError::Other(anyhow::anyhow!("Failed to create ort tensor: {}", e))
                    })?;

            let outputs = session
                .run_with_options(ort::inputs![input_name.as_str() => ort_tensor], options)
                .map_err(|e| AleError::Other(anyhow::anyhow!("LLM inference failed: {}", e)))?;

            let output = &outputs[0];
            let (shape, data) = output
                .try_extract_tensor::<f32>()
                .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to extract logits: {}", e)))?;

            let dims: Vec<usize> = shape.iter().map(|&d| d as usize).collect();
            let vocab_size = *dims.last().unwrap_or(&0);
            if vocab_size == 0 || data.is_empty() {
                break;
            }

            let last_logits_start = data.len().saturating_sub(vocab_size);
            let last_logits = &data[last_logits_start..];

            let next_token =
                Self::sample_token(last_logits, self.config.temperature, self.config.top_p);

            if next_token == 0 || next_token == 2 || next_token == 50256 {
                break;
            }

            token_ids.push(next_token as i64);
            generated += 1;
        }

        let output_ids: Vec<usize> = token_ids[prompt_len..]
            .iter()
            .map(|&id| id as usize)
            .collect();
        let text = ids_to_text(&output_ids);

        Ok(LlmResponse {
            text,
            tokens_used: generated,
            finish_reason: if generated >= max_tokens {
                "length".to_string()
            } else {
                "stop".to_string()
            },
        })
    }
}

#[async_trait]
impl LanguageModel for LocalLlm {
    async fn generate(&self, prompt: &str) -> Result<LlmResponse> {
        if self.session.is_none() {
            return Err(AleError::NotInitialized("Local LLM model"));
        }
        let options = std::sync::Arc::new(
            ort::session::RunOptions::new()
                .map_err(|_| AleError::ConfigError("Cannot create inference options".into()))?,
        );
        let abort = options.clone();
        let owned = self.clone();
        let prompt = prompt.to_owned();
        crate::blocking_worker::run(
            self.slot.clone(),
            Some(std::sync::Arc::new(move || {
                let _ = abort.terminate();
            })),
            move |cancel| owned.generate_blocking(&prompt, &options, &cancel),
        )
        .await
    }

    async fn generate_stream(&self, prompt: &str) -> Result<Box<dyn tokio::io::AsyncRead + Unpin>> {
        let response = self.generate(prompt).await?;
        Ok(Box::new(std::io::Cursor::new(response.text.into_bytes())))
    }

    fn model_info(&self) -> crate::ModelInfo {
        crate::ModelInfo {
            name: self
                .config
                .model_name
                .clone()
                .unwrap_or_else(|| "local-llm".to_string()),
            version: "1.0".to_string(),
            device: "cpu".to_string(),
            loaded: self.session.is_some(),
        }
    }
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
            if id == 50256 {
                break;
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
    text.trim().to_string()
}

/// 远程API LLM
pub struct RemoteLlm {
    config: LlmConfig,
    client: reqwest::Client,
}

impl RemoteLlm {
    pub async fn new(config: LlmConfig) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|_| AleError::ConfigError("Cannot create model HTTP client".into()))?;
        Ok(Self { config, client })
    }

    fn response_text(response_body: &serde_json::Value) -> Result<String> {
        response_body["choices"][0]["message"]["content"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| AleError::Other(anyhow::anyhow!("Missing LLM response content")))
    }
}

#[async_trait]
impl LanguageModel for RemoteLlm {
    async fn generate(&self, prompt: &str) -> Result<LlmResponse> {
        let api_url = self
            .config
            .api_url
            .as_ref()
            .ok_or(AleError::Other(anyhow::anyhow!("API URL not specified")))?;

        let api_key = self
            .config
            .api_key
            .as_ref()
            .ok_or(AleError::Other(anyhow::anyhow!("API key not specified")))?;

        let model_name = self
            .config
            .model_name
            .as_ref()
            .ok_or(AleError::Other(anyhow::anyhow!("Model name not specified")))?;

        let request_body = serde_json::json!({
            "model": model_name,
            "messages": [{"role": "user", "content": prompt}],
            "max_tokens": self.config.max_tokens,
            "temperature": self.config.temperature,
            "top_p": self.config.top_p
        });

        let response = self
            .client
            .post(api_url)
            .timeout(
                crate::model_api::deadline()
                    .saturating_duration_since(tokio::time::Instant::now())
                    .min(std::time::Duration::from_secs(30)),
            )
            .header("Authorization", format!("Bearer {}", api_key))
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| AleError::Other(anyhow::anyhow!("API request failed: {}", e)))?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(AleError::Other(anyhow::anyhow!(
                "API request failed: {}",
                error_text
            )));
        }

        let response_body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to parse response: {}", e)))?;

        let text = Self::response_text(&response_body)?;
        let tokens_used = response_body["usage"]["total_tokens"].as_u64().unwrap_or(0) as usize;

        Ok(LlmResponse {
            text,
            tokens_used,
            finish_reason: "stop".to_string(),
        })
    }

    async fn generate_stream(&self, prompt: &str) -> Result<Box<dyn tokio::io::AsyncRead + Unpin>> {
        let response = self.generate(prompt).await?;
        Ok(Box::new(std::io::Cursor::new(response.text.into_bytes())))
    }

    fn model_info(&self) -> crate::ModelInfo {
        crate::ModelInfo {
            name: self.config.model_name.clone().unwrap_or_default(),
            version: "1.0".to_string(),
            device: "remote".to_string(),
            loaded: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_response_text_rejects_missing_content() {
        let response = serde_json::json!({"choices": [{"message": {}}]});
        assert!(RemoteLlm::response_text(&response).is_err());
    }

    #[test]
    fn test_response_text_rejects_empty_content() {
        let response = serde_json::json!({"choices": [{"message": {"content": "  "}}]});
        assert!(RemoteLlm::response_text(&response).is_err());
    }

    #[test]
    fn test_response_text_accepts_content() {
        let response = serde_json::json!({"choices": [{"message": {"content": "hello"}}]});
        assert_eq!(RemoteLlm::response_text(&response).unwrap(), "hello");
    }

    #[tokio::test]
    async fn test_local_llm_encode_simple() {
        let llm = LocalLlm::new(LlmConfig::default()).await.unwrap();
        let ids = llm.encode_simple("hi");
        assert_eq!(ids, vec![104, 105]);
    }

    #[test]
    fn test_ids_to_text_ascii() {
        assert_eq!(ids_to_text(&[104, 101, 108, 108, 111]), "hello");
    }

    #[test]
    fn test_ids_to_text_empty() {
        assert!(ids_to_text(&[]).is_empty());
    }

    #[test]
    fn test_ids_to_text_skips_special() {
        assert!(ids_to_text(&[0, 1, 2, 3]).is_empty());
    }

    #[tokio::test]
    async fn test_local_llm_not_loaded() {
        let llm = LocalLlm::new(LlmConfig::default()).await.unwrap();
        assert!(!llm.is_loaded());
        let info = llm.model_info();
        assert!(!info.loaded);
    }

    #[test]
    fn test_llm_config_default() {
        let config = LlmConfig::default();
        assert!(matches!(config.backend, LlmBackend::Local));
        assert_eq!(config.max_tokens, 512);
    }
}

/// LLM工厂
pub struct LlmFactory;

impl LlmFactory {
    pub async fn create(config: LlmConfig) -> Result<Box<dyn LanguageModel>> {
        match config.backend {
            LlmBackend::Local => {
                let llm = LocalLlm::new(config).await?;
                Ok(Box::new(llm))
            }
            LlmBackend::Remote => {
                let llm = RemoteLlm::new(config).await?;
                Ok(Box::new(llm))
            }
        }
    }
}
