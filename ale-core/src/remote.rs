use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const REMOTE_PROTOCOL_VERSION: u32 = 4;
pub const DEFAULT_REMOTE_PORT: u16 = 37654;
pub const MAX_AUDIO_CHUNK_BYTES: usize = 24_576;
pub const MAX_RECORDING_SECONDS: u64 = 60;
pub const MIN_SAMPLE_RATE_HZ: u32 = 8_000;
pub const MAX_SAMPLE_RATE_HZ: u32 = 96_000;

/// Number of characters in a Misaka-Network v4 pairing code.
pub const PAIRING_CODE_CHARS: usize = 8;
/// Crockford Base32 alphabet; deliberately excludes `I`, `L`, `O` and `U`.
pub const PAIRING_CODE_ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Canonicalize a pairing code: exactly [`PAIRING_CODE_CHARS`] ASCII
/// Crockford Base32 characters, case-insensitive, returned upper-case.
/// Ambiguous characters (`I`, `L`, `O`, `U`) and every other byte are rejected.
pub fn normalize_pairing_code(code: &str) -> Option<String> {
    if code.len() != PAIRING_CODE_CHARS {
        return None;
    }
    let mut normalized = String::with_capacity(PAIRING_CODE_CHARS);
    for byte in code.bytes() {
        let upper = byte.to_ascii_uppercase();
        if !PAIRING_CODE_ALPHABET.contains(&upper) {
            return None;
        }
        normalized.push(char::from(upper));
    }
    Some(normalized)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteMessage {
    ClientHello(ClientHello),
    ServerHello(ServerHello),
    CommandRequest(CommandRequest),
    AudioStart(AudioStart),
    AudioChunk(AudioChunk),
    AudioEnd(AudioEnd),
    CancelRequest(CancelRequest),
    CommandPreview(CommandPreview),
    ProgressUpdate(ProgressUpdate),
    DecisionRequest(DecisionRequest),
    DecisionResponse(DecisionResponse),
    AssistantOutput(AssistantOutput),
    ConfirmExecution(ConfirmExecution),
    ExecutionStatus(ExecutionStatus),
    Ping(Ping),
    Pong(Pong),
    Error(RemoteError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientHello {
    pub protocol_version: u32,
    pub device_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerHello {
    pub protocol_version: u32,
    pub device_name: String,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandRequest {
    pub request_id: String,
    pub input: CommandInput,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "input", rename_all = "snake_case")]
pub enum CommandInput {
    Text { text: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioFormat {
    #[serde(rename = "pcm_s16le")]
    PcmS16Le,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioStart {
    pub request_id: String,
    pub format: AudioFormat,
    pub sample_rate_hz: u32,
    pub channels: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioChunk {
    pub request_id: String,
    pub sequence: u32,
    pub pcm_base64: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioEnd {
    pub request_id: String,
    pub chunk_count: u32,
    pub total_frames: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelRequest {
    pub request_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandPreview {
    pub request_id: String,
    pub response_text: String,
    pub action_steps: Vec<String>,
    pub confirmation_text: String,
    pub requires_confirmation: bool,
    pub has_plan: bool,
    /// `legacy` or `verified`; absent on older senders.
    #[serde(default)]
    pub task_mode: Option<String>,
    /// Human-readable postconditions for the mobile confirmation card.
    #[serde(default)]
    pub verification: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressUpdate {
    pub request_id: String,
    pub stage: ProgressStage,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProgressStage {
    Transcribing,
    CapturingState,
    Summarizing,
    Planning,
    Grounding,
    AwaitingDecision,
    Executing,
    Verifying,
    VerifiedStep,
    VerifiedVerify,
    VerifiedRecover,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionRequest {
    pub request_id: String,
    pub decision_id: String,
    pub kind: DecisionKind,
    pub prompt: String,
    pub expires_in_seconds: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionKind {
    UseRemoteModel,
    UploadFullScreenshot,
    RiskChanged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecisionResponse {
    pub request_id: String,
    pub decision_id: String,
    pub approved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantOutput {
    pub request_id: Option<String>,
    pub kind: AssistantOutputKind,
    pub display_text: String,
    pub speech_text: String,
    pub interrupt: bool,
    pub sensitive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssistantOutputKind {
    Information,
    Confirmation,
    Error,
    Result,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmExecution {
    pub request_id: String,
    pub approved: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionStatus {
    pub request_id: String,
    pub state: ExecutionState,
    pub message: String,
    pub actions_executed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    PreviewReady,
    Executing,
    Completed,
    Failed,
    Cancelled,
    OutcomeUnknown,
    NeedsUser,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ping {
    pub nonce: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pong {
    pub nonce: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteError {
    pub request_id: Option<String>,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingInfo {
    pub host: String,
    pub port: u16,
    pub session_id: String,
    pub code: String,
    pub name: String,
}

impl PairingInfo {
    pub fn uri(&self) -> String {
        format!(
            "misaka-network://pair?host={}&port={}&sid={}&code={}&name={}",
            urlencoding::encode(&self.host),
            self.port,
            urlencoding::encode(&self.session_id),
            urlencoding::encode(&self.code),
            urlencoding::encode(&self.name)
        )
    }

    pub fn websocket_url(&self) -> String {
        match self.host.parse::<std::net::IpAddr>() {
            Ok(std::net::IpAddr::V6(_)) => format!("ws://[{}]:{}", self.host, self.port),
            _ => format!("ws://{}:{}", self.host, self.port),
        }
    }

    pub fn from_uri(uri: &str) -> Result<Self, String> {
        let parsed = url::Url::parse(uri).map_err(|error| error.to_string())?;
        if parsed.scheme() != "misaka-network"
            || parsed.host_str() != Some("pair")
            || !parsed.path().is_empty()
            || parsed.fragment().is_some()
        {
            return Err("不是 Misaka-Network 配对链接".to_string());
        }

        let mut host = None;
        let mut port = None;
        let mut session_id = None;
        let mut code = None;
        let mut name = None;

        for (key, value) in parsed.query_pairs() {
            let target = match key.as_ref() {
                "host" => &mut host,
                "sid" => &mut session_id,
                "code" => &mut code,
                "name" => &mut name,
                "port" => {
                    if port.is_some() {
                        return Err("配对链接包含重复字段".to_string());
                    }
                    let parsed_port = value.parse::<u16>().map_err(|_| "端口无效".to_string())?;
                    if parsed_port == 0 {
                        return Err("端口无效".to_string());
                    }
                    port = Some(parsed_port);
                    continue;
                }
                _ => return Err(format!("未知配对字段: {key}")),
            };
            if target.is_some() {
                return Err("配对链接包含重复字段".to_string());
            }
            *target = Some(value.to_string());
        }

        let host = host.ok_or_else(|| "缺少 host".to_string())?;
        host.parse::<std::net::IpAddr>()
            .map_err(|_| "host 必须是有效 IP 地址".to_string())?;
        let session_id = session_id.ok_or_else(|| "缺少 sid".to_string())?;
        Uuid::parse_str(&session_id).map_err(|_| "sid 无效".to_string())?;
        let code = code.ok_or_else(|| "缺少配对码".to_string())?;
        let code = normalize_pairing_code(&code)
            .ok_or_else(|| "配对码必须是 8 位 Crockford Base32 字符".to_string())?;
        let name = name.unwrap_or_else(|| "Desktop".to_string());
        if name.trim().is_empty() || name.len() > 128 {
            return Err("设备名称无效".to_string());
        }

        Ok(Self {
            host,
            port: port.ok_or_else(|| "缺少 port".to_string())?,
            session_id,
            code,
            name,
        })
    }
}

pub fn new_request_id() -> String {
    Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pairing_uri_roundtrips() {
        let info = PairingInfo {
            host: "192.168.1.2".to_string(),
            port: 37654,
            session_id: Uuid::new_v4().to_string(),
            code: "7Q2M9X4B".to_string(),
            name: "MacBook".to_string(),
        };

        assert_eq!(PairingInfo::from_uri(&info.uri()).unwrap(), info);
    }

    #[test]
    fn pairing_uri_rejects_missing_or_weak_credentials() {
        assert!(PairingInfo::from_uri(
            "misaka-network://pair?host=192.168.1.2&port=37654&code=7Q2M9X4B"
        )
        .is_err());
        for code in [
            "7Q2M9X4",
            "7Q2M9X4BB",
            "7Q2M-9X4",
            "7Q2M9X4I",
            "7Q2M9X4L",
            "7Q2M9X4O",
            "7Q2M9X4U",
            "7q2m9x4!",
        ] {
            let uri = format!(
                "misaka-network://pair?host=192.168.1.2&port=37654&sid={}&code={code}",
                Uuid::new_v4()
            );
            assert!(
                PairingInfo::from_uri(&uri).is_err(),
                "pairing code {code:?} must be rejected"
            );
        }
    }

    #[test]
    fn pairing_uri_normalizes_lowercase_crockford_code() {
        let uri = format!(
            "misaka-network://pair?host=192.168.1.2&port=37654&sid={}&code=7q2m9x4b",
            Uuid::new_v4()
        );
        let info = PairingInfo::from_uri(&uri).unwrap();
        assert_eq!(info.code, "7Q2M9X4B");
        assert_eq!(PairingInfo::from_uri(&info.uri()).unwrap(), info);
    }

    #[test]
    fn pairing_code_normalizer_accepts_only_eight_crockford_characters() {
        assert_eq!(
            normalize_pairing_code("7q2m9x4b").as_deref(),
            Some("7Q2M9X4B")
        );
        assert_eq!(
            normalize_pairing_code("01234567").as_deref(),
            Some("01234567")
        );
        assert_eq!(
            normalize_pairing_code("ABCDEFGH").as_deref(),
            Some("ABCDEFGH")
        );
        assert!(normalize_pairing_code("0123456").is_none());
        assert!(normalize_pairing_code("012345678").is_none());
        for ambiguous in ["0123456I", "0123456L", "0123456O", "0123456U"] {
            assert!(
                normalize_pairing_code(ambiguous).is_none(),
                "{ambiguous} must be rejected"
            );
        }
        assert!(normalize_pairing_code("0123456-").is_none());
    }

    #[test]
    fn pairing_uri_rejects_legacy_scheme() {
        let legacy = format!(
            "ale-my-eyes://pair?host=192.168.1.2&port=37654&sid={}&code=7Q2M9X4B",
            Uuid::new_v4()
        );
        assert!(PairingInfo::from_uri(&legacy).is_err());
    }

    #[test]
    fn maximum_audio_chunk_serializes_below_noise_limit() {
        use base64::Engine;
        let message = RemoteMessage::AudioChunk(AudioChunk {
            request_id: new_request_id(),
            sequence: u32::MAX,
            pcm_base64: base64::engine::general_purpose::STANDARD
                .encode(vec![0; MAX_AUDIO_CHUNK_BYTES]),
        });
        assert!(serde_json::to_vec(&message).unwrap().len() < 48 * 1024);
    }

    #[test]
    fn execution_states_serialize_new_v4_values() {
        assert_eq!(
            serde_json::to_string(&ExecutionState::OutcomeUnknown).unwrap(),
            "\"outcome_unknown\""
        );
        assert_eq!(
            serde_json::to_string(&ExecutionState::NeedsUser).unwrap(),
            "\"needs_user\""
        );
    }

    #[test]
    fn command_preview_accepts_legacy_and_v4_payloads() {
        let legacy: CommandPreview = serde_json::from_str(
            r#"{"request_id":"r","response_text":"x","action_steps":[],"confirmation_text":"c","requires_confirmation":false,"has_plan":false}"#,
        )
        .unwrap();
        assert_eq!(legacy.task_mode, None);
        assert_eq!(legacy.verification, None);

        let verified: CommandPreview = serde_json::from_str(
            r#"{"request_id":"r","response_text":"x","action_steps":["a"],"confirmation_text":"c","requires_confirmation":true,"has_plan":true,"task_mode":"verified","verification":["button is enabled"]}"#,
        )
        .unwrap();
        assert_eq!(verified.task_mode.as_deref(), Some("verified"));
        assert_eq!(
            verified.verification.unwrap(),
            vec!["button is enabled".to_string()]
        );
    }

    #[test]
    fn misaka_v4_matches_golden_fixture() {
        let messages = vec![
            RemoteMessage::ClientHello(ClientHello {
                protocol_version: 4,
                device_name: "Android".into(),
            }),
            RemoteMessage::ServerHello(ServerHello {
                protocol_version: 4,
                device_name: "Desktop".into(),
                session_id: "session".into(),
            }),
            RemoteMessage::CommandRequest(CommandRequest {
                request_id: "text".into(),
                input: CommandInput::Text {
                    text: "hello".into(),
                },
            }),
            RemoteMessage::AudioStart(AudioStart {
                request_id: "audio".into(),
                format: AudioFormat::PcmS16Le,
                sample_rate_hz: 48_000,
                channels: 1,
            }),
            RemoteMessage::AudioChunk(AudioChunk {
                request_id: "audio".into(),
                sequence: 0,
                pcm_base64: "AAA=".into(),
            }),
            RemoteMessage::AudioEnd(AudioEnd {
                request_id: "audio".into(),
                chunk_count: 1,
                total_frames: 1,
                sha256: "digest".into(),
            }),
            RemoteMessage::CancelRequest(CancelRequest {
                request_id: "audio".into(),
            }),
            RemoteMessage::CommandPreview(CommandPreview {
                request_id: "audio".into(),
                response_text: "ready".into(),
                action_steps: vec![],
                confirmation_text: String::new(),
                requires_confirmation: false,
                has_plan: false,
                task_mode: Some("legacy".into()),
                verification: None,
            }),
            RemoteMessage::ProgressUpdate(ProgressUpdate {
                request_id: "audio".into(),
                stage: ProgressStage::Planning,
                message: "planning".into(),
            }),
            RemoteMessage::DecisionRequest(DecisionRequest {
                request_id: "audio".into(),
                decision_id: "decision".into(),
                kind: DecisionKind::UseRemoteModel,
                prompt: "use remote".into(),
                expires_in_seconds: 30,
            }),
            RemoteMessage::DecisionResponse(DecisionResponse {
                request_id: "audio".into(),
                decision_id: "decision".into(),
                approved: true,
            }),
            RemoteMessage::AssistantOutput(AssistantOutput {
                request_id: Some("audio".into()),
                kind: AssistantOutputKind::Information,
                display_text: "display".into(),
                speech_text: "speech".into(),
                interrupt: false,
                sensitive: false,
            }),
            RemoteMessage::ConfirmExecution(ConfirmExecution {
                request_id: "audio".into(),
                approved: true,
            }),
            RemoteMessage::ExecutionStatus(ExecutionStatus {
                request_id: "audio".into(),
                state: ExecutionState::Completed,
                message: "done".into(),
                actions_executed: 1,
            }),
            RemoteMessage::Ping(Ping { nonce: 1 }),
            RemoteMessage::Pong(Pong { nonce: 1 }),
            RemoteMessage::Error(RemoteError {
                request_id: Some("audio".into()),
                code: "TEST".into(),
                message: "failed".into(),
            }),
        ];
        let expected: serde_json::Value =
            serde_json::from_str(include_str!("../tests/fixtures/remote-v4.json")).unwrap();
        assert_eq!(serde_json::to_value(messages).unwrap(), expected);
    }
}
