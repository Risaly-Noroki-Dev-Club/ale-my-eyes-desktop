@echo off
setlocal EnableExtensions

rem Download pinned model snapshots for Ale, My Eyes! on Windows.
rem Usage: download-models.bat [all|sensevoice|qwen|showui] [--models-dir PATH] [--workers N] [--retry-hours N] [--yes|--unattended]

for %%I in ("%~dp0..") do set "REPO_ROOT=%%~fI"
set "MODEL_ROOT=%REPO_ROOT%\models"
set "MODE=all"
set "ASSUME_YES=0"
set "HF_WORKERS=8"
set "RETRY_HOURS=24"
set "RETRY_DELAY_SECONDS=60"
set "PYTHON_CMD="
set "PAUSE_ON_EXIT=0"

if "%~1"=="" goto interactive_menu
goto parse_args

:interactive_menu
set "PAUSE_ON_EXIT=1"
cls
echo +------------------------------------------------------------------+
echo ^|             Ale, My Eyes! - Pinned Model Downloader             ^|
echo +------------------------------------------------------------------+
echo ^|  Models: all pinned models - about 31 GB                        ^|
echo ^|  Directory: repository models folder                            ^|
echo ^|  Parallel workers: 8                                            ^|
echo +------------------------------------------------------------------+
echo ^|  U. Unattended - retry forever, no further prompts              ^|
echo ^|  N. Normal - retry for 24 hours and confirm licenses            ^|
echo +------------------------------------------------------------------+
choice /C UN /N /M "Run mode [U=unattended,N=normal]: "
set "MENU_CHOICE=%errorlevel%"
if "%MENU_CHOICE%"=="1" (
    set "ASSUME_YES=1"
    set "RETRY_HOURS=0"
    set "PAUSE_ON_EXIT=0"
)
if "%MENU_CHOICE%"=="2" (
    set "ASSUME_YES=0"
    set "RETRY_HOURS=24"
)
goto args_done

:parse_args
if "%~1"=="" goto args_done
if /I "%~1"=="--yes" (
    set "ASSUME_YES=1"
    shift
    goto parse_args
)
if /I "%~1"=="--unattended" (
    set "ASSUME_YES=1"
    set "RETRY_HOURS=0"
    shift
    goto parse_args
)
if /I "%~1"=="--models-dir" (
    if "%~2"=="" goto usage_error
    for %%I in ("%~2") do set "MODEL_ROOT=%%~fI"
    shift
    shift
    goto parse_args
)
if /I "%~1"=="--workers" (
    if "%~2"=="" goto usage_error
    set "HF_WORKERS=%~2"
    shift
    shift
    goto parse_args
)
if /I "%~1"=="--retry-hours" (
    if "%~2"=="" goto usage_error
    set "RETRY_HOURS=%~2"
    shift
    shift
    goto parse_args
)
if /I "%~1"=="all" (
    set "MODE=all"
    shift
    goto parse_args
)
if /I "%~1"=="sensevoice" (
    set "MODE=sensevoice"
    shift
    goto parse_args
)
if /I "%~1"=="qwen" (
    set "MODE=qwen"
    shift
    goto parse_args
)
if /I "%~1"=="showui" (
    set "MODE=showui"
    shift
    goto parse_args
)
goto usage_error

