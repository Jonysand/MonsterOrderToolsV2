@echo off
setlocal EnableExtensions

rem =====================================================================
rem Windows 本地一键打包：产出裸 exe + NSIS 安装包 + MSI 安装包，
rem 与 macOS 端 build-macos.sh 同口径（tauri bundle targets: all）。
rem 用法：双击运行，或在终端执行 build-windows.bat
rem
rem 编码说明：本文件必须保持 ANSI/GBK 编码。cmd 对超过单个读取缓冲区
rem （512 字节）的 UTF-8 批处理存在多字节字符位置解析缺陷，会导致后续
rem 行错位执行；GBK 编码无此问题。请勿将本文件另存为 UTF-8。
rem =====================================================================

cd /d "%~dp0"

rem rustup 默认装到 %USERPROFILE%\.cargo\bin，显式补上以防不在 PATH 中
set "PATH=%USERPROFILE%\.cargo\bin;%PATH%"

for %%T in (node npm cargo) do (
  where %%T >nul 2>&1
  if errorlevel 1 (
    echo [ERROR] 未找到 %%T，请先安装并确保其在 PATH 中
    goto :fail
  )
)

if not exist node_modules (
  echo [1/2] 未检测到 node_modules，执行 npm ci 安装前端依赖...
  call npm ci
  if errorlevel 1 goto :fail
)

rem 版本号取自 src-tauri/tauri.conf.json；beforeBuildCommand 会自动先跑 npm run build
echo [2/2] 生产打包：npm run tauri build（首次编译耗时较长）
call npm run tauri build
if errorlevel 1 goto :fail

set "EXE=%CD%\src-tauri\target\release\MonsterOrderWilds-Ascendance.exe"
set "NSIS_DIR=src-tauri\target\release\bundle\nsis"
set "MSI_DIR=src-tauri\target\release\bundle\msi"

echo.
echo 构建完成，产物：
if exist "%EXE%" echo   裸程序: %EXE%
if exist "%NSIS_DIR%\*-setup.exe" for %%F in ("%NSIS_DIR%\*-setup.exe") do echo   NSIS 安装包: %%~fF
if exist "%MSI_DIR%\*.msi" for %%F in ("%MSI_DIR%\*.msi") do echo   MSI 安装包: %%~fF
echo 注：产物未做代码签名，分发后首次运行可能触发 SmartScreen 提示。

pause
exit /b 0

:fail
echo.
echo [ERROR] 构建失败，请检查上方日志。
pause
exit /b 1
