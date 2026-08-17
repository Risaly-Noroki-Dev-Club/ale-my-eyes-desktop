use ale_core::remote::RemoteMessage;
use snow::{Builder, TransportState};

const NOISE_PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";
const CHUNK_MAGIC: &[u8; 4] = b"AME2";
const CHUNK_HEADER_LEN: usize = 20;
const MAX_NOISE_PLAINTEXT: usize = 48 * 1024;
pub(crate) const MAX_ENCRYPTED_FRAME_BYTES: usize = MAX_NOISE_PLAINTEXT + 16;
pub(crate) const MAX_SECURE_MESSAGE_BYTES: usize = 1024 * 1024;

pub struct SecureChannel {
    transport: TransportState,
    incoming: Option<IncomingMessage>,
}

struct IncomingMessage {
    id: u64,
    total_chunks: u32,
    next_chunk: u32,
    payload: Vec<u8>,
}

impl SecureChannel {
    pub fn encrypt_message(&mut self, message: &RemoteMessage) -> Result<Vec<Vec<u8>>, String> {
        let payload = serde_json::to_vec(message).map_err(|error| error.to_string())?;
        if payload.len() > MAX_SECURE_MESSAGE_BYTES {
            return Err("MESSAGE_TOO_LARGE".to_string());
        }
        if payload.len() <= MAX_NOISE_PLAINTEXT {
            return Ok(vec![self.encrypt_payload(&payload)?]);
        }

        let chunk_payload_size = MAX_NOISE_PLAINTEXT - CHUNK_HEADER_LEN;
        let total_chunks = payload.len().div_ceil(chunk_payload_size);
        let total_chunks = u32::try_from(total_chunks)
            .map_err(|_| "secure message requires too many chunks".to_string())?;
        let message_id = rand::random::<u64>();
        let mut frames = Vec::with_capacity(total_chunks as usize);
        for (index, chunk) in payload.chunks(chunk_payload_size).enumerate() {
            let mut framed = Vec::with_capacity(CHUNK_HEADER_LEN + chunk.len());
            framed.extend_from_slice(CHUNK_MAGIC);
            framed.extend_from_slice(&message_id.to_be_bytes());
            framed.extend_from_slice(&(index as u32).to_be_bytes());
            framed.extend_from_slice(&total_chunks.to_be_bytes());
            framed.extend_from_slice(chunk);
            frames.push(self.encrypt_payload(&framed)?);
        }
        Ok(frames)
    }

    fn encrypt_payload(&mut self, payload: &[u8]) -> Result<Vec<u8>, String> {
        let mut out = vec![0u8; payload.len() + 1024];
        let len = self
            .transport
            .write_message(payload, &mut out)
            .map_err(|error| error.to_string())?;
        out.truncate(len);
        Ok(out)
    }

