[CmdletBinding()]
param(
    [switch]$InstallMissing,
    [switch]$SkipInstall,
    [switch]$Package,
    [switch]$BuildOnly,
    [string]$VsInstallerPath,
    [string]$RustupInstallerPath,
    [string]$LlvmInstallerPath
)

$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$repoRoot = Split-Path -Parent $PSScriptRoot
$releaseDir = Join-Path $repoRoot 'target\release'
$target = 'x86_64-pc-windows-msvc'

function Test-Command([string]$Name) {
    return $null -ne (Get-Command $Name -ErrorAction SilentlyContinue)
}

function Get-VsInstallation {
    $vswhere = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer\vswhere.exe'
    if (-not (Test-Path -LiteralPath $vswhere)) { return $null }
    $path = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
    if ([string]::IsNullOrWhiteSpace($path)) { return $null }
    return $path.Trim()
}

function Test-WindowsSdk {
    $kitsRoot = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots' -Name KitsRoot10 -ErrorAction SilentlyContinue).KitsRoot10
    if ([string]::IsNullOrWhiteSpace($kitsRoot)) { return $false }
    $includeRoot = Join-Path $kitsRoot 'Include'
    return $null -ne (Get-ChildItem -LiteralPath $includeRoot -Directory -ErrorAction SilentlyContinue |
        Where-Object { Test-Path -LiteralPath (Join-Path $_.FullName 'um\Windows.h') } |
        Select-Object -First 1)
}

function Find-LibclangDirectory {
    $candidates = @($env:LIBCLANG_PATH, (Join-Path ${env:ProgramFiles} 'LLVM\bin'),
        (Join-Path ${env:ProgramFiles(x86)} 'LLVM\bin')) |
        Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    foreach ($candidate in $candidates) {
        if (Test-Path -LiteralPath $candidate -PathType Leaf) { $candidate = Split-Path -Parent $candidate }
        if ((Test-Path -LiteralPath (Join-Path $candidate 'libclang.dll')) -or
            (Test-Path -LiteralPath (Join-Path $candidate 'clang.dll'))) {
            return $candidate
        }
    }
    return $null
}

function Get-MissingPrerequisites {
    $missing = @()
    if (-not (Test-Command 'rustup.exe') -or -not (Test-Command 'cargo.exe')) { $missing += 'Rust' }
    if (-not (Get-VsInstallation) -or -not (Test-WindowsSdk)) { $missing += 'Visual Studio 2022 C++ Build Tools and Windows SDK' }
    if (-not (Find-LibclangDirectory)) { $missing += 'LLVM libclang' }
    return $missing
}

function Get-Installer([string]$Path, [string]$Url, [string]$Name) {
    if ($Path) {
        if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) { throw "Installer not found: $Path" }
        return (Resolve-Path -LiteralPath $Path).Path
    }
    $destination = Join-Path $env:TEMP $Name
    Write-Host "Downloading $Url"
    Invoke-WebRequest -Uri $Url -OutFile $destination -UseBasicParsing
    return $destination
}

