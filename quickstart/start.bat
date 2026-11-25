@echo off
setlocal
rem One-click start for a Rustlink node plus the example music bot.
rem Double-click this file; it runs start.ps1 next to it.
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0start.ps1" %*
set "RUSTLINK_EXIT=%ERRORLEVEL%"
if not "%RUSTLINK_EXIT%"=="0" (
    if not "%RUSTLINK_NO_PAUSE%"=="1" pause
)
exit /b %RUSTLINK_EXIT%
