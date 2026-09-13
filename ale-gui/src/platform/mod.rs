use crate::screen_capture::ScreenCoordinateSpace;
use ale_core::actions::ActionPlan;
use ale_core::model_scheduler::BoundingBox;
use ale_core::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct CapturedImage {
    pub jpeg_data: Vec<u8>,
    pub coordinate_space: ScreenCoordinateSpace,
}

#[derive(Clone, Debug)]
pub struct AccessibilityNode {
    pub node_id: String,
    pub role: Option<String>,
    pub label: Option<String>,
    pub bounds: BoundingBox,
}

#[derive(Clone, Debug)]
pub struct AccessibilitySnapshot {
    pub application_id: Option<String>,
    pub nodes: Vec<AccessibilityNode>,
}

/// 统一的自动化执行结果
pub struct ExecutionResult {
    pub actions_executed: usize,
}

#[derive(Clone)]
pub struct ExecutionControl {
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
    wake: Arc<(std::sync::Mutex<()>, std::sync::Condvar)>,
}

impl ExecutionControl {
    pub fn new(deadline: Instant) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline,
            wake: Arc::new((std::sync::Mutex::new(()), std::sync::Condvar::new())),
        }
    }

    pub fn cancel(&self) {
        let _guard = self.wake.0.lock().unwrap_or_else(|e| e.into_inner());
        self.cancelled.store(true, Ordering::Release);
        self.wake.1.notify_all();
    }

    pub fn remaining(&self) -> std::time::Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    pub fn wait(&self, duration: std::time::Duration) -> Result<()> {
        let until = Instant::now() + duration;
        let mut guard = self.wake.0.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            self.check()?;
            let remaining = until
                .saturating_duration_since(Instant::now())
                .min(self.remaining());
            if remaining.is_zero() {
                return self.check();
            }
            guard = self
                .wake
                .1
                .wait_timeout(guard, remaining)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }

    pub fn timed_out(&self) -> bool {
        Instant::now() >= self.deadline
    }

    pub fn check(&self) -> Result<()> {
        if self.timed_out() {
            return Err(ale_core::AleError::Other(anyhow::anyhow!(
                "CONFIRM_TIMEOUT"
            )));
        }
        if self.cancelled.load(Ordering::Acquire) {
            return Err(ale_core::AleError::Other(anyhow::anyhow!("CANCELLED")));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PlatformCapabilities {
    pub image_capture: bool,
    pub automation: bool,
    pub local_microphone: bool,
}

/// Desktop 负责屏幕捕获和执行自动化操作。
pub trait PlatformService: Send + Sync {
    /// 捕获当前屏幕画面及其桌面坐标空间。
    fn capture_image(&self) -> Option<CapturedImage>;

    fn capture_image_now(&self) -> Option<CapturedImage> {
        self.capture_image()
    }

    fn capture_accessibility(
        &self,
        _coordinate_space: &ScreenCoordinateSpace,
    ) -> Option<AccessibilitySnapshot> {
        None
    }

    /// 执行自动化操作计划
    fn execute_plan(&self, plan: &ActionPlan, approved: bool) -> Result<ExecutionResult>;

    fn execute_plan_controlled(
        &self,
        plan: &ActionPlan,
        approved: bool,
        control: &ExecutionControl,
    ) -> Result<ExecutionResult> {
        control.check()?;
        self.execute_plan(plan, approved)
    }

    /// 自动化引擎是否就绪
    fn is_automation_ready(&self) -> bool;

    /// Prevent capture while credentials or other sensitive settings are visible.
    fn set_sensitive_ui_visible(&self, visible: bool);

    fn capabilities(&self) -> PlatformCapabilities;
}

/// 为当前编译目标创建平台服务实例
pub fn create_platform() -> Box<dyn PlatformService> {
    Box::new(desktop::DesktopPlatform::new())
}

mod desktop;

static CAPTURE_SLOT: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));
static ACCESSIBILITY_SLOT: std::sync::LazyLock<Arc<tokio::sync::Semaphore>> =
    std::sync::LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));

pub async fn capture_now(platform: Arc<dyn PlatformService>) -> Option<CapturedImage> {
    capture_async(platform, true).await
}
pub async fn capture_cached(platform: Arc<dyn PlatformService>) -> Option<CapturedImage> {
    capture_async(platform, false).await
}
async fn capture_async(platform: Arc<dyn PlatformService>, fresh: bool) -> Option<CapturedImage> {
    let permit = CAPTURE_SLOT.clone().try_acquire_owned().ok()?;
    let worker = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let start = Instant::now();
        let result = if fresh {
            platform.capture_image_now()
        } else {
            platform.capture_image()
        };
        ale_core::diagnostics::record(
            "capture_complete",
            &[
                ("elapsed_ms", start.elapsed().as_millis() as u64),
                ("success", result.is_some() as u64),
            ],
        );
        result
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), worker)
        .await
        .ok()?
        .ok()?
}

pub async fn accessibility(
    platform: Arc<dyn PlatformService>,
    space: ScreenCoordinateSpace,
) -> Option<AccessibilitySnapshot> {
    let permit = ACCESSIBILITY_SLOT.clone().try_acquire_owned().ok()?;
    let worker = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let start = Instant::now();
        let result = platform.capture_accessibility(&space);
        ale_core::diagnostics::record(
            "accessibility_complete",
            &[
                ("elapsed_ms", start.elapsed().as_millis() as u64),
                (
                    "nodes",
                    result.as_ref().map_or(0, |snapshot| snapshot.nodes.len()) as u64,
                ),
            ],
        );
        result
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), worker)
        .await
        .ok()?
        .ok()?
}

#[cfg(test)]
mod control_tests {
    use super::*;
    #[test]
    fn cancellation_wakes_long_wait() {
        let control = ExecutionControl::new(Instant::now() + std::time::Duration::from_secs(60));
        let other = control.clone();
        let task = std::thread::spawn(move || other.wait(std::time::Duration::from_secs(30)));
        std::thread::sleep(std::time::Duration::from_millis(30));
        let start = Instant::now();
        control.cancel();
        assert!(task.join().unwrap().is_err());
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
    }
}
