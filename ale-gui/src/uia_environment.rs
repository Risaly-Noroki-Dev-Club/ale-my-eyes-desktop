//! Windows UI Automation adapter for the desktop harness.
//!
//! [`UiaEnvironment`] implements `ale_core::harness::Environment` on top of an
//! injectable [`UiaBridge`]. The bridge is the only platform-specific piece:
//! on Windows it shells out to PowerShell/UIA; in tests it is a mock, which is
//! why every parsing, re-validation and receipt-mapping rule below is covered
//! on the macOS host too.
//!
//! Contract notes:
//! * `observe` always performs a fresh tree walk; sequence numbers increase.
//! * `execute` re-reads the tree, re-validates the confirmed window, target
//!   identity and enabled state, and only then dispatches. A changed or
//!   unreadable target returns `NotApplied` without issuing input.
//! * Dropping the future cancels a running PowerShell helper so it cannot keep
//!   typing after the runner abandoned the task.
//! * Non-Windows builds are explicitly unavailable instead of faking support.

use crate::verified_task::PROPERTY_WHITELIST;
use ale_core::harness::{
    ActionRequest, Environment, Node, Observation, Operation, OperationKind, Receipt,
    WindowIdentity,
};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::platform::ExecutionControl;

/// Maximum number of nodes one observation may carry (`harness::run` rejects more).
pub const MAX_OBSERVED_NODES: usize = 512;
/// Bound on one bridge observation call, leaving room inside the runner's
/// 10 s per-call budget for validation and retries.
pub const OBSERVE_TIMEOUT: Duration = Duration::from_secs(4);
/// Bound on one bridge execute call; input may already have been issued when it
/// expires, so the adapter reports `Uncertain` in that case.
pub const EXECUTE_TIMEOUT: Duration = Duration::from_secs(8);

/// Foreground window as reported by the bridge.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct RawWindow {
    pub process_id: u32,
    pub window_id: String,
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Deserialize)]
pub struct RawBounds {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

/// One accessibility node in the raw bridge format. `bounds` is captured for
/// host-side diagnostics/display; the harness [`Node`] contract only carries
/// identity, role, label, enabled and verifiable properties.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct RawNode {
    pub id: String,
    pub role: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
    #[serde(default)]
    pub bounds: Option<RawBounds>,
}

#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct RawObservation {
    pub window: RawWindow,
    pub nodes: Vec<RawNode>,
}

/// One already-authorized action handed to the platform bridge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiaAction {
    pub window: WindowIdentity,
    /// Tree path from `observe`, e.g. `0/2/1`.
    pub target_id: String,
    pub role: String,
    pub label: String,
    pub operation: OperationKind,
    pub value: Option<String>,
}

/// Result of one platform dispatch attempt. `NotApplied` guarantees no input
/// was issued; `Uncertain` means input may have happened and must not be replayed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BridgeReceipt {
    Dispatched,
    NotApplied(String),
    Uncertain(String),
}

impl From<BridgeReceipt> for Receipt {
    fn from(receipt: BridgeReceipt) -> Self {
        match receipt {
            BridgeReceipt::Dispatched => Receipt::Dispatched,
            BridgeReceipt::NotApplied(reason) => Receipt::NotApplied(reason),
            BridgeReceipt::Uncertain(reason) => Receipt::Uncertain(reason),
        }
    }
}

/// Platform-specific UIA implementation. Must be `Send` because it runs on the
/// blocking pool; it must honor `ExecutionControl` cancellation.
pub trait UiaBridge: Send {
    fn observe(&mut self, control: &ExecutionControl) -> Result<RawObservation, String>;
    fn execute(&mut self, action: &UiaAction, control: &ExecutionControl) -> BridgeReceipt;
}

/// True when this build can drive real Windows UI Automation.
pub fn uia_available() -> bool {
    cfg!(target_os = "windows")
}

/// Create the platform bridge for this build, or `None` when verified tasks are
/// not supported here (the caller then falls back to the legacy path).
pub fn create_platform_bridge() -> Option<Box<dyn UiaBridge>> {
    #[cfg(target_os = "windows")]
    {
        Some(Box::new(WindowsUiaBridge))
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
    }
}

/// Create a ready adapter for this build, or `None` when unavailable.
pub fn create_platform_environment() -> Option<UiaEnvironment> {
    create_platform_bridge().map(UiaEnvironment::new)
}

/// Parses the JSON produced by the observe helper. Kept public and platform
/// independent so the parser can be unit-tested on any host.
pub fn parse_observation_json(json: &str) -> Result<RawObservation, String> {
    serde_json::from_str(json).map_err(|error| format!("invalid uia observation: {error}"))
}

