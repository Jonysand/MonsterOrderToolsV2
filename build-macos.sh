#!/usr/bin/env bash
# macOS 本地一键打包（Apple Silicon）：产出 .app + .dmg，与 CI bundle-macos 同口径
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
  echo "[1/2] 未检测到 node_modules，执行 npm ci 安装前端依赖..."
  npm ci
fi

# 版本号取自 src-tauri/tauri.conf.json；beforeBuildCommand 会自动先跑 npm run build
echo "[2/2] 生产打包：npm run tauri build（首次编译耗时较长）"
npm run tauri build

APP="src-tauri/target/release/bundle/macos/MonsterOrderWilds-Ascendance.app"
DMG_DIR="src-tauri/target/release/bundle/dmg"

echo
echo "构建完成，产物："
if [ -d "$APP" ]; then
  echo "  App: $PWD/$APP"
fi
if [ -d "$DMG_DIR" ]; then
  for f in "$DMG_DIR"/*.dmg; do
    if [ -e "$f" ]; then
      echo "  DMG: $PWD/$f"
    fi
  done
fi
echo "注：产物为 adhoc 签名、未公证，换机首次打开需右键「打开」放行。"
