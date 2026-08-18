@echo off
setlocal
cd /d "%~dp0\..\.."
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0setup-windows-test.ps1" -Install
set "ALE_EXIT=%ERRORLEVEL%"
echo.
if not "%ALE_EXIT%"=="0" echo Setup did not complete. Review the messages above.
pause
exit /b %ALE_EXIT%
