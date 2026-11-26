@echo off
rem Bypass for this process only, so boot.ps1 and play.ps1 can run unsigned.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0boot.ps1" %*
