@echo off
setlocal
cd /d "%~dp0\.."
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0build-windows.ps1" %*
set "ALE_EXIT=%ERRORLEVEL%"
echo.
if not "%ALE_EXIT%"=="0" echo Windows build failed. Review the error above.
pause
exit /b %ALE_EXIT%
