mod gpu;
mod llama;
mod scheduler;
mod sensevoice;

use ale_core::model_ipc::{read_message, write_message, IpcEnvelope, IpcReply, MODEL_IPC_VERSION};
use anyhow::{Context, Result};
use base64::Engine;
use futures::{stream::FuturesUnordered, StreamExt};
use serde::Deserialize;
use std::collections::HashMap;
use std::future::Future;
#[cfg(unix)]
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::oneshot;

#[derive(Deserialize)]
struct Bootstrap {
    endpoint: String,
    token_base64: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let mut line = String::new();
    BufReader::new(tokio::io::stdin())
        .read_line(&mut line)
        .await
        .context("read bootstrap from inherited stdin")?;
    let bootstrap: Bootstrap = serde_json::from_str(line.trim()).context("invalid bootstrap")?;
    let token = base64::engine::general_purpose::STANDARD
        .decode(bootstrap.token_base64)
        .context("invalid bootstrap token")?;
    if token.len() < 32 {
        anyhow::bail!("bootstrap token is too short");
    }

    run_endpoint(bootstrap.endpoint, token).await
}

#[cfg(unix)]
async fn run_endpoint(endpoint: String, token: Vec<u8>) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    let path = PathBuf::from(endpoint);
    if path.exists() {
        std::fs::remove_file(&path).context("remove stale modeld socket")?;
    }
    let listener = UnixListener::bind(&path).context("bind modeld unix socket")?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    let result = async {
        let (stream, _) = listener.accept().await?;
        serve_connection(stream, token).await
    }
    .await;
    let _ = std::fs::remove_file(path);
    result
}

#[cfg(windows)]
async fn run_endpoint(endpoint: String, token: Vec<u8>) -> Result<()> {
    use tokio::net::windows::named_pipe::ServerOptions;

    let server = ServerOptions::new()
        .first_pipe_instance(true)
        .create(&endpoint)
        .context("create modeld named pipe")?;
    server
        .connect()
        .await
        .context("connect modeld named pipe")?;
    serve_connection(server, token).await
}

type PendingJob = Pin<Box<dyn Future<Output = (String, IpcReply)> + Send>>;

