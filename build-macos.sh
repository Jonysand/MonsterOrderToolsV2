#!/usr/bin/env bash
# macOS 本地一键打包（Apple Silicon）：连产「完整版」与「Lite 纯排队版」两套 .app + .dmg，
# 与 Windows 端 build-windows.bat 同口径
# Lite 版 = Cargo feature `lite` + src-tauri/tauri.conf.lite.json（编译期定型，运行期不可切换）
# 用法：./build-macos.sh
set -euo pipefail

cd "$(dirname "$0")"

# rustup 默认装到 ~/.cargo/bin，非交互 shell 的 PATH 里没有 cargo，需显式补上
export PATH="$HOME/.cargo/bin:$PATH"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "[ERROR] 本脚本仅用于 macOS 构建，当前系统为 $(uname -s)" >&2
  exit 1
fi

for tool in node npm cargo; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "[ERROR] 未找到 $tool，请先安装并确保其在 PATH 中" >&2
    exit 1
  fi
done

if [ ! -d node_modules ]; then
  echo "[1/4] 未检测到 node_modules，执行 npm ci 安装前端依赖..."
  npm ci
fi

# 版本号取自 src-tauri/tauri.conf.json；beforeBuildCommand 会自动先跑前端构建
# --- 凭据预置检查：除开播身份码外的全部凭据由发行方打包进 credentials.dat，缺失即终止构建 ---
if [ ! -f MonsterOrderWilds_configs/credentials.dat ]; then
  if [ -f scripts/credentials.json ]; then
    echo "[2/4] 未检测到 credentials.dat，正在由 scripts/credentials.json 生成..."
    python3 scripts/generate_credentials.py
  else
    echo "[ERROR] 未检测到 MonsterOrderWilds_configs/credentials.dat，无法打包内置凭据。" >&2
    echo "        请填写 scripts/credentials.json 后运行: python3 scripts/generate_credentials.py" >&2
    exit 1
  fi
fi

echo "[3/4] 完整版生产打包：npm run tauri build（首次编译耗时较长）"
npm run tauri build

echo "[4/4] Lite 纯排队版生产打包：npm run tauri build -- --features lite --config src-tauri/tauri.conf.lite.json"
npm run tauri build -- --features lite --config src-tauri/tauri.conf.lite.json

APP="src-tauri/target/release/bundle/macos/MonsterOrderWilds-Ascendance.app"
APP_LITE="src-tauri/target/release/bundle/macos/MonsterOrderWilds-Ascendance-Lite.app"
DMG_DIR="src-tauri/target/release/bundle/dmg"

echo
echo "构建完成，产物："
if [ -d "$APP" ]; then
  echo "  [完整版] App: $PWD/$APP"
fi
if [ -d "$APP_LITE" ]; then
  echo "  [Lite版] App: $PWD/$APP_LITE"
fi
if [ -d "$DMG_DIR" ]; then
  for f in "$DMG_DIR"/*.dmg; do
    if [ -e "$f" ]; then
      echo "  DMG: $PWD/$f"
    fi
  done
fi
echo "注：产物为 adhoc 签名、未公证，换机首次打开需右键「打开」放行。"
