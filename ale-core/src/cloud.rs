use crate::{AleError, Result};
use async_trait::async_trait;
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
    pub provider: CloudProvider,
    pub api_key: String,
    pub api_url: String,
    pub model: String,
    pub wire_api: String,
    pub reasoning_effort: String,
    pub store_responses: bool,
    pub max_tokens: usize,
    pub timeout: Duration,
    pub retry_count: u32,
}

impl Default for CloudConfig {
    fn default() -> Self {
        Self {
            provider: CloudProvider::OpenAI,
            api_key: String::new(),
            api_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o".to_string(),
            wire_api: "chat_completions".to_string(),
            reasoning_effort: String::new(),
            store_responses: false,
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
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self { config, client }
    }

    fn chat_content(response_body: &serde_json::Value) -> Result<String> {
        response_body["choices"][0]["message"]["content"]
            .as_str()
            .filter(|content| !content.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| AleError::CloudApiError("Missing chat response content".to_string()))
    }

    fn uses_responses_api(&self) -> bool {
        self.config.wire_api == "responses"
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.config.api_url.trim_end_matches('/'), path)
    }

    fn responses_request(&self, input: serde_json::Value) -> serde_json::Value {
        let mut body = serde_json::json!({
            "model": self.config.model,
            "input": input,
            "max_output_tokens": self.config.max_tokens,
            "store": self.config.store_responses,
        });
        if !self.config.reasoning_effort.trim().is_empty() {
            body["reasoning"] = serde_json::json!({
                "effort": self.config.reasoning_effort,
            });
        }
        body
    }

    fn responses_content(response_body: &serde_json::Value) -> Result<String> {
        let content = response_body["output"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item["content"].as_array())
            .flatten()
            .filter(|part| part["type"].as_str() == Some("output_text"))
            .filter_map(|part| part["text"].as_str())
            .collect::<Vec<_>>()
            .join("");

        if content.trim().is_empty() {
            Err(AleError::CloudApiError(
                "Missing Responses API output text".to_string(),
            ))
        } else {
            Ok(content)
        }
    }

    fn tokens_used(response_body: &serde_json::Value) -> usize {
        response_body["usage"]["total_tokens"]
            .as_u64()
            .unwrap_or_else(|| {
                response_body["usage"]["input_tokens"].as_u64().unwrap_or(0)
                    + response_body["usage"]["output_tokens"]
                        .as_u64()
                        .unwrap_or(0)
            }) as usize
    }

    fn responses_tools(tools: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
        tools
            .into_iter()
            .map(|tool| {
                if tool["type"].as_str() == Some("function") && tool.get("function").is_some() {
                    let function = &tool["function"];
                    let mut flattened = serde_json::json!({
                        "type": "function",
                        "name": function["name"],
                        "parameters": function["parameters"],
                    });
                    if let Some(description) = function["description"].as_str() {
                        flattened["description"] = serde_json::json!(description);
                    }
                    flattened
                } else {
                    tool
                }
            })
            .collect()
    }

    fn responses_tool_calls(response_body: &serde_json::Value) -> Vec<ToolCall> {
        response_body["output"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["type"].as_str() == Some("function_call"))
            .map(|item| ToolCall {
                id: item["call_id"]
                    .as_str()
                    .or_else(|| item["id"].as_str())
                    .unwrap_or_default()
                    .to_string(),
                function: FunctionCall {
                    name: item["name"].as_str().unwrap_or_default().to_string(),
                    arguments: item["arguments"].as_str().unwrap_or_default().to_string(),
                },
            })
            .collect()
    }

    fn transcription_text(response_body: &serde_json::Value) -> Result<String> {
        response_body["text"]
            .as_str()
            .filter(|text| !text.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| AleError::CloudApiError("Missing transcription text".to_string()))
    }
}

