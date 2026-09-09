use crate::cloud::{CloudConfig, CloudMessage, FunctionCall, ToolCall};
use crate::{AleError, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;
#[cfg(test)]
#[path = "model_api_tests.rs"]
pub(crate) mod tests;
tokio::task_local! { pub static DEADLINE: tokio::time::Instant; }
pub fn deadline() -> tokio::time::Instant {
    DEADLINE
        .try_with(|deadline| *deadline)
        .unwrap_or_else(|_| tokio::time::Instant::now() + Duration::from_secs(85))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireApi {
    #[default]
    OpenaiChatCompletions,
    OpenaiResponses,
    AnthropicMessages,
    GoogleGenerateContent,
}
impl WireApi {
    pub const ALL: [Self; 4] = [
        Self::OpenaiChatCompletions,
        Self::OpenaiResponses,
        Self::AnthropicMessages,
        Self::GoogleGenerateContent,
    ];
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorKind {
    Authentication,
    Permission,
    NotFound,
    Unsupported,
    RateLimited,
    Network,
    Service,
    Timeout,
    InvalidRequest,
    InvalidOutput,
    Refused,
    Cancelled,
}
#[derive(Clone, Debug, thiserror::Error, Serialize, Deserialize)]
#[error("{kind:?}: {message}")]
pub struct ModelCallError {
    pub kind: ErrorKind,
    pub http_status: Option<u16>,
    pub retry_after_ms: Option<u64>,
    pub message: String,
}
impl ModelCallError {
    pub fn new(kind: ErrorKind, message: &str) -> Self {
        Self {
            kind,
            message: message.into(),
            http_status: None,
            retry_after_ms: None,
        }
    }
    pub fn transient(&self) -> bool {
        matches!(
            self.kind,
            ErrorKind::Network | ErrorKind::RateLimited | ErrorKind::Service | ErrorKind::Timeout
        )
    }
}
pub fn network_error(error: reqwest::Error) -> AleError {
    ModelCallError::new(
        if error.is_timeout() {
            ErrorKind::Timeout
        } else {
            ErrorKind::Network
        },
        "Model request failed",
    )
    .into()
}
pub async fn checked(response: reqwest::Response) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let kind = match status.as_u16() {
        401 => ErrorKind::Authentication,
        403 => ErrorKind::Permission,
        404 => ErrorKind::NotFound,
        408 | 504 => ErrorKind::Timeout,
        429 => ErrorKind::RateLimited,
        500..=599 => ErrorKind::Service,
        _ => ErrorKind::InvalidRequest,
    };
    let retry_after_ms = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.parse::<u64>()
                .ok()
                .map(|s| s.saturating_mul(1000))
                .or_else(|| {
                    chrono::DateTime::parse_from_rfc2822(v).ok().map(|at| {
                        (at.timestamp_millis() - chrono::Utc::now().timestamp_millis()).max(0)
                            as u64
                    })
                })
        });
    // Upstream bodies and URLs can echo credentials or screenshots.
    Err(ModelCallError {
        kind,
        http_status: Some(status.as_u16()),
        retry_after_ms,
        message: format!("Model endpoint returned HTTP {}", status.as_u16()),
    }
    .into())
}
fn invalid(message: &str) -> AleError {
    ModelCallError::new(ErrorKind::InvalidOutput, message).into()
}
pub(crate) fn response_error(error: reqwest::Error) -> AleError {
    if error.is_timeout() || error.is_body() || error.is_connect() {
        network_error(error)
    } else {
        invalid("Malformed model response")
    }
}
#[derive(Clone, Debug, Default)]
pub struct ModelRequest {
    pub messages: Vec<CloudMessage>,
    pub image: Option<Vec<u8>>,
    pub tools: Vec<Value>,
    pub require_tool: bool,
}
#[derive(Clone, Debug)]
pub struct ModelResponse {
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    pub model: String,
    pub tokens_used: usize,
    pub completion: Completion,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completion {
    Complete,
}
impl ModelRequest {
    pub fn text(text: &str) -> Self {
        Self {
            messages: vec![CloudMessage {
                role: "user".into(),
                content: text.into(),
            }],
            ..Self::default()
        }
    }
}
fn image_part(bytes: &[u8]) -> Result<(&'static str, String)> {
    let mime = match image::guess_format(bytes) {
        Ok(image::ImageFormat::Png) => "image/png",
        Ok(image::ImageFormat::Jpeg) => "image/jpeg",
        Ok(image::ImageFormat::WebP) => "image/webp",
        Ok(image::ImageFormat::Gif) => "image/gif",
        _ => {
            return Err(
                ModelCallError::new(ErrorKind::InvalidRequest, "Unsupported image format").into(),
            )
        }
    };
    Ok((mime, STANDARD.encode(bytes)))
}
fn tool_functions(request: &ModelRequest) -> Result<Vec<Value>> {
    request
        .tools
        .iter()
        .map(|tool| {
            let function = tool.get("function").unwrap_or(tool);
            if function["name"].as_str().is_none_or(str::is_empty)
                || !function["parameters"].is_object()
            {
                return Err(ModelCallError::new(
                    ErrorKind::InvalidRequest,
                    "Invalid tool definition",
                )
                .into());
            }
            jsonschema::validator_for(&function["parameters"]).map_err(|_| {
                ModelCallError::new(ErrorKind::InvalidRequest, "Invalid tool schema")
            })?;
            Ok(function.clone())
        })
        .collect()
}
pub fn request_body(config: &CloudConfig, request: &ModelRequest) -> Result<(String, Value)> {
    let functions = tool_functions(request)?;
    let image = request.image.as_deref().map(image_part).transpose()?;
    let last_user = request.messages.iter().rposition(|m| m.role == "user");
    if (image.is_some() && last_user.is_none()) || (request.require_tool && functions.is_empty()) {
        return Err(ModelCallError::new(
            ErrorKind::InvalidRequest,
            "Image or required tools missing from request",
        )
        .into());
    }
    let base = config.api_url.trim_end_matches('/');
    let result = match config.wire_api {
        WireApi::OpenaiChatCompletions => {
            let messages: Vec<Value> = request.messages.iter().enumerate().map(|(i,m)| {
                let mut content = json!(m.content);
                if Some(i) == last_user {
                    if let Some((mime, data)) = &image { content = json!([{"type":"text","text":m.content},{"type":"image_url","image_url":{"url":format!("data:{mime};base64,{data}")}}]); }
                }
                json!({"role":m.role,"content":content})
            }).collect();
            let mut body = json!({"model":config.model,"messages":messages,"max_tokens":config.max_tokens,"store":false});
            if !functions.is_empty() {
                body["tools"] = functions
                    .iter()
                    .map(|f| json!({"type":"function","function":f}))
                    .collect();
                body["tool_choice"] = json!(if request.require_tool {
                    "required"
                } else {
                    "auto"
                });
            }
            (format!("{base}/chat/completions"), body)
        }
        WireApi::OpenaiResponses => {
            let input: Vec<Value> = request.messages.iter().enumerate().map(|(i,m)| {
                let mut content = vec![json!({"type":if m.role == "assistant" {"output_text"} else {"input_text"},"text":m.content})];
                if Some(i) == last_user { if let Some((mime,data)) = &image { content.push(json!({"type":"input_image","image_url":format!("data:{mime};base64,{data}")})); } }
                json!({"role":m.role,"content":content})
            }).collect();
            let mut body = json!({"model":config.model,"input":input,"max_output_tokens":config.max_tokens,"store":false});
            if !functions.is_empty() {
                body["tools"] = functions.iter().map(|f| json!({"type":"function","name":f["name"],"description":f.get("description").cloned().unwrap_or(json!("")),"parameters":f["parameters"],"strict":false})).collect();
                body["tool_choice"] = json!(if request.require_tool {
                    "required"
                } else {
                    "auto"
                });
            }
            (format!("{base}/responses"), body)
        }
        WireApi::AnthropicMessages => {
            let messages: Vec<Value> = request.messages.iter().enumerate().filter(|(_,m)| m.role != "system").map(|(i,m)| {
                let mut content = vec![json!({"type":"text","text":m.content})];
                if Some(i) == last_user { if let Some((mime,data)) = &image { content.push(json!({"type":"image","source":{"type":"base64","media_type":mime,"data":data}})); } }
                json!({"role":m.role,"content":content})
            }).collect();
            let mut body =
                json!({"model":config.model,"messages":messages,"max_tokens":config.max_tokens});
            let system = request
                .messages
                .iter()
                .filter(|m| m.role == "system")
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if !system.is_empty() {
                body["system"] = json!(system);
            }
            if !functions.is_empty() {
                body["tools"] = functions.iter().map(|f| json!({"name":f["name"],"description":f.get("description").cloned().unwrap_or(json!("")),"input_schema":f["parameters"]})).collect();
                body["tool_choice"] =
                    json!({"type":if request.require_tool { "any" } else { "auto" }});
            }
            (format!("{base}/messages"), body)
        }
        WireApi::GoogleGenerateContent => {
            let contents: Vec<Value> = request
                .messages
                .iter()
                .enumerate()
                .filter(|(_, m)| m.role != "system")
                .map(|(i, m)| {
                    let mut parts = vec![json!({"text":m.content})];
                    if Some(i) == last_user {
                        if let Some((mime, data)) = &image {
                            parts.push(json!({"inlineData":{"mimeType":mime,"data":data}}));
                        }
                    }
                    json!({"role":if m.role=="assistant" {"model"} else {"user"},"parts":parts})
                })
                .collect();
            let mut body = json!({"contents":contents,"generationConfig":{"maxOutputTokens":config.max_tokens}});
            let system = request
                .messages
                .iter()
                .filter(|m| m.role == "system")
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            if !system.is_empty() {
                body["systemInstruction"] = json!({"parts":[{"text":system}]});
            }
            if !functions.is_empty() {
                let declarations: Vec<Value> = functions.iter().map(|f| json!({"name": f["name"], "description": f.get("description").cloned().unwrap_or(json!("")), "parametersJsonSchema": f["parameters"]})).collect();
                body["tools"] = json!([{"functionDeclarations":declarations}]);
                body["toolConfig"] = json!({"functionCallingConfig":{"mode":if request.require_tool {"ANY"} else {"AUTO"}}});
            }
            (
                format!(
                    "{base}/models/{}:generateContent",
                    urlencoding::encode(config.model.trim_start_matches("models/"))
                ),
                body,
            )
        }
    };
    Ok(result)
}
pub fn parse_response(
    config: &CloudConfig,
    request: &ModelRequest,
    body: Value,
) -> Result<ModelResponse> {
    let mut text = Vec::new();
    let mut calls = Vec::new();
    let mut add_call = |id: String, name: Option<&str>, arguments: Value| -> Result<()> {
        let name = name
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid("Missing tool name"))?;
        let arguments: Value = if let Some(value) = arguments.as_str() {
            serde_json::from_str(value).map_err(|_| invalid("Malformed tool arguments"))?
        } else {
            arguments
        };
        if !arguments.is_object() {
            return Err(invalid("Tool arguments must be an object"));
        }
        let functions = tool_functions(request)?;
        let function = functions
            .iter()
            .find(|f| f["name"] == name)
            .ok_or_else(|| invalid("Unknown tool returned"))?;
        validate_arguments(&function["parameters"], &arguments)?;
        if id.is_empty() {
            return Err(invalid("Missing tool call ID"));
        }
        if calls.iter().any(|call: &ToolCall| call.id == id) {
            return Err(invalid("Duplicate tool call ID"));
        }
        calls.push(ToolCall {
            id,
            function: FunctionCall {
                name: name.into(),
                arguments: arguments.to_string(),
            },
        });
        Ok(())
    };
    let (model, tokens) = match config.wire_api {
        WireApi::OpenaiChatCompletions => {
            let choice = &body["choices"][0];
            let message = &choice["message"];
            if !message["refusal"].is_null() {
                return Err(
                    ModelCallError::new(ErrorKind::Refused, "Model refused this request").into(),
                );
            }
            if !matches!(
                choice["finish_reason"].as_str(),
                Some("stop" | "tool_calls")
            ) {
                return Err(invalid("Model output was incomplete or filtered"));
            }
            if let Some(t) = message["content"].as_str() {
                text.push(t.to_string());
            }
            if let Some(list) = message["tool_calls"].as_array() {
                for call in list {
                    add_call(
                        call["id"].as_str().unwrap_or_default().into(),
                        call["function"]["name"].as_str(),
                        call["function"]["arguments"].clone(),
                    )?;
                }
            }
            (
                body["model"].as_str(),
                body["usage"]["total_tokens"].as_u64(),
            )
        }
        WireApi::OpenaiResponses => {
            if body["status"].as_str() != Some("completed") {
                return Err(invalid("Response did not complete"));
            }
            for item in body["output"]
                .as_array()
                .ok_or_else(|| invalid("Missing response output"))?
            {
                if item
                    .get("status")
                    .is_some_and(|status| status != "completed")
                {
                    return Err(invalid("Response item did not complete"));
                }
                match item["type"].as_str() {
                    Some("function_call") => add_call(
                        item["call_id"].as_str().unwrap_or_default().into(),
                        item["name"].as_str(),
                        item["arguments"].clone(),
                    )?,
                    Some("message") => {
                        if let Some(parts) = item["content"].as_array() {
                            for part in parts {
                                if part["type"] == "refusal" {
                                    return Err(ModelCallError::new(
                                        ErrorKind::Refused,
                                        "Model refused this request",
                                    )
                                    .into());
                                }
                                if part["type"] == "output_text" {
                                    if let Some(t) = part["text"].as_str() {
                                        text.push(t.into());
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            (
                body["model"].as_str(),
                body["usage"]["total_tokens"].as_u64(),
            )
        }
        WireApi::AnthropicMessages => {
            if body["stop_reason"] == "refusal" {
                return Err(
                    ModelCallError::new(ErrorKind::Refused, "Model refused this request").into(),
                );
            }
            if !matches!(
                body["stop_reason"].as_str(),
                Some("end_turn" | "stop_sequence" | "tool_use")
            ) {
                return Err(invalid("Model output did not complete"));
            }
            for part in body["content"]
                .as_array()
                .ok_or_else(|| invalid("Missing message content"))?
            {
                match part["type"].as_str() {
                    Some("text") => {
                        if let Some(t) = part["text"].as_str() {
                            text.push(t.into());
                        }
                    }
                    Some("tool_use") => add_call(
                        part["id"].as_str().unwrap_or_default().into(),
                        part["name"].as_str(),
                        part["input"].clone(),
                    )?,
                    _ => {}
                }
            }
            (
                body["model"].as_str(),
                Some(
                    body["usage"]["input_tokens"].as_u64().unwrap_or(0)
                        + body["usage"]["output_tokens"].as_u64().unwrap_or(0),
                ),
            )
        }
        WireApi::GoogleGenerateContent => {
            let candidate = &body["candidates"][0];
            if body["promptFeedback"].get("blockReason").is_some()
                || matches!(
                    candidate["finishReason"].as_str(),
                    Some("SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT")
                )
            {
                return Err(
                    ModelCallError::new(ErrorKind::Refused, "Model refused this request").into(),
                );
            }
            if candidate["finishReason"].as_str() != Some("STOP") {
                return Err(invalid("Model output did not complete"));
            }
            for (index, part) in candidate["content"]["parts"]
                .as_array()
                .ok_or_else(|| invalid("Missing model parts"))?
                .iter()
                .enumerate()
            {
                if let Some(t) = part["text"].as_str().filter(|_| part["thought"] != true) {
                    text.push(t.into());
                }
                if let Some(call) = part.get("functionCall") {
                    add_call(
                        call["id"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("gemini-{index}")),
                        call["name"].as_str(),
                        call["args"].clone(),
                    )?;
                }
            }
            (
                body["modelVersion"].as_str(),
                body["usageMetadata"]["totalTokenCount"].as_u64(),
            )
        }
    };
    let content = text.join("\n");
    if content.trim().is_empty() && calls.is_empty() {
        return Err(invalid("Missing model content or tools"));
    }
    if request.require_tool && calls.is_empty() {
        return Err(invalid("Expected tool call was not returned"));
    }
    Ok(ModelResponse {
        content,
        tool_calls: calls,
        model: model.unwrap_or(&config.model).into(),
        tokens_used: tokens.unwrap_or(0) as usize,
        completion: Completion::Complete,
    })
}
fn validate_arguments(schema: &Value, value: &Value) -> Result<()> {
    let validator = jsonschema::validator_for(schema)
        .map_err(|_| ModelCallError::new(ErrorKind::InvalidRequest, "Invalid tool schema"))?;
    if !validator.is_valid(value) {
        return Err(invalid("Tool arguments do not match the declared schema"));
    }
    Ok(())
}
pub async fn generate(
    config: &CloudConfig,
    client: &reqwest::Client,
    request: ModelRequest,
) -> Result<ModelResponse> {
    let (url, body) = request_body(config, &request)?;
    let builder = client.post(url).json(&body);
    let builder = match config.wire_api {
        WireApi::AnthropicMessages => builder
            .header("x-api-key", &config.api_key)
            .header("anthropic-version", "2023-06-01"),
        WireApi::GoogleGenerateContent => builder.header("x-goog-api-key", &config.api_key),
        _ => builder.bearer_auth(&config.api_key),
    };
    let response = checked(builder.send().await.map_err(network_error)?).await?;
    let body = response.json().await.map_err(response_error)?;
    parse_response(config, &request, body)
}
pub fn client(config: &CloudConfig) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(config.timeout)
        .connect_timeout(Duration::from_secs(5).min(config.timeout))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("valid HTTP client configuration")
}
pub async fn retry<T, F, Fut>(
    deadline: tokio::time::Instant,
    retries: u32,
    mut call: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    for attempt in 0..=retries.min(1) {
        if tokio::time::Instant::now() >= deadline {
            return Err(ModelCallError::new(ErrorKind::Timeout, "Model budget exhausted").into());
        }
        let result = tokio::time::timeout_at(deadline, call())
            .await
            .unwrap_or_else(|_| {
                Err(ModelCallError::new(ErrorKind::Timeout, "Model budget exhausted").into())
            });
        match result {
            Err(AleError::ModelCall(ref error))
                if error.transient() && attempt < retries.min(1) =>
            {
                let jitter = 200 + (chrono::Utc::now().timestamp_subsec_millis() as u64 % 201);
                let delay = Duration::from_millis(error.retry_after_ms.unwrap_or(jitter));
                if tokio::time::Instant::now() + delay >= deadline {
                    return result;
                }
                tokio::time::sleep(delay).await;
            }
            _ => return result,
        }
    }
    unreachable!()
}
