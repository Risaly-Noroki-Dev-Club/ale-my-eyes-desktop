use crate::actions::ActionPlan;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const REMOTE_PROTOCOL_VERSION: u32 = 1;
pub const DEFAULT_REMOTE_PORT: u16 = 37654;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteMessage {
    ClientHello(ClientHello),
    ServerHello(ServerHello),
    CommandRequest(CommandRequest),
    CommandPreview(CommandPreview),
    ConfirmExecution(ConfirmExecution),
    ExecutionStatus(ExecutionStatus),
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
    AudioWav { wav_base64: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommandPreview {
    pub request_id: String,
    pub response_text: String,
    pub action_steps: Vec<String>,
    pub confirmation_text: String,
    pub requires_confirmation: bool,
    pub has_plan: bool,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteError {
    pub request_id: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone)]
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
            "ale-my-eyes://pair?host={}&port={}&sid={}&code={}&name={}",
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
        if parsed.scheme() != "ale-my-eyes"
            || parsed.host_str() != Some("pair")
            || !parsed.path().is_empty()
            || parsed.fragment().is_some()
        {
            return Err("不是 Ale, My Eyes! 配对链接".to_string());
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
        if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("配对码必须是六位数字".to_string());
        }
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

#[derive(Debug, Clone)]
pub struct PendingRemotePlan {
    pub request_id: String,
    pub plan: ActionPlan,
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
            code: "123456".to_string(),
            name: "MacBook".to_string(),
        };

        let restored = PairingInfo::from_uri(&info.uri()).unwrap();
        assert_eq!(restored.host, info.host);
        assert_eq!(restored.port, info.port);
        assert_eq!(restored.session_id, info.session_id);
        assert_eq!(restored.code, info.code);
        assert_eq!(restored.name, info.name);
    }

    #[test]
    fn pairing_uri_rejects_missing_or_weak_credentials() {
        assert!(PairingInfo::from_uri(
            "ale-my-eyes://pair?host=192.168.1.2&port=37654&code=123456"
        )
        .is_err());
        assert!(PairingInfo::from_uri(
            "ale-my-eyes://pair?host=192.168.1.2&port=37654&sid=not-a-uuid&code=123456"
        )
        .is_err());
        assert!(PairingInfo::from_uri(&format!(
            "ale-my-eyes://pair?host=192.168.1.2&port=37654&sid={}&code=12345x",
            Uuid::new_v4()
        ))
        .is_err());
    }
}
