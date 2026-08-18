[CmdletBinding()]
param([string]$ModelsDir)

$ErrorActionPreference = "Stop"
if ($env:OS -ne "Windows_NT") { throw "This acceptance tool requires Windows." }

function Import-VsEnvironment {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
    if (-not (Test-Path -LiteralPath $vswhere)) { throw "vswhere.exe is missing. Run setup-windows-test.bat first." }
    $installation = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if ([string]::IsNullOrWhiteSpace($installation)) { throw "Visual Studio C++ Build Tools are missing." }
    $vsDevCmd = Join-Path $installation.Trim() "Common7\Tools\VsDevCmd.bat"
    & cmd.exe /d /s /c "`"$vsDevCmd`" -no_logo -arch=x64 -host_arch=x64 && set" | ForEach-Object {
        if ($_ -match '^([^=]+)=(.*)$') {
            [Environment]::SetEnvironmentVariable($Matches[1], $Matches[2], "Process")
        }
    }
    if ($null -eq (Get-Command link.exe -ErrorAction SilentlyContinue)) {
        throw "MSVC linker is unavailable after loading VsDevCmd.bat."
    }
}

function Redact-ReportPaths([string]$Directory) {
    $replacements = @($ModelsDir, $repoRoot, $env:USERPROFILE, $env:USERNAME, $env:COMPUTERNAME, $env:USERDOMAIN) |
        Where-Object { -not [string]::IsNullOrWhiteSpace($_) } |
        Sort-Object Length -Descending -Unique
    Get-ChildItem -LiteralPath $Directory -File -Recurse | Where-Object {
        $_.Extension -in @(".json", ".log", ".txt")
    } | ForEach-Object {
        $content = Get-Content -LiteralPath $_.FullName -Raw
        foreach ($value in $replacements) { $content = $content.Replace($value, "<REDACTED>") }
        Set-Content -LiteralPath $_.FullName -Value $content -Encoding UTF8
    }
}

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
if ([string]::IsNullOrWhiteSpace($ModelsDir)) {
    $ModelsDir = Join-Path $repoRoot "models"
} else {
    $ModelsDir = (Resolve-Path $ModelsDir).Path
}
$runtime = Join-Path $ModelsDir ".runtime"
$capabilities = Join-Path $runtime "runtime-capabilities.json"
$fixtures = Join-Path $runtime "fixtures"
if (-not (Test-Path -LiteralPath $capabilities)) {
    throw "Run scripts\model-runtime\run-windows-amd.bat first; runtime capability evidence is missing."
}
if (-not (Test-Path -LiteralPath (Join-Path $fixtures "expected.json"))) {
    throw "Runtime fixtures are missing. Rerun run-windows-amd.bat."
}
$hostLine = (& rustc.exe -vV | Select-String '^host:').Line
if ($hostLine -ne "host: x86_64-pc-windows-msvc") {
    throw "Rust MSVC is required. Run setup-windows-test.bat first. Found: $hostLine"
}
Import-VsEnvironment

& cargo.exe build --release --locked -p ale-cli -p ale-gui -p ale-modeld
if ($LASTEXITCODE -ne 0) { throw "Native Windows release build failed." }

$timestamp = Get-Date -Format "yyyyMMdd-HHmmss"
$reportRoot = Join-Path $repoRoot "target\model-runtime-reports"
$reportDir = Join-Path $reportRoot "modeld-$timestamp"
$reportZip = Join-Path $reportRoot "modeld-acceptance-$timestamp.zip"
New-Item -ItemType Directory -Force -Path $reportDir | Out-Null
$acceptance = Join-Path $repoRoot "target\release\ale-modeld-acceptance.exe"
$modeld = Join-Path $repoRoot "target\release\ale-modeld.exe"
$gui = Join-Path $repoRoot "target\release\ale-gui.exe"
$supervisorReport = Join-Path $reportDir "gui-modeld-supervisor.json"
& $gui --modeld-supervisor-check $ModelsDir $supervisorReport
if ($LASTEXITCODE -ne 0) {
    throw "Desktop modeld supervisor acceptance failed. See $supervisorReport"
}
& $acceptance --modeld $modeld --models-dir $ModelsDir --fixtures-dir $fixtures --report-dir $reportDir
$exitCode = $LASTEXITCODE

[ordered]@{
    captured_at_utc = (Get-Date).ToUniversalTime().ToString("o")
    git_commit = (& git.exe -C $repoRoot rev-parse HEAD)
    rust_host = $hostLine
    windows = (Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, BuildNumber)
    gpus = @(Get-CimInstance Win32_VideoController | ForEach-Object {
        $vendorDevice = $null
        if ($_.PNPDeviceID -match "VEN_([0-9A-F]+)&DEV_([0-9A-F]+)") {
            $vendorDevice = "VEN_$($Matches[1])&DEV_$($Matches[2])"
        }
        [ordered]@{ name = $_.Name; driver_version = $_.DriverVersion; pnp_vendor_device = $vendorDevice }
    })
    rustc = (& rustc.exe -vV)
    cargo = (& cargo.exe --version)
} | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath (Join-Path $reportDir "system.json") -Encoding UTF8

Copy-Item -LiteralPath $capabilities -Destination (Join-Path $reportDir "runtime-capabilities.json") -Force
$runtimeModels = Join-Path $runtime "runtime-models.json"
if (Test-Path -LiteralPath $runtimeModels) {
    Copy-Item -LiteralPath $runtimeModels -Destination (Join-Path $reportDir "runtime-models.json") -Force
}
Redact-ReportPaths $reportDir
Compress-Archive -Force -Path (Join-Path $reportDir "*") -DestinationPath $reportZip
Write-Host "modeld report: $reportZip"
exit $exitCode