/// Convert a raw bridge observation into the harness contract.
pub fn build_observation(sequence: u64, raw: RawObservation) -> Result<Observation, String> {
    let window_id = raw.window.window_id.trim();
    if window_id.is_empty() {
        return Err("uia observation has no window id".to_string());
    }
    if raw.nodes.len() > MAX_OBSERVED_NODES {
        return Err(format!(
            "uia observation has {} nodes; limit is {MAX_OBSERVED_NODES}",
            raw.nodes.len()
        ));
    }
    let window = WindowIdentity {
        process_id: raw.window.process_id,
        window_id: window_id.to_string(),
    };
    let mut seen = HashSet::with_capacity(raw.nodes.len());
    let mut nodes = Vec::with_capacity(raw.nodes.len());
    for raw_node in raw.nodes {
        let id = raw_node.id.trim().to_string();
        if id.is_empty() {
            return Err("uia observation contains a node without an id".to_string());
        }
        if !seen.insert(id.clone()) {
            return Err(format!("uia observation contains duplicate node id {id}"));
        }
        let role = raw_node.role.trim().to_string();
        let label = raw_node.label.trim().to_string();
        if role.is_empty() && label.is_empty() {
            // Nothing can be targeted, so keep it out of the model-visible tree.
            continue;
        }
        let mut properties = BTreeMap::new();
        for (key, value) in raw_node.properties {
            if PROPERTY_WHITELIST.contains(&key.as_str()) {
                properties.insert(key, value);
            }
        }
        // Make `enabled` verifiable as a predicate property as well.
        properties.insert(
            "enabled".to_string(),
            if raw_node.enabled { "true" } else { "false" }.to_string(),
        );
        nodes.push(Node {
            id,
            role,
            label,
            enabled: raw_node.enabled,
            properties,
        });
    }
    Ok(Observation {
        sequence,
        window,
        nodes,
    })
}

/// `ale_core::harness::Environment` implementation over a [`UiaBridge`].
pub struct UiaEnvironment {
    bridge: Arc<Mutex<Box<dyn UiaBridge>>>,
    expected_window: Option<WindowIdentity>,
    sequence: u64,
    cancelled: Arc<AtomicBool>,
}

impl UiaEnvironment {
    pub fn new(bridge: Box<dyn UiaBridge>) -> Self {
        Self::with_cancel_flag(bridge, Arc::new(AtomicBool::new(false)))
    }

    /// Share a cancellation flag with the request layer. The runner's `watch`
    /// channel stops the run; this flag lets the adapter refuse to start new
    /// input even if a host-side drop races with the blocking bridge.
    pub fn with_cancel_flag(bridge: Box<dyn UiaBridge>, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            bridge: Arc::new(Mutex::new(bridge)),
            expected_window: None,
            sequence: 0,
            cancelled,
        }
    }

    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    pub fn expected_window(&self) -> Option<&WindowIdentity> {
        self.expected_window.as_ref()
    }

    /// Acquire one fresh observation without running a task. The request path
    /// uses it to bind a verified task to the current foreground window.
    pub async fn observe_once(&mut self) -> Result<Observation, String> {
        self.observe_inner().await
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    async fn observe_inner(&mut self) -> Result<Observation, String> {
        if self.is_cancelled() {
            return Err("verified task cancelled before observation".to_string());
        }
        let bridge = self.bridge.clone();
        // The guard cancels the helper if this future is dropped mid-call.
        let guard = CancelOnDrop::new(Instant::now() + OBSERVE_TIMEOUT);
        let control = guard.control();
        let joined = tokio::task::spawn_blocking(move || {
            let mut bridge = bridge.lock().unwrap_or_else(|poison| poison.into_inner());
            bridge.observe(&control)
        })
        .await;
        drop(guard);
        let raw = match joined {
            Ok(result) => result?,
            Err(_) => return Err("uia observer panicked".to_string()),
        };
        self.sequence = self.sequence.saturating_add(1);
        let observation = build_observation(self.sequence, raw)?;
        if self.expected_window.is_none() {
            self.expected_window = Some(observation.window.clone());
        }
        Ok(observation)
    }

    async fn execute_inner(&mut self, request: &ActionRequest) -> Receipt {
        if self.is_cancelled() {
            return Receipt::NotApplied("verified task cancelled before input".to_string());
        }
        let Some(expected_window) = self.expected_window.clone() else {
            return Receipt::NotApplied("no confirmed foreground window".to_string());
        };
        if request.observation.window != expected_window {
            return Receipt::NotApplied("request observation window changed".to_string());
        }
        // Re-read immediately before input; never trust the approved snapshot.
        let fresh = match self.observe_inner().await {
            Ok(observation) => observation,
            Err(error) => return Receipt::NotApplied(format!("uia re-validation failed: {error}")),
        };
        if fresh.window != expected_window {
            return Receipt::NotApplied("foreground window changed before input".to_string());
        }
        let Some(confirmed_node) = request
            .observation
            .nodes
            .iter()
            .find(|node| node.id == request.step.target_id)
        else {
            return Receipt::NotApplied("confirmed target is missing from the request".to_string());
        };
        let Some(actual_node) = fresh
            .nodes
            .iter()
            .find(|node| node.id == request.step.target_id)
        else {
            return Receipt::NotApplied("target disappeared before input".to_string());
        };
        if !actual_node.enabled {
            return Receipt::NotApplied("target disabled before input".to_string());
        }
        if actual_node.role != confirmed_node.role || actual_node.label != confirmed_node.label {
            return Receipt::NotApplied("target identity changed before input".to_string());
        }
        if self.is_cancelled() {
            return Receipt::NotApplied("verified task cancelled before dispatch".to_string());
        }
        let (operation, value) = match &request.step.operation {
            Operation::Invoke => (OperationKind::Invoke, None),
            Operation::SetValue { value } => (OperationKind::SetValue, Some(value.clone())),
        };
        let action = UiaAction {
            window: expected_window,
            target_id: request.step.target_id.clone(),
            role: actual_node.role.clone(),
            label: actual_node.label.clone(),
            operation,
            value,
        };
        let bridge = self.bridge.clone();
        let guard = CancelOnDrop::new(Instant::now() + EXECUTE_TIMEOUT);
        let control = guard.control();
        let joined = tokio::task::spawn_blocking(move || {
            let mut bridge = bridge.lock().unwrap_or_else(|poison| poison.into_inner());
            bridge.execute(&action, &control)
        })
        .await;
        drop(guard);
        match joined {
            Ok(receipt) => receipt.into(),
            // The helper panicked; we cannot know whether it dispatched.
            Err(_) => Receipt::Uncertain("uia executor panicked; input state unknown".to_string()),
        }
    }
}

