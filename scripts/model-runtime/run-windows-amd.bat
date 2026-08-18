@echo off
setlocal EnableExtensions

title Ale, My Eyes! - Windows AMD Model Runtime Acceptance
echo Ale, My Eyes! Windows AMD model runtime acceptance
echo This may take several hours on the first run while models are converted.
echo.

powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0run-windows-amd.ps1"
set "ALE_TEST_EXIT=%ERRORLEVEL%"

echo.
if "%ALE_TEST_EXIT%"=="0" (
    echo Acceptance completed successfully.
) else (
    echo Acceptance finished with exit code %ALE_TEST_EXIT%.
)
echo The report ZIP is under target\model-runtime-reports.
pause
exit /b %ALE_TEST_EXIT%
