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

async fn isolated_modeld(
    path: Option<&std::path::Path>,
) -> (
    tempfile::TempDir,
    ale_core::child_process::ManagedAsyncChild,
    tokio::net::UnixStream,
) {
    let root = tempfile::Builder::new()
        .prefix("ame-ipc-")
        .tempdir_in("/tmp")
        .unwrap();
    let endpoint = root.path().join("ipc.sock");
    let mut command = Command::new(env!("CARGO_BIN_EXE_ale-modeld"));
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(path) = path {
        command.env("PATH", path);
    }
    let mut child = ale_core::child_process::ManagedAsyncChild::spawn(&mut command).unwrap();
    let token = vec![0x5a_u8; 32];
    let bootstrap = serde_json::json!({"endpoint":endpoint,"token_base64":base64::engine::general_purpose::STANDARD.encode(&token)});
    let mut input = child.child.stdin.take().unwrap();
    input
        .write_all(format!("{bootstrap}\n").as_bytes())
        .await
        .unwrap();
    input.shutdown().await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        match tokio::net::UnixStream::connect(&endpoint).await {
            Ok(stream) => break stream,
            Err(_) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(20)).await
            }
            Err(error) => panic!("{error}"),
        }
    };
    write_message(
        &mut stream,
        &IpcEnvelope {
            protocol_version: MODEL_IPC_VERSION,
            request_id: "auth".into(),
            kind: IpcRequestKind::Authenticate as i32,
            payload: token,
        },
    )
    .await
    .unwrap();
    let _: IpcReply = read_message(&mut stream).await.unwrap();
    (root, child, stream)
}

#[tokio::test]
async fn partial_frame_survives_maintenance_tick() {
    use prost::Message;
    let (_root, mut child, mut stream) = isolated_modeld(None).await;
    let envelope = IpcEnvelope {
        protocol_version: MODEL_IPC_VERSION,
        request_id: "fragmented".into(),
        kind: IpcRequestKind::Health as i32,
        payload: b"null".to_vec(),
    };
    let bytes = envelope.encode_to_vec();
    stream.write_u32(bytes.len() as u32).await.unwrap();
    tokio::time::sleep(Duration::from_millis(5300)).await;
    stream.write_all(&bytes).await.unwrap();
    let reply: IpcReply = tokio::time::timeout(Duration::from_secs(1), read_message(&mut stream))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply.request_id, "fragmented");
    assert_eq!(reply.status, IpcReplyStatus::Ok as i32);
    child
        .kill_tree_and_wait(Duration::from_secs(2))
        .await
        .unwrap();
}

#[tokio::test]
async fn hung_gpu_probe_does_not_block_health_or_cancel() {
    use std::os::unix::fs::PermissionsExt;
    let tools = tempfile::tempdir().unwrap();
    let stub = tools.path().join("nvidia-smi");
    std::fs::write(&stub, "#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o700)).unwrap();
    let (_root, mut child, mut stream) = isolated_modeld(Some(tools.path())).await;
    for (id, kind, payload) in [
        ("health", IpcRequestKind::Health, b"null".to_vec()),
        (
            "cancel",
            IpcRequestKind::Cancel,
            br#"{"target_request_id":"missing"}"#.to_vec(),
        ),
    ] {
        write_message(
            &mut stream,
            &IpcEnvelope {
                protocol_version: MODEL_IPC_VERSION,
                request_id: id.into(),
                kind: kind as i32,
                payload,
            },
        )
        .await
        .unwrap();
        let reply: IpcReply =
            tokio::time::timeout(Duration::from_secs(1), read_message(&mut stream))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(reply.request_id, id);
        assert_eq!(reply.status, IpcReplyStatus::Ok as i32);
    }
    tokio::time::sleep(Duration::from_secs(3)).await;
    write_message(
        &mut stream,
        &IpcEnvelope {
            protocol_version: MODEL_IPC_VERSION,
            request_id: "after-timeout".into(),
            kind: IpcRequestKind::Health as i32,
            payload: b"null".to_vec(),
        },
    )
    .await
    .unwrap();
    let reply: IpcReply = tokio::time::timeout(Duration::from_secs(1), read_message(&mut stream))
        .await
        .unwrap()
        .unwrap();
    let health: SchedulerHealth = serde_json::from_slice(&reply.payload).unwrap();
    assert_eq!(
        health.diagnostics.unwrap().gpu_error_code.as_deref(),
        Some("GPU_PROBE_TIMEOUT")
    );
    child
        .kill_tree_and_wait(Duration::from_secs(2))
        .await
        .unwrap();
}
