@echo off
setlocal
cd /d "%~dp0"
if not exist "src-tauri\target\release\novel-overlay.exe" (
  echo 未找到构建产物，请先执行构建：src-tauri\target\release\novel-overlay.exe
  pause
  exit /b 1
)
start "" "%~dp0src-tauri\target\release\novel-overlay.exe"
endlocal
