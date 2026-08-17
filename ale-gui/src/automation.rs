use crate::platform::ExecutionControl;
use ale_core::actions::{Action, ActionPlan, FileOp, MouseButton};
use ale_core::{AleError, Result};
#[cfg(not(target_os = "windows"))]
use enigo::Coordinate;
use enigo::{Button as EnigoButton, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use std::path::{Component, Path, PathBuf};

/// 自动化引擎配置
#[derive(Debug, Clone)]
pub struct AutomationConfig {
    /// 是否需要确认高风险操作
    pub require_confirmation: bool,
    /// 操作间隔（毫秒）
    pub action_delay_ms: u64,
    /// 鼠标移动速度（毫秒）
    pub mouse_move_duration_ms: u64,
}

impl Default for AutomationConfig {
    fn default() -> Self {
        Self {
            require_confirmation: true,
            action_delay_ms: 100,
            mouse_move_duration_ms: 200,
        }
    }
}

/// 自动化执行结果
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub success: bool,
    pub actions_executed: usize,
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
#[cfg(any(target_os = "windows", test))]
struct VirtualDesktopBounds {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// 桌面自动化引擎
pub struct AutomationEngine {
    enigo: Enigo,
    config: AutomationConfig,
}

impl AutomationEngine {
    pub fn new(config: AutomationConfig) -> Result<Self> {
        let enigo = Enigo::new(&Settings::default())
            .map_err(|e| AleError::Other(anyhow::anyhow!("Failed to initialize Enigo: {}", e)))?;

        Ok(Self { enigo, config })
    }

    /// 执行操作计划
    pub fn execute_plan(&mut self, plan: &ActionPlan, approved: bool) -> Result<ExecutionResult> {
        self.execute_plan_inner(plan, approved, None)
    }

    pub fn execute_plan_controlled(
        &mut self,
        plan: &ActionPlan,
        approved: bool,
        control: &ExecutionControl,
    ) -> Result<ExecutionResult> {
        self.execute_plan_inner(plan, approved, Some(control))
    }

    fn execute_plan_inner(
        &mut self,
        plan: &ActionPlan,
        approved: bool,
        control: Option<&ExecutionControl>,
    ) -> Result<ExecutionResult> {
        plan.validate()?;
        if plan.requires_confirmation && !approved {
            return Err(AleError::Other(anyhow::anyhow!(
                "操作需要用户确认: {}",
                plan.explanation
            )));
        }

        let mut executed = 0;
        for action in &plan.actions {
            if let Some(control) = control {
                control.check()?;
            }
            self.execute_action(action)?;
            executed += 1;
            if let Some(control) = control {
                control.check()?;
            }
            if self.config.action_delay_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(
                    self.config.action_delay_ms,
                ));
            }
        }

        if let Some(control) = control {
            control.check()?;
        }

        Ok(ExecutionResult {
            success: true,
            actions_executed: executed,
            error: None,
        })
    }

    /// 执行单个操作
    pub fn execute_action(&mut self, action: &Action) -> Result<()> {
        match action {
            Action::Click { x, y, button } => {
                self.mouse_move(*x, *y)?;
                self.mouse_click(*button)?;
            }
            Action::DoubleClick { x, y } => {
                self.mouse_move(*x, *y)?;
                self.mouse_click(MouseButton::Left)?;
                std::thread::sleep(std::time::Duration::from_millis(50));
                self.mouse_click(MouseButton::Left)?;
            }
            Action::MouseMove { x, y } => {
                self.mouse_move(*x, *y)?;
            }
            Action::Scroll {
                x,
                y,
                delta_x,
                delta_y,
            } => {
                self.mouse_move(*x, *y)?;
                self.enigo
                    .scroll(*delta_y as i32, enigo::Axis::Vertical)
                    .map_err(|e| AleError::Other(anyhow::anyhow!("Scroll failed: {}", e)))?;
                if *delta_x != 0.0 {
                    self.enigo
                        .scroll(*delta_x as i32, enigo::Axis::Horizontal)
                        .map_err(|e| {
                            AleError::Other(anyhow::anyhow!("Horizontal scroll failed: {}", e))
                        })?;
                }
            }
            Action::Type { text } => {
                self.enigo
                    .text(text)
                    .map_err(|e| AleError::Other(anyhow::anyhow!("Type failed: {}", e)))?;
            }
            Action::Key { key, modifiers } => {
                // 先按修饰键
                for modifier in modifiers {
                    let mod_key = parse_key(modifier);
                    self.enigo.key(mod_key, Direction::Press).map_err(|e| {
                        AleError::Other(anyhow::anyhow!("Modifier key press failed: {}", e))
                    })?;
                }

                // 按主键
                let main_key = parse_key(key);
                self.enigo
                    .key(main_key, Direction::Click)
                    .map_err(|e| AleError::Other(anyhow::anyhow!("Key press failed: {}", e)))?;

                // 释放修饰键（逆序）
                for modifier in modifiers.iter().rev() {
                    let mod_key = parse_key(modifier);
                    self.enigo.key(mod_key, Direction::Release).map_err(|e| {
                        AleError::Other(anyhow::anyhow!("Modifier key release failed: {}", e))
                    })?;
                }
            }
            Action::Wait { ms } => {
                std::thread::sleep(std::time::Duration::from_millis(*ms));
            }
            Action::OpenApp { name } => {
                // 跨平台打开应用
                open_application(name)?;
            }
            Action::CloseApp { name } => {
                // 关闭应用（发送 Alt+F4 或 Cmd+Q）
                close_application(name)?;
            }
            Action::OpenUrl { url } => {
                open_url(url)?;
            }
            Action::FileOperation {
                operation,
                path,
                target,
            } => {
                execute_file_op(*operation, path, target.as_deref())?;
            }
        }
        Ok(())
    }

    fn mouse_move(&mut self, x: f64, y: f64) -> Result<()> {
        #[cfg(target_os = "windows")]
        {
            move_mouse_on_windows_virtual_desktop(x, y)
        }
        #[cfg(not(target_os = "windows"))]
        {
            self.enigo
                .move_mouse(x as i32, y as i32, Coordinate::Abs)
                .map_err(|e| AleError::Other(anyhow::anyhow!("Mouse move failed: {}", e)))
        }
    }

    fn mouse_click(&mut self, button: MouseButton) -> Result<()> {
        let btn = match button {
            MouseButton::Left => EnigoButton::Left,
            MouseButton::Right => EnigoButton::Right,
            MouseButton::Middle => EnigoButton::Middle,
        };
        self.enigo
            .button(btn, Direction::Click)
            .map_err(|e| AleError::Other(anyhow::anyhow!("Mouse click failed: {}", e)))
    }
}

