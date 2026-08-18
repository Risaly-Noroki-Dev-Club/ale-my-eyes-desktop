@echo off
setlocal
cd /d "%~dp0\..\.."
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0run-windows-modeld.ps1" %*
set "ALE_EXIT=%ERRORLEVEL%"
echo.
if "%ALE_EXIT%"=="0" (
    echo The report ZIP is under target\model-runtime-reports.
) else (
    echo Modeld acceptance did not complete; no new report was generated.
)
pause
exit /b %ALE_EXIT%
