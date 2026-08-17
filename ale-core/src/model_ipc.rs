use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MODEL_IPC_VERSION: u32 = 1;
pub const MAX_MODEL_IPC_MESSAGE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, PartialEq, Message)]
pub struct IpcEnvelope {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(string, tag = "2")]
    pub request_id: String,
    #[prost(enumeration = "IpcRequestKind", tag = "3")]
    pub kind: i32,
    #[prost(bytes = "vec", tag = "4")]
    pub payload: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
pub struct IpcReply {
    #[prost(uint32, tag = "1")]
    pub protocol_version: u32,
    #[prost(string, tag = "2")]
    pub request_id: String,
    #[prost(enumeration = "IpcReplyStatus", tag = "3")]
    pub status: i32,
    #[prost(bytes = "vec", tag = "4")]
    pub payload: Vec<u8>,
    #[prost(string, tag = "5")]
    pub error_code: String,
    #[prost(string, tag = "6")]
    pub error_message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum IpcRequestKind {
    Authenticate = 0,
    Health = 1,
    Schedule = 2,
    Cancel = 3,
    ConfigureRemote = 4,
    Shutdown = 5,
    ConfigureModels = 6,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
pub enum IpcReplyStatus {
    Ok = 0,
    Error = 1,
    DecisionRequired = 2,
}

pub async fn write_message<W, M>(writer: &mut W, message: &M) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
    M: Message,
{
    let encoded = message.encode_to_vec();
    if encoded.len() > MAX_MODEL_IPC_MESSAGE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "model IPC message exceeds size limit",
        ));
    }
    writer.write_u32(encoded.len() as u32).await?;
    writer.write_all(&encoded).await?;
    writer.flush().await
}

pub async fn read_message<R, M>(reader: &mut R) -> std::io::Result<M>
where
    R: AsyncRead + Unpin,
    M: Message + Default,
{
    let length = reader.read_u32().await? as usize;
    if length == 0 || length > MAX_MODEL_IPC_MESSAGE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid model IPC frame length",
        ));
    }
    let mut encoded = vec![0; length];
    reader.read_exact(&mut encoded).await?;
    M::decode(encoded.as_slice()).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid model IPC protobuf: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn framed_protobuf_roundtrips() {
        let (mut client, mut server) = tokio::io::duplex(1024);
        let expected = IpcEnvelope {
            protocol_version: MODEL_IPC_VERSION,
            request_id: "request-1".to_string(),
            kind: IpcRequestKind::Health as i32,
            payload: b"payload".to_vec(),
        };
        write_message(&mut client, &expected).await.unwrap();
        let actual: IpcEnvelope = read_message(&mut server).await.unwrap();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected_before_allocation() {
        let frame_length = (MAX_MODEL_IPC_MESSAGE_BYTES as u32 + 1).to_be_bytes();
        let mut bytes = frame_length.as_slice();
        let result: std::io::Result<IpcEnvelope> = read_message(&mut bytes).await;
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidData);
    }
}
