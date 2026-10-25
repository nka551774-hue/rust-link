@echo off
setlocal
powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File "%~dp0start.ps1" %*
set "RUSTLINK_EXIT=%ERRORLEVEL%"
if not "%RUSTLINK_EXIT%"=="0" (
    echo Rustlink exited with code %RUSTLINK_EXIT%.
    if "%~1"=="" if not "%RUSTLINK_NO_PAUSE%"=="1" pause
)
exit /b %RUSTLINK_EXIT%