impl Environment for UiaEnvironment {
    // Hand-desugared `#[async_trait]` implementation: the bridge call itself is
    // blocking, so it is moved to `spawn_blocking` inside the future.
    fn observe<'life0, 'async_trait>(
        &'life0 mut self,
    ) -> Pin<Box<dyn Future<Output = Result<Observation, String>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { self.observe_inner().await })
    }

    fn execute<'life0, 'life1, 'async_trait>(
        &'life0 mut self,
        request: &'life1 ActionRequest,
    ) -> Pin<Box<dyn Future<Output = Receipt> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move { self.execute_inner(request).await })
    }
}

/// Cancels the helper's `ExecutionControl` when dropped, i.e. when the runner
/// times out or abandons the future.
struct CancelOnDrop(ExecutionControl);

impl CancelOnDrop {
    fn new(deadline: Instant) -> Self {
        Self(ExecutionControl::new(deadline))
    }

    fn control(&self) -> ExecutionControl {
        self.0.clone()
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[cfg(target_os = "windows")]
struct WindowsUiaBridge;

#[cfg(target_os = "windows")]
impl UiaBridge for WindowsUiaBridge {
    fn observe(&mut self, control: &ExecutionControl) -> Result<RawObservation, String> {
        let output = run_powershell(OBSERVE_SCRIPT, None, control)
            .map_err(|error| format!("uia observe helper failed: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "uia observe helper exited with {:?}: {}",
                output.status.code(),
                summarize_stderr(&output.stderr)
            ));
        }
        parse_observation_json(&String::from_utf8_lossy(&output.stdout))
    }

    fn execute(&mut self, action: &UiaAction, control: &ExecutionControl) -> BridgeReceipt {
        let payload = match serde_json::to_string(&WireExecuteAction {
            process_id: action.window.process_id,
            window_id: &action.window.window_id,
            target_id: &action.target_id,
            role: &action.role,
            label: &action.label,
            operation: match action.operation {
                OperationKind::Invoke => "invoke",
                OperationKind::SetValue => "set_value",
            },
            value: action.value.as_deref(),
        }) {
            Ok(payload) => payload,
            Err(error) => {
                return BridgeReceipt::NotApplied(format!("cannot encode request: {error}"))
            }
        };
        if payload.len() > 16 * 1024 {
            return BridgeReceipt::NotApplied("uia execute payload too large".to_string());
        }
        let output = match run_powershell(EXECUTE_SCRIPT, Some(&payload), control) {
            Ok(output) => output,
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => {
                return BridgeReceipt::Uncertain(
                    "uia execute helper timed out; input may have been issued".to_string(),
                )
            }
            Err(error) => {
                // The helper process may already have been running, so its
                // failure does not prove that no input was issued. NotApplied is
                // reserved for errors raised before the helper starts.
                return BridgeReceipt::Uncertain(format!(
                    "uia execute helper failed after start: {error}; input state unknown"
                ));
            }
        };
        if !output.status.success() {
            // The helper exits non-zero only when it could not report at all.
            return BridgeReceipt::Uncertain(format!(
                "uia execute helper exited with {:?}: {}; input state unknown",
                output.status.code(),
                summarize_stderr(&output.stderr)
            ));
        }
        parse_execute_reply(&String::from_utf8_lossy(&output.stdout))
    }
}

/// Maps the execute helper's JSON reply. A malformed reply is treated as
/// uncertain because the helper may have issued input before failing.
pub fn parse_execute_reply(output: &str) -> BridgeReceipt {
    #[derive(Deserialize)]
    struct Reply {
        status: String,
        #[serde(default)]
        detail: String,
    }
    let reply: Reply = match serde_json::from_str(output.trim()) {
        Ok(reply) => reply,
        Err(_) => {
            return BridgeReceipt::Uncertain(
                "uia execute helper returned malformed output".to_string(),
            )
        }
    };
    match reply.status.as_str() {
        "dispatched" => BridgeReceipt::Dispatched,
        "not_applied" => BridgeReceipt::NotApplied(reply.detail),
        "uncertain" => BridgeReceipt::Uncertain(reply.detail),
        other => BridgeReceipt::Uncertain(format!("unknown uia reply status {other:?}")),
    }
}

#[cfg(target_os = "windows")]
#[derive(serde::Serialize)]
struct WireExecuteAction<'a> {
    process_id: u32,
    window_id: &'a str,
    target_id: &'a str,
    role: &'a str,
    label: &'a str,
    operation: &'a str,
    value: Option<&'a str>,
}

