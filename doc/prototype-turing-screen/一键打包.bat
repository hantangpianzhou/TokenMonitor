@echo off
chcp 65001 >nul
cd /d "%~dp0"

set PY=C:\Users\Lenovo\.workbuddy\binaries\python\versions\3.13.12\python.exe

echo ============================================
echo   TokenScreen 一键打包（方案1 / PyInstaller）
echo ============================================
echo.

"%PY%" build_exe.py
if errorlevel 1 (
    echo.
    echo [失败] 打包未成功，请看上方报错。
    pause
    exit /b 1
)

echo.
echo [完成] 产物在 dist\TokenScreen.exe
echo 本地自检：dist\TokenScreen.exe --demo --dry-run --once
echo.
pause
