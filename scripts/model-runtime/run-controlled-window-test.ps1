[CmdletBinding()]
param([switch]$Execute)

$ErrorActionPreference = "Stop"
if ($env:OS -ne "Windows_NT") { throw "This controlled test requires Windows." }

Add-Type -AssemblyName UIAutomationClient
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class AleControlledInput {
  [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr hwnd);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
  [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr hwnd);
  [DllImport("user32.dll")] public static extern int GetSystemMetrics(int index);
  [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
}
'@

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
$surfaceScript = Join-Path $PSScriptRoot "windows-test-surface.ps1"
$reportRoot = Join-Path $repoRoot "target\model-runtime-reports"
New-Item -ItemType Directory -Force -Path $reportRoot | Out-Null
$timestamp = Get-Date -Format "yyyyMMdd-HHmmss"
$reportPath = Join-Path $reportRoot "controlled-window-$timestamp.json"
$surface = Start-Process powershell.exe -PassThru -ArgumentList @(
    "-NoLogo", "-NoProfile", "-ExecutionPolicy", "Bypass", "-File", ('"' + $surfaceScript + '"')
)

$report = [ordered]@{
    schema_version = 1
    mode = $(if ($Execute) { "controlled_input" } else { "dry_run" })
    window_found = $false
    target_found = $false
    foreground_verified = $false
    click_executed = $false
    postcondition_observed = $false
    target_bounds = $null
    dpi = $null
    virtual_desktop = $null
    git_commit = (& git.exe -C $repoRoot rev-parse HEAD 2>$null)
    error = $null
}

try {
    $window = $null
    $deadline = (Get-Date).AddSeconds(15)
    $windowCondition = New-Object System.Windows.Automation.PropertyCondition(
        [System.Windows.Automation.AutomationElement]::NameProperty,
        "ALE MODEL RUNTIME CONTROLLED TEST"
    )
    while ($null -eq $window -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 200
        $window = [System.Windows.Automation.AutomationElement]::RootElement.FindFirst(
            [System.Windows.Automation.TreeScope]::Children,
            $windowCondition
        )
    }
    if ($null -eq $window) { throw "controlled test window was not found" }
    $report.window_found = $true
    if ($window.Current.ProcessId -ne $surface.Id) {
        throw "controlled window process ID does not match the launched fixture"
    }
    $hwnd = [IntPtr]$window.Current.NativeWindowHandle
    $report.dpi = [AleControlledInput]::GetDpiForWindow($hwnd)
    $report.virtual_desktop = @(
        [AleControlledInput]::GetSystemMetrics(76),
        [AleControlledInput]::GetSystemMetrics(77),
        [AleControlledInput]::GetSystemMetrics(78),
        [AleControlledInput]::GetSystemMetrics(79)
    )

    $targetCondition = New-Object System.Windows.Automation.PropertyCondition(
        [System.Windows.Automation.AutomationElement]::NameProperty,
        "SAVE button inside Settings dialog"
    )
    $target = $window.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $targetCondition)
    if ($null -eq $target) { throw "allowlisted SAVE target was not found through UI Automation" }
    $report.target_found = $true
    $rect = $target.Current.BoundingRectangle
    if ($rect.Width -le 0 -or $rect.Height -le 0) { throw "target has invalid UIA bounds" }
    $report.target_bounds = @($rect.Left, $rect.Top, $rect.Right, $rect.Bottom)

    if ($Execute) {
        $confirmation = Read-Host "Type YES to click only the allowlisted controlled test button"
        if ($confirmation -cne "YES") { throw "controlled input was not confirmed" }
        [void][AleControlledInput]::SetForegroundWindow($hwnd)
        Start-Sleep -Milliseconds 250
        if ([AleControlledInput]::GetForegroundWindow() -ne $hwnd) {
            throw "controlled test window is not foreground"
        }
        $report.foreground_verified = $true
        $x = [int][Math]::Round($rect.Left + $rect.Width / 2)
        $y = [int][Math]::Round($rect.Top + $rect.Height / 2)
        if (-not [AleControlledInput]::SetCursorPos($x, $y)) { throw "SetCursorPos failed" }
        [AleControlledInput]::mouse_event(0x0002, 0, 0, 0, [UIntPtr]::Zero)
        [AleControlledInput]::mouse_event(0x0004, 0, 0, 0, [UIntPtr]::Zero)
        $report.click_executed = $true
        Start-Sleep -Milliseconds 500
        $savedCondition = New-Object System.Windows.Automation.PropertyCondition(
            [System.Windows.Automation.AutomationElement]::NameProperty,
            "SAVED"
        )
        $saved = $window.FindFirst([System.Windows.Automation.TreeScope]::Descendants, $savedCondition)
        $report.postcondition_observed = $null -ne $saved
        if (-not $report.postcondition_observed) { throw "SAVED postcondition was not observed" }
    }
} catch {
    $report.error = $_.Exception.Message
} finally {
    $report | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $reportPath -Encoding UTF8
    if (-not $surface.HasExited) { Stop-Process -Id $surface.Id -Force }
}

Write-Host "Controlled test report: $reportPath"
if ($null -ne $report.error) {
    Write-Host "FAIL: $($report.error)" -ForegroundColor Red
    exit 2
}
Write-Host "PASS: $($report.mode)" -ForegroundColor Green