#[cfg(target_os = "windows")]
fn powershell_command(script: &str) -> std::process::Command {
    let mut command = std::process::Command::new("powershell.exe");
    command.args([
        "-NoLogo",
        "-NoProfile",
        "-NonInteractive",
        "-Command",
        script,
    ]);
    command
}

#[cfg(target_os = "windows")]
fn run_powershell(
    script: &str,
    request: Option<&str>,
    control: &ExecutionControl,
) -> std::io::Result<std::process::Output> {
    let mut command = powershell_command(script);
    if let Some(request) = request {
        command.env("ALE_UIA_EXECUTE", request);
    }
    crate::platform_worker::bounded_output(&mut command, control)
}

#[cfg(target_os = "windows")]
fn summarize_stderr(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .chars()
        .take(300)
        .collect::<String>()
}

/// Observe script: foreground window identity plus a bounded ControlView tree
/// walk. Node ids are tree paths ("0", "0/2", ...) so the execute helper can
/// re-resolve the exact same element instead of trusting a duplicated
/// AutomationId. Password fields are skipped. Output is one compressed JSON line.
#[cfg(target_os = "windows")]
const OBSERVE_SCRIPT: &str = r#"
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
Add-Type -AssemblyName UIAutomationClient
Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; public static class AleNative { [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow(); }'
$handle = [AleNative]::GetForegroundWindow()
if ($null -eq $handle -or [string]$handle -eq '0') { exit 2 }
$root = [System.Windows.Automation.AutomationElement]::FromHandle($handle)
if ($null -eq $root) { exit 3 }
$window = [ordered]@{
  process_id = [int]$root.Current.ProcessId
  window_id = [string]$handle
  title = [string]$root.Current.Name
}
$walker = [System.Windows.Automation.TreeWalker]::ControlViewWalker
$clock = [Diagnostics.Stopwatch]::StartNew()
$queue = New-Object System.Collections.Generic.Queue[object]
$queue.Enqueue(@($root, '0', 0))
$nodes = New-Object System.Collections.Generic.List[object]
while ($queue.Count -gt 0 -and $nodes.Count -lt 512 -and $clock.ElapsedMilliseconds -lt 2500) {
  $item = $queue.Dequeue()
  $element = $item[0]
  $path = [string]$item[1]
  $depth = [int]$item[2]
  try {
    if ($element.Current.IsPassword) { continue }
    if ($depth -lt 16 -and ($queue.Count + $nodes.Count) -lt 1024) {
      $child = $walker.GetFirstChild($element)
      $index = 0
      while ($null -ne $child -and ($queue.Count + $nodes.Count) -lt 1024 -and $clock.ElapsedMilliseconds -lt 2500) {
        $queue.Enqueue(@($child, ($path + '/' + [string]$index), ($depth + 1)))
        $child = $walker.GetNextSibling($child)
        $index++
      }
    }
    $props = [ordered]@{}
    try {
      $value = $null
      if ($element.TryGetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern, [ref]$value)) {
        $props['value'] = [string]$value.Current.Value
      }
    } catch {}
    try {
      $toggle = $null
      if ($element.TryGetCurrentPattern([System.Windows.Automation.TogglePattern]::Pattern, [ref]$toggle)) {
        $props['toggle'] = [string]$toggle.Current.ToggleState
      }
    } catch {}
    try {
      $selection = $null
      if ($element.TryGetCurrentPattern([System.Windows.Automation.SelectionItemPattern]::Pattern, [ref]$selection)) {
        $props['selected'] = [string]$selection.Current.IsSelected
      }
    } catch {}
    $props['enabled'] = [string]$element.Current.IsEnabled
    $rect = $element.Current.BoundingRectangle
    $nodes.Add([ordered]@{
      id = $path
      role = [string]$element.Current.ControlType.ProgrammaticName
      label = [string]$element.Current.Name
      enabled = [bool]$element.Current.IsEnabled
      properties = $props
      bounds = [ordered]@{ left = $rect.Left; top = $rect.Top; width = $rect.Width; height = $rect.Height }
    })
  } catch {}
}
[ordered]@{ window = $window; nodes = $nodes } | ConvertTo-Json -Depth 6 -Compress
"#;