#[cfg(any(target_os = "windows", test))]
fn normalize_virtual_desktop_point(
    x: f64,
    y: f64,
    bounds: VirtualDesktopBounds,
) -> Result<(i32, i32)> {
    fn normalize_axis(value: f64, origin: i32, extent: i32) -> Result<i32> {
        if !value.is_finite() || extent <= 0 {
            return Err(AleError::ConfigError(
                "Windows 虚拟桌面坐标空间无效".to_string(),
            ));
        }
        let minimum = origin as f64;
        let maximum = origin as f64 + extent as f64 - 1.0;
        if value < minimum || value > maximum {
            return Err(AleError::ConfigError(format!(
                "自动化坐标 {value} 超出 Windows 虚拟桌面范围 {minimum}..={maximum}"
            )));
        }
        if extent == 1 {
            return Ok(0);
        }
        Ok((((value - minimum) * 65_535.0) / (extent as f64 - 1.0)).round() as i32)
    }

    Ok((
        normalize_axis(x, bounds.x, bounds.width)?,
        normalize_axis(y, bounds.y, bounds.height)?,
    ))
}

#[cfg(target_os = "windows")]
fn move_mouse_on_windows_virtual_desktop(x: f64, y: f64) -> Result<()> {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
        SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_MOVE,
        MOUSEEVENTF_VIRTUALDESK, MOUSEINPUT,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
        SM_YVIRTUALSCREEN,
    };

    let bounds = unsafe {
        VirtualDesktopBounds {
            x: GetSystemMetrics(SM_XVIRTUALSCREEN),
            y: GetSystemMetrics(SM_YVIRTUALSCREEN),
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
        }
    };
    let (dx, dy) = normalize_virtual_desktop_point(x, y, bounds)?;
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let sent = unsafe { SendInput(1, &input, std::mem::size_of::<INPUT>() as i32) };
    if sent != 1 {
        return Err(AleError::Other(anyhow::anyhow!(
            "Windows SendInput mouse move failed: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(())
}

/// 解析按键名称为 Enigo Key
fn parse_key(name: &str) -> Key {
    match name.to_lowercase().as_str() {
        "ctrl" | "control" => Key::Control,
        "alt" => Key::Alt,
        "shift" => Key::Shift,
        "super" | "win" | "meta" | "cmd" | "command" => Key::Meta,
        "enter" | "return" => Key::Return,
        "tab" => Key::Tab,
        "space" => Key::Space,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "escape" | "esc" => Key::Escape,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" => Key::PageUp,
        "pagedown" => Key::PageDown,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        _ => {
            // 单字符按键
            if name.len() == 1 {
                name.chars()
                    .next()
                    .map(Key::Unicode)
                    .unwrap_or(Key::Unicode('?'))
            } else {
                Key::Unicode('?')
            }
        }
    }
}

/// 打开应用程序
fn open_application(name: &str) -> Result<()> {
    let name = safe_application_name(name)?;
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new(name).spawn().map_err(|e| {
            AleError::Other(anyhow::anyhow!("Failed to open app '{}': {}", name, e))
        })?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("cmd")
            .args(["/C", "start", "", name])
            .spawn()
            .map_err(|e| {
                AleError::Other(anyhow::anyhow!("Failed to open app '{}': {}", name, e))
            })?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .args(["-a", name])
            .spawn()
            .map_err(|e| {
                AleError::Other(anyhow::anyhow!("Failed to open app '{}': {}", name, e))
            })?;
    }
    Ok(())
}

/// 关闭应用程序
fn close_application(name: &str) -> Result<()> {
    let name = safe_application_name(name)?;
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("pkill")
            .args(["-x", name])
            .spawn()
            .map_err(|e| {
                AleError::Other(anyhow::anyhow!("Failed to close app '{}': {}", name, e))
            })?;
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("taskkill")
            .args(["/IM", &format!("{}.exe", name), "/F"])
            .spawn()
            .map_err(|e| {
                AleError::Other(anyhow::anyhow!("Failed to close app '{}': {}", name, e))
            })?;
    }
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("osascript")
            .args(["-e", &format!("quit app \"{}\"", name)])
            .spawn()
            .map_err(|e| {
                AleError::Other(anyhow::anyhow!("Failed to close app '{}': {}", name, e))
            })?;
    }
    Ok(())
}

