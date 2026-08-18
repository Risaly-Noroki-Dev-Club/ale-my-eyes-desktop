@echo off
setlocal
cd /d "%~dp0\..\.."
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0run-controlled-window-test.ps1" %*
set "ALE_EXIT=%ERRORLEVEL%"
echo.
pause
exit /b %ALE_EXIT%
