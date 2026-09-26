@echo off
setlocal EnableExtensions

rem =====================================================================
rem 凭据文件生成器（双击运行）：
rem   1. 读取本目录下的 credentials.json（明文模板，已被 .gitignore 排除）
rem   2. 生成 ..\MonsterOrderWilds_configs\credentials.dat，随安装包打包
rem 算法与 src-tauri\src\credentials.rs 及原工程完全对称
rem （Base64 + @MonsterOrderSecret@ 魔数 + HMAC-SHA256 防篡改签名）。
rem
rem 编码说明：本文件必须保持 ANSI/GBK 编码，请勿另存为 UTF-8
rem （原因同 build-windows.bat 头部注释）。
rem =====================================================================

cd /d "%~dp0"

if not exist "credentials.json" (
  echo [ERROR] 未找到 credentials.json：
  echo         请复制 credentials.example.json 为 credentials.json 并填入真实凭据。
  pause
  exit /b 1
)

where python >nul 2>&1
if errorlevel 1 (
  where py >nul 2>&1
  if errorlevel 1 (
    echo [ERROR] 未找到 python，请先安装 Python 并确保其在 PATH 中。
    pause
    exit /b 1
  )
  py "generate_credentials.py"
) else (
  python "generate_credentials.py"
)
if errorlevel 1 (
  echo.
  echo [ERROR] 凭据文件生成失败，请检查上方日志。
  pause
  exit /b 1
)

echo.
echo 下一步：运行仓库根目录的 build-windows.bat（或 build-macos.sh）打包，
echo         credentials.dat 将随安装包分发。
pause
exit /b 0
