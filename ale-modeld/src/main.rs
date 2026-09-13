mod gpu;
mod llama;
mod scheduler;
mod sensevoice;

use ale_core::model_ipc::{read_message, write_message, IpcEnvelope, IpcReply, MODEL_IPC_VERSION};
use anyhow::{Context, Result};
use base64::Engine;
use futures::{stream::FuturesUnordered, StreamExt};
use prost::Message;
use serde::Deserialize;
use std::collections::HashMap;
use std::future::Future;
#[cfg(unix)]
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::oneshot;

#[derive(Deserialize)]
struct Bootstrap {
    endpoint: String,
    token_base64: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = ale_core::diagnostics::install_from_env("modeld");
    ale_core::diagnostics::record("modeld_start", &[]);
    if std::env::args().any(|arg| arg == "--sensevoice-worker") {
        return sensevoice::run_worker().await;
    }
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .try_init()
        .ok();

    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(45),
        BufReader::new(tokio::io::stdin().take(4097)).read_line(&mut line),
    )
    .await
    .context("bootstrap deadline")??;
    if line.len() > 4096 || !line.ends_with('\n') {
        anyhow::bail!("invalid bootstrap length");
    }
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
        let (stream, _) =
            tokio::time::timeout(std::time::Duration::from_secs(45), listener.accept()).await??;
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
    tokio::time::timeout(std::time::Duration::from_secs(45), server.connect())
        .await
        .context("pipe accept deadline")??;
    serve_connection(server, token).await
}

type PendingJob = Pin<Box<dyn Future<Output = (String, IpcReply)> + Send>>;

struct ReplyQueue {
    sender: tokio::sync::mpsc::Sender<(IpcReply, tokio::sync::OwnedSemaphorePermit)>,
    budget: Arc<tokio::sync::Semaphore>,
}
impl ReplyQueue {
    fn try_send(&self, reply: IpcReply) -> Result<(), ()> {
        let length = reply.encoded_len();
        if length > ale_core::model_ipc::MAX_MODEL_IPC_MESSAGE_BYTES {
            return Err(());
        }
        let permit = self
            .budget
            .clone()
            .try_acquire_many_owned(length as u32)
            .map_err(|_| ())?;
        self.sender.try_send((reply, permit)).map_err(|_| ())
    }
}