fn safe_application_name(name: &str) -> Result<&str> {
    let name = name.trim();
    if name.is_empty() {
        return Err(AleError::Other(anyhow::anyhow!(
            "Application name cannot be empty"
        )));
    }

    if name.contains('/') || name.contains('\\') {
        return Err(AleError::Other(anyhow::anyhow!(
            "Application name cannot contain path separators"
        )));
    }

    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '.' | '_' | '-'))
    {
        return Err(AleError::Other(anyhow::anyhow!(
            "Application name contains unsafe characters"
        )));
    }

    Ok(name)
}

/// 打开 URL
fn open_url(url: &str) -> Result<()> {
    let url = safe_url(url)?;
    open::that(url).map_err(|e| AleError::Other(anyhow::anyhow!("Failed to open URL: {}", e)))
}

fn safe_url(url: &str) -> Result<&str> {
    let url = url.trim();
    if url.is_empty() {
        return Err(AleError::Other(anyhow::anyhow!("URL cannot be empty")));
    }
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        return Err(AleError::Other(anyhow::anyhow!(
            "Only http:// and https:// URLs are allowed"
        )));
    }

    Ok(url)
}

/// 执行文件操作
fn execute_file_op(op: FileOp, path: &str, target: Option<&str>) -> Result<()> {
    let path = safe_automation_path(path)?;
    match op {
        FileOp::Create => {
            if path.to_string_lossy().ends_with('/') || path.to_string_lossy().ends_with('\\') {
                std::fs::create_dir_all(&path)?;
            } else {
                std::fs::write(&path, "")?;
            }
        }
        FileOp::Delete => {
            if path.is_dir() {
                return Err(AleError::Other(anyhow::anyhow!(
                    "为避免递归删除，自动化不允许删除目录"
                )));
            } else {
                std::fs::remove_file(&path)?;
            }
        }
        FileOp::Move | FileOp::Copy => {
            let target = target.ok_or_else(|| {
                AleError::Other(anyhow::anyhow!("Move/Copy requires target path"))
            })?;
            let target = safe_automation_path(target)?;
            if op == FileOp::Move {
                std::fs::rename(&path, &target)?;
            } else {
                std::fs::copy(&path, &target)?;
            }
        }
        FileOp::Rename => {
            let target = target
                .ok_or_else(|| AleError::Other(anyhow::anyhow!("Rename requires new name")))?;
            let target = safe_automation_path(target)?;
            std::fs::rename(&path, &target)?;
        }
    }
    Ok(())
}

