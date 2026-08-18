[CmdletBinding()]
param(
    [switch]$Install
)

$ErrorActionPreference = "Stop"

function Test-Command([string]$Name) {
    return $null -ne (Get-Command $Name -ErrorAction SilentlyContinue)
}

function Get-VsInstallation([string]$VsWhere) {
    if (-not (Test-Path -LiteralPath $VsWhere)) { return $null }
    $path = & $VsWhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if ([string]::IsNullOrWhiteSpace($path)) { return $null }
    return $path.Trim()
}

function Test-WindowsSdk {
    $kitsRoot = (Get-ItemProperty "HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots" -Name KitsRoot10 -ErrorAction SilentlyContinue).KitsRoot10
    if ([string]::IsNullOrWhiteSpace($kitsRoot)) { return $false }
    $includeRoot = Join-Path $kitsRoot "Include"
    return $null -ne (Get-ChildItem -LiteralPath $includeRoot -Directory -ErrorAction SilentlyContinue |
        Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName "um\Windows.h") } |
        Select-Object -First 1)
}

function Import-VsEnvironment([string]$Installation) {
    $vsDevCmd = Join-Path $Installation "Common7\Tools\VsDevCmd.bat"
    if (-not (Test-Path -LiteralPath $vsDevCmd)) { throw "VsDevCmd.bat is missing: $vsDevCmd" }
    & cmd.exe /d /s /c "`"$vsDevCmd`" -no_logo -arch=x64 -host_arch=x64 && set" | ForEach-Object {
        if ($_ -match '^([^=]+)=(.*)$') {
            [Environment]::SetEnvironmentVariable($Matches[1], $Matches[2], "Process")
        }
    }
}

if ($env:OS -ne "Windows_NT") {
    throw "This setup tool must run on Windows."
}

$missing = New-Object System.Collections.Generic.List[string]
if (-not (Test-Command "rustup.exe")) { $missing.Add("Rust stable MSVC") }
if (-not (Test-Command "cargo.exe")) { $missing.Add("Cargo") }

$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
$installation = Get-VsInstallation $vswhere
$hasCpp = -not [string]::IsNullOrWhiteSpace($installation)
$hasSdk = Test-WindowsSdk
if (-not $hasCpp) { $missing.Add("Visual Studio 2022 C++ Build Tools") }
if (-not $hasSdk) { $missing.Add("Windows 10/11 SDK") }

if ($missing.Count -gt 0 -and -not $Install) {
    Write-Host "Missing Windows build prerequisites:" -ForegroundColor Yellow
    $missing | ForEach-Object { Write-Host "  - $_" }
    Write-Host ""
    Write-Host "Rerun with installation enabled:" -ForegroundColor Cyan
    Write-Host "  powershell -ExecutionPolicy Bypass -File scripts\model-runtime\setup-windows-test.ps1 -Install"
    exit 2
}

if ($missing.Count -gt 0) {
    if (-not (Test-Command "winget.exe")) {
        throw "winget is required for unattended prerequisite installation."
    }
    if (-not $hasCpp -or -not $hasSdk) {
        & winget.exe install --id Microsoft.VisualStudio.2022.BuildTools --exact `
            --accept-package-agreements --accept-source-agreements --silent `
            --override "--wait --passive --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended"
        if ($LASTEXITCODE -ne 0) { throw "Visual Studio Build Tools installation failed: $LASTEXITCODE" }
    }
    if (-not (Test-Command "rustup.exe")) {
        & winget.exe install --id Rustlang.Rustup --exact --accept-package-agreements --accept-source-agreements --silent
        if ($LASTEXITCODE -ne 0) { throw "rustup installation failed: $LASTEXITCODE" }
        $cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
        if (Test-Path -LiteralPath $cargoBin) { $env:Path = "$cargoBin;$env:Path" }
    }
}

$installation = Get-VsInstallation $vswhere
if ([string]::IsNullOrWhiteSpace($installation)) { throw "Visual Studio C++ Build Tools were not detected after setup." }
if (-not (Test-WindowsSdk)) { throw "Windows SDK was not detected after setup; a reboot or Build Tools repair may be required." }
Import-VsEnvironment $installation
if (-not (Test-Command "cl.exe") -or -not (Test-Command "link.exe")) {
    throw "MSVC compiler/linker are unavailable after loading VsDevCmd.bat."
}

& rustup.exe toolchain install stable-x86_64-pc-windows-msvc --profile minimal
if ($LASTEXITCODE -ne 0) { throw "Rust MSVC toolchain installation failed: $LASTEXITCODE" }
& rustup.exe default stable-x86_64-pc-windows-msvc
if ($LASTEXITCODE -ne 0) { throw "Rust MSVC toolchain selection failed: $LASTEXITCODE" }

$hostLine = (& rustc.exe -vV | Select-String '^host:').Line
if ($hostLine -ne "host: x86_64-pc-windows-msvc") {
    throw "Unexpected Rust host after setup: $hostLine"
}

Write-Host "Windows MSVC test environment is ready." -ForegroundColor Green
Write-Host "Build command: cargo build --release --locked -p ale-cli -p ale-gui -p ale-modeld"
