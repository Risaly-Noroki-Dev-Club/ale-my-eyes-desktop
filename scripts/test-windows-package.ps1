$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$archive = Join-Path $repoRoot 'ale-my-eyes-windows.zip'
$tempBase = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { [System.IO.Path]::GetTempPath() }
$smokeRoot = Join-Path $tempBase ("ale-my-eyes-windows-smoke-" + [guid]::NewGuid())
$extractRoot = Join-Path $smokeRoot 'extract'
$profileRoot = Join-Path $smokeRoot 'profile'

New-Item -ItemType Directory -Path $extractRoot, $profileRoot | Out-Null
try {
    Expand-Archive -LiteralPath $archive -DestinationPath $extractRoot
    $package = Join-Path $extractRoot 'ale-my-eyes-windows'
    foreach ($name in @('ale-gui.exe', 'ale-cli.exe', 'start-gui.bat', 'README.txt', 'LICENSE')) {
        if (-not (Test-Path -LiteralPath (Join-Path $package $name) -PathType Leaf)) {
            throw "Missing Windows package file: $name"
        }
    }
    if (Test-Path -LiteralPath (Join-Path $package 'config')) {
        throw 'Windows package contains a package-local config directory'
    }

    $env:APPDATA = $profileRoot
    $env:LOCALAPPDATA = $profileRoot
    $cliOutput = & (Join-Path $package 'ale-cli.exe') status 2>&1
    if ($LASTEXITCODE -ne 0 -or ($cliOutput -join "`n") -notmatch 'Ale, My Eyes! CLI') {
        throw "Packaged CLI smoke test failed: $cliOutput"
    }

    $gui = Start-Process -FilePath (Join-Path $package 'ale-gui.exe') -WorkingDirectory $package -PassThru
    try {
        $listener = $null
        for ($attempt = 0; $attempt -lt 15; $attempt++) {
            Start-Sleep -Seconds 1
            if ($gui.HasExited) {
                throw "Packaged GUI exited early with code $($gui.ExitCode)"
            }
            $listener = Get-NetTCPConnection -State Listen -LocalPort 37654 -ErrorAction SilentlyContinue |
                Where-Object { $_.OwningProcess -eq $gui.Id }
            if ($listener) { break }
        }
        if (-not $listener) {
            throw 'Packaged GUI did not listen on TCP port 37654'
        }
    }
    finally {
        if (-not $gui.HasExited) {
            Stop-Process -Id $gui.Id -Force
            $gui.WaitForExit()
        }
    }

    Write-Host 'Windows package CLI and GUI smoke tests passed.'
}
finally {
    if (Test-Path -LiteralPath $smokeRoot) {
        Remove-Item -LiteralPath $smokeRoot -Recurse -Force
    }
}