:args_done
echo(%HF_WORKERS%| findstr /R /X "[1-9][0-9]*" >nul
if errorlevel 1 (
    echo ERROR: --workers must be an integer from 1 to 32.
    goto failed
)
if %HF_WORKERS% GTR 32 (
    echo ERROR: --workers must be an integer from 1 to 32.
    goto failed
)
echo(%RETRY_HOURS%| findstr /R /X "[0-9][0-9]*" >nul
if errorlevel 1 (
    echo ERROR: --retry-hours must be zero or a positive integer.
    goto failed
)
set /A MAX_RETRY_ATTEMPTS=%RETRY_HOURS%*60
if not exist "%MODEL_ROOT%" mkdir "%MODEL_ROOT%"
if errorlevel 1 goto failed
if not exist "%MODEL_ROOT%\.downloads" mkdir "%MODEL_ROOT%\.downloads"
if errorlevel 1 goto failed
set "DOWNLOAD_LOG=%MODEL_ROOT%\.downloads\download-models.log"
>> "%DOWNLOAD_LOG%" echo [%DATE% %TIME%] Started mode=%MODE% workers=%HF_WORKERS% retry_hours=%RETRY_HOURS%

set "REQUIRED_GB=40"
if /I "%MODE%"=="sensevoice" set "REQUIRED_GB=1"
if /I "%MODE%"=="qwen" set "REQUIRED_GB=20"
if /I "%MODE%"=="showui" set "REQUIRED_GB=16"
set "ALE_MODEL_ROOT=%MODEL_ROOT%"

where powershell.exe >nul 2>&1
if errorlevel 1 (
    echo ERROR: powershell.exe is required for disk and SHA-256 checks.
    goto failed
)

for /f %%G in ('powershell.exe -NoProfile -Command "[math]::Floor((Get-Item -LiteralPath $env:ALE_MODEL_ROOT).PSDrive.Free / 1GB)"') do set "FREE_GB=%%G"
echo.
echo Ale, My Eyes! pinned model downloader
echo Target: %MODEL_ROOT%
echo Free space: %FREE_GB% GB
echo Required free space for this selection: %REQUIRED_GB% GB
echo Parallel Hugging Face file workers: %HF_WORKERS%
if "%RETRY_HOURS%"=="0" echo Retry policy: unlimited, every %RETRY_DELAY_SECONDS% seconds
if not "%RETRY_HOURS%"=="0" echo Retry policy: up to %RETRY_HOURS% hours, every %RETRY_DELAY_SECONDS% seconds
echo.
if /I "%MODE%"=="all" (
    echo   SenseVoiceSmall INT8       about 0.2 GB   model license
    echo   Qwen2.5-VL-7B-Instruct     about 16.6 GB  Apache-2.0
    echo   ShowUI-2B                   about 13.3 GB  MIT
)

powershell.exe -NoProfile -Command "if ((Get-Item -LiteralPath $env:ALE_MODEL_ROOT).PSDrive.Free -lt %REQUIRED_GB%GB) { exit 1 }"
if errorlevel 1 (
    echo ERROR: The target drive does not have enough free space.
    goto failed
)

if "%ASSUME_YES%"=="0" (
    choice /C YN /N /M "Download the selected pinned models and accept their licenses? [Y/N] "
    if errorlevel 2 exit /b 2
)

if /I "%MODE%"=="all" goto download_all
if /I "%MODE%"=="sensevoice" goto download_sensevoice_only
if /I "%MODE%"=="qwen" goto download_qwen_only
if /I "%MODE%"=="showui" goto download_showui_only
goto usage_error

:download_all
call :run_with_retry download_sensevoice "SenseVoiceSmall"
if errorlevel 1 goto failed
call :run_with_retry ensure_huggingface_hub "huggingface_hub setup"
if errorlevel 1 goto failed
call :run_with_retry download_qwen "Qwen2.5-VL-7B-Instruct"
if errorlevel 1 goto failed
call :run_with_retry download_showui "ShowUI-2B"
if errorlevel 1 goto failed
goto success

:download_sensevoice_only
call :run_with_retry download_sensevoice "SenseVoiceSmall"
if errorlevel 1 goto failed
goto success

:download_qwen_only
call :run_with_retry ensure_huggingface_hub "huggingface_hub setup"
if errorlevel 1 goto failed
call :run_with_retry download_qwen "Qwen2.5-VL-7B-Instruct"
if errorlevel 1 goto failed
goto success

:download_showui_only
call :run_with_retry ensure_huggingface_hub "huggingface_hub setup"
if errorlevel 1 goto failed
call :run_with_retry download_showui "ShowUI-2B"
if errorlevel 1 goto failed
goto success

:run_with_retry
set "RETRY_TASK=%~1"
set "RETRY_NAME=%~2"
set /A RETRY_ATTEMPT=0

:retry_task_loop
call :%RETRY_TASK%
if not errorlevel 1 exit /b 0
set /A RETRY_ATTEMPT+=1
if not "%RETRY_HOURS%"=="0" if %RETRY_ATTEMPT% GEQ %MAX_RETRY_ATTEMPTS% (
    echo [%DATE% %TIME%] %RETRY_NAME% exceeded the %RETRY_HOURS%-hour retry window.
    >> "%DOWNLOAD_LOG%" echo [%DATE% %TIME%] %RETRY_NAME% exceeded retry window after %RETRY_ATTEMPT% failures.
    exit /b 1
)
echo [%DATE% %TIME%] %RETRY_NAME% failed. Retry %RETRY_ATTEMPT% starts in %RETRY_DELAY_SECONDS% seconds.
>> "%DOWNLOAD_LOG%" echo [%DATE% %TIME%] %RETRY_NAME% failed; retry=%RETRY_ATTEMPT% delay_seconds=%RETRY_DELAY_SECONDS%.
powershell.exe -NoProfile -Command "Start-Sleep -Seconds %RETRY_DELAY_SECONDS%"
goto retry_task_loop

:download_qwen
call :download_hf_snapshot "Qwen2.5-VL-7B-Instruct" "Qwen/Qwen2.5-VL-7B-Instruct" "cc594898137f460bfe9f0759e9844b3ce807cfb5"
exit /b %errorlevel%

:download_showui
call :download_hf_snapshot "ShowUI-2B" "showlab/ShowUI-2B" "cabec4fcc48d15ffd3efe0b33ea9bc7d41509d60"
exit /b %errorlevel%

:download_hf_snapshot
set "HF_MODEL_NAME=%~1"
set "HF_REPO=%~2"
set "HF_REVISION=%~3"
set "HF_DEST=%MODEL_ROOT%\%~1"
echo.
echo [MODEL] %~1
echo Repository: %~2
echo Revision: %~3
if exist "%HF_DEST%\.ale-revision" (
    findstr /X /C:"%~3" "%HF_DEST%\.ale-revision" >nul 2>&1
    if not errorlevel 1 (
        echo Pinned revision marker found; verifying files and resuming missing shards.
    )
)
set "HF_HUB_DOWNLOAD_TIMEOUT=120"
set "HF_HUB_ETAG_TIMEOUT=30"
set "HF_XET_HIGH_PERFORMANCE=1"
%PYTHON_CMD% -c "import sys; from huggingface_hub import snapshot_download; snapshot_download(repo_id=sys.argv[1], revision=sys.argv[2], local_dir=sys.argv[3], max_workers=int(sys.argv[4]))" "%~2" "%~3" "%HF_DEST%" "%HF_WORKERS%"
if errorlevel 1 exit /b 1
> "%HF_DEST%\.ale-revision" echo %~3
exit /b 0

:ensure_huggingface_hub
where py.exe >nul 2>&1
if not errorlevel 1 set "PYTHON_CMD=py -3"
if defined PYTHON_CMD goto python_found
where python.exe >nul 2>&1
if not errorlevel 1 set "PYTHON_CMD=python"
if not defined PYTHON_CMD (
    echo ERROR: Python 3 is required. Install it from https://www.python.org/downloads/windows/
    exit /b 1
)

:python_found
%PYTHON_CMD% -c "import huggingface_hub" >nul 2>&1
if not errorlevel 1 exit /b 0
echo The Python package huggingface_hub is not installed.
if "%ASSUME_YES%"=="0" (
    choice /C YN /N /M "Install huggingface_hub with hf_xet now? [Y/N] "
    if errorlevel 2 exit /b 1
)
%PYTHON_CMD% -m pip install --user --upgrade "huggingface_hub[hf_xet]>=0.34,<2"
if errorlevel 1 exit /b 1
%PYTHON_CMD% -c "import huggingface_hub" >nul 2>&1
exit /b %errorlevel%

:download_sensevoice
set "SENSE_NAME=sherpa-onnx-sense-voice-zh-en-ja-ko-yue-int8-2024-07-17"
set "SENSE_ARCHIVE=%MODEL_ROOT%\.downloads\%SENSE_NAME%.tar.bz2"
set "SENSE_ARCHIVE_SHA=7d1efa2138a65b0b488df37f8b89e3d91a60676e416f515b952358d83dfd347e"
set "SENSE_MODEL_SHA=c71f0ce00bec95b07744e116345e33d8cbbe08cef896382cf907bf4b51a2cd51"
set "SENSE_TOKENS_SHA=f449eb28dc567533d7fa59be34e2abca8784f771850c78a47fb731a31429a1dc"
set "SENSE_URL=https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/%SENSE_NAME%.tar.bz2"
set "SENSE_DEST=%MODEL_ROOT%\SenseVoiceSmall"
set "SENSE_STAGE=%MODEL_ROOT%\.downloads\sensevoice-stage"
echo.
echo [MODEL] SenseVoiceSmall INT8
call :verify_sha256 "%SENSE_DEST%\model.int8.onnx" "%SENSE_MODEL_SHA%"
if errorlevel 1 goto fetch_sensevoice
call :verify_sha256 "%SENSE_DEST%\tokens.txt" "%SENSE_TOKENS_SHA%"
if errorlevel 1 goto fetch_sensevoice
echo Already installed and SHA-256 verified.
exit /b 0

:fetch_sensevoice
where curl.exe >nul 2>&1
if errorlevel 1 (
    echo ERROR: curl.exe is required to download SenseVoiceSmall.
    exit /b 1
)
where tar.exe >nul 2>&1
if errorlevel 1 (
    echo ERROR: tar.exe is required to extract SenseVoiceSmall.
    exit /b 1
)
if not exist "%MODEL_ROOT%\.downloads" mkdir "%MODEL_ROOT%\.downloads"
call :verify_sha256 "%SENSE_ARCHIVE%" "%SENSE_ARCHIVE_SHA%"
if not errorlevel 1 goto extract_sensevoice
if exist "%SENSE_ARCHIVE%" for %%I in ("%SENSE_ARCHIVE%") do if %%~zI GEQ 163002883 del /Q "%SENSE_ARCHIVE%"
curl.exe --fail --location --retry 5 --retry-delay 3 --speed-limit 1024 --speed-time 120 --continue-at - --output "%SENSE_ARCHIVE%" "%SENSE_URL%"
if errorlevel 1 exit /b 1
call :verify_sha256 "%SENSE_ARCHIVE%" "%SENSE_ARCHIVE_SHA%"
if errorlevel 1 (
    echo ERROR: SenseVoiceSmall archive SHA-256 mismatch.
    del /Q "%SENSE_ARCHIVE%" >nul 2>&1
    exit /b 1
)

:extract_sensevoice
if exist "%SENSE_STAGE%" rmdir /S /Q "%SENSE_STAGE%"
mkdir "%SENSE_STAGE%"
tar.exe -xjf "%SENSE_ARCHIVE%" -C "%SENSE_STAGE%"
if errorlevel 1 exit /b 1
if not exist "%SENSE_STAGE%\%SENSE_NAME%\model.int8.onnx" (
    echo ERROR: SenseVoiceSmall archive has an unexpected layout.
    exit /b 1
)
if not exist "%SENSE_DEST%" mkdir "%SENSE_DEST%"
copy /Y "%SENSE_STAGE%\%SENSE_NAME%\model.int8.onnx" "%SENSE_DEST%\model.int8.onnx" >nul
if errorlevel 1 exit /b 1
copy /Y "%SENSE_STAGE%\%SENSE_NAME%\tokens.txt" "%SENSE_DEST%\tokens.txt" >nul
if errorlevel 1 exit /b 1
copy /Y "%SENSE_STAGE%\%SENSE_NAME%\LICENSE" "%SENSE_DEST%\LICENSE" >nul
copy /Y "%SENSE_STAGE%\%SENSE_NAME%\README.md" "%SENSE_DEST%\README.md" >nul
call :verify_sha256 "%SENSE_DEST%\model.int8.onnx" "%SENSE_MODEL_SHA%"
if errorlevel 1 exit /b 1
call :verify_sha256 "%SENSE_DEST%\tokens.txt" "%SENSE_TOKENS_SHA%"
if errorlevel 1 exit /b 1
rmdir /S /Q "%SENSE_STAGE%"
echo SenseVoiceSmall installed and SHA-256 verified.
exit /b 0

:verify_sha256
if not exist "%~1" exit /b 1
set "VERIFY_FILE=%~1"
set "VERIFY_HASH=%~2"
powershell.exe -NoProfile -Command "if ((Get-FileHash -Algorithm SHA256 -LiteralPath $env:VERIFY_FILE).Hash -ieq $env:VERIFY_HASH) { exit 0 } else { exit 1 }" >nul 2>&1
exit /b %errorlevel%

:success
echo.
echo Download completed successfully.
echo Models directory: %MODEL_ROOT%
echo NOTE: Qwen and ShowUI become available only after the pinned Vulkan calibration passes.
>> "%DOWNLOAD_LOG%" echo [%DATE% %TIME%] Completed mode=%MODE%.
if "%PAUSE_ON_EXIT%"=="1" pause
exit /b 0

:usage_error
echo Usage: %~nx0 [all^|sensevoice^|qwen^|showui] [--models-dir PATH] [--workers 1-32] [--retry-hours N] [--yes^|--unattended]
exit /b 64

:failed
echo.
echo ERROR: Model download did not complete.
if defined DOWNLOAD_LOG (
    >> "%DOWNLOAD_LOG%" echo [%DATE% %TIME%] Failed mode=%MODE%.
)
if "%PAUSE_ON_EXIT%"=="1" pause
exit /b 1
