use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub(crate) async fn mock_http(
    status: &str,
    body: Value,
    headers: &str,
) -> (String, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let body = body.to_string();
    let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}", body.len());
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut chunk = [0; 4096];
            let count = stream.read(&mut chunk).await.unwrap();
            assert!(count > 0);
            request.extend_from_slice(&chunk[..count]);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]).to_lowercase();
                let length: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .unwrap()
                    .trim()
                    .parse()
                    .unwrap();
                if request.len() >= end + 4 + length {
                    break;
                }
            }
        }
        stream.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&request).into_owned()
    });
    (format!("http://{address}/v1"), task)
}

fn tool_request(image: bool) -> ModelRequest {
    ModelRequest {
        messages: vec![
            CloudMessage {
                role: "system".into(),
                content: "safe test".into(),
            },
            CloudMessage {
                role: "user".into(),
                content: "inspect".into(),
            },
        ],
        image: image.then(crate::model_probe::fixture_image),
        tools: vec![
            json!({"type":"function","function":{"name":"probe_echo","parameters":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"],"additionalProperties":false}}}),
        ],
        require_tool: true,
    }
}

fn tool_response(wire: WireApi) -> Value {
    match wire {
        WireApi::OpenaiChatCompletions => {
            json!({"choices":[{"finish_reason":"tool_calls","message":{"content":null,"tool_calls":[{"id":"call-1","type":"function","function":{"name":"probe_echo","arguments":"{\"value\":\"red\"}"}}]}}]})
        }
        WireApi::OpenaiResponses => {
            json!({"status":"completed","output":[{"type":"reasoning","summary":[]},{"type":"function_call","call_id":"call-1","name":"probe_echo","arguments":"{\"value\":\"red\"}"}]})
        }
        WireApi::AnthropicMessages => {
            json!({"stop_reason":"tool_use","content":[{"type":"tool_use","id":"call-1","name":"probe_echo","input":{"value":"red"}}]})
        }
        WireApi::GoogleGenerateContent => {
            json!({"candidates":[{"finishReason":"STOP","content":{"parts":[{"functionCall":{"name":"probe_echo","args":{"value":"red"}}}]}}]})
        }
    }
}

#[tokio::test]
async fn four_protocols_send_auth_images_and_tools_and_normalize_tool_only_output() {
    for wire in WireApi::ALL {
        let (url, captured) = mock_http("200 OK", tool_response(wire), "").await;
        let config = CloudConfig {
            wire_api: wire,
            api_url: url,
            api_key: "test-secret".into(),
            model: "test-model".into(),
            ..Default::default()
        };
        let output = generate(&config, &client(&config), tool_request(true))
            .await
            .unwrap();
        assert_eq!(output.tool_calls.len(), 1);
        assert_eq!(output.tool_calls[0].function.name, "probe_echo");
        assert!(output.content.is_empty());
        let request = captured.await.unwrap();
        let (headers, body) = request.split_once("\r\n\r\n").unwrap();
        let headers = headers.to_lowercase();
        assert!(!headers.lines().next().unwrap().contains("test-secret"));
        let body: Value = serde_json::from_str(body).unwrap();
        match wire {
            WireApi::OpenaiChatCompletions => {
                assert!(headers.starts_with("post /v1/chat/completions "));
                assert!(headers.contains("authorization: bearer test-secret"));
                assert_eq!(body["tools"][0]["function"]["name"], "probe_echo");
                assert!(body["messages"][1]["content"][1]["image_url"]["url"]
                    .as_str()
                    .unwrap()
                    .starts_with("data:image/png;base64,"));
            }
            WireApi::OpenaiResponses => {
                assert!(headers.starts_with("post /v1/responses "));
                assert!(headers.contains("authorization: bearer test-secret"));
                assert_eq!(body["tools"][0]["name"], "probe_echo");
                assert_eq!(body["input"][1]["content"][1]["type"], "input_image");
                assert_eq!(body["store"], false);
            }
            WireApi::AnthropicMessages => {
                assert!(headers.starts_with("post /v1/messages "));
                assert!(headers.contains("x-api-key: test-secret"));
                assert!(headers.contains("anthropic-version: 2023-06-01"));
                assert_eq!(body["system"], "safe test");
                assert_eq!(
                    body["messages"][0]["content"][1]["source"]["media_type"],
                    "image/png"
                );
                assert_eq!(body["tool_choice"]["type"], "any");
            }
            WireApi::GoogleGenerateContent => {
                assert!(headers.starts_with("post /v1/models/test-model:generatecontent "));
                assert!(headers.contains("x-goog-api-key: test-secret"));
                assert_eq!(
                    body["contents"][0]["parts"][1]["inlineData"]["mimeType"],
                    "image/png"
                );
                assert_eq!(
                    body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"]["type"],
                    "object"
                );
            }
        }
        let (_, text_body) = request_body(&config, &tool_request(false)).unwrap();
        assert!(
            text_body.get("tools").is_some(),
            "text-only tools lost for {wire:?}"
        );
        assert!(!text_body.to_string().contains("base64"));
    }
}

#[test]
fn rejects_truncated_unknown_and_schema_invalid_tools() {
    for wire in WireApi::ALL {
        let config = CloudConfig {
            wire_api: wire,
            ..Default::default()
        };
        let request = tool_request(false);
        let mut body = tool_response(wire);
        match wire {
            WireApi::OpenaiChatCompletions => body["choices"][0]["finish_reason"] = json!("length"),
            WireApi::OpenaiResponses => body["status"] = json!("incomplete"),
            WireApi::AnthropicMessages => body["stop_reason"] = json!("max_tokens"),
            WireApi::GoogleGenerateContent => {
                body["candidates"][0]["finishReason"] = json!("MAX_TOKENS")
            }
        }
        assert!(parse_response(&config, &request, body).is_err());
        let mut no_tools = request.clone();
        no_tools.tools.clear();
        assert!(parse_response(&config, &no_tools, tool_response(wire)).is_err());
        let mut different_schema = request.clone();
        different_schema.tools[0]["function"]["parameters"]["properties"]["value"]["type"] =
            json!("number");
        assert!(parse_response(&config, &different_schema, tool_response(wire)).is_err());
    }
    let schema = json!({"oneOf":[{"type":"object","required":["x"],"properties":{"x":{"type":"number","minimum":0}}},{"type":"string"}]});
    assert!(validate_arguments(&schema, &json!({"x":-1})).is_err());
    assert!(validate_arguments(&schema, &json!({})).is_err());
    assert!(validate_arguments(&schema, &json!({"x":1})).is_ok());
}

#[test]
fn responses_collect_multiple_items_and_reject_malformed_arguments() {
    let config = CloudConfig {
        wire_api: WireApi::OpenaiResponses,
        ..Default::default()
    };
    let mut body = tool_response(config.wire_api);
    body["output"].as_array_mut().unwrap().push(json!({"type":"message","content":[{"type":"output_text","text":"first"},{"type":"output_text","text":"second"}]}));
    body["output"].as_array_mut().unwrap().push(json!({"type":"function_call","call_id":"call-2","name":"probe_echo","arguments":"{\"value\":\"red\"}"}));
    let response = parse_response(&config, &tool_request(false), body.clone()).unwrap();
    assert_eq!(response.content, "first\nsecond");
    assert_eq!(response.tool_calls.len(), 2);
    body["output"][1]["arguments"] = json!("{broken");
    assert!(parse_response(&config, &tool_request(false), body).is_err());
}

#[tokio::test]
async fn http_errors_are_typed_and_redact_upstream_content() {
    for (status, kind) in [
        ("401 Unauthorized", ErrorKind::Authentication),
        ("403 Forbidden", ErrorKind::Permission),
        ("404 Not Found", ErrorKind::NotFound),
        ("429 Too Many Requests", ErrorKind::RateLimited),
        ("503 Unavailable", ErrorKind::Service),
    ] {
        let (url, captured) = mock_http(
            status,
            json!({"error":"echoed-secret"}),
            "Retry-After: 2\r\n",
        )
        .await;
        let config = CloudConfig {
            api_url: url,
            ..Default::default()
        };
        let error = generate(&config, &client(&config), ModelRequest::text("hello"))
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("echoed-secret"));
        let AleError::ModelCall(error) = error else {
            panic!("untyped error")
        };
        assert_eq!(error.kind, kind);
        assert_eq!(error.retry_after_ms, Some(2000));
        captured.await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn retries_share_deadline_and_never_start_expired_requests() {
    use std::cell::Cell;
    let count = Cell::new(0);
    let result: Result<()> = retry(tokio::time::Instant::now(), 1, || {
        count.set(count.get() + 1);
        async { Ok(()) }
    })
    .await;
    assert!(result.is_err());
    assert_eq!(count.get(), 0);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    let result: Result<()> = retry(deadline, 1, || {
        count.set(count.get() + 1);
        async {
            let mut error = ModelCallError::new(ErrorKind::RateLimited, "limited");
            error.retry_after_ms = Some(2000);
            Err(error.into())
        }
    })
    .await;
    assert!(result.is_err());
    assert_eq!(count.get(), 1);
    count.set(0);
    let result: Result<()> = retry(deadline, 99, || {
        count.set(count.get() + 1);
        async { Err(ModelCallError::new(ErrorKind::Service, "down").into()) }
    })
    .await;
    assert!(result.is_err());
    assert_eq!(count.get(), 2);
}

#[tokio::test(start_paused = true)]
async fn configured_cloud_budget_can_exceed_thirty_seconds() {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let result = retry(deadline, 0, || async {
        tokio::time::sleep(Duration::from_secs(45)).await;
        Ok(42)
    })
    .await
    .unwrap();
    assert_eq!(result, 42);
    let expired: Result<()> = retry(deadline, 0, || async {
        tokio::time::sleep(Duration::from_secs(20)).await;
        Ok(())
    })
    .await;
    assert!(matches!(
        expired,
        Err(AleError::ModelCall(ModelCallError {
            kind: ErrorKind::Timeout,
            ..
        }))
    ));
}
