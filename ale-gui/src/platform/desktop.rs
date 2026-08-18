#[cfg(target_os = "windows")]
use super::AccessibilityNode;
use super::{
    AccessibilitySnapshot, CapturedImage, ExecutionControl, ExecutionResult, PlatformCapabilities,
};
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

    fn capture_image_now(&self) -> Option<CapturedImage> {
        if self.sensitive_ui_visible.load(Ordering::Acquire) {
            return None;
        }
        let capture = self.screen_capture.as_ref()?.capture_now_jpeg().ok()?;
        Some(CapturedImage {
            jpeg_data: capture.jpeg_data,
            coordinate_space: capture.coordinate_space,
        })
    }

    fn capture_accessibility(
        &self,
        coordinate_space: &crate::screen_capture::ScreenCoordinateSpace,
    ) -> Option<AccessibilitySnapshot> {
        capture_accessibility_snapshot(coordinate_space)
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

#[cfg(not(target_os = "windows"))]
fn capture_accessibility_snapshot(
    _coordinate_space: &crate::screen_capture::ScreenCoordinateSpace,
) -> Option<AccessibilitySnapshot> {
    None
}

#[cfg(target_os = "windows")]
fn capture_accessibility_snapshot(
    coordinate_space: &crate::screen_capture::ScreenCoordinateSpace,
) -> Option<AccessibilitySnapshot> {
    #[derive(serde::Deserialize)]
    struct RawSnapshot {
        application_id: Option<String>,
        nodes: Vec<RawNode>,
    }

    #[derive(serde::Deserialize)]
    struct RawNode {
        node_id: String,
        role: Option<String>,
        label: Option<String>,
        left: f64,
        top: f64,
        width: f64,
        height: f64,
    }

    const SCRIPT: &str = r#"
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
Add-Type -AssemblyName UIAutomationClient
Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; public static class AleNative { [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow(); }'
$root = [System.Windows.Automation.AutomationElement]::FromHandle([AleNative]::GetForegroundWindow())
if ($null -eq $root) { exit 2 }
$nodes = New-Object System.Collections.Generic.List[object]
$elements = $root.FindAll([System.Windows.Automation.TreeScope]::Descendants, [System.Windows.Automation.Condition]::TrueCondition)
$limit = [Math]::Min($elements.Count, 512)
for ($index = 0; $index -lt $limit; $index++) {
  $element = $elements.Item($index)
  try {
    if ($element.Current.IsPassword) { continue }
    $rect = $element.Current.BoundingRectangle
    if ($rect.Width -le 0 -or $rect.Height -le 0) { continue }
    $nodes.Add([ordered]@{
      node_id = [string]$element.Current.AutomationId
      role = [string]$element.Current.ControlType.ProgrammaticName
      label = [string]$element.Current.Name
      left = $rect.Left
      top = $rect.Top
      width = $rect.Width
      height = $rect.Height
    })
  } catch {}
}
[ordered]@{ application_id = [string]$root.Current.Name; nodes = $nodes } | ConvertTo-Json -Depth 4 -Compress
"#;
    let output = std::process::Command::new("powershell.exe")
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            SCRIPT,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let raw: RawSnapshot = serde_json::from_slice(&output.stdout).ok()?;
    let desktop_width = coordinate_space.desktop_width as f64;
    let desktop_height = coordinate_space.desktop_height as f64;
    if desktop_width <= 0.0 || desktop_height <= 0.0 {
        return None;
    }
    let nodes = raw
        .nodes
        .into_iter()
        .filter_map(|node| {
            let left = (node.left - coordinate_space.desktop_x as f64) / desktop_width;
            let top = (node.top - coordinate_space.desktop_y as f64) / desktop_height;
            let right = (left + node.width / desktop_width).clamp(0.0, 1.0);
            let bottom = (top + node.height / desktop_height).clamp(0.0, 1.0);
            let left = left.clamp(0.0, 1.0);
            let top = top.clamp(0.0, 1.0);
            let bounds = ale_core::model_scheduler::BoundingBox {
                x: left as f32,
                y: top as f32,
                width: (right - left) as f32,
                height: (bottom - top) as f32,
            };
            bounds.is_normalized().then_some(AccessibilityNode {
                node_id: node.node_id,
                role: node.role.filter(|value| !value.is_empty()),
                label: node.label.filter(|value| !value.is_empty()),
                bounds,
            })
        })
        .collect();
    Some(AccessibilitySnapshot {
        application_id: raw.application_id.filter(|value| !value.is_empty()),
        nodes,
    })
}
