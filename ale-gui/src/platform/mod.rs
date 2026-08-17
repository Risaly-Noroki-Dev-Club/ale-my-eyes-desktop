use crate::screen_capture::ScreenCoordinateSpace;
use ale_core::actions::ActionPlan;
use ale_core::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct CapturedImage {
    pub jpeg_data: Vec<u8>,
    pub coordinate_space: ScreenCoordinateSpace,
}

/// 统一的自动化执行结果
pub struct ExecutionResult {
    pub actions_executed: usize,
}

#[derive(Clone)]
pub struct ExecutionControl {
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
}

impl ExecutionControl {
    pub fn new(deadline: Instant) -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            deadline,
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
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
