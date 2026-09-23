param(
    [ValidateRange(120,86400)][int]$DurationSeconds = 120,
    [string]$PackageName = 'ale-my-eyes-windows',
    [ValidateSet('default','software','opengl')][string]$Renderer = 'default',
    [switch]$FaultEvidence
)
$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$archive = Join-Path $repoRoot "$PackageName.zip"
$tempBase = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }
$smokeRoot = Join-Path $tempBase ("ale-windows-smoke-" + [guid]::NewGuid())
$extractRoot = Join-Path $smokeRoot 'extract'
$profileRoot = Join-Path $smokeRoot 'profile'
$logRoot = Join-Path $smokeRoot 'logs'
$reportRoot = Join-Path $repoRoot 'target/windows-acceptance'
$report = Join-Path $smokeRoot 'ui-acceptance.json'
$oldAppData = $env:APPDATA
$oldLocalAppData = $env:LOCALAPPDATA
$oldBackend = $env:SLINT_BACKEND
$oldDiagnostics = $env:ALE_DIAGNOSTICS_DIRECTORY
$oldCredentialNamespace = $env:ALE_TEST_CREDENTIAL_NAMESPACE
New-Item -ItemType Directory -Force -Path $extractRoot,$profileRoot,$logRoot,$reportRoot | Out-Null
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class AleWindowProbe {
 [DllImport("user32.dll", SetLastError=true)] public static extern IntPtr SendMessageTimeout(IntPtr hwnd, uint msg, UIntPtr w, IntPtr l, uint flags, uint timeout, out UIntPtr result);
 [DllImport("user32.dll")] public static extern bool SetWindowPos(IntPtr hwnd, IntPtr after, int x, int y, int cx, int cy, uint flags);
}
'@
function Read-DiagnosticEvents([string]$Directory) {
    foreach ($file in @(Get-ChildItem -LiteralPath $Directory -Filter 'ale-events-*.jsonl' -ErrorAction SilentlyContinue)) {
        foreach ($line in @(Get-Content -LiteralPath $file.FullName -ErrorAction SilentlyContinue)) {
            # A writer may still be appending the final line. Retry it on the
            # next poll; an incomplete line must not abort evidence collection.
            if (-not [string]::IsNullOrWhiteSpace($line)) {
                try { $line | ConvertFrom-Json -ErrorAction Stop } catch { }
            }
        }
    }
}
function Read-TargetSnapshots([string]$Directory, [int]$TargetPid, [string]$Session, [long]$NotBeforeUnixMs = 0) {
    Get-ChildItem -LiteralPath $Directory -Filter 'ale-diagnostic-*.json' -ErrorAction SilentlyContinue | ForEach-Object {
        try { Get-Content -LiteralPath $_.FullName -Raw | ConvertFrom-Json -ErrorAction Stop } catch { }
    } | Where-Object {
        $_.pid -eq $TargetPid -and $_.session -eq $Session -and $_.independent_windows_monitor -and
        $_.timestamp_unix_ms -ge $NotBeforeUnixMs
    }
}
function Wait-DiagnosticCapture([string]$Directory, [int]$TargetPid, [int]$TimeoutSeconds) {
    $deadline = [System.Diagnostics.Stopwatch]::StartNew()
    $session = $null
    $terminalStatus = $null
    do {
        $records = @(Read-DiagnosticEvents -Directory $Directory)
        if (-not $session) {
            $start = $records | Where-Object {
                ($_.pid -eq $TargetPid -and $_.component -eq 'gui' -and $_.event -eq 'process_start') -or
                ($_.component -eq 'diagnostic-helper' -and $_.event -eq 'monitor_started' -and $_.fields.target_pid -eq $TargetPid)
            } | Select-Object -First 1
            if ($start) { $session = $start.session }
        }
        if ($session) {
            $terminal = $records | Where-Object {
                $_.session -eq $session -and $_.component -eq 'diagnostic-helper' -and
                $_.event -in @('dump_completed', 'dump_failed', 'dump_timeout', 'dump_spawn_failed')
            } | Sort-Object event_sequence | Select-Object -Last 1
            if ($terminal) {
                $terminalStatus = $terminal.event
                $incident = $records | Where-Object {
                    $_.session -eq $session -and $_.component -eq 'diagnostic-helper' -and
                    $_.event -in @('ui_hang_detected', 'crash_notification')
                } | Sort-Object event_sequence | Select-Object -Last 1
                $notBefore = if ($incident) { [long]$incident.timestamp_unix_ms } else { [long]0 }
                # The snapshot writer is independent of the capture worker. A
                # completed dump does not imply its incident snapshot is on disk.
                $snapshotReady = @(Read-TargetSnapshots -Directory $Directory -TargetPid $TargetPid -Session $session -NotBeforeUnixMs $notBefore).Count -gt 0
                if ($terminalStatus -ne 'dump_completed' -or $snapshotReady) {
                    return [pscustomobject]@{ status=$terminalStatus; session=$session; waited_seconds=$deadline.Elapsed.TotalSeconds; incident_unix_ms=$notBefore }
                }
            }
        }
        Start-Sleep -Milliseconds 250
    } while ($deadline.Elapsed.TotalSeconds -lt $TimeoutSeconds)
    $status = if ($terminalStatus -eq 'dump_completed') { 'snapshot_timeout' } else { 'evidence_timeout' }
    return [pscustomobject]@{ status=$status; session=$session; waited_seconds=$deadline.Elapsed.TotalSeconds }
}
try {
    Expand-Archive -LiteralPath $archive -DestinationPath $extractRoot
    $package = Join-Path $extractRoot $PackageName
    foreach ($name in @('ale-gui.exe','ale-cli.exe','ale-modeld.exe','start-gui.bat','DIAGNOSTICS.md','build-manifest.json')) {
        if (-not (Test-Path -LiteralPath (Join-Path $package $name) -PathType Leaf)) { throw "Missing package file: $name" }
    }
    if ((Test-Path (Join-Path $package 'config')) -or (Test-Path (Join-Path $package 'config.json'))) { throw 'Package contains runtime configuration' }
    $manifest = Get-Content (Join-Path $package 'build-manifest.json') -Raw | ConvertFrom-Json
    foreach ($file in $manifest.files.PSObject.Properties) {
        $hash = (Get-FileHash (Join-Path $package $file.Name) -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($hash -ne $file.Value.sha256) { throw "Binary/manifest mismatch: $($file.Name)" }
    }
    $env:APPDATA = $profileRoot
    $env:LOCALAPPDATA = $profileRoot
    $env:ALE_DIAGNOSTICS_DIRECTORY = $logRoot
    $env:ALE_TEST_CREDENTIAL_NAMESPACE = [guid]::NewGuid().ToString()
    $env:SLINT_BACKEND = switch ($Renderer) { 'software' {'winit-software'} 'opengl' {'winit-femtovg'} default {$null} }
    # --help verifies executable startup without loading or saving user config.
    $cliOutput = & (Join-Path $package 'ale-cli.exe') --help 2>&1
    if ($LASTEXITCODE -ne 0) { throw "CLI help startup failed: $cliOutput" }
    $uiProfile = Join-Path $smokeRoot 'ui-profile'
    $gui = Start-Process -FilePath (Join-Path $package 'ale-gui.exe') -WorkingDirectory $package -ArgumentList @('--ui-acceptance-profile', ('"' + $uiProfile + '"'), '--ui-acceptance', ('"' + $report + '"'), $DurationSeconds) -PassThru
    $started = Get-Date
    $responsive = 0
    $failures = 0
    $moved = $false
    try {
        while (-not $gui.HasExited) {
            Start-Sleep -Seconds 1
            $gui.Refresh()
            $elapsed = ((Get-Date) - $started).TotalSeconds
            if ($elapsed -gt ($DurationSeconds + 60)) { throw 'GUI failed to exit normally after acceptance interval' }
            if ($gui.MainWindowHandle -ne [IntPtr]::Zero) {
                [UIntPtr]$reply = [UIntPtr]::Zero
                $ok = [AleWindowProbe]::SendMessageTimeout($gui.MainWindowHandle, 0, [UIntPtr]::Zero, [IntPtr]::Zero, 0x22, 500, [ref]$reply)
                if ($ok -eq [IntPtr]::Zero) { $failures++ } else { $responsive++; $failures=0 }
                if ($failures -ge 3) { throw 'GUI stopped answering native window messages' }
                if (-not $moved -and $elapsed -ge 30) { $moved = [AleWindowProbe]::SetWindowPos($gui.MainWindowHandle,[IntPtr]::Zero,80,80,1000,760,0x14); if (-not $moved) { throw 'Native window movement failed' } }
            }
        }
        if ($gui.ExitCode -ne 0) { throw "GUI exit code: $($gui.ExitCode)" }
        if (-not (Test-Path $report)) { throw 'GUI did not produce an acceptance report' }
        $ui = Get-Content $report -Raw | ConvertFrom-Json
        if (-not $ui.passed -or $ui.duration_seconds -lt $DurationSeconds -or $responsive -lt ($DurationSeconds-15)) { throw "GUI acceptance failed: $(Get-Content $report -Raw)" }
        $snapshots = @(Get-ChildItem $logRoot -Filter 'ale-diagnostic-*.json' | ForEach-Object { Get-Content $_.FullName -Raw | ConvertFrom-Json } | Where-Object { $_.pid -eq $gui.Id })
        if ($snapshots.Count -lt 2) { throw 'Independent minute snapshots missing' }
        if (@($snapshots | Where-Object { $_.ui_unresponsive }).Count -gt 0) { throw 'Independent monitor reported UI unresponsive' }
        $result = [ordered]@{ passed=$true; target=$manifest.target; duration_seconds=$DurationSeconds; renderer=$Renderer; moved=$moved; build_id=$manifest.build_id; native_responses=$responsive; ui=$ui; binary_hash=$manifest.files.'ale-gui.exe'.sha256 }
        $result | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $reportRoot "ui-$Renderer-$DurationSeconds.json") -Encoding utf8
        Write-Host 'Native window response, countdown, navigation, movement and normal exit passed.'
    } catch {
        $acceptanceError = $_
        try {
            $gui.Refresh()
            if (-not $gui.HasExited) {
                # Three failed window probes can precede the independent monitor's
                # five-second hang threshold. Leave time for its 15-second capture
                # budget before finally terminating the failed acceptance target.
                $capture = Wait-DiagnosticCapture -Directory $logRoot -TargetPid $gui.Id -TimeoutSeconds 30
                Write-Warning "GUI acceptance failed; diagnostic capture status: $($capture.status)"
            }
        } catch {
            Write-Warning "Could not collect diagnostic capture status: $($_.Exception.Message)"
        }
        throw $acceptanceError
    } finally {
        if (-not $gui.HasExited) {
            Stop-Process -Id $gui.Id -Force
            if (-not $gui.WaitForExit(10000)) { Write-Warning 'Failed GUI did not exit within the cleanup deadline' }
        }
}
if ($FaultEvidence) {
    foreach ($fault in @('busy','lock','panic','access-violation')) {
        $faultRoot = Join-Path $smokeRoot "fault-$fault"
        New-Item -ItemType Directory -Force -Path $faultRoot | Out-Null
        $env:ALE_DIAGNOSTICS_DIRECTORY = $faultRoot
        $faultProfile = Join-Path $faultRoot 'profile'
        $faultProcess = Start-Process -FilePath (Join-Path $package 'ale-gui.exe') -WorkingDirectory $package -ArgumentList @('--ui-acceptance-profile', ('"' + $faultProfile + '"'), '--ui-fault',$fault) -PassThru
        try {
            # This budget includes startup and the fault's five-second timer.
            # Keep waiting after a crash exits: its capture helper may still be
            # writing the snapshot. A terminal event replaces arbitrary sleeps.
            $capture = Wait-DiagnosticCapture -Directory $faultRoot -TargetPid $faultProcess.Id -TimeoutSeconds 45
            if ($capture.status -ne 'dump_completed') { throw "Capture failed for ${fault}: $($capture.status)" }
            $session = $capture.session
            $snapshots = @(Read-TargetSnapshots -Directory $faultRoot -TargetPid $faultProcess.Id -Session $session -NotBeforeUnixMs $capture.incident_unix_ms)
            $events = @(Read-DiagnosticEvents -Directory $faultRoot | Where-Object { $_.session -eq $session })
            $crash = $fault -in @('panic', 'access-violation')
            $expectedCrashKind = if ($fault -eq 'panic') { 2 } else { 1 }
            $prefix = if ($crash) { 'ale-crash' } else { 'ale-hang' }
            $dumps = @(Get-ChildItem -LiteralPath $faultRoot -Filter "$prefix-*-$session-$($faultProcess.Id).dmp" -ErrorAction SilentlyContinue)
            if ($dumps.Count -ne 1 -or $dumps[0].Length -le 0) { throw "Expected one nonempty local dump for $fault; found $($dumps.Count)" }
            $sidecar = Get-Content -LiteralPath ([System.IO.Path]::ChangeExtension($dumps[0].FullName, 'json')) -Raw | ConvertFrom-Json
            if ($sidecar.pid -ne $faultProcess.Id -or $sidecar.dump_bytes -ne $dumps[0].Length) { throw "Dump identity or size mismatch for $fault" }
            $incident = @($events | Where-Object {
                $_.component -eq 'diagnostic-helper' -and
                (($crash -and $_.event -eq 'crash_notification' -and $_.fields.kind -eq $expectedCrashKind) -or
                 (-not $crash -and $_.event -eq 'ui_hang_detected'))
            })
            if ($incident.Count -eq 0 -or $snapshots.Count -eq 0) { throw "Independent incident evidence missing for $fault" }
            $faultEvidence = [ordered]@{ fault=$fault; pid=$faultProcess.Id; session=$session; capture_status=$capture.status; snapshots=$snapshots.Count; events=$events.Count; dumps=$dumps.Count; dump_bytes=$dumps[0].Length; local_only=$true }
            $faultEvidence | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $reportRoot "fault-$fault.json") -Encoding utf8
        } finally {
            $faultProcess.Refresh()
            if (-not $faultProcess.HasExited) {
                Stop-Process -Id $faultProcess.Id -Force
                if (-not $faultProcess.WaitForExit(10000)) { throw "Fault target did not exit within the cleanup deadline: $fault" }
            }
        }
    }
}
} finally {
    $env:APPDATA=$oldAppData
    $env:LOCALAPPDATA=$oldLocalAppData
    $env:SLINT_BACKEND=$oldBackend
    $env:ALE_DIAGNOSTICS_DIRECTORY=$oldDiagnostics
    $env:ALE_TEST_CREDENTIAL_NAMESPACE=$oldCredentialNamespace
    # Preserve failure diagnostics locally; never attach dumps to CI uploads.
    Write-Host "Local acceptance evidence: $smokeRoot"
}
