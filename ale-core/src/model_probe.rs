use crate::cloud::{CloudApiFactory, CloudConfig};
use crate::model_api::{self, ErrorKind, ModelCallError, ModelRequest};
use crate::Result;
use serde_json::json;
use std::io::Cursor;

#[derive(Clone, Copy, Debug)]
pub enum Probe {
    Text,
    Image,
    TextTool,
    ImageTool,
    Transcription,
}
impl Probe {
    pub const PLANNING: [Self; 4] = [Self::Text, Self::Image, Self::TextTool, Self::ImageTool];
    pub fn label(self, en: bool) -> &'static str {
        match (self, en) {
            (Self::Text, false) => "文本推理",
            (Self::Text, true) => "Text",
            (Self::Image, false) => "图片理解",
            (Self::Image, true) => "Image",
            (Self::TextTool, false) => "纯文本工具调用",
            (Self::TextTool, true) => "Text + tools",
            (Self::ImageTool, false) => "图片工具调用",
            (Self::ImageTool, true) => "Image + tools",
            (Self::Transcription, false) => "语音转写",
            (Self::Transcription, true) => "Transcription",
        }
    }
}
pub fn fixture_image() -> Vec<u8> {
    let image = image::RgbImage::from_pixel(64, 64, image::Rgb([255, 0, 0]));
    let mut bytes = Cursor::new(Vec::new());
    image
        .write_to(&mut bytes, image::ImageFormat::Png)
        .expect("valid fixture image");
    bytes.into_inner()
}
pub async fn run(config: &CloudConfig, probe: Probe) -> Result<()> {
    let api = CloudApiFactory::create(config.clone());
    let deadline =
        tokio::time::Instant::now() + config.timeout.min(std::time::Duration::from_secs(80));
    let expected = if matches!(probe, Probe::Image | Probe::ImageTool) {
        "red"
    } else {
        "ALE_OK"
    };
    let result = model_api::retry(deadline,0,||async {
        if matches!(probe,Probe::Transcription) {
            let response = api.transcribe(include_bytes!("../assets/model-probe.wav")).await?;
            let normalized = response.content.to_lowercase().replace("1","one").replace("2","two").replace("3","three");
            return Ok(normalized.contains("one") && normalized.contains("two") && normalized.contains("three"));
        }
        let mut request = ModelRequest::text(if matches!(probe,Probe::Image|Probe::ImageTool) {
            "Identify the solid image color. Use its lowercase English name. If a tool is available, call probe_echo with the color as value. Otherwise answer with only the color."
        } else { "Return exactly ALE_OK. If a tool is available, call probe_echo with value ALE_OK instead." });
        if matches!(probe,Probe::Image|Probe::ImageTool) { request.image = Some(fixture_image()); }
        if matches!(probe,Probe::TextTool|Probe::ImageTool) {
            request.require_tool = true;
            request.tools = vec![json!({"type":"function","function":{"name":"probe_echo","description":"Return a test value; no side effects.","parameters":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"],"additionalProperties":false}}})];
        }
        let response = api.generate(request).await?;
        Ok(if matches!(probe,Probe::TextTool|Probe::ImageTool) {
            response.tool_calls.len()==1 && response.tool_calls[0].function.name=="probe_echo" &&
            serde_json::from_str::<serde_json::Value>(&response.tool_calls[0].function.arguments).ok().is_some_and(|v|v["value"]==expected)
        } else { response.content.trim().trim_matches(|c:char|!c.is_alphanumeric()&&c!='_').eq_ignore_ascii_case(expected) })
    }).await?;
    if result {
        Ok(())
    } else {
        Err(ModelCallError::new(
            ErrorKind::InvalidOutput,
            "Capability sample returned an unexpected result",
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_api::tests::mock_http;
    use tokio::io::AsyncReadExt;

    #[tokio::test]
    async fn all_probes_validate_builtin_samples_and_transcription_model() {
        for probe in [
            Probe::Text,
            Probe::Image,
            Probe::TextTool,
            Probe::ImageTool,
            Probe::Transcription,
        ] {
            let expected = if matches!(probe, Probe::Image | Probe::ImageTool) {
                "red"
            } else {
                "ALE_OK"
            };
            let body = match probe {
                Probe::Transcription => json!({"text":"Testing one two three."}),
                Probe::TextTool | Probe::ImageTool => {
                    json!({"choices":[{"finish_reason":"tool_calls","message":{"tool_calls":[{"id":"sample","function":{"name":"probe_echo","arguments":json!({"value":expected}).to_string()}}]}}]})
                }
                _ => json!({"choices":[{"finish_reason":"stop","message":{"content":expected}}]}),
            };
            let (api_url, captured) = mock_http("200 OK", body, "").await;
            let config = CloudConfig {
                api_url,
                api_key: "sample-key".into(),
                model: "custom-asr-or-planner".into(),
                ..Default::default()
            };
            run(&config, probe).await.unwrap();
            let request = captured.await.unwrap();
            if matches!(probe, Probe::Transcription) {
                assert!(request.starts_with("POST /v1/audio/transcriptions "));
                assert!(request.contains("custom-asr-or-planner"));
                assert!(request.contains("RIFF"));
            } else {
                assert!(request.contains("ALE_OK") || request.contains("solid image color"));
            }
        }
    }

    #[tokio::test]
    async fn cancelling_a_probe_drops_its_http_connection() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let config = CloudConfig {
            api_url: format!("http://{}", listener.local_addr().unwrap()),
            ..Default::default()
        };
        let task = tokio::spawn(async move { run(&config, Probe::Text).await });
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0; 4096];
        assert!(socket.read(&mut buffer).await.unwrap() > 0);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        let count =
            tokio::time::timeout(std::time::Duration::from_secs(1), socket.read(&mut buffer))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn legacy_engine_transcription_uses_independent_credentials_without_primary() {
        let (api_url, captured) = mock_http("200 OK", json!({"text":"independent"}), "").await;
        let mut engine = crate::inference::AdaptiveInference::new(Default::default());
        engine.configure_transcription(&crate::config::TranscriptionConfig {
            enabled: true,
            endpoint: crate::config::CloudApiConfig {
                api_url,
                api_key: "asr-only-key".into(),
                model: "custom-transcriber".into(),
                ..Default::default()
            },
        });
        assert_eq!(
            engine
                .transcribe(include_bytes!("../assets/model-probe.wav"))
                .await
                .unwrap()
                .data,
            "independent"
        );
        let request = captured.await.unwrap();
        assert!(request.contains("Bearer asr-only-key"));
        assert!(request.contains("custom-transcriber"));
    }
}