    pub fn decrypt_frame(
        &mut self,
        frame: &[u8],
        max_message_size: usize,
    ) -> Result<Option<RemoteMessage>, String> {
        if frame.len() > MAX_ENCRYPTED_FRAME_BYTES {
            return Err("MESSAGE_TOO_LARGE".to_string());
        }
        let mut out = vec![0u8; frame.len() + 1024];
        let len = self
            .transport
            .read_message(frame, &mut out)
            .map_err(|error| error.to_string())?;
        out.truncate(len);

        if !out.starts_with(CHUNK_MAGIC) {
            if self.incoming.take().is_some() {
                return Err("INVALID_CHUNK_SEQUENCE".to_string());
            }
            if out.len() > max_message_size {
                return Err("MESSAGE_TOO_LARGE".to_string());
            }
            return serde_json::from_slice(&out)
                .map(Some)
                .map_err(|error| error.to_string());
        }
        if out.len() < CHUNK_HEADER_LEN {
            self.incoming = None;
            return Err("INVALID_CHUNK_HEADER".to_string());
        }

        let id = u64::from_be_bytes(out[4..12].try_into().expect("fixed-size message id"));
        let index = u32::from_be_bytes(out[12..16].try_into().expect("fixed-size chunk index"));
        let total_chunks =
            u32::from_be_bytes(out[16..20].try_into().expect("fixed-size chunk count"));
        if total_chunks == 0 || index >= total_chunks {
            self.incoming = None;
            return Err("INVALID_CHUNK_SEQUENCE".to_string());
        }

        if index == 0 {
            if self.incoming.is_some() {
                self.incoming = None;
                return Err("INVALID_CHUNK_SEQUENCE".to_string());
            }
            self.incoming = Some(IncomingMessage {
                id,
                total_chunks,
                next_chunk: 0,
                payload: Vec::new(),
            });
        }
        let incoming = self
            .incoming
            .as_mut()
            .ok_or_else(|| "MISSING_CHUNK_START".to_string())?;
        if incoming.id != id
            || incoming.total_chunks != total_chunks
            || incoming.next_chunk != index
        {
            self.incoming = None;
            return Err("INVALID_CHUNK_SEQUENCE".to_string());
        }
        if incoming
            .payload
            .len()
            .saturating_add(out.len() - CHUNK_HEADER_LEN)
            > max_message_size
        {
            self.incoming = None;
            return Err("MESSAGE_TOO_LARGE".to_string());
        }
        incoming.payload.extend_from_slice(&out[CHUNK_HEADER_LEN..]);
        incoming.next_chunk += 1;

        if incoming.next_chunk != incoming.total_chunks {
            return Ok(None);
        }
        let payload = self
            .incoming
            .take()
            .ok_or_else(|| "MISSING_CHUNK_START".to_string())?
            .payload;
        serde_json::from_slice(&payload)
            .map(Some)
            .map_err(|error| error.to_string())
    }
}

pub fn pairing_code() -> String {
    format!("{:06}", rand::random::<u32>() % 1_000_000)
}

pub fn session_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn device_name() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "Ale Device".to_string())
}

fn psk_from_code(code: &str) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"ale-my-eyes-remote-v1");
    hasher.update(code.as_bytes());
    hasher.finalize().into()
}

fn noise_params() -> Result<snow::params::NoiseParams, String> {
    NOISE_PATTERN
        .parse()
        .map_err(|error: snow::Error| error.to_string())
}

pub fn server_handshake_reply(
    code: &str,
    client_message: &[u8],
) -> Result<(SecureChannel, Vec<u8>), String> {
    let psk = psk_from_code(code);
    let mut noise = Builder::new(noise_params()?)
        .psk(0, &psk)
        .build_responder()
        .map_err(|error| error.to_string())?;
    let mut scratch = vec![0u8; 1024];
    noise
        .read_message(client_message, &mut scratch)
        .map_err(|error| error.to_string())?;

    let mut reply = vec![0u8; 1024];
    let len = noise
        .write_message(&[], &mut reply)
        .map_err(|error| error.to_string())?;
    reply.truncate(len);
    let transport = noise
        .into_transport_mode()
        .map_err(|error| error.to_string())?;
    Ok((
        SecureChannel {
            transport,
            incoming: None,
        },
        reply,
    ))
}

#[cfg(test)]
pub(crate) struct TestClientHandshake {
    initiator: snow::HandshakeState,
}

#[cfg(test)]
impl TestClientHandshake {
    pub(crate) fn finish(mut self, server_reply: &[u8]) -> Result<SecureChannel, String> {
        let mut scratch = vec![0; 1024];
        self.initiator
            .read_message(server_reply, &mut scratch)
            .map_err(|error| error.to_string())?;
        Ok(SecureChannel {
            transport: self
                .initiator
                .into_transport_mode()
                .map_err(|error| error.to_string())?,
            incoming: None,
        })
    }
}