/// Execute script: re-resolve window + tree path + role/name, re-check enabled,
/// then invoke exactly one UIA pattern. Every failure before the pattern call is
/// `not_applied`; an exception from the pattern call itself is `uncertain`.
#[cfg(target_os = "windows")]
const EXECUTE_SCRIPT: &str = r#"
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
Add-Type -AssemblyName UIAutomationClient
Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; public static class AleNative { [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow(); }'
function Reply([string]$status, [string]$detail) {
  [ordered]@{ status = $status; detail = $detail } | ConvertTo-Json -Compress
  exit 0
}
try {
  $request = $env:ALE_UIA_EXECUTE | ConvertFrom-Json
  $handle = [AleNative]::GetForegroundWindow()
  if ($null -eq $handle -or [string]$handle -eq '0') { Reply 'not_applied' 'no foreground window' }
  if ([string]$handle -ne [string]$request.window_id) { Reply 'not_applied' 'foreground window changed' }
  $root = [System.Windows.Automation.AutomationElement]::FromHandle($handle)
  if ($null -eq $root) { Reply 'not_applied' 'foreground window has no UIA root' }
  if ([string]$root.Current.ProcessId -ne [string]$request.process_id) { Reply 'not_applied' 'foreground process changed' }
  $parts = ([string]$request.target_id).Split('/')
  if ($parts.Count -lt 1 -or $parts[0] -ne '0') { Reply 'not_applied' 'invalid tree path' }
  $walker = [System.Windows.Automation.TreeWalker]::ControlViewWalker
  $node = $root
  for ($level = 1; $level -lt $parts.Count; $level++) {
    $wanted = [int]$parts[$level]
    $child = $walker.GetFirstChild($node)
    $index = 0
    $found = $null
    while ($null -ne $child) {
      if ($index -eq $wanted) { $found = $child; break }
      $child = $walker.GetNextSibling($child)
      $index++
    }
    if ($null -eq $found) { Reply 'not_applied' 'target path no longer resolves' }
    $node = $found
  }
  if ([string]$node.Current.ControlType.ProgrammaticName -ne [string]$request.role) { Reply 'not_applied' 'target role changed' }
  if (([string]$node.Current.Name).Trim() -ne [string]$request.label) { Reply 'not_applied' 'target label changed' }
  if (-not $node.Current.IsEnabled) { Reply 'not_applied' 'target disabled' }
  if ([string]$request.operation -eq 'invoke') {
    $pattern = $null
    if (-not $node.TryGetCurrentPattern([System.Windows.Automation.InvokePattern]::Pattern, [ref]$pattern)) { Reply 'not_applied' 'target has no invoke pattern' }
    try {
      ([System.Windows.Automation.InvokePattern]$pattern).Invoke()
    } catch {
      Reply 'uncertain' ('invoke raised: ' + $_.Exception.Message)
    }
    Reply 'dispatched' ''
  } elseif ([string]$request.operation -eq 'set_value') {
    $pattern = $null
    if (-not $node.TryGetCurrentPattern([System.Windows.Automation.ValuePattern]::Pattern, [ref]$pattern)) { Reply 'not_applied' 'target has no value pattern' }
    try {
      ([System.Windows.Automation.ValuePattern]$pattern).SetValue([string]$request.value)
    } catch {
      Reply 'uncertain' ('set_value raised: ' + $_.Exception.Message)
    }
    Reply 'dispatched' ''
  } else {
    Reply 'not_applied' 'unsupported operation'
  }
} catch {
  Reply 'uncertain' ('helper error: ' + $_.Exception.Message)
}
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    fn raw_window() -> RawWindow {
        RawWindow {
            process_id: 4242,
            window_id: "987654".to_string(),
            title: Some("设置".to_string()),
        }
    }

    fn raw_node(id: &str, role: &str, label: &str) -> RawNode {
        RawNode {
            id: id.to_string(),
            role: role.to_string(),
            label: label.to_string(),
            enabled: true,
            properties: BTreeMap::new(),
            bounds: None,
        }
    }

    fn raw_observation(nodes: Vec<RawNode>) -> RawObservation {
        RawObservation {
            window: raw_window(),
            nodes,
        }
    }

    fn request(status_value: &str) -> ActionRequest {
        ActionRequest {
            id: "a1".to_string(),
            observation: Observation {
                sequence: 1,
                window: WindowIdentity {
                    process_id: 4242,
                    window_id: "987654".to_string(),
                },
                nodes: vec![Node {
                    id: "0/1".to_string(),
                    role: "ControlType.Button".to_string(),
                    label: "保存".to_string(),
                    enabled: true,
                    properties: BTreeMap::new(),
                }],
            },
            step: ale_core::harness::Step {
                target_id: "0/1".to_string(),
                operation: Operation::Invoke,
                expected: vec![ale_core::harness::Predicate {
                    role: "ControlType.Text".to_string(),
                    label: "状态".to_string(),
                    property: "value".to_string(),
                    equals: status_value.to_string(),
                }],
                explanation: "点击保存".to_string(),
            },
        }
    }

    #[derive(Default)]
    struct ScriptedBridgeState {
        observations: VecDeque<RawObservation>,
        receipts: VecDeque<BridgeReceipt>,
        execute_calls: usize,
        executed: Vec<UiaAction>,
    }

    #[derive(Clone, Default)]
    struct ScriptedBridge {
        state: Arc<Mutex<ScriptedBridgeState>>,
    }

    impl ScriptedBridge {
        fn with_observations(observations: Vec<RawObservation>) -> Self {
            let state = ScriptedBridgeState {
                observations: observations.into(),
                ..Default::default()
            };
            Self {
                state: Arc::new(Mutex::new(state)),
            }
        }

        fn push_receipt(&self, receipt: BridgeReceipt) {
            self.state.lock().unwrap().receipts.push_back(receipt);
        }

        fn execute_calls(&self) -> usize {
            self.state.lock().unwrap().execute_calls
        }

        fn executed(&self) -> Vec<UiaAction> {
            self.state.lock().unwrap().executed.clone()
        }
    }

    impl UiaBridge for ScriptedBridge {
        fn observe(&mut self, _control: &ExecutionControl) -> Result<RawObservation, String> {
            self.state
                .lock()
                .unwrap()
                .observations
                .pop_front()
                .ok_or_else(|| "no more mock observations".to_string())
        }

        fn execute(&mut self, action: &UiaAction, _control: &ExecutionControl) -> BridgeReceipt {
            let mut state = self.state.lock().unwrap();
            state.execute_calls += 1;
            state.executed.push(action.clone());
            state
                .receipts
                .pop_front()
                .unwrap_or(BridgeReceipt::Dispatched)
        }
    }

    /// A bridge whose UI actually changes after a save click, used to prove the
    /// whole `harness::run` loop works through this Environment.
    #[derive(Default)]
    struct LiveBridgeState {
        saved: bool,
        observations: usize,
    }

    #[derive(Clone, Default)]
    struct LiveBridge {
        state: Arc<Mutex<LiveBridgeState>>,
        execute_calls: Arc<AtomicUsize>,
    }

    impl LiveBridge {
        fn execute_calls(&self) -> usize {
            self.execute_calls.load(Ordering::Acquire)
        }
    }

    impl UiaBridge for LiveBridge {
        fn observe(&mut self, _control: &ExecutionControl) -> Result<RawObservation, String> {
            let mut state = self.state.lock().unwrap();
            state.observations += 1;
            let mut status = raw_node("0/2", "ControlType.Text", "状态");
            status.properties.insert(
                "value".to_string(),
                if state.saved {
                    "已保存"
                } else {
                    "未保存"
                }
                .to_string(),
            );
            Ok(raw_observation(vec![
                raw_node("0/1", "ControlType.Button", "保存"),
                status,
            ]))
        }

        fn execute(&mut self, action: &UiaAction, _control: &ExecutionControl) -> BridgeReceipt {
            self.execute_calls.fetch_add(1, Ordering::AcqRel);
            if action.target_id == "0/1" && action.operation == OperationKind::Invoke {
                self.state.lock().unwrap().saved = true;
                return BridgeReceipt::Dispatched;
            }
            BridgeReceipt::NotApplied("unexpected action".to_string())
        }
    }

    fn environment(bridge: ScriptedBridge) -> UiaEnvironment {
        UiaEnvironment::new(Box::new(bridge))
    }

    #[test]
    fn observation_json_parses_powershell_shape() {
        let json = r#"{
            "window": {"process_id": 1234, "window_id": "555", "title": "对话框"},
            "nodes": [
                {"id": "0", "role": "ControlType.Window", "label": "对话框", "enabled": true,
                 "properties": {"enabled": "True"}, "bounds": {"left": 1, "top": 2, "width": 3, "height": 4}},
                {"id": "0/1", "role": "ControlType.Edit", "label": "缩放", "enabled": true,
                 "properties": {"value": "125", "toggle": "On", "selected": "False"}}
            ]
        }"#;
        let raw = parse_observation_json(json).expect("valid observation");
        assert_eq!(raw.window.process_id, 1234);
        assert_eq!(raw.window.window_id, "555");
        assert_eq!(raw.nodes.len(), 2);
        assert_eq!(raw.nodes[0].bounds.unwrap().width, 3.0);
        let observation = build_observation(7, raw).expect("observation");
        assert_eq!(observation.sequence, 7);
        assert_eq!(observation.window.window_id, "555");
        assert_eq!(observation.nodes[1].properties["value"], "125");
        assert_eq!(observation.nodes[1].properties["enabled"], "true");
        assert_eq!(observation.nodes[1].properties["toggle"], "On");
    }

    #[test]
    fn observation_json_rejects_malformed_payloads() {
        assert!(parse_observation_json("not json").is_err());
        assert!(parse_observation_json(r#"{"nodes":[]}"#).is_err());
        assert!(parse_observation_json(
            r#"{"window":{"process_id":"not-a-number","window_id":"1"},"nodes":[]}"#
        )
        .is_err());
        assert!(parse_observation_json(
            r#"{"window":{"process_id":1,"window_id":"1"},"nodes":[{"role":"ControlType.Text"}]}"#
        )
        .is_err());
    }

    #[test]
    fn build_observation_enforces_contract_limits() {
        let duplicate = raw_observation(vec![
            raw_node("0/1", "ControlType.Button", "保存"),
            raw_node("0/1", "ControlType.Button", "取消"),
        ]);
        assert!(build_observation(1, duplicate).is_err());

        let no_id = raw_observation(vec![RawNode {
            id: "  ".to_string(),
            ..raw_node("x", "ControlType.Button", "保存")
        }]);
        assert!(build_observation(1, no_id).is_err());

        let empty_window = RawObservation {
            window: RawWindow {
                process_id: 1,
                window_id: "   ".to_string(),
                title: None,
            },
            nodes: vec![],
        };
        assert!(build_observation(1, empty_window).is_err());

        let too_many = raw_observation(
            (0..MAX_OBSERVED_NODES + 1)
                .map(|index| raw_node(&format!("0/{index}"), "ControlType.Button", "x"))
                .collect(),
        );
        assert!(build_observation(1, too_many).is_err());

        // Nodes without any identity are dropped instead of confusing matching.
        let unnamed = raw_observation(vec![RawNode {
            id: "0/5".to_string(),
            role: "  ".to_string(),
            label: "  ".to_string(),
            enabled: true,
            properties: BTreeMap::new(),
            bounds: None,
        }]);
        let observation = build_observation(1, unnamed).unwrap();
        assert!(observation.nodes.is_empty());
    }

    #[tokio::test]
    async fn observe_increments_sequence_and_sets_expected_window() {
        let bridge = ScriptedBridge::with_observations(vec![
            raw_observation(vec![raw_node("0/1", "ControlType.Button", "保存")]),
            raw_observation(vec![raw_node("0/1", "ControlType.Button", "保存")]),
        ]);
        let calls = bridge.clone();
        let mut environment = environment(bridge);
        let first = environment
            .observe_inner()
            .await
            .expect("first observation");
        let second = environment
            .observe_inner()
            .await
            .expect("second observation");
        assert_eq!(first.sequence, 1);
        assert_eq!(second.sequence, 2);
        assert_eq!(
            environment.expected_window(),
            Some(&WindowIdentity {
                process_id: 4242,
                window_id: "987654".to_string(),
            })
        );
        assert_eq!(first.nodes[0].properties["enabled"], "true");
        assert_eq!(calls.execute_calls(), 0);
    }

    #[tokio::test]
    async fn execute_maps_bridge_receipts_exactly() {
        for (bridge_receipt, expected) in [
            (BridgeReceipt::Dispatched, Receipt::Dispatched),
            (
                BridgeReceipt::NotApplied("target moved".to_string()),
                Receipt::NotApplied("target moved".to_string()),
            ),
            (
                BridgeReceipt::Uncertain("maybe".to_string()),
                Receipt::Uncertain("maybe".to_string()),
            ),
        ] {
            let bridge = ScriptedBridge::with_observations(vec![
                raw_observation(vec![
                    raw_node("0/1", "ControlType.Button", "保存"),
                    raw_node("0/2", "ControlType.Text", "状态"),
                ]),
                raw_observation(vec![
                    raw_node("0/1", "ControlType.Button", "保存"),
                    raw_node("0/2", "ControlType.Text", "状态"),
                ]),
            ]);
            let calls = bridge.clone();
            calls.push_receipt(bridge_receipt);
            let mut environment = UiaEnvironment::new(Box::new(bridge));
            environment.observe_inner().await.expect("observation");
            let receipt = environment.execute_inner(&request("已保存")).await;
            assert_eq!(receipt, expected);
            assert_eq!(calls.execute_calls(), 1);
            let action = &calls.executed()[0];
            assert_eq!(action.target_id, "0/1");
            assert_eq!(action.role, "ControlType.Button");
            assert_eq!(action.operation, OperationKind::Invoke);
            assert_eq!(action.window.window_id, "987654");
        }
    }

    #[tokio::test]
    async fn execute_revalidates_before_input() {
        // Target disappeared between approval and dispatch.
        let bridge = ScriptedBridge::with_observations(vec![
            raw_observation(vec![
                raw_node("0/1", "ControlType.Button", "保存"),
                raw_node("0/2", "ControlType.Text", "状态"),
            ]),
            raw_observation(vec![raw_node("0/2", "ControlType.Text", "状态")]),
        ]);
        let calls = bridge.clone();
        let mut environment = UiaEnvironment::new(Box::new(bridge));
        environment.observe_inner().await.unwrap();
        let receipt = environment.execute_inner(&request("已保存")).await;
        assert!(matches!(receipt, Receipt::NotApplied(_)));
        assert_eq!(calls.execute_calls(), 0, "no input must be issued");

        // Target label changed.
        let bridge = ScriptedBridge::with_observations(vec![
            raw_observation(vec![
                raw_node("0/1", "ControlType.Button", "保存"),
                raw_node("0/2", "ControlType.Text", "状态"),
            ]),
            raw_observation(vec![
                raw_node("0/1", "ControlType.Button", "删除"),
                raw_node("0/2", "ControlType.Text", "状态"),
            ]),
        ]);
        let calls = bridge.clone();
        let mut environment = UiaEnvironment::new(Box::new(bridge));
        environment.observe_inner().await.unwrap();
        assert!(matches!(
            environment.execute_inner(&request("已保存")).await,
            Receipt::NotApplied(_)
        ));
        assert_eq!(calls.execute_calls(), 0);

        // Disabled target.
        let mut disabled = raw_node("0/1", "ControlType.Button", "保存");
        disabled.enabled = false;
        let bridge = ScriptedBridge::with_observations(vec![
            raw_observation(vec![
                raw_node("0/1", "ControlType.Button", "保存"),
                raw_node("0/2", "ControlType.Text", "状态"),
            ]),
            raw_observation(vec![disabled, raw_node("0/2", "ControlType.Text", "状态")]),
        ]);
        let calls = bridge.clone();
        let mut environment = UiaEnvironment::new(Box::new(bridge));
        environment.observe_inner().await.unwrap();
        assert!(matches!(
            environment.execute_inner(&request("已保存")).await,
            Receipt::NotApplied(_)
        ));
        assert_eq!(calls.execute_calls(), 0);

        // Foreground window changed.
        let bridge = ScriptedBridge::with_observations(vec![
            raw_observation(vec![raw_node("0/1", "ControlType.Button", "保存")]),
            RawObservation {
                window: RawWindow {
                    process_id: 1,
                    window_id: "111".to_string(),
                    title: None,
                },
                nodes: vec![raw_node("0/1", "ControlType.Button", "保存")],
            },
        ]);
        let calls = bridge.clone();
        let mut environment = UiaEnvironment::new(Box::new(bridge));
        environment.observe_inner().await.unwrap();
        assert!(matches!(
            environment.execute_inner(&request("已保存")).await,
            Receipt::NotApplied(_)
        ));
        assert_eq!(calls.execute_calls(), 0);
    }

    #[tokio::test]
    async fn cancelled_flag_prevents_input() {
        let bridge = ScriptedBridge::with_observations(vec![raw_observation(vec![
            raw_node("0/1", "ControlType.Button", "保存"),
            raw_node("0/2", "ControlType.Text", "状态"),
        ])]);
        let calls = bridge.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut environment = UiaEnvironment::with_cancel_flag(Box::new(bridge), cancelled.clone());
        environment.observe_inner().await.unwrap();
        cancelled.store(true, Ordering::Release);
        assert!(matches!(
            environment.execute_inner(&request("已保存")).await,
            Receipt::NotApplied(_)
        ));
        assert_eq!(calls.execute_calls(), 0);
    }

    #[tokio::test]
    async fn execute_without_a_confirmed_window_never_calls_the_bridge() {
        let bridge = ScriptedBridge::default();
        let calls = bridge.clone();
        let mut environment = UiaEnvironment::new(Box::new(bridge));
        assert!(matches!(
            environment.execute_inner(&request("已保存")).await,
            Receipt::NotApplied(_)
        ));
        assert_eq!(calls.execute_calls(), 0);
    }

    #[test]
    fn execute_reply_parsing_never_reports_false_success() {
        assert_eq!(
            parse_execute_reply(r#"{"status":"dispatched","detail":""}"#),
            BridgeReceipt::Dispatched
        );
        assert_eq!(
            parse_execute_reply(r#"{"status":"not_applied","detail":"disabled"}"#),
            BridgeReceipt::NotApplied("disabled".to_string())
        );
        assert_eq!(
            parse_execute_reply(r#"{"status":"uncertain","detail":"maybe"}"#),
            BridgeReceipt::Uncertain("maybe".to_string())
        );
        assert!(matches!(
            parse_execute_reply("garbage"),
            BridgeReceipt::Uncertain(_)
        ));
        assert!(matches!(
            parse_execute_reply(r#"{"status":"done"}"#),
            BridgeReceipt::Uncertain(_)
        ));
    }

    #[test]
    fn platform_availability_is_explicit() {
        #[cfg(not(target_os = "windows"))]
        {
            assert!(!uia_available());
            assert!(create_platform_bridge().is_none());
            assert!(create_platform_environment().is_none());
        }
        #[cfg(target_os = "windows")]
        {
            assert!(uia_available());
            assert!(create_platform_bridge().is_some());
        }
    }

    #[tokio::test]
    async fn full_harness_run_completes_through_the_uia_environment() {
        use crate::verified_authorizer::ConfirmedTaskAuthorizer;
        use crate::verified_planner::ScriptedPlanner;
        use crate::verified_task::{build_task, parse_verified_task_arguments};
        use ale_core::harness::{run, Status};
        use serde_json::json;
        use tokio::sync::watch;

        let proposal = parse_verified_task_arguments(
            &json!({
                "goal": "保存设置",
                "explanation": "点击保存",
                "risk_level": "low",
                "steps": [{
                    "operation": "invoke",
                    "target": {"role": "ControlType.Button", "label": "保存"},
                    "expected": {"role": "ControlType.Text", "label": "状态", "property": "value", "equals": "已保存"},
                    "summary": "点击保存"
                }]
            })
            .to_string(),
        )
        .expect("proposal");
        let window = WindowIdentity {
            process_id: 4242,
            window_id: "987654".to_string(),
        };
        let task = build_task(&proposal, "保存设置", &window, true).unwrap();
        let bridge = LiveBridge::default();
        let calls = bridge.clone();
        let mut environment = UiaEnvironment::new(Box::new(bridge));
        let mut planner = ScriptedPlanner::new(proposal.steps.clone());
        let mut authorizer = ConfirmedTaskAuthorizer::new(window, &proposal);
        let (_cancel, cancellation) = watch::channel(false);
        let report = run(
            &task,
            &mut planner,
            &mut environment,
            &mut authorizer,
            cancellation,
        )
        .await;
        assert_eq!(report.status, Status::Completed, "{}", report.reason);
        assert_eq!(calls.execute_calls(), 1);
    }
}