async fn serve_connection<S>(mut stream: S, mut token: Vec<u8>) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let auth: IpcEnvelope = tokio::time::timeout(
        std::time::Duration::from_secs(45),
        read_message(&mut stream),
    )
    .await
    .context("authentication deadline")??;
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
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (incoming_tx, mut incoming) = tokio::sync::mpsc::channel(2);
    let budget = Arc::new(tokio::sync::Semaphore::new(128 * 1024 * 1024));
    let (sender, mut outgoing) =
        tokio::sync::mpsc::channel::<(IpcReply, tokio::sync::OwnedSemaphorePermit)>(32);
    let replies = ReplyQueue {
        sender,
        budget: budget.clone(),
    };
    let mut reader_task = tokio::spawn(async move {
        loop {
            let request = ale_core::model_ipc::read_message_with_budget::<_, IpcEnvelope>(
                &mut reader,
                budget.clone(),
            )
            .await?;
            if incoming_tx.send(request).await.is_err() {
                return Ok::<_, std::io::Error>(());
            }
        }
    });
    let mut writer_task = tokio::spawn(async move {
        while let Some((reply, _reservation)) = outgoing.recv().await {
            tokio::time::timeout(
                std::time::Duration::from_secs(3),
                write_message(&mut writer, &reply),
            )
            .await
            .map_err(|_| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "IPC reply deadline")
            })??;
        }
        Ok::<_, std::io::Error>(())
    });
    let mut jobs = FuturesUnordered::<PendingJob>::new();
    let mut cancellations = HashMap::<String, (oneshot::Sender<()>, usize)>::new();
    let mut admitted_bytes = 0usize;
    let mut maintenance = tokio::time::interval(std::time::Duration::from_secs(5));
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result: Result<()> = async {
        loop {
            tokio::select! {
                result = &mut reader_task => { if !matches!(&result,Ok(Ok(_))) {ale_core::diagnostics::record("ipc_reader_failed",&[]);} result??; break; },
                result = &mut writer_task => { result??; break; },
                _ = maintenance.tick() => scheduler.maintenance(),
                Some((request,reservation)) = incoming.recv() => {
                    let kind = ale_core::model_ipc::IpcRequestKind::try_from(request.kind).ok();
                    match kind {
                        Some(ale_core::model_ipc::IpcRequestKind::Schedule) => {
                            let bytes = request.payload.len();
                            let code = if cancellations.contains_key(&request.request_id) { Some("DUPLICATE_REQUEST_ID") }
                                else if cancellations.len() >= 32 || admitted_bytes.saturating_add(bytes) > 128 * 1024 * 1024 { Some("SCHEDULER_BUSY") }
                                else { None };
                            if let Some(code) = code {
                                replies.try_send(scheduler::error_reply(request.request_id, code, code)).map_err(|_| anyhow::anyhow!("IPC reply queue full"))?;
                                continue;
                            }
                            let job_id = request.request_id.clone();
                            let (cancel, cancelled) = oneshot::channel();
                            cancellations.insert(job_id.clone(), (cancel, bytes));
                            admitted_bytes += bytes;
                            let scheduler = scheduler.clone();
                            ale_core::diagnostics::record("model_job_admitted",&[("operation_id", ale_core::diagnostics::correlation_id(&job_id)), ("active_jobs",cancellations.len() as u64),("bytes",admitted_bytes as u64)]);
                            jobs.push(Box::pin(async move {
                                let _reservation=reservation;
                                let reply = tokio::select! {
                                    reply = scheduler.handle(request) => reply,
                                    _ = cancelled => scheduler::error_reply(job_id.clone(), "CANCELLED", "model job was cancelled"),
                                };
                                (job_id, reply)
                            }));
                        }
                        Some(ale_core::model_ipc::IpcRequestKind::Cancel) => {
                            let target = serde_json::from_slice::<ale_core::model_scheduler::CancelModelJob>(&request.payload);
                            let reply = match target {
                                Ok(target) if !target.target_request_id.trim().is_empty() => {
                                    let accepted = if let Some((sender, bytes)) = cancellations.remove(&target.target_request_id) {
                                        // Keep the admission reservation until the cancelled job has actually dropped.
                                        cancellations.insert(target.target_request_id, (oneshot::channel().0, bytes));
                                        sender.send(()).is_ok()
                                    } else { false };
                                    scheduler::ok_json(request.request_id, &serde_json::json!({"accepted":accepted}))
                                }
                                _ => scheduler::error_reply(request.request_id, "INVALID_CANCEL", "invalid cancellation target"),
                            };
                            replies.try_send(reply).map_err(|_| anyhow::anyhow!("IPC reply queue full"))?;
                        }
                        Some(ale_core::model_ipc::IpcRequestKind::Shutdown) => {
                            for (_, (cancel, _)) in cancellations.drain() { let _ = cancel.send(()); }
                            jobs.clear();
                            replies.try_send(scheduler::ok_json(request.request_id, &serde_json::json!({"accepted":true}))).map_err(|_| anyhow::anyhow!("IPC reply queue full"))?;
                            break;
                        }
                        _ => {
                            scheduler.set_admission(cancellations.len(), admitted_bytes);
                            let reply = scheduler.handle(request).await;
                            replies.try_send(reply).map_err(|_| anyhow::anyhow!("IPC reply queue full"))?;
                        }
                    }
                }
                Some((id, reply)) = jobs.next(), if !jobs.is_empty() => {
                    ale_core::diagnostics::record("model_job_finished", &[("operation_id", ale_core::diagnostics::correlation_id(&id)), ("status", reply.status as u64)]);
                    if let Some((_, bytes)) = cancellations.remove(&id) { admitted_bytes = admitted_bytes.saturating_sub(bytes); }
                    replies.try_send(reply).map_err(|_| anyhow::anyhow!("IPC reply queue full"))?;
                }
            }
        }
        Ok(())
    }.await;
    jobs.clear();
    cancellations.clear();
    scheduler.shutdown().await;
    reader_task.abort();
    drop(replies);
    if !writer_task.is_finished()
        && tokio::time::timeout(std::time::Duration::from_secs(3), &mut writer_task)
            .await
            .is_err()
    {
        writer_task.abort();
    }
    tracing::info!(
        event = "modeld_connection_stopped",
        failed = result.is_err()
    );
    result
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
            transcription: None,
            revision: 0,
            primary: RemoteEndpointConfig {
                wire_api: Default::default(),
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
            runtime_snapshot: None,
            remote_snapshot: None,
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
