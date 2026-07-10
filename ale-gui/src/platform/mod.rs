use ale_core::actions::ActionPlan;
use ale_core::Result;

/// 统一的自动化执行结果
pub struct ExecutionResult {
    pub actions_executed: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct PlatformCapabilities {
    pub image_capture: bool,
    pub automation: bool,
    pub local_microphone: bool,
}

/// Desktop 负责屏幕捕获和执行自动化操作。
pub trait PlatformService: Send + Sync {
    /// 捕获当前屏幕画面，返回 JPEG 字节。
    fn capture_image(&self) -> Option<Vec<u8>>;

    /// 执行自动化操作计划
    fn execute_plan(&self, plan: &ActionPlan, approved: bool) -> Result<ExecutionResult>;

    /// 自动化引擎是否就绪
    fn is_automation_ready(&self) -> bool;

    fn capabilities(&self) -> PlatformCapabilities;
}

/// 为当前编译目标创建平台服务实例
pub fn create_platform() -> Box<dyn PlatformService> {
    Box::new(desktop::DesktopPlatform::new())
}

mod desktop;
