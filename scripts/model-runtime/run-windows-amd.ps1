[CmdletBinding()]
param(
    [string]$ModelsDir,
    [ValidateRange(1, 120)]
    [int]$InferenceTimeoutMinutes = 30
)

$ErrorActionPreference = "Stop"
$ProgressPreference = "SilentlyContinue"

$LlamaBuild = "b10472"
$LlamaArchiveUrl = "https://github.com/ggml-org/llama.cpp/releases/download/b10472/llama-b10472-bin-win-vulkan-x64.zip"
$LlamaArchiveSha256 = "2104e62c7e5237f2190240cdc987d8c3946a77051f696771d03b8d762a9d2fae"
$LlamaSourceUrl = "https://github.com/ggml-org/llama.cpp/archive/refs/tags/b10472.zip"
$LlamaSourceSha256 = "f3859350968a2101ac1ce436d59aec6945851a83f7739bd80db6aa155856cae2"
$UvVersion = "0.12.5"
$UvArchiveUrl = "https://github.com/astral-sh/uv/releases/download/0.12.5/uv-x86_64-pc-windows-msvc.zip"
$UvArchiveSha256 = "4c4d49d8738847d9b71ba319e49a5688c93eac0fe6204b1df24e98528dddf39a"

function Write-Stage([string]$Message) {
    Write-Host ""
    Write-Host ("=" * 72) -ForegroundColor DarkGray
    Write-Host $Message -ForegroundColor Cyan
    Write-Host ("=" * 72) -ForegroundColor DarkGray
}

function Get-Sha256([string]$Path) {
    return (Get-FileHash -LiteralPath $Path -Algorithm SHA256).Hash.ToLowerInvariant()
}

function Invoke-Download([string]$Url, [string]$Destination) {
    $attempt = 0
    while ($attempt -lt 12) {
        $attempt++
        Write-Host "Download attempt $attempt`: $Url"
        & curl.exe --fail --location --retry 8 --retry-delay 5 --speed-limit 1024 --speed-time 180 --continue-at "-" --output $Destination $Url
        if ($LASTEXITCODE -eq 0) {
            return
        }
        Start-Sleep -Seconds ([Math]::Min(60, $attempt * 5))
    }
    throw "Download failed after $attempt attempts: $Url"
}

function Get-VerifiedArchive([string]$Url, [string]$Destination, [string]$ExpectedSha256) {
    if (Test-Path -LiteralPath $Destination) {
        if ((Get-Sha256 $Destination) -eq $ExpectedSha256) {
            Write-Host "Verified cached archive: $Destination"
            return
        }
    }
    Invoke-Download $Url $Destination
    if ((Get-Sha256 $Destination) -ne $ExpectedSha256) {
        Remove-Item -LiteralPath $Destination -Force
        Write-Host "Cached partial archive was invalid; downloading a clean copy."
        Invoke-Download $Url $Destination
    }
    $actual = Get-Sha256 $Destination
    if ($actual -ne $ExpectedSha256) {
        throw "SHA-256 mismatch for $Destination. Expected $ExpectedSha256, got $actual"
    }
}

function Expand-PinnedArchive(
    [string]$Archive,
    [string]$Destination,
    [string]$ExpectedSha256,
    [string]$RequiredFile
) {
    $marker = Join-Path $Destination ".ale-archive-sha256"
    $required = Join-Path $Destination $RequiredFile
    if ((Test-Path -LiteralPath $marker) -and (Test-Path -LiteralPath $required)) {
        if ((Get-Content -LiteralPath $marker -Raw).Trim() -eq $ExpectedSha256) {
            return
        }
    }
    if (Test-Path -LiteralPath $Destination) {
        Remove-Item -LiteralPath $Destination -Recurse -Force
    }
    New-Item -ItemType Directory -Path $Destination | Out-Null
    Expand-Archive -LiteralPath $Archive -DestinationPath $Destination -Force
    Set-Content -LiteralPath $marker -Value $ExpectedSha256 -Encoding ASCII
    if (-not (Test-Path -LiteralPath $required)) {
        throw "Expected archive file is missing: $required"
    }
}