#[cfg(test)]
pub(crate) fn test_client_handshake_start(
    code: &str,
) -> Result<(TestClientHandshake, Vec<u8>), String> {
    let psk = psk_from_code(code);
    let mut initiator = Builder::new(noise_params()?)
        .psk(0, &psk)
        .build_initiator()
        .map_err(|error| error.to_string())?;
    let mut first = vec![0; 1024];
    let len = initiator
        .write_message(&[], &mut first)
        .map_err(|error| error.to_string())?;
    first.truncate(len);
    Ok((TestClientHandshake { initiator }, first))
}

#[cfg(test)]
fn secure_channel_pair(code: &str) -> (SecureChannel, SecureChannel) {
    let params = noise_params().unwrap();
    let psk = psk_from_code(code);
    let mut initiator = Builder::new(params.clone())
        .psk(0, &psk)
        .build_initiator()
        .unwrap();
    let mut responder = Builder::new(params).psk(0, &psk).build_responder().unwrap();
    let mut first = vec![0; 1024];
    let first_len = initiator.write_message(&[], &mut first).unwrap();
    let mut scratch = vec![0; 1024];
    responder
        .read_message(&first[..first_len], &mut scratch)
        .unwrap();
    let mut second = vec![0; 1024];
    let second_len = responder.write_message(&[], &mut second).unwrap();
    initiator
        .read_message(&second[..second_len], &mut scratch)
        .unwrap();
    (
        SecureChannel {
            transport: initiator.into_transport_mode().unwrap(),
            incoming: None,
        },
        SecureChannel {
            transport: responder.into_transport_mode().unwrap(),
            incoming: None,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ale_core::remote::{
        CommandInput, CommandPreview, CommandRequest, ConfirmExecution, ExecutionState,
        ExecutionStatus, RemoteError, ServerHello, REMOTE_PROTOCOL_VERSION,
    };
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    #[test]
    fn roundtrips_fragmented_encrypted_message() {
        let (mut sender, mut receiver) = secure_channel_pair("123456");
        let message = RemoteMessage::Error(RemoteError {
            request_id: Some("stress".to_string()),
            code: "STRESS".to_string(),
            message: "A".repeat(512 * 1024),
        });
        let frames = sender.encrypt_message(&message).unwrap();
        assert!(frames.len() > 8);

        let mut decoded = None;
        for frame in frames {
            let next = receiver
                .decrypt_frame(&frame, MAX_SECURE_MESSAGE_BYTES)
                .unwrap();
            assert!(decoded.is_none());
            if next.is_some() {
                decoded = next;
            }
        }
        let RemoteMessage::Error(error) = decoded.unwrap() else {
            panic!("expected remote error");
        };
        assert_eq!(error.message.len(), 512 * 1024);
    }

    #[test]
    fn rejects_reassembled_message_over_limit_before_json_parse() {
        let (mut sender, mut receiver) = secure_channel_pair("123456");
        let message = RemoteMessage::Error(ale_core::remote::RemoteError {
            request_id: None,
            code: "TEST".to_string(),
            message: "x".repeat(128 * 1024),
        });
        let frames = sender.encrypt_message(&message).unwrap();
        let mut error = None;
        for frame in frames {
            if let Err(next_error) = receiver.decrypt_frame(&frame, 64 * 1024) {
                error = Some(next_error);
                break;
            }
        }
        assert_eq!(error.as_deref(), Some("MESSAGE_TOO_LARGE"));
    }

    #[tokio::test]
    async fn loopback_noise_preview_confirm_status_integration() {
        let code = "654321";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut socket = tokio_tungstenite::accept_async(stream).await.unwrap();
            let handshake = socket.next().await.unwrap().unwrap().into_data();
            let (mut secure, reply) = server_handshake_reply(code, &handshake).unwrap();
            socket.send(Message::Binary(reply)).await.unwrap();
            for frame in secure
                .encrypt_message(&RemoteMessage::ServerHello(ServerHello {
                    protocol_version: REMOTE_PROTOCOL_VERSION,
                    device_name: "test-desktop".to_string(),
                    session_id: "session".to_string(),
                }))
                .unwrap()
            {
                socket.send(Message::Binary(frame)).await.unwrap();
            }

            let request = loop {
                let frame = socket.next().await.unwrap().unwrap().into_data();
                if let Some(message) = secure.decrypt_frame(&frame, 1024 * 1024).unwrap() {
                    break message;
                }
            };
            let RemoteMessage::CommandRequest(request) = request else {
                panic!("expected command request");
            };
            for frame in secure
                .encrypt_message(&RemoteMessage::CommandPreview(CommandPreview {
                    request_id: request.request_id,
                    response_text: "preview".to_string(),
                    action_steps: vec!["wait".to_string()],
                    confirmation_text: "confirm".to_string(),
                    requires_confirmation: true,
                    has_plan: true,
                }))
                .unwrap()
            {
                socket.send(Message::Binary(frame)).await.unwrap();
            }

            let confirm = loop {
                let frame = socket.next().await.unwrap().unwrap().into_data();
                if let Some(message) = secure.decrypt_frame(&frame, 1024 * 1024).unwrap() {
                    break message;
                }
            };
            let RemoteMessage::ConfirmExecution(confirm) = confirm else {
                panic!("expected confirmation");
            };
            for frame in secure
                .encrypt_message(&RemoteMessage::ExecutionStatus(ExecutionStatus {
                    request_id: confirm.request_id,
                    state: ExecutionState::Completed,
                    message: "done".to_string(),
                    actions_executed: 1,
                }))
                .unwrap()
            {
                socket.send(Message::Binary(frame)).await.unwrap();
            }
        });

        let params = noise_params().unwrap();
        let psk = psk_from_code(code);
        let mut initiator = Builder::new(params).psk(0, &psk).build_initiator().unwrap();
        let mut first = vec![0; 1024];
        let first_len = initiator.write_message(&[], &mut first).unwrap();
        let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}"))
            .await
            .unwrap();
        socket
            .send(Message::Binary(first[..first_len].to_vec()))
            .await
            .unwrap();
        let reply = socket.next().await.unwrap().unwrap().into_data();
        let mut scratch = vec![0; 1024];
        initiator.read_message(&reply, &mut scratch).unwrap();
        let mut secure = SecureChannel {
            transport: initiator.into_transport_mode().unwrap(),
            incoming: None,
        };

        let hello = loop {
            let frame = socket.next().await.unwrap().unwrap().into_data();
            if let Some(message) = secure.decrypt_frame(&frame, 1024 * 1024).unwrap() {
                break message;
            }
        };
        assert!(matches!(hello, RemoteMessage::ServerHello(_)));
        for frame in secure
            .encrypt_message(&RemoteMessage::CommandRequest(CommandRequest {
                request_id: "request".to_string(),
                input: CommandInput::Text {
                    text: "test".to_string(),
                },
            }))
            .unwrap()
        {
            socket.send(Message::Binary(frame)).await.unwrap();
        }
        let preview = loop {
            let frame = socket.next().await.unwrap().unwrap().into_data();
            if let Some(message) = secure.decrypt_frame(&frame, 1024 * 1024).unwrap() {
                break message;
            }
        };
        let RemoteMessage::CommandPreview(preview) = preview else {
            panic!("expected preview");
        };
        assert!(preview.has_plan);
        for frame in secure
            .encrypt_message(&RemoteMessage::ConfirmExecution(ConfirmExecution {
                request_id: preview.request_id,
                approved: true,
            }))
            .unwrap()
        {
            socket.send(Message::Binary(frame)).await.unwrap();
        }
        let status = loop {
            let frame = socket.next().await.unwrap().unwrap().into_data();
            if let Some(message) = secure.decrypt_frame(&frame, 1024 * 1024).unwrap() {
                break message;
            }
        };
        let RemoteMessage::ExecutionStatus(status) = status else {
            panic!("expected execution status");
        };
        assert_eq!(status.state, ExecutionState::Completed);
        server.await.unwrap();
    }
}
