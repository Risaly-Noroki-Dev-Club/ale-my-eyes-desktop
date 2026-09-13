//! A save belongs to the service, not to the UI future waiting for its result.
use crate::modeld::SupervisedModeldClient;
use ale_core::{config::AppConfig, AleEngine};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{oneshot, Notify};

static OUTSTANDING: AtomicUsize = AtomicUsize::new(0);
static DRAINED: Notify = Notify::const_new();

pub async fn drain() {
    loop {
        let changed = DRAINED.notified();
        if OUTSTANDING.load(Ordering::Acquire) == 0 {
            return;
        }
        changed.await;
    }
}

struct Ticket;
impl Drop for Ticket {
    fn drop(&mut self) {
        OUTSTANDING.fetch_sub(1, Ordering::AcqRel);
        DRAINED.notify_one();
    }
}

struct SaveRequest {
    config: AppConfig,
    reply: oneshot::Sender<Result<AppConfig, String>>,
    _ticket: Ticket,
    queued: std::time::Instant,
    operation: u64,
}

pub struct SettingsService {
    pending: Arc<Mutex<Option<SaveRequest>>>,
    wake: Arc<Notify>,
    closed: Arc<AtomicBool>,
}

impl SettingsService {
    pub fn start(
        engine: Arc<tokio::sync::Mutex<AleEngine>>,
        client: Option<SupervisedModeldClient>,
    ) -> Self {
        let pending = Arc::new(Mutex::new(None::<SaveRequest>));
        let wake = Arc::new(Notify::new());
        let queue = pending.clone();
        let notify = wake.clone();
        let closed = Arc::new(AtomicBool::new(false));
        let stopped = closed.clone();
        tokio::spawn(async move {
            loop {
                notify.notified().await;
                loop {
                    let request = queue.lock().unwrap_or_else(|e| e.into_inner()).take();
                    let Some(request) = request else { break };
                    let started = std::time::Instant::now();
                    ale_core::diagnostics::record(
                        "settings_save_started",
                        &[
                            ("operation_id", request.operation),
                            ("queue_ms", request.queued.elapsed().as_millis() as u64),
                        ],
                    );
                    let result = save_transaction(&engine, client.as_ref(), request.config).await;
                    ale_core::diagnostics::record(
                        "settings_save_finished",
                        &[
                            ("operation_id", request.operation),
                            ("elapsed_ms", started.elapsed().as_millis() as u64),
                            ("success", result.is_ok() as u64),
                        ],
                    );
                    let _ = request.reply.send(result);
                }
                if stopped.load(Ordering::Acquire) {
                    break;
                }
            }
        });
        Self {
            pending,
            wake,
            closed,
        }
    }

    pub fn submit(
        &self,
        config: AppConfig,
    ) -> Result<oneshot::Receiver<Result<AppConfig, String>>, String> {
        let (reply, receiver) = oneshot::channel();
        let mut pending = self
            .pending
            .try_lock()
            .map_err(|_| "设置服务正忙，请重试 / Settings service busy".to_string())?;
        OUTSTANDING.fetch_add(1, Ordering::AcqRel);
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        if let Some(previous) = pending.replace(SaveRequest {
            config,
            reply,
            _ticket: Ticket,
            queued: std::time::Instant::now(),
            operation: NEXT.fetch_add(1, Ordering::Relaxed),
        }) {
            let _ = previous.reply.send(Err("SAVE_SUPERSEDED".into()));
        }
        drop(pending);
        self.wake.notify_one();
        Ok(receiver)
    }
}

impl Drop for SettingsService {
    fn drop(&mut self) {
        // A started transaction still owns its request and finishes/rolls back.
        self.closed.store(true, Ordering::Release);
        self.wake.notify_one();
    }
}

async fn save_transaction(
    engine: &tokio::sync::Mutex<AleEngine>,
    client: Option<&SupervisedModeldClient>,
    config: AppConfig,
) -> Result<AppConfig, String> {
    let queued = std::time::Instant::now();
    let mut engine = engine.lock().await;
    ale_core::diagnostics::record(
        "settings_engine_lock_acquired",
        &[("wait_ms", queued.elapsed().as_millis() as u64)],
    );
    let previous = engine.config().clone();
    engine
        .update_config_async(config.clone())
        .await
        .map_err(|e| e.to_string())?;
    if let Some(client) = client {
        if let Err(error) = client.update_config(config.clone()).await {
            engine.update_config_async(previous).await.map_err(|_| {
                "模型设置同步失败，且设置回滚失败；请重新保存设置 / Settings rollback failed"
                    .to_string()
            })?;
            return Err(error);
        }
    }
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ale_core::secret_store::SecretStore;
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct SlowStore {
        value: Mutex<Option<String>>,
        started: AtomicBool,
    }
    impl SecretStore for SlowStore {
        fn get_api_key(&self) -> ale_core::Result<Option<String>> {
            Ok(self.value.lock().unwrap().clone())
        }
        fn set_api_key(&self, key: &str) -> ale_core::Result<()> {
            self.started.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_millis(150));
            *self.value.lock().unwrap() = Some(key.into());
            Ok(())
        }
        fn delete_api_key(&self) -> ale_core::Result<()> {
            *self.value.lock().unwrap() = None;
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn abandoned_save_finishes_and_pending_save_is_latest() {
        let dir = std::env::temp_dir().join(format!("ale-settings-{}", uuid::Uuid::new_v4()));
        let store = Arc::new(SlowStore::default());
        let engine = Arc::new(tokio::sync::Mutex::new(
            AleEngine::new_with_secret_store(&dir.join("config.json"), store.clone())
                .await
                .unwrap(),
        ));
        let service = SettingsService::start(engine.clone(), None);
        let mut config = engine.lock().await.config().clone();
        config.cloud_api.api_key = "test-A".into();
        let first = service.submit(config.clone()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !store.started.load(Ordering::Acquire) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        // There is only one executor thread in this runtime: a synchronous save
        // on it would prevent this timer from waking until the store finished.
        let start = Instant::now();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(start.elapsed() < Duration::from_millis(100));
        drop(first);
        config.cloud_api.api_key = "test-B".into();
        let second = service.submit(config.clone()).unwrap();
        config.cloud_api.api_key = "test-C".into();
        let third = service.submit(config).unwrap();
        assert_eq!(second.await.unwrap().unwrap_err(), "SAVE_SUPERSEDED");
        assert_eq!(third.await.unwrap().unwrap().cloud_api.api_key, "test-C");
        assert_eq!(store.value.lock().unwrap().as_deref(), Some("test-C"));
        assert_eq!(engine.lock().await.config().cloud_api.api_key, "test-C");
        drop(service);
        drain().await;
        std::fs::remove_dir_all(dir).unwrap();
    }
}
