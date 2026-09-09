#[cfg(test)]
use crate::AleError;
use crate::Result;
use async_trait::async_trait;
#[cfg(test)]
use base64::{engine::general_purpose, Engine as _};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// 云端API提供商
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CloudProvider {
    OpenAI,
    Anthropic,
    Google,
    Azure,
    Custom(String),
}

/// 云端API配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudConfig {
    #[serde(default)]
    pub wire_api: crate::model_api::WireApi,
    pub provider: CloudProvider,
    pub api_key: String,
    pub api_url: String,
    pub model: String,
    pub max_tokens: usize,
    pub timeout: Duration,
    pub retry_count: u32,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            wire_api: Default::default(),
            provider: CloudProvider::OpenAI,
            api_key: String::new(),
            api_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o".to_string(),
            max_tokens: 1024,
            timeout: Duration::from_secs(30),
            retry_count: 3,
        }
    }
}

/// 云端API响应
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudResponse {
    pub content: String,
    pub tokens_used: usize,
    pub model: String,
    pub provider: CloudProvider,
}

/// 云端API trait
#[async_trait]
pub trait CloudApi: Send + Sync {
    async fn generate(
        &self,
        request: crate::model_api::ModelRequest,
    ) -> Result<crate::model_api::ModelResponse>;
    /// 发送文本请求
    async fn chat(&self, messages: Vec<CloudMessage>) -> Result<CloudResponse>;

    /// 发送图像请求（描述模式）
    async fn vision(&self, image_data: &[u8], prompt: &str) -> Result<CloudResponse>;

    /// 发送图像请求（问答模式，支持 Function Calling）
    async fn vision_ask(
        &self,
        image_data: &[u8],
        question: &str,
        tools: Option<Vec<serde_json::Value>>,
    ) -> Result<VisionResponse>;

    /// 语音识别
    async fn transcribe(&self, audio_data: &[u8]) -> Result<CloudResponse>;

    /// 语音合成
    async fn synthesize(&self, text: &str) -> Result<Vec<u8>>;

    /// 检查连接状态
    async fn health_check(&self) -> Result<bool>;
}

/// 视觉问答响应（支持 Function Calling）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisionResponse {
    /// 文本回答
    pub content: String,
    /// 工具调用（如果有）
    pub tool_calls: Option<Vec<ToolCall>>,
    pub tokens_used: usize,
    pub model: String,
}

/// 工具调用
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub function: FunctionCall,
}

/// 函数调用
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

/// 云端消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudMessage {
    pub role: String,
    pub content: String,
}

/// OpenAI API 实现
pub struct OpenAIApi {
    config: CloudConfig,
    client: reqwest::Client,
}

impl OpenAIApi {
    pub fn new(config: CloudConfig) -> Self {
        let client = crate::model_api::client(&config);
        Self { config, client }
    }

    #[cfg(test)]
    fn chat_content(response_body: &serde_json::Value) -> Result<String> {
        response_body["choices"][0]["message"]["content"]
            .as_str()
            .filter(|content| !content.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| AleError::CloudApiError("Missing chat response content".to_string()))
    }

    fn transcription_text(response_body: &serde_json::Value) -> Result<String> {
        response_body["text"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                crate::model_api::ModelCallError::new(
                    crate::model_api::ErrorKind::InvalidOutput,
                    "Missing transcription text",
                )
                .into()
            })
    }

    #[cfg(test)]
    fn vision_request_body(&self, image_data: &[u8], prompt: &str) -> serde_json::Value {
        let image_base64 = general_purpose::STANDARD.encode(image_data);
        serde_json::json!({
            "model": self.config.model,
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "text",
                            "text": prompt
                        },
                        {
                            "type": "image_url",
                            "image_url": {
                                "url": format!("data:image/jpeg;base64,{}", image_base64)
                            }
                        }
                    ]
                }
            ],
            "max_tokens": self.config.max_tokens,
        })
    }
}