fn safe_automation_path(path: &str) -> Result<PathBuf> {
    if path.trim().is_empty() {
        return Err(AleError::Other(anyhow::anyhow!(
            "Automation file path cannot be empty"
        )));
    }

    let path = Path::new(path);
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(AleError::Other(anyhow::anyhow!(
            "Automation file path cannot contain parent directory components"
        )));
    }

    let home = home_dir().ok_or_else(|| {
        AleError::Other(anyhow::anyhow!(
            "Cannot determine home directory for file operation"
        ))
    })?;
    let candidate = if path.is_absolute() {
        path.to_path_buf()
    } else {
        home.join(path)
    };

    if candidate == home || !candidate.starts_with(&home) {
        return Err(AleError::Other(anyhow::anyhow!(
            "Automation file operation is limited to user home directory"
        )));
    }

    let canonical_home = home.canonicalize()?;
    let existing_parent = nearest_existing_parent(&candidate)?.canonicalize()?;
    if !existing_parent.starts_with(&canonical_home) {
        return Err(AleError::Other(anyhow::anyhow!(
            "自动化文件路径不能通过符号链接离开用户目录"
        )));
    }

    Ok(candidate)
}

fn nearest_existing_parent(path: &Path) -> Result<&Path> {
    let mut parent = path.parent();
    while let Some(candidate) = parent {
        if candidate.exists() {
            return Ok(candidate);
        }
        parent = candidate.parent();
    }
    Err(AleError::Other(anyhow::anyhow!(
        "Cannot find an existing parent directory for file operation"
    )))
}

fn home_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE").map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_key() {
        assert!(matches!(parse_key("ctrl"), Key::Control));
        assert!(matches!(parse_key("alt"), Key::Alt));
        assert!(matches!(parse_key("enter"), Key::Return));
        assert!(matches!(parse_key("a"), Key::Unicode('a')));
    }

    #[test]
    fn test_automation_config_default() {
        let config = AutomationConfig::default();
        assert!(config.require_confirmation);
        assert_eq!(config.action_delay_ms, 100);
    }

    #[test]
    fn windows_virtual_desktop_normalization_handles_negative_origins() {
        let bounds = VirtualDesktopBounds {
            x: -1920,
            y: -200,
            width: 3840,
            height: 1280,
        };
        assert_eq!(
            normalize_virtual_desktop_point(-1920.0, -200.0, bounds).unwrap(),
            (0, 0)
        );
        assert_eq!(
            normalize_virtual_desktop_point(1919.0, 1079.0, bounds).unwrap(),
            (65_535, 65_535)
        );
        let (center_x, center_y) = normalize_virtual_desktop_point(-0.5, 439.5, bounds).unwrap();
        assert_eq!((center_x, center_y), (32_768, 32_768));
    }

    #[test]
    fn windows_virtual_desktop_normalization_rejects_out_of_bounds_points() {
        let bounds = VirtualDesktopBounds {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        for point in [(-1.0, 0.0), (1920.0, 0.0), (0.0, -1.0), (0.0, 1080.0)] {
            assert!(normalize_virtual_desktop_point(point.0, point.1, bounds).is_err());
        }
    }

    #[test]
    fn test_safe_application_name_accepts_common_names() {
        assert_eq!(safe_application_name("notepad").unwrap(), "notepad");
        assert_eq!(
            safe_application_name("Google Chrome").unwrap(),
            "Google Chrome"
        );
        assert_eq!(
            safe_application_name("code-insiders").unwrap(),
            "code-insiders"
        );
    }

    #[test]
    fn test_safe_application_name_rejects_unsafe_names() {
        assert!(safe_application_name("").is_err());
        assert!(safe_application_name("../evil").is_err());
        assert!(safe_application_name("calc && rm -rf ~").is_err());
        assert!(safe_application_name("app\"name").is_err());
    }

    #[test]
    fn test_safe_url_accepts_http_urls() {
        assert_eq!(
            safe_url("https://example.com").unwrap(),
            "https://example.com"
        );
        assert_eq!(
            safe_url("http://example.com").unwrap(),
            "http://example.com"
        );
    }

    #[test]
    fn test_safe_url_rejects_unsafe_schemes() {
        assert!(safe_url("").is_err());
        assert!(safe_url("file:///etc/passwd").is_err());
        assert!(safe_url("javascript:alert(1)").is_err());
        assert!(safe_url("mailto:test@example.com").is_err());
    }

    #[test]
    fn test_safe_automation_path_rejects_empty() {
        assert!(safe_automation_path("  ").is_err());
    }

    #[test]
    fn test_safe_automation_path_rejects_home_root() {
        let home = home_dir().unwrap();
        assert!(safe_automation_path(&home.to_string_lossy()).is_err());
    }

    #[test]
    fn test_safe_automation_path_rejects_parent_dir() {
        assert!(safe_automation_path("../outside.txt").is_err());
    }

    #[test]
    fn test_safe_automation_path_allows_relative_path() {
        let path = safe_automation_path("Documents/test.txt").unwrap();
        assert!(path.starts_with(home_dir().unwrap()));
    }
}
