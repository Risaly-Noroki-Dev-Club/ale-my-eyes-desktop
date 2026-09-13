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
    $cliOutput = & (Join-Path $package 'ale-cli.exe') status 2>&1
    if ($LASTEXITCODE -ne 0) { throw "CLI failed: $cliOutput" }
    $gui = Start-Process -FilePath (Join-Path $package 'ale-gui.exe') -WorkingDirectory $package -ArgumentList @('--ui-acceptance', ('"' + $report + '"'), $DurationSeconds) -PassThru
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
    } finally {
        if (-not $gui.HasExited) { Stop-Process -Id $gui.Id -Force; $gui.WaitForExit() }
}
if ($FaultEvidence) {
    foreach ($fault in @('busy','lock','panic','access-violation')) {
        $faultRoot = Join-Path $smokeRoot "fault-$fault"
        New-Item -ItemType Directory -Force -Path $faultRoot | Out-Null
        $env:ALE_DIAGNOSTICS_DIRECTORY = $faultRoot
        $faultProcess = Start-Process -FilePath (Join-Path $package 'ale-gui.exe') -WorkingDirectory $package -ArgumentList @('--ui-fault',$fault) -PassThru
        $faultStart = Get-Date
        while (-not $faultProcess.HasExited -and ((Get-Date)-$faultStart).TotalSeconds -lt 35) { Start-Sleep -Milliseconds 250; $faultProcess.Refresh() }
        if (-not $faultProcess.HasExited) { Stop-Process -Id $faultProcess.Id -Force; $faultProcess.WaitForExit() }
        Start-Sleep -Seconds 2
        $snapshots = @(Get-ChildItem $faultRoot -Filter 'ale-diagnostic-*.json' -ErrorAction SilentlyContinue)
        $events = @(Get-ChildItem $faultRoot -Filter 'ale-events-*.jsonl' -ErrorAction SilentlyContinue)
        $dumps = @(Get-ChildItem $faultRoot -Filter 'ale-hang-*.dmp' -ErrorAction SilentlyContinue)
        if ($fault -eq 'panic' -or $fault -eq 'access-violation') {
            if ($dumps.Count -gt 1) { throw "More than one dump for one $fault incident" }
        } elseif ($dumps.Count -ne 1) { throw "Expected one local dump for $fault; found $($dumps.Count)" }
        if ($events.Count -eq 0 -or $snapshots.Count -eq 0) { throw "Independent evidence missing for $fault" }
        $faultEvidence = [ordered]@{ fault=$fault; snapshots=$snapshots.Count; events=$events.Count; dumps=$dumps.Count; dump_bytes=if($dumps.Count){$dumps[0].Length}else{0}; local_only=$true }
        $faultEvidence | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $reportRoot "fault-$fault.json") -Encoding utf8
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