#[async_trait]
impl CloudApi for OpenAIApi {
    async fn generate(
        &self,
        request: crate::model_api::ModelRequest,
    ) -> Result<crate::model_api::ModelResponse> {
        crate::model_api::generate(&self.config, &self.client, request).await
    }
    async fn chat(&self, messages: Vec<CloudMessage>) -> Result<CloudResponse> {
        let response = self
            .generate(crate::model_api::ModelRequest {
                messages,
                ..Default::default()
            })
            .await?;
        Ok(CloudResponse {
            content: response.content,
            tokens_used: response.tokens_used,
            model: response.model,
            provider: self.config.provider.clone(),
        })
    }
    async fn vision(&self, image_data: &[u8], prompt: &str) -> Result<CloudResponse> {
        let response = self.vision_ask(image_data, prompt, None).await?;
        Ok(CloudResponse {
            content: response.content,
            tokens_used: response.tokens_used,
            model: response.model,
            provider: self.config.provider.clone(),
        })
    }
    async fn vision_ask(
        &self,
        image_data: &[u8],
        question: &str,
        tools: Option<Vec<serde_json::Value>>,
    ) -> Result<VisionResponse> {
        let response = self
            .generate(crate::model_api::ModelRequest {
                image: Some(image_data.to_vec()),
                tools: tools.unwrap_or_default(),
                ..crate::model_api::ModelRequest::text(question)
            })
            .await?;
        Ok(VisionResponse {
            content: response.content,
            tool_calls: (!response.tool_calls.is_empty()).then_some(response.tool_calls),
            tokens_used: response.tokens_used,
            model: response.model,
        })
    }
    async fn transcribe(&self, audio_data: &[u8]) -> Result<CloudResponse> {
        if !matches!(
            self.config.wire_api,
            crate::model_api::WireApi::OpenaiChatCompletions
                | crate::model_api::WireApi::OpenaiResponses
        ) {
            return Err(crate::model_api::ModelCallError::new(
                crate::model_api::ErrorKind::Unsupported,
                "Configure an independent OpenAI-compatible transcription endpoint",
            )
            .into());
        }
        let model = &self.config.model;
        let form = reqwest::multipart::Form::new()
            .part(
                "file",
                reqwest::multipart::Part::bytes(audio_data.to_vec())
                    .file_name("audio.wav")
                    .mime_str("audio/wav")
                    .expect("valid MIME"),
            )
            .text("model", model.to_string());
        let response = self
            .client
            .post(format!(
                "{}/audio/transcriptions",
                self.config.api_url.trim_end_matches('/')
            ))
            .bearer_auth(&self.config.api_key)
            .multipart(form)
            .send()
            .await
            .map_err(crate::model_api::network_error)?;
        let body = crate::model_api::checked(response)
            .await?
            .json()
            .await
            .map_err(crate::model_api::response_error)?;
        Ok(CloudResponse {
            content: Self::transcription_text(&body)?,
            tokens_used: 0,
            model: model.into(),
            provider: self.config.provider.clone(),
        })
    }
    async fn synthesize(&self, text: &str) -> Result<Vec<u8>> {
        if !matches!(
            self.config.wire_api,
            crate::model_api::WireApi::OpenaiChatCompletions
                | crate::model_api::WireApi::OpenaiResponses
        ) {
            return Err(crate::model_api::ModelCallError::new(
                crate::model_api::ErrorKind::Unsupported,
                "Speech synthesis requires an OpenAI-compatible endpoint",
            )
            .into());
        }
        let response = self.client.post(format!("{}/audio/speech",self.config.api_url.trim_end_matches('/')))
            .bearer_auth(&self.config.api_key).json(&serde_json::json!({"model":"tts-1","input":text,"voice":"alloy","response_format":"wav"}))
            .send().await.map_err(crate::model_api::network_error)?;
        Ok(crate::model_api::checked(response)
            .await?
            .bytes()
            .await
            .map_err(crate::model_api::network_error)?
            .to_vec())
    }
    async fn health_check(&self) -> Result<bool> {
        let response = self
            .generate(crate::model_api::ModelRequest::text("Reply with OK."))
            .await?;
        Ok(!response.content.trim().is_empty())
    }
}

/// 云端API工厂
pub struct CloudApiFactory;

