#![cfg(unix)]

use ale_core::model_ipc::{
    read_message, write_message, IpcEnvelope, IpcReply, IpcReplyStatus, IpcRequestKind,
    MODEL_IPC_VERSION,
};
use ale_core::model_scheduler::SchedulerHealth;
use base64::Engine;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufWriter};
use tokio::process::Command;

#[tokio::test]
async fn spawned_modeld_authenticates_and_serves_health_over_unix_socket() {
    let endpoint = std::path::PathBuf::from("/tmp")
        .join(format!("ale-modeld-test-{}.sock", uuid::Uuid::new_v4()));
    let token = vec![0x5a_u8; 32];
    let mut child = Command::new(env!("CARGO_BIN_EXE_ale-modeld"))
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let bootstrap = serde_json::json!({
        "endpoint": endpoint,
        "token_base64": base64::engine::general_purpose::STANDARD.encode(&token),
    });
    let mut stdin = BufWriter::new(child.stdin.take().unwrap());
    stdin
        .write_all(format!("{bootstrap}\n").as_bytes())
        .await
        .unwrap();
    stdin.shutdown().await.unwrap();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        match tokio::net::UnixStream::connect(&endpoint).await {
            Ok(stream) => break stream,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Err(error) => panic!("modeld did not create its socket: {error}"),
        }
    };

    write_message(
        &mut stream,
        &IpcEnvelope {
            protocol_version: MODEL_IPC_VERSION,
            request_id: "auth".to_string(),
            kind: IpcRequestKind::Authenticate as i32,
            payload: token,
        },
    )
    .await
    .unwrap();
    let auth: IpcReply = read_message(&mut stream).await.unwrap();
    assert_eq!(auth.status, IpcReplyStatus::Ok as i32);

    write_message(
        &mut stream,
        &IpcEnvelope {
            protocol_version: MODEL_IPC_VERSION,
            request_id: "health".to_string(),
            kind: IpcRequestKind::Health as i32,
            payload: serde_json::to_vec(&serde_json::Value::Null).unwrap(),
        },
    )
    .await
    .unwrap();
    let health_reply: IpcReply = read_message(&mut stream).await.unwrap();
    let health: SchedulerHealth = serde_json::from_slice(&health_reply.payload).unwrap();
    assert_eq!(health.service, "ale-modeld");
    assert_eq!(health.protocol_version, MODEL_IPC_VERSION);

    write_message(
        &mut stream,
        &IpcEnvelope {
            protocol_version: MODEL_IPC_VERSION,
            request_id: "shutdown".to_string(),
            kind: IpcRequestKind::Shutdown as i32,
            payload: Vec::new(),
        },
    )
    .await
    .unwrap();
    let shutdown: IpcReply = read_message(&mut stream).await.unwrap();
    assert_eq!(shutdown.status, IpcReplyStatus::Ok as i32);
    assert!(child.wait().await.unwrap().success());
    assert!(!endpoint.exists());
}