function Expand-PinnedLlamaSource(
    [string]$Archive,
    [string]$Destination,
    [string]$ExpectedSha256,
    [string]$Build
) {
    $sourceFolder = "llama.cpp-$Build"
    $required = Join-Path $Destination "$sourceFolder\convert_hf_to_gguf.py"
    $marker = Join-Path $Destination ".ale-archive-sha256"
    if ((Test-Path -LiteralPath $marker) -and (Test-Path -LiteralPath $required)) {
        if ((Get-Content -LiteralPath $marker -Raw).Trim() -eq $ExpectedSha256) {
            return
        }
    }
    if (Test-Path -LiteralPath $Destination) {
        Remove-Item -LiteralPath $Destination -Recurse -Force
    }
    New-Item -ItemType Directory -Path $Destination | Out-Null
    & tar.exe -xf $Archive -C $Destination `
        "$sourceFolder/convert_hf_to_gguf.py" `
        "$sourceFolder/conversion" `
        "$sourceFolder/gguf-py" `
        "$sourceFolder/requirements"
    if ($LASTEXITCODE -ne 0 -or -not (Test-Path -LiteralPath $required)) {
        throw "Pinned llama.cpp conversion source could not be extracted"
    }
    Set-Content -LiteralPath $marker -Value $ExpectedSha256 -Encoding ASCII
}