async fn serve_connection<S>(mut stream: S, mut token: Vec<u8>) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send,
{
    let auth: IpcEnvelope = read_message(&mut stream).await?;
    let authenticated = auth.protocol_version == MODEL_IPC_VERSION
        && auth.kind == ale_core::model_ipc::IpcRequestKind::Authenticate as i32
        && constant_time_eq(&auth.payload, &token);
    token.fill(0);
    if !authenticated {
        anyhow::bail!("modeld authentication failed");
    }
    write_message(
        &mut stream,
        &IpcReply {
            protocol_version: MODEL_IPC_VERSION,
            request_id: auth.request_id,
            status: ale_core::model_ipc::IpcReplyStatus::Ok as i32,
            payload: Vec::new(),
            error_code: String::new(),
            error_message: String::new(),
        },
    )
    .await?;

    let scheduler = Arc::new(scheduler::ModelScheduler::default());
    let mut jobs = FuturesUnordered::<PendingJob>::new();
    let mut cancellations = HashMap::<String, oneshot::Sender<()>>::new();
    let mut maintenance = tokio::time::interval(std::time::Duration::from_secs(5));
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = maintenance.tick() => scheduler.maintenance(),
            request = read_message::<_, IpcEnvelope>(&mut stream) => {
                let request = request?;
                let kind = ale_core::model_ipc::IpcRequestKind::try_from(request.kind).ok();
                match kind {
                    Some(ale_core::model_ipc::IpcRequestKind::Schedule) => {
                        if cancellations.contains_key(&request.request_id) {
                            write_message(
                                &mut stream,
                                &scheduler::error_reply(
                                    request.request_id,
                                    "DUPLICATE_REQUEST_ID",
                                    "model job request ID is already running",
                                ),
                            )
                            .await?;
                            continue;
                        }
                        let job_id = request.request_id.clone();
                        let (cancel, cancelled) = oneshot::channel();
                        cancellations.insert(job_id.clone(), cancel);
                        let scheduler = scheduler.clone();
                        jobs.push(Box::pin(async move {
                            let reply = tokio::select! {
                                reply = scheduler.handle(request) => reply,
                                _ = cancelled => scheduler::error_reply(
                                    job_id.clone(),
                                    "CANCELLED",
                                    "model job was cancelled",
                                ),
                            };
                            (job_id, reply)
                        }));
                    }
                    Some(ale_core::model_ipc::IpcRequestKind::Cancel) => {
                        let target = serde_json::from_slice::<ale_core::model_scheduler::CancelModelJob>(
                            &request.payload,
                        );
                        let reply = match target {
                            Ok(target) if !target.target_request_id.trim().is_empty() => {
                                let accepted = cancellations
                                    .remove(&target.target_request_id)
                                    .is_some_and(|cancel| cancel.send(()).is_ok());
                                scheduler::ok_json(
                                    request.request_id,
                                    &serde_json::json!({"accepted": accepted}),
                                )
                            }
                            Ok(_) => scheduler::error_reply(
                                request.request_id,
                                "INVALID_CANCEL",
                                "cancel target request ID is empty",
                            ),
                            Err(error) => scheduler::error_reply(
                                request.request_id,
                                "INVALID_CANCEL",
                                &error.to_string(),
                            ),
                        };
                        write_message(&mut stream, &reply).await?;
                    }
                    Some(ale_core::model_ipc::IpcRequestKind::Shutdown) => {
                        for (_, cancel) in cancellations.drain() {
                            let _ = cancel.send(());
                        }
                        let reply = scheduler::ok_json(
                            request.request_id,
                            &serde_json::json!({"accepted": true}),
                        );
                        write_message(&mut stream, &reply).await?;
                        return Ok(());
                    }
                    _ => {
                        let reply = scheduler.handle(request).await;
                        write_message(&mut stream, &reply).await?;
                    }
                }
            }
            Some((job_id, reply)) = jobs.next(), if !jobs.is_empty() => {
                cancellations.remove(&job_id);
                write_message(&mut stream, &reply).await?;
            }
        }
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use ale_core::actions::RiskLevel;
    use ale_core::model_ipc::{IpcReplyStatus, IpcRequestKind};
    use ale_core::model_scheduler::{
        CancelModelJob, JobPrivacy, ModelCapability, ModelJob, RemoteEndpointConfig,
        RemotePlanningJob, RemoteProviderSet, SchedulerPriority,
    };
    use tokio::io::AsyncReadExt;

    #[test]
    fn token_comparison_checks_all_bytes() {
        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"sale"));
        assert!(!constant_time_eq(b"same", b"short"));
    }

    #[tokio::test]
    async fn cancellation_is_processed_while_a_model_job_is_running() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request).await;
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        });

        let token = vec![7_u8; 32];
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(serve_connection(server, token.clone()));
        write_message(
            &mut client,
            &IpcEnvelope {
                protocol_version: MODEL_IPC_VERSION,
                request_id: "auth".to_string(),
                kind: IpcRequestKind::Authenticate as i32,
                payload: token,
            },
        )
        .await
        .unwrap();
        let _: IpcReply = read_message(&mut client).await.unwrap();

        let providers = RemoteProviderSet {
            primary: RemoteEndpointConfig {
                provider: "openai".to_string(),
                api_key: "test".to_string(),
                api_url: format!("http://{address}"),
                model: "test".to_string(),
                max_tokens: 16,
                timeout_seconds: 20,
            },
            backup: None,
            backup_enabled: false,
            backup_pre_authorized: false,
            circuit_failure_threshold: 3,
            circuit_open_seconds: 60,
        };
        write_message(
            &mut client,
            &IpcEnvelope {
                protocol_version: MODEL_IPC_VERSION,
                request_id: "configure".to_string(),
                kind: IpcRequestKind::ConfigureRemote as i32,
                payload: serde_json::to_vec(&providers).unwrap(),
            },
        )
        .await
        .unwrap();
        let configured: IpcReply = read_message(&mut client).await.unwrap();
        assert_eq!(configured.status, IpcReplyStatus::Ok as i32);

        let job = ModelJob {
            request_id: "slow-job".to_string(),
            capability: ModelCapability::RemotePlanning,
            priority: SchedulerPriority::InteractiveRequest,
            deadline_unix_ms: chrono::Utc::now().timestamp_millis() + 10_000,
            risk_ceiling: RiskLevel::High,
            snapshot_id: Some("snapshot-1".to_string()),
            privacy: JobPrivacy {
                allow_remote: true,
                allow_full_screenshot: false,
                allow_sensitive_content: false,
            },
            payload: serde_json::to_value(RemotePlanningJob {
                question: "wait".to_string(),
                image_base64: None,
                tools: None,
            })
            .unwrap(),
        };
        write_message(
            &mut client,
            &IpcEnvelope {
                protocol_version: MODEL_IPC_VERSION,
                request_id: "slow-job".to_string(),
                kind: IpcRequestKind::Schedule as i32,
                payload: serde_json::to_vec(&job).unwrap(),
            },
        )
        .await
        .unwrap();
        write_message(
            &mut client,
            &IpcEnvelope {
                protocol_version: MODEL_IPC_VERSION,
                request_id: "cancel-command".to_string(),
                kind: IpcRequestKind::Cancel as i32,
                payload: serde_json::to_vec(&CancelModelJob {
                    target_request_id: "slow-job".to_string(),
                })
                .unwrap(),
            },
        )
        .await
        .unwrap();

        let first: IpcReply = read_message(&mut client).await.unwrap();
        let second: IpcReply = read_message(&mut client).await.unwrap();
        let replies = [first, second];
        assert!(replies
            .iter()
            .any(|reply| { reply.request_id == "slow-job" && reply.error_code == "CANCELLED" }));
        assert!(replies.iter().any(|reply| {
            reply.request_id == "cancel-command" && reply.status == IpcReplyStatus::Ok as i32
        }));

        write_message(
            &mut client,
            &IpcEnvelope {
                protocol_version: MODEL_IPC_VERSION,
                request_id: "shutdown".to_string(),
                kind: IpcRequestKind::Shutdown as i32,
                payload: Vec::new(),
            },
        )
        .await
        .unwrap();
        let _: IpcReply = read_message(&mut client).await.unwrap();
        server_task.await.unwrap().unwrap();
    }
}
