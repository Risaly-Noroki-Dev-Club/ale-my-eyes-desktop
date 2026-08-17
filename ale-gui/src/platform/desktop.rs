use super::{CapturedImage, ExecutionControl, ExecutionResult, PlatformCapabilities};
use crate::automation::{AutomationConfig, AutomationEngine};
use crate::screen_capture::{CaptureConfig, ScreenCapture};
use ale_core::actions::ActionPlan;
use ale_core::{AleError, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// 桌面平台服务：屏幕捕获 + enigo 自动化
pub struct DesktopPlatform {
    screen_capture: Option<ScreenCapture>,
    automation: Option<Mutex<AutomationEngine>>,
    sensitive_ui_visible: AtomicBool,
}

impl DesktopPlatform {
    pub fn new() -> Self {
        let mut platform = Self {
            screen_capture: None,
            automation: None,
            sensitive_ui_visible: AtomicBool::new(false),
        };
        platform.init();
        platform
    }

    fn init(&mut self) {
        // 启动屏幕捕获
        let sc = ScreenCapture::new(CaptureConfig::default());
        if let Err(e) = sc.start() {
            tracing::warn!("Screen capture failed to start: {}", e);
        } else {
            self.screen_capture = Some(sc);
        }

        // 创建自动化引擎
        let automation_config = AutomationConfig::default();
        match AutomationEngine::new(automation_config) {
            Ok(ae) => self.automation = Some(Mutex::new(ae)),
            Err(e) => tracing::warn!("Automation engine failed: {}", e),
        }
    }
}

impl super::PlatformService for DesktopPlatform {
    fn capture_image(&self) -> Option<CapturedImage> {
        if self.sensitive_ui_visible.load(Ordering::Acquire) {
            return None;
        }
        let capture = self.screen_capture.as_ref()?.latest_capture()?;
        Some(CapturedImage {
            jpeg_data: capture.jpeg_data,
            coordinate_space: capture.coordinate_space,
        })
    }

    fn execute_plan(&self, plan: &ActionPlan, approved: bool) -> Result<ExecutionResult> {
        let auto = self
            .automation
            .as_ref()
            .ok_or_else(|| AleError::Other(anyhow::anyhow!("自动化引擎不可用")))?;

        let mut guard = auto
            .lock()
            .map_err(|e| AleError::Other(anyhow::anyhow!("自动化引擎锁失败: {}", e)))?;

        let result = guard.execute_plan(plan, approved)?;
        Ok(ExecutionResult {
            actions_executed: result.actions_executed,
        })
    }

    fn execute_plan_controlled(
        &self,
        plan: &ActionPlan,
        approved: bool,
        control: &ExecutionControl,
    ) -> Result<ExecutionResult> {
        let auto = self
            .automation
            .as_ref()
            .ok_or_else(|| AleError::Other(anyhow::anyhow!("自动化引擎不可用")))?;
        let mut guard = auto
            .lock()
            .map_err(|e| AleError::Other(anyhow::anyhow!("自动化引擎锁失败: {}", e)))?;
        let result = guard.execute_plan_controlled(plan, approved, control)?;
        Ok(ExecutionResult {
            actions_executed: result.actions_executed,
        })
    }

    fn is_automation_ready(&self) -> bool {
        self.automation.is_some()
    }

    fn set_sensitive_ui_visible(&self, visible: bool) {
        if visible {
            self.sensitive_ui_visible.store(true, Ordering::Release);
        }
        if let Some(ref screen_capture) = self.screen_capture {
            screen_capture.set_suspended(visible);
        }
        if !visible {
            self.sensitive_ui_visible.store(false, Ordering::Release);
        }
    }

    fn capabilities(&self) -> PlatformCapabilities {
        PlatformCapabilities {
            image_capture: self.screen_capture.is_some(),
            automation: self.automation.is_some(),
            local_microphone: true,
        }
    }
}