function Redact-ReportPaths([string]$Directory) {
    $replacements = @(
        [pscustomobject]@{ From = $ModelsDir; To = "<MODELS_DIR>" },
        [pscustomobject]@{ From = $RepoRoot; To = "<REPO_ROOT>" },
        [pscustomobject]@{ From = $env:USERPROFILE; To = "<USERPROFILE>" },
        [pscustomobject]@{ From = $env:USERNAME; To = "<USERNAME>" },
        [pscustomobject]@{ From = $env:COMPUTERNAME; To = "<COMPUTERNAME>" },
        [pscustomobject]@{ From = $env:USERDOMAIN; To = "<USERDOMAIN>" }
    ) | Where-Object { -not [string]::IsNullOrWhiteSpace($_.From) } |
        Sort-Object { $_.From.Length } -Descending
    Get-ChildItem -LiteralPath $Directory -File -Recurse | Where-Object {
        $_.Extension -in @(".json", ".log", ".txt")
    } | ForEach-Object {
        $content = Get-Content -LiteralPath $_.FullName -Raw
        foreach ($replacement in $replacements) {
            $content = $content.Replace($replacement.From, $replacement.To)
            $escaped = $replacement.From.Replace("\", "\\")
            $content = $content.Replace($escaped, $replacement.To)
        }
        Set-Content -LiteralPath $_.FullName -Value $content -Encoding UTF8
    }
}

if ($env:OS -ne "Windows_NT") {
    throw "This acceptance tool must run on Windows."
}
if (-not [Environment]::Is64BitOperatingSystem) {
    throw "A 64-bit Windows installation is required."
}
if (-not (Get-Command curl.exe -ErrorAction SilentlyContinue)) {
    throw "Windows curl.exe is required."
}
if (-not (Get-Command tar.exe -ErrorAction SilentlyContinue)) {
    throw "Windows tar.exe is required."
}

$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..\..")).Path
if ([string]::IsNullOrWhiteSpace($ModelsDir)) {
    $ModelsDir = Join-Path $RepoRoot "models"
} else {
    $ModelsDir = (Resolve-Path $ModelsDir).Path
}
$RequiredModels = @("Qwen2.5-VL-7B-Instruct", "ShowUI-2B")
foreach ($model in $RequiredModels) {
    if (-not (Test-Path -LiteralPath (Join-Path $ModelsDir $model) -PathType Container)) {
        throw "Required model directory is missing: $(Join-Path $ModelsDir $model)"
    }
}

$RuntimeRoot = Join-Path $ModelsDir ".runtime"
$Downloads = Join-Path $RuntimeRoot "downloads"
$ToolsRoot = Join-Path $RuntimeRoot "tools"
$GgufRoot = Join-Path $RuntimeRoot "gguf"
$Venv = Join-Path $RuntimeRoot "python"
$FixtureDir = Join-Path $RuntimeRoot "fixtures"
$Timestamp = Get-Date -Format "yyyyMMdd-HHmmss"
$ReportRoot = Join-Path $RepoRoot "target\model-runtime-reports"
$ReportDir = Join-Path $ReportRoot $Timestamp
$ReportZip = Join-Path $ReportRoot "model-runtime-report-$Timestamp.zip"
New-Item -ItemType Directory -Force -Path $Downloads, $ToolsRoot, $GgufRoot, $FixtureDir, $ReportDir | Out-Null

$ExitCode = 1
$TranscriptStarted = $false
try {
    Start-Transcript -LiteralPath (Join-Path $ReportDir "acceptance-transcript.txt") -Force | Out-Null
    $TranscriptStarted = $true

    Write-Stage "1/6 - Collecting non-sensitive Windows and GPU inventory"
    $os = Get-CimInstance Win32_OperatingSystem
    $cpu = Get-CimInstance Win32_Processor | Select-Object -First 1
    $gpus = @(Get-CimInstance Win32_VideoController | ForEach-Object {
        $reportedRam = $null
        if ($null -ne $_.AdapterRAM) {
            $reportedRam = [uint64]$_.AdapterRAM
        }
        $pnpVendorDevice = $null
        if ($_.PNPDeviceID -match "VEN_([0-9A-F]+)&DEV_([0-9A-F]+)") {
            $pnpVendorDevice = "VEN_$($Matches[1])&DEV_$($Matches[2])"
        }
        [ordered]@{
            name = $_.Name
            driver_version = $_.DriverVersion
            adapter_ram_reported_bytes = $reportedRam
            pnp_vendor_device = $pnpVendorDevice
        }
    })
    $inventory = [ordered]@{
        captured_at_utc = (Get-Date).ToUniversalTime().ToString("o")
        windows = [ordered]@{ caption = $os.Caption; version = $os.Version; build = $os.BuildNumber }
        cpu = [ordered]@{ name = $cpu.Name; physical_cores = $cpu.NumberOfCores; logical_processors = $cpu.NumberOfLogicalProcessors }
        memory = [ordered]@{
            total_bytes = [uint64]$os.TotalVisibleMemorySize * 1024
            free_bytes = [uint64]$os.FreePhysicalMemory * 1024
        }
        gpus = $gpus
        powershell = $PSVersionTable.PSVersion.ToString()
    }
    $inventory | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $ReportDir "system.json") -Encoding UTF8
    $w6800 = @($gpus | Where-Object { $_.name -match "W\s*6800" })
    if ($w6800.Count -eq 0) {
        Write-Warning "Win32_VideoController did not report a Radeon PRO W6800; Vulkan probing will decide the test."
    }

    Write-Stage "2/6 - Downloading and verifying pinned test tools"
    $UvArchive = Join-Path $Downloads "uv-$UvVersion-windows-x64.zip"
    $LlamaArchive = Join-Path $Downloads "llama-$LlamaBuild-windows-vulkan-x64.zip"
    $SourceArchive = Join-Path $Downloads "llama-$LlamaBuild-source.zip"
    Get-VerifiedArchive $UvArchiveUrl $UvArchive $UvArchiveSha256
    Get-VerifiedArchive $LlamaArchiveUrl $LlamaArchive $LlamaArchiveSha256
    Get-VerifiedArchive $LlamaSourceUrl $SourceArchive $LlamaSourceSha256

    $UvRoot = Join-Path $ToolsRoot "uv-$UvVersion"
    $LlamaRoot = Join-Path $ToolsRoot "llama-$LlamaBuild-vulkan"
    $SourceRoot = Join-Path $ToolsRoot "llama-$LlamaBuild-source"
    Expand-PinnedArchive $UvArchive $UvRoot $UvArchiveSha256 "uv.exe"
    Expand-PinnedArchive $LlamaArchive $LlamaRoot $LlamaArchiveSha256 "llama-server.exe"
    Expand-PinnedLlamaSource $SourceArchive $SourceRoot $LlamaSourceSha256 $LlamaBuild
    $Uv = Join-Path $UvRoot "uv.exe"
    $LlamaCli = Join-Path $LlamaRoot "llama-cli.exe"
    $LlamaServer = Join-Path $LlamaRoot "llama-server.exe"
    $Quantize = Join-Path $LlamaRoot "llama-quantize.exe"
    $LlamaSource = Join-Path $SourceRoot "llama.cpp-$LlamaBuild"
    foreach ($requiredTool in @($LlamaCli, $LlamaServer, $Quantize)) {
        if (-not (Test-Path -LiteralPath $requiredTool -PathType Leaf)) {
            throw "Pinned llama.cpp archive is missing required tool: $requiredTool"
        }
    }

    $deviceStart = New-Object System.Diagnostics.ProcessStartInfo
    $deviceStart.FileName = $LlamaCli
    $deviceStart.Arguments = "--list-devices"
    $deviceStart.UseShellExecute = $false
    $deviceStart.CreateNoWindow = $true
    $deviceStart.RedirectStandardOutput = $true
    $deviceStart.RedirectStandardError = $true
    $deviceProcess = New-Object System.Diagnostics.Process
    $deviceProcess.StartInfo = $deviceStart
    if (-not $deviceProcess.Start()) {
        throw "llama.cpp Vulkan device probing could not start"
    }
    $deviceStdout = $deviceProcess.StandardOutput.ReadToEnd()
    $deviceStderr = $deviceProcess.StandardError.ReadToEnd()
    $deviceProcess.WaitForExit()
    $deviceExitCode = $deviceProcess.ExitCode
    $deviceText = $deviceStdout + [Environment]::NewLine + $deviceStderr
    $deviceText | Set-Content -LiteralPath (Join-Path $ReportDir "00-devices-preflight.log") -Encoding UTF8
    if ($deviceExitCode -ne 0) {
        throw "llama.cpp Vulkan device probing failed with exit code $deviceExitCode"
    }
    if ($deviceText -notmatch "(?i)vulkan" -or $deviceText -notmatch "(?i)(AMD|Radeon|WX\s*9100)") {
        throw "llama.cpp Vulkan did not detect an AMD GPU; update the Radeon Pro driver before conversion"
    }

    Write-Stage "3/6 - Preparing isolated Python 3.11 conversion environment"
    $env:UV_PYTHON_PREFERENCE = "only-managed"
    $env:UV_CACHE_DIR = Join-Path $RuntimeRoot "uv-cache"
    & $Uv venv --python 3.11 $Venv
    if ($LASTEXITCODE -ne 0) { throw "uv failed to create the Python environment" }
    $Python = Join-Path $Venv "Scripts\python.exe"
    $Requirements = Join-Path $LlamaSource "requirements\requirements-convert_hf_to_gguf.txt"
    & $Uv pip install --python $Python -r $Requirements "pillow==11.3.0"
    if ($LASTEXITCODE -ne 0) { throw "Python conversion dependencies failed to install" }

    Write-Stage "4/6 - Generating deterministic UI fixtures"
    & $Python (Join-Path $PSScriptRoot "generate_fixtures.py") --output-dir $FixtureDir
    if ($LASTEXITCODE -ne 0) { throw "Fixture generation failed" }
    $ReportFixtures = Join-Path $ReportDir "fixtures"
    New-Item -ItemType Directory -Force -Path $ReportFixtures | Out-Null
    Copy-Item -Path (Join-Path $FixtureDir "*") -Destination $ReportFixtures -Force

    Write-Stage "5/6 - Converting two pinned models to restart-safe Q4_K_M GGUF"
    $ModelManifest = Join-Path $RuntimeRoot "runtime-models.json"
    & $Python (Join-Path $PSScriptRoot "prepare_gguf.py") `
        --models-dir $ModelsDir `
        --output-dir $GgufRoot `
        --llama-source $LlamaSource `
        --quantize $Quantize `
        --llama-build $LlamaBuild `
        --manifest $ModelManifest
    if ($LASTEXITCODE -ne 0) { throw "GGUF conversion failed" }
    Write-Stage "6/6 - Running real Vulkan multimodal inference"
    $GitCommit = (& git.exe -C $RepoRoot rev-parse HEAD 2>$null)
    if ([string]::IsNullOrWhiteSpace($GitCommit)) { $GitCommit = "unknown" }
    & $Python (Join-Path $PSScriptRoot "runtime_acceptance.py") `
        --llama-cli $LlamaCli `
        --models-manifest $ModelManifest `
        --fixtures-dir $FixtureDir `
        --report-dir $ReportDir `
        --capabilities-out (Join-Path $RuntimeRoot "runtime-capabilities.json") `
        --git-commit $GitCommit `
        --timeout-seconds ($InferenceTimeoutMinutes * 60)
    $ExitCode = $LASTEXITCODE
} catch {
    $message = $_.Exception.Message
    Write-Host "FAILED: $message" -ForegroundColor Red
    [ordered]@{
        passed = $false
        phase = "bootstrap_or_conversion"
        error = $message
    } | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $ReportDir "failure.json") -Encoding UTF8
    $ExitCode = 1
} finally {
    Get-ChildItem -LiteralPath $GgufRoot -Filter "*.conversion.log" -ErrorAction SilentlyContinue | Copy-Item -Destination $ReportDir -Force -ErrorAction SilentlyContinue
    if ($TranscriptStarted) {
        Stop-Transcript | Out-Null
    }
    Redact-ReportPaths $ReportDir
    New-Item -ItemType Directory -Force -Path $ReportRoot | Out-Null
    if (Test-Path -LiteralPath $ReportZip) {
        Remove-Item -LiteralPath $ReportZip -Force
    }
    Compress-Archive -Path (Join-Path $ReportDir "*") -DestinationPath $ReportZip -CompressionLevel Optimal
    Write-Host ""
    Write-Host "Report directory: $ReportDir"
    Write-Host "Report ZIP:       $ReportZip"
}

exit $ExitCode
