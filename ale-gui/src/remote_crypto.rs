use ale_core::remote::RemoteMessage;
use snow::{Builder, TransportState};

const NOISE_PATTERN: &str = "Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s";

pub struct SecureChannel {
    transport: TransportState,
}

impl SecureChannel {
    pub fn encrypt_message(&mut self, message: &RemoteMessage) -> Result<Vec<u8>, String> {
        let payload = serde_json::to_vec(message).map_err(|error| error.to_string())?;
        let mut out = vec![0u8; payload.len() + 1024];
        let len = self
            .transport
            .write_message(&payload, &mut out)
            .map_err(|error| error.to_string())?;
        out.truncate(len);
        Ok(out)
    }

    pub fn decrypt_message(&mut self, frame: &[u8]) -> Result<RemoteMessage, String> {
        let mut out = vec![0u8; frame.len() + 1024];
        let len = self
            .transport
            .read_message(frame, &mut out)
            .map_err(|error| error.to_string())?;
        out.truncate(len);
        serde_json::from_slice(&out).map_err(|error| error.to_string())
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
    Ok((SecureChannel { transport }, reply))
}
