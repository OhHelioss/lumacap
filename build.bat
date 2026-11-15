@echo off
cd /d "%~dp0"
cargo build --release
if errorlevel 1 (
  echo.
  echo BUILD FAILED - scroll up for the error, or screenshot it.
  pause
  exit /b 1
)
copy /y target\release\lumacap.exe lumacap.exe >nul
echo.
echo Done: %~dp0lumacap.exe
pause
