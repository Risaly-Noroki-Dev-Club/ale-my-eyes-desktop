@echo off
setlocal
cd /d "%~dp0\..\.."
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0run-windows-modeld.ps1" %*
set "ALE_EXIT=%ERRORLEVEL%"
echo.
echo The report ZIP is under target\model-runtime-reports.
pause
exit /b %ALE_EXIT%
