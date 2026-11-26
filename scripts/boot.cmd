@echo off
rem Bypass for this process only, so boot.ps1 and play.ps1 can run unsigned.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0boot.ps1" %*
rem Keep a double-clicked window open so the output (or the error) can be read.
echo.
echo boot finished (exit %ERRORLEVEL%). Crash reports, if any: %LOCALAPPDATA%\victory\data\7952987A\temp
pause