#[async_trait]
impl CloudApi for OpenAIApi {
    async fn chat(&self, messages: Vec<CloudMessage>) -> Result<CloudResponse> {
        let (url, request_body) = if self.uses_responses_api() {
            (
                self.endpoint("responses"),
                self.responses_request(serde_json::json!(messages)),
            )
        } else {
            (
                self.endpoint("chat/completions"),
                serde_json::json!({
                    "model": self.config.model,
                    "messages": messages,
                    "max_tokens": self.config.max_tokens,
                }),
            )
        };

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| AleError::CloudApiError(format!("Request failed: {}", e)))?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(AleError::CloudApiError(format!(
                "API error: {}",
                error_text
            )));
        }

        let response_body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| AleError::CloudApiError(format!("Parse error: {}", e)))?;

        let content = if self.uses_responses_api() {
            Self::responses_content(&response_body)?
        } else {
            Self::chat_content(&response_body)?
        };
        let tokens_used = Self::tokens_used(&response_body);

        Ok(CloudResponse {
            content,
            tokens_used,
            model: self.config.model.clone(),
            provider: self.config.provider.clone(),
        })
    }

    async fn vision(&self, image_data: &[u8], prompt: &str) -> Result<CloudResponse> {
        let image_base64 = general_purpose::STANDARD.encode(image_data);
        let image_url = format!("data:image/jpeg;base64,{}", image_base64);
        let (url, request_body) = if self.uses_responses_api() {
            let input = serde_json::json!([{
                "role": "user",
                "content": [
                    {"type": "input_text", "text": prompt},
                    {"type": "input_image", "image_url": image_url}
                ]
            }]);
            (self.endpoint("responses"), self.responses_request(input))
        } else {
            (
                self.endpoint("chat/completions"),
                serde_json::json!({
                    "model": self.config.model,
                    "messages": [{
                        "role": "user",
                        "content": [
                            {"type": "text", "text": prompt},
                            {"type": "image_url", "image_url": {"url": image_url}}
                        ]
                    }],
                    "max_tokens": self.config.max_tokens,
                }),
            )
        };

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| AleError::CloudApiError(format!("Request failed: {}", e)))?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(AleError::CloudApiError(format!(
                "API error: {}",
                error_text
            )));
        }

        let response_body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| AleError::CloudApiError(format!("Parse error: {}", e)))?;

        let content = if self.uses_responses_api() {
            Self::responses_content(&response_body)?
        } else {
            Self::chat_content(&response_body)?
        };
        let tokens_used = Self::tokens_used(&response_body);

        Ok(CloudResponse {
            content,
            tokens_used,
            model: self.config.model.clone(),
            provider: self.config.provider.clone(),
        })
    }

    async fn transcribe(&self, audio_data: &[u8]) -> Result<CloudResponse> {
        let url = self.endpoint("audio/transcriptions");

        // 创建multipart表单
        let form = reqwest::multipart::Form::new()
            .part(
                "file",
                reqwest::multipart::Part::bytes(audio_data.to_vec())
                    .file_name("audio.wav")
                    .mime_str("audio/wav")
                    .map_err(|e| AleError::CloudApiError(format!("Invalid MIME type: {e}")))?,
            )
            .text("model", "whisper-1");

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .multipart(form)
            .send()
            .await
            .map_err(|e| AleError::CloudApiError(format!("Request failed: {}", e)))?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(AleError::CloudApiError(format!(
                "API error: {}",
                error_text
            )));
        }

        let response_body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| AleError::CloudApiError(format!("Parse error: {}", e)))?;

        let text = Self::transcription_text(&response_body)?;

        Ok(CloudResponse {
            content: text,
            tokens_used: 0,
            model: "whisper-1".to_string(),
            provider: CloudProvider::OpenAI,
        })
    }

    async fn vision_ask(
        &self,
        image_data: &[u8],
        question: &str,
        tools: Option<Vec<serde_json::Value>>,
    ) -> Result<VisionResponse> {
        let image_base64 = general_purpose::STANDARD.encode(image_data);
        let image_url = format!("data:image/jpeg;base64,{}", image_base64);
        let system_prompt = "你是 Ale, My Eyes! 智能视觉辅助助手。用户会发送一张图片和一个问题，请根据图片内容回答问题。如果用户要求执行操作，请使用提供的工具函数。";
        let (url, mut request_body) = if self.uses_responses_api() {
            let input = serde_json::json!([
                {"role": "system", "content": system_prompt},
                {
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": question},
                        {"type": "input_image", "image_url": image_url}
                    ]
                }
            ]);
            (self.endpoint("responses"), self.responses_request(input))
        } else {
            (
                self.endpoint("chat/completions"),
                serde_json::json!({
                    "model": self.config.model,
                    "messages": [
                        {"role": "system", "content": system_prompt},
                        {
                            "role": "user",
                            "content": [
                                {"type": "text", "text": question},
                                {"type": "image_url", "image_url": {"url": image_url}}
                            ]
                        }
                    ],
                    "max_tokens": self.config.max_tokens,
                }),
            )
        };

        if let Some(tools) = tools {
            request_body["tools"] = if self.uses_responses_api() {
                serde_json::json!(Self::responses_tools(tools))
            } else {
                serde_json::json!(tools)
            };
        }

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| AleError::CloudApiError(format!("Request failed: {}", e)))?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(AleError::CloudApiError(format!(
                "API error: {}",
                error_text
            )));
        }

        let response_body: serde_json::Value = response
            .json()
            .await
            .map_err(|e| AleError::CloudApiError(format!("Parse error: {}", e)))?;

        let (content, tool_calls) = if self.uses_responses_api() {
            (
                Self::responses_content(&response_body).unwrap_or_default(),
                Self::responses_tool_calls(&response_body),
            )
        } else {
            let message = &response_body["choices"][0]["message"];
            let calls = message["tool_calls"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|tc| ToolCall {
                    id: tc["id"].as_str().unwrap_or_default().to_string(),
                    function: FunctionCall {
                        name: tc["function"]["name"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        arguments: tc["function"]["arguments"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                    },
                })
                .collect();
            (
                message["content"].as_str().unwrap_or_default().to_string(),
                calls,
            )
        };
        if content.trim().is_empty() && tool_calls.is_empty() {
            return Err(AleError::CloudApiError(
                "Missing vision response content or tool calls".to_string(),
            ));
        }
        let tokens_used = Self::tokens_used(&response_body);

        Ok(VisionResponse {
            content,
            tool_calls: (!tool_calls.is_empty()).then_some(tool_calls),
            tokens_used,
            model: self.config.model.clone(),
        })
    }

    async fn synthesize(&self, text: &str) -> Result<Vec<u8>> {
        let url = self.endpoint("audio/speech");

        let request_body = serde_json::json!({
            "model": "tts-1",
            "input": text,
            "voice": "alloy",
            "response_format": "wav",
        });

        let response = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .json(&request_body)
            .send()
            .await
            .map_err(|e| AleError::CloudApiError(format!("Request failed: {}", e)))?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(AleError::CloudApiError(format!(
                "API error: {}",
                error_text
            )));
        }

        let audio_data = response
            .bytes()
            .await
            .map_err(|e| AleError::CloudApiError(format!("Failed to read audio: {}", e)))?;

        Ok(audio_data.to_vec())
    }

    async fn health_check(&self) -> Result<bool> {
        if self.uses_responses_api() {
            let response = self
                .chat(vec![CloudMessage {
                    role: "user".to_string(),
                    content: "Reply with OK.".to_string(),
                }])
                .await?;
            return Ok(!response.content.trim().is_empty());
        }

        let url = self.endpoint("models");

        let response = self
            .client
            .get(&url)
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .send()
            .await
            .map_err(|e| AleError::CloudApiError(format!("Health check failed: {}", e)))?;

        Ok(response.status().is_success())
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

    #[test]
    fn test_cloud_config_default() {
        let config = CloudConfig::default();
        assert_eq!(config.api_url, "https://api.openai.com/v1");
        assert_eq!(config.model, "gpt-4o");
        assert_eq!(config.wire_api, "chat_completions");
        assert!(config.reasoning_effort.is_empty());
        assert!(!config.store_responses);
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
    fn test_responses_request_includes_reasoning_and_storage_policy() {
        let api = OpenAIApi::new(CloudConfig {
            model: "gpt-5.5".to_string(),
            wire_api: "responses".to_string(),
            reasoning_effort: "xhigh".to_string(),
            store_responses: false,
            max_tokens: 2048,
            ..Default::default()
        });
        let body = api.responses_request(serde_json::json!([
            {"role": "user", "content": "hello"}
        ]));
        assert_eq!(body["model"], "gpt-5.5");
        assert_eq!(body["max_output_tokens"], 2048);
        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert_eq!(body["store"], false);
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn test_responses_content_collects_output_text() {
        let response = serde_json::json!({
            "output": [
                {"type": "reasoning", "content": []},
                {"type": "message", "content": [
                    {"type": "output_text", "text": "hello "},
                    {"type": "output_text", "text": "world"}
                ]}
            ]
        });
        assert_eq!(
            OpenAIApi::responses_content(&response).unwrap(),
            "hello world"
        );
    }

    #[test]
    fn test_responses_content_rejects_missing_output_text() {
        let response = serde_json::json!({"output": [{"type": "reasoning"}]});
        assert!(OpenAIApi::responses_content(&response).is_err());
    }

    #[test]
    fn test_responses_tools_flattens_chat_completions_function() {
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "click",
                "description": "Click an item",
                "parameters": {"type": "object"}
            }
        })];
        let converted = OpenAIApi::responses_tools(tools);
        assert_eq!(converted[0]["type"], "function");
        assert_eq!(converted[0]["name"], "click");
        assert_eq!(converted[0]["description"], "Click an item");
        assert!(converted[0].get("function").is_none());
    }

    #[test]
    fn test_responses_tool_calls_maps_call_id() {
        let response = serde_json::json!({
            "output": [{
                "type": "function_call",
                "call_id": "call_123",
                "name": "click",
                "arguments": "{\"x\":10}"
            }]
        });
        let calls = OpenAIApi::responses_tool_calls(&response);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_123");
        assert_eq!(calls[0].function.name, "click");
        assert_eq!(calls[0].function.arguments, "{\"x\":10}");
    }

    #[test]
    fn test_tokens_used_supports_responses_usage() {
        let response = serde_json::json!({
            "usage": {"input_tokens": 12, "output_tokens": 8}
        });
        assert_eq!(OpenAIApi::tokens_used(&response), 20);
    }
}