function Install-Prerequisites([string[]]$Missing) {
    if ($Missing -contains 'Visual Studio 2022 C++ Build Tools and Windows SDK') {
        $installer = Get-Installer $VsInstallerPath 'https://aka.ms/vs/17/release/vs_BuildTools.exe' 'ale-vs_BuildTools.exe'
        Write-Host 'Installing the C++ desktop workload and Windows SDK...'
        $process = Start-Process -FilePath $installer -ArgumentList @('--wait', '--passive', '--norestart',
            '--add', 'Microsoft.VisualStudio.Workload.VCTools', '--includeRecommended') -Wait -PassThru
        if ($process.ExitCode -notin @(0, 3010)) { throw "Visual Studio installer failed: $($process.ExitCode)" }
        if ($process.ExitCode -eq 3010) { throw 'Visual Studio requested a restart. Restart Windows, then rerun build-windows.cmd.' }
    }
    if ($Missing -contains 'Rust') {
        $installer = Get-Installer $RustupInstallerPath 'https://win.rustup.rs/x86_64' 'ale-rustup-init.exe'
        $process = Start-Process -FilePath $installer -ArgumentList @('-y', '--default-host', $target,
            '--profile', 'minimal') -Wait -PassThru
        if ($process.ExitCode -ne 0) { throw "Rust installer failed: $($process.ExitCode)" }
        $env:Path = "$(Join-Path $env:USERPROFILE '.cargo\bin');$env:Path"
    }
    if ($Missing -contains 'LLVM libclang') {
        if ($LlvmInstallerPath) {
            $installer = Get-Installer $LlvmInstallerPath '' 'ale-llvm.exe'
        } else {
            $release = Invoke-RestMethod -Uri 'https://api.github.com/repos/llvm/llvm-project/releases/latest' -Headers @{ 'User-Agent' = 'Ale-Windows-Builder' }
            $asset = $release.assets | Where-Object { $_.name -match '^LLVM-[0-9.]+-win64\.(msi|exe)$' } | Select-Object -First 1
            if (-not $asset) { throw 'LLVM Windows x64 installer not found in the latest official release. Pass -LlvmInstallerPath.' }
            $installer = Get-Installer '' $asset.browser_download_url $asset.name
        }
        if ([System.IO.Path]::GetExtension($installer) -eq '.msi') {
            $process = Start-Process -FilePath 'msiexec.exe' -ArgumentList @('/i', "`"$installer`"", '/passive', '/norestart') -Wait -PassThru
        } else {
            Write-Host 'Complete the LLVM installer, keeping the default installation directory.'
            $process = Start-Process -FilePath $installer -Wait -PassThru
        }
        if ($process.ExitCode -eq 3010) { throw 'LLVM requested a restart. Restart Windows, then rerun build-windows.cmd.' }
        if ($process.ExitCode -ne 0) { throw "LLVM installer failed: $($process.ExitCode)" }
    }
}

function Import-VsEnvironment([string]$Installation) {
    $vsDevCmd = Join-Path $Installation 'Common7\Tools\VsDevCmd.bat'
    if (-not (Test-Path -LiteralPath $vsDevCmd)) { throw "VsDevCmd.bat not found: $vsDevCmd" }
    & cmd.exe /d /s /c "`"$vsDevCmd`" -no_logo -arch=x64 -host_arch=x64 && set" | ForEach-Object {
        if ($_ -match '^([^=]+)=(.*)$') {
            [Environment]::SetEnvironmentVariable($Matches[1], $Matches[2], 'Process')
        }
    }
    if (-not (Test-Command 'cl.exe') -or -not (Test-Command 'link.exe')) {
        throw 'MSVC compiler or linker is unavailable after loading VsDevCmd.bat.'
    }
}

function Get-FileHashValue([string]$Path) {
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Get-SourceId {
    $paths = @('Cargo.toml', 'Cargo.lock')
    foreach ($crate in @('ale-core', 'ale-cli', 'ale-gui', 'ale-modeld')) {
        $paths += "$crate/Cargo.toml", "$crate/build.rs"
        foreach ($directory in @('src', 'tests')) {
            $source = Join-Path $repoRoot "$crate\$directory"
            if (Test-Path -LiteralPath $source) {
                $paths += @(Get-ChildItem -LiteralPath $source -Recurse -File -Filter '*.rs' |
                    ForEach-Object { $_.FullName.Substring($repoRoot.Length + 1).Replace('\', '/') })
            }
        }
    }
    foreach ($directory in @('ale-gui/ui', 'assets', 'scripts', '.cargo', 'vendor/i-slint-backend-winit-1.16.1')) {
        $source = Join-Path $repoRoot $directory
        if (Test-Path -LiteralPath $source) {
            $paths += @(Get-ChildItem -LiteralPath $source -Recurse -File -Force |
                Where-Object { $_.FullName -notmatch '[\\/](\.git|__pycache__)[\\/]' } |
                ForEach-Object { $_.FullName.Substring($repoRoot.Length + 1).Replace('\', '/') })
        }
    }
    $paths = [string[]]@($paths | Where-Object { Test-Path -LiteralPath (Join-Path $repoRoot $_) } | Select-Object -Unique)
    [Array]::Sort($paths, [StringComparer]::Ordinal)
    $stream = New-Object System.IO.MemoryStream
    try {
        foreach ($path in $paths) {
            $nameBytes = [System.Text.Encoding]::UTF8.GetBytes($path)
            $stream.Write($nameBytes, 0, $nameBytes.Length)
            $stream.WriteByte(0)
            $hex = Get-FileHashValue (Join-Path $repoRoot $path)
            for ($index = 0; $index -lt $hex.Length; $index += 2) {
                $stream.WriteByte([Convert]::ToByte($hex.Substring($index, 2), 16))
            }
        }
        $sha256 = [System.Security.Cryptography.SHA256]::Create()
        try { return ([BitConverter]::ToString($sha256.ComputeHash($stream.ToArray()))).Replace('-', '').ToLowerInvariant() }
        finally { $sha256.Dispose() }
    } finally { $stream.Dispose() }
}

function Copy-BuildRuntimes([string]$BuildLog) {
    $selected = @{}
    foreach ($line in Get-Content -LiteralPath $BuildLog) {
        if (-not $line) { continue }
        $message = $line | ConvertFrom-Json
        if ($message.reason -ne 'build-script-executed' -or
            $message.package_id -notmatch '(sherpa|ort-sys)') { continue }
        foreach ($dll in Get-ChildItem -LiteralPath $message.out_dir -Recurse -File -Filter '*.dll') {
            $hash = Get-FileHashValue $dll.FullName
            $key = $dll.Name.ToLowerInvariant()
            if ($selected.ContainsKey($key) -and $selected[$key].Hash -ne $hash) {
                throw "Conflicting native runtimes named $($dll.Name); inspect $BuildLog"
            }
            $selected[$key] = @{ Path = $dll.FullName; Hash = $hash; Name = $dll.Name }
        }
    }
    if (-not $selected.ContainsKey('sherpa-onnx-c-api.dll')) {
        throw "This MSVC build did not report sherpa-onnx-c-api.dll. Inspect $BuildLog"
    }
    $runtimeHashes = @{}
    foreach ($dll in $selected.Values) {
        Copy-Item -LiteralPath $dll.Path -Destination (Join-Path $releaseDir $dll.Name) -Force
        $runtimeHashes[$dll.Name] = $dll.Hash
    }
    Write-Host "Copied $($selected.Count) native runtime DLL(s) beside the executables."
    return $runtimeHashes
}

function Write-PackageManifest([string]$Directory, [hashtable]$RuntimeHashes, [string]$SourceId) {
    $files = @{}
    foreach ($file in Get-ChildItem -LiteralPath $Directory -File) {
        if ($file.Extension -notin @('.exe', '.dll', '.pdb')) { continue }
        $files[$file.Name] = @{ bytes = $file.Length; sha256 = (Get-FileHashValue $file.FullName) }
    }
    $commit = $null
    $uncommitted = $false
    if ((Test-Path -LiteralPath (Join-Path $repoRoot '.git')) -and (Test-Command 'git.exe')) {
        $commit = & git.exe -C $repoRoot rev-parse HEAD 2>$null
        if ($LASTEXITCODE -eq 0) {
            $uncommitted = @( & git.exe -C $repoRoot status --porcelain ).Count -gt 0
        } else {
            $commit = $null
        }
    }
    $manifest = @{
        schema_version = 2
        target = $target
        build_id = $SourceId
        source_sha256 = $SourceId
        git_commit = $commit
        uncommitted_source = $uncommitted
        rustc = ((& rustc.exe -vV) -join "`n")
        local_asr_compiled = $true
        native_acceptance = 'pending'
        runtime_dlls = $RuntimeHashes
        files = $files
    }
    $manifest | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $Directory 'build-manifest.json') -Encoding UTF8
}

function New-WindowsPackage([hashtable]$RuntimeHashes, [string]$SourceId) {
    $packageDir = Join-Path $repoRoot 'ale-my-eyes-windows'
    $archive = "$packageDir.zip"
    $symbolsDir = Join-Path $repoRoot "target\windows-symbols\$target"
    $symbolsArchive = Join-Path $repoRoot 'ale-my-eyes-windows-symbols.zip'
    foreach ($path in @($packageDir, $archive, $symbolsDir, $symbolsArchive)) {
        if (Test-Path -LiteralPath $path) { Remove-Item -LiteralPath $path -Recurse -Force }
    }
    New-Item -ItemType Directory -Path $packageDir, $symbolsDir -Force | Out-Null
    foreach ($name in @('ale-cli.exe', 'ale-gui.exe', 'ale-modeld.exe', 'ale-modeld-acceptance.exe')) {
        Copy-Item -LiteralPath (Join-Path $releaseDir $name) -Destination $packageDir
        Copy-Item -LiteralPath (Join-Path $releaseDir $name) -Destination $symbolsDir
    }
    foreach ($name in @('ale_cli.pdb', 'ale_gui.pdb', 'ale_modeld.pdb', 'ale_modeld_acceptance.pdb')) {
        $source = Join-Path $releaseDir $name
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "Missing MSVC symbols: $source" }
        Copy-Item -LiteralPath $source -Destination $packageDir
        Copy-Item -LiteralPath $source -Destination $symbolsDir
    }
    foreach ($name in $RuntimeHashes.Keys) {
        Copy-Item -LiteralPath (Join-Path $releaseDir $name) -Destination $packageDir
    }
    Copy-Item -LiteralPath (Join-Path $repoRoot 'LICENSE') -Destination $packageDir
    Copy-Item -LiteralPath (Join-Path $repoRoot 'ale-gui\DIAGNOSTICS.md') -Destination $packageDir
    [System.IO.File]::WriteAllText((Join-Path $packageDir 'start-gui.bat'), "@echo off`r`ncd /d `"%~dp0`"`r`nstart `"`" `"%~dp0ale-gui.exe`"`r`nexit /b`r`n", [System.Text.Encoding]::ASCII)
    [System.IO.File]::WriteAllText((Join-Path $packageDir 'README.txt'),
        "Ale, My Eyes! Desktop`r`nRun start-gui.bat or ale-gui.exe and configure the endpoint in Settings.`r`nModel weights are not included. See DIAGNOSTICS.md for logging and model requirements.`r`nConfiguration is stored in the current Windows user's config directory, not beside the executable.`r`n", [System.Text.Encoding]::ASCII)
    if ((Get-SourceId) -ne $SourceId) { throw 'Source inputs changed during packaging. Rebuild before delivery.' }
    Write-PackageManifest $packageDir $RuntimeHashes $SourceId
    Copy-Item -LiteralPath (Join-Path $packageDir 'build-manifest.json') -Destination $symbolsDir
    Compress-Archive -LiteralPath $packageDir -DestinationPath $archive -Force
    Compress-Archive -Path (Join-Path $symbolsDir '*') -DestinationPath $symbolsArchive -Force
    Write-Host "Windows package: $archive" -ForegroundColor Green
    Write-Host "Symbols: $symbolsArchive"
}

if ($env:OS -ne 'Windows_NT') { throw 'Run this script on Windows.' }
if ($InstallMissing -and $SkipInstall) { throw 'Choose either -InstallMissing or -SkipInstall.' }
if ($Package -and $BuildOnly) { throw 'Choose either -Package or -BuildOnly.' }
Set-Location $repoRoot
$cargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if (Test-Path -LiteralPath $cargoBin) { $env:Path = "$cargoBin;$env:Path" }
$missing = @(Get-MissingPrerequisites)
if ($missing.Count -gt 0) {
    Write-Host "Missing: $($missing -join ', ')" -ForegroundColor Yellow
    if ($SkipInstall) { throw 'Install the missing prerequisites and rerun the build.' }
    if (-not $InstallMissing) {
        $answer = Read-Host 'Install missing prerequisites now? [Y/n]'
        if ($answer -and $answer -notmatch '^[Yy]$') { throw 'Build cancelled.' }
    }
    Install-Prerequisites $missing
    $remaining = @(Get-MissingPrerequisites)
    if ($remaining.Count -gt 0) { throw "Prerequisites still missing: $($remaining -join ', '). Rerun after installation or restart." }
}
Import-VsEnvironment (Get-VsInstallation)
& rustup.exe toolchain install stable-x86_64-pc-windows-msvc --profile minimal
if ($LASTEXITCODE -ne 0) { throw 'Rust MSVC toolchain installation failed.' }
$env:LIBCLANG_PATH = Find-LibclangDirectory
if (-not $env:LIBCLANG_PATH) { throw 'libclang.dll is not available.' }
if ((& rustc.exe +stable-x86_64-pc-windows-msvc -vV | Select-String '^host:').Line -ne "host: $target") {
    throw "Rust host must be $target."
}
$env:CARGO_PROFILE_RELEASE_DEBUG = '2'
New-Item -ItemType Directory -Path $releaseDir -Force | Out-Null
$buildLog = Join-Path $repoRoot "target\windows-package-build-$target.jsonl"
$sourceId = Get-SourceId
$env:ALE_BUILD_ID = $sourceId
Write-Host 'Building Windows MSVC release binaries...'
& cargo.exe +stable-x86_64-pc-windows-msvc build --release --locked -p ale-cli -p ale-gui -p ale-modeld --bins --message-format=json-render-diagnostics 1> $buildLog
if ($LASTEXITCODE -ne 0) { throw "Cargo build failed. Build messages: $buildLog" }
$runtimes = Copy-BuildRuntimes $buildLog
Write-Host "Runnable binaries and native DLLs: $releaseDir" -ForegroundColor Green
if (-not $Package -and -not $BuildOnly) {
    $answer = Read-Host 'Create the Windows ZIP package now? [y/N]'
    $Package = $answer -match '^[Yy]$'
}
if ($Package) { New-WindowsPackage $runtimes $sourceId }
