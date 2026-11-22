@echo off
rem One-click start for a Rustlink node plus the example music bot.
rem Double-click this file; it runs start.ps1 next to it.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0start.ps1" %*
if errorlevel 1 pause
