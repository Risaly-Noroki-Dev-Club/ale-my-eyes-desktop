//! Bounded native work. A timed-out kernel retains its permit until it actually exits.
use crate::{AleError, Result};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, OnceLock,
};
use tokio::sync::Semaphore;

pub(crate) fn slot() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(1))
}

struct Cancellation {
    flag: Arc<AtomicBool>,
    native: Option<Arc<dyn Fn() + Send + Sync>>,
    armed: bool,
}
impl Drop for Cancellation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.flag.store(true, Ordering::Relaxed);
        crate::diagnostics::record("native_cancel_requested", &[]);
        if let Some(cancel) = &self.native {
            cancel();
        }
    }
}

pub(crate) fn check(flag: &AtomicBool) -> Result<()> {
    if flag.load(Ordering::Relaxed) {
        Err(crate::model_api::ModelCallError::new(
            crate::model_api::ErrorKind::Cancelled,
            "Local operation cancelled",
        )
        .into())
    } else {
        Ok(())
    }
}

pub(crate) async fn run<T: Send + 'static>(
    slot: Arc<Semaphore>,
    native: Option<Arc<dyn Fn() + Send + Sync>>,
    work: impl FnOnce(Arc<AtomicBool>) -> Result<T> + Send + 'static,
) -> Result<T> {
    static GLOBAL: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let permit = slot.try_acquire_owned().map_err(|_| {
        AleError::ConfigError(
            "Local model is still busy; wait for the previous worker to finish".into(),
        )
    })?;
    let global = GLOBAL
        .get_or_init(|| Arc::new(Semaphore::new(2)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| AleError::ConfigError("Local inference workers are busy".into()))?;
    let flag = Arc::new(AtomicBool::new(false));
    let mut cancellation = Cancellation {
        flag: flag.clone(),
        native,
        armed: true,
    };
    let task = tokio::task::spawn_blocking(move || {
        let (_permit, _global) = (permit, global);
        check(&flag)?;
        crate::diagnostics::record("native_worker_start", &[]);
        let result = work(flag);
        crate::diagnostics::record("native_worker_done", &[("success", result.is_ok() as u64)]);
        result
    });
    let deadline = crate::model_api::deadline()
        .min(tokio::time::Instant::now() + std::time::Duration::from_secs(80));
    let result = tokio::time::timeout_at(deadline, task)
        .await
        .map_err(|_| {
            crate::model_api::ModelCallError::new(
                crate::model_api::ErrorKind::Timeout,
                "Local operation timed out; cancellation requested",
            )
        })?
        .map_err(|_| AleError::ConfigError("Local worker failed".into()))?;
    cancellation.armed = false;
    drop(cancellation);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cancelled_waiter_keeps_worker_slot_until_actual_exit() {
        let slot = slot();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first_slot = slot.clone();
        let task = tokio::spawn(async move {
            run(first_slot, None, move |_| {
                let _ = started_tx.send(());
                release_rx.recv().unwrap();
                Ok(())
            })
            .await
        });
        started_rx.await.unwrap();
        task.abort();
        let _ = task.await;
        assert!(run(slot.clone(), None, |_| Ok(())).await.is_err());
        release_tx.send(()).unwrap();
        for _ in 0..100 {
            if slot.available_permits() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(slot.available_permits(), 1);
    }
}