impl CloudApiFactory {
    pub fn create(config: CloudConfig) -> Box<dyn CloudApi> {
        match config.provider {
            CloudProvider::OpenAI => Box::new(OpenAIApi::new(config)),
            _ => {
                // 其他提供商的实现
                Box::new(OpenAIApi::new(config))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn mock_http_response(status: &str, body: &str, delay: Duration) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let status = status.to_string();
        let body = body.to_string();
        tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut request = vec![0; 32 * 1024];
            let _ = stream.read(&mut request).await;
            tokio::time::sleep(delay).await;
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        format!("http://{address}")
    }

    fn mock_api(api_url: String, timeout: Duration) -> OpenAIApi {
        OpenAIApi::new(CloudConfig {
            api_key: "test".to_string(),
            api_url,
            timeout,
            retry_count: 0,
            ..Default::default()
        })
    }

    #[test]
    fn test_cloud_config_default() {
        let config = CloudConfig::default();
        assert_eq!(config.api_url, "https://api.openai.com/v1");
        assert_eq!(config.model, "gpt-4o");
        assert_eq!(config.max_tokens, 1024);
        assert_eq!(config.timeout, Duration::from_secs(30));
    }

    #[test]
    fn test_cloud_provider_serialization() {
        let provider = CloudProvider::OpenAI;
        let json = serde_json::to_string(&provider).unwrap();
        let restored: CloudProvider = serde_json::from_str(&json).unwrap();
        assert!(matches!(restored, CloudProvider::OpenAI));
    }

    #[test]
    fn test_cloud_api_factory_creates_openai() {
        let config = CloudConfig {
            provider: CloudProvider::OpenAI,
            api_key: "test".to_string(),
            ..Default::default()
        };
        let _api = CloudApiFactory::create(config);
    }

    #[test]
    fn test_cloud_api_factory_custom_provider() {
        let config = CloudConfig {
            provider: CloudProvider::Custom("test".to_string()),
            api_key: "test".to_string(),
            ..Default::default()
        };
        let _api = CloudApiFactory::create(config);
    }

    #[test]
    fn test_chat_content_rejects_missing_content() {
        let response = serde_json::json!({"choices": [{"message": {}}]});
        assert!(OpenAIApi::chat_content(&response).is_err());
    }

    #[test]
    fn test_chat_content_rejects_empty_content() {
        let response = serde_json::json!({"choices": [{"message": {"content": "  "}}]});
        assert!(OpenAIApi::chat_content(&response).is_err());
    }

    #[test]
    fn test_chat_content_accepts_content() {
        let response = serde_json::json!({"choices": [{"message": {"content": "hello"}}]});
        assert_eq!(OpenAIApi::chat_content(&response).unwrap(), "hello");
    }

    #[test]
    fn test_transcription_text_rejects_missing_text() {
        let response = serde_json::json!({});
        assert!(OpenAIApi::transcription_text(&response).is_err());
    }

    #[test]
    fn vision_description_uses_configured_model() {
        let api = OpenAIApi::new(CloudConfig {
            model: "custom-vision-model".to_string(),
            ..Default::default()
        });
        let request = api.vision_request_body(b"jpeg", "describe this");
        assert_eq!(request["model"], "custom-vision-model");
        assert_eq!(
            request["messages"][0]["content"][0]["text"],
            "describe this"
        );
    }

    #[tokio::test]
    async fn mock_openai_chat_success() {
        let url = mock_http_response(
            "200 OK",
            r#"{"choices":[{"finish_reason":"stop","message":{"content":"ok"}}],"usage":{"total_tokens":3}}"#,
            Duration::ZERO,
        )
        .await;
        let response = mock_api(url, Duration::from_secs(1))
            .chat(vec![CloudMessage {
                role: "user".to_string(),
                content: "hello".to_string(),
            }])
            .await
            .unwrap();
        assert_eq!(response.content, "ok");
        assert_eq!(response.tokens_used, 3);
    }

    #[tokio::test]
    async fn mock_openai_transcription_success() {
        let url = mock_http_response("200 OK", r#"{"text":"heard"}"#, Duration::ZERO).await;
        let response = mock_api(url, Duration::from_secs(1))
            .transcribe(b"RIFF-test-wav")
            .await
            .unwrap();
        assert_eq!(response.content, "heard");
    }

    #[tokio::test]
    async fn mock_openai_timeout_is_reported() {
        let url = mock_http_response(
            "200 OK",
            r#"{"choices":[{"message":{"content":"late"}}]}"#,
            Duration::from_millis(150),
        )
        .await;
        let error = mock_api(url, Duration::from_millis(25))
            .chat(Vec::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("Timeout"));
    }

    #[tokio::test]
    async fn mock_openai_rate_limit_is_reported() {
        let url = mock_http_response(
            "429 Too Many Requests",
            r#"{"error":"rate_limit"}"#,
            Duration::ZERO,
        )
        .await;
        let error = mock_api(url, Duration::from_secs(1))
            .chat(Vec::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("RateLimited"));
        assert!(!error.contains("rate_limit"));
    }

    #[tokio::test]
    async fn mock_openai_malformed_json_is_rejected() {
        let url = mock_http_response("200 OK", "not-json", Duration::ZERO).await;
        let error = mock_api(url, Duration::from_secs(1))
            .chat(Vec::new())
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("InvalidOutput"));
    }
}
