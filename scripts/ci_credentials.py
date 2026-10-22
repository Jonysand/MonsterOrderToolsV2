# -*- coding: utf-8 -*-
"""CI/Release 构建前 credentials.dat 准备脚本（GitHub Actions 专用）。

tauri.conf.json 将 credentials.dat 声明为随包资源，tauri-build 在编译期校验其存在；
但该加密凭据按安全规范不入库（.gitignore），CI 检出后不存在，故构建前须先生成。

取值优先级：
  1. 环境变量 CREDENTIALS_JSON_B64 非空 → Base64 解码为明文凭据（发布真实凭据通道：
     在 GitHub Secrets 配置 scripts/credentials.json 明文内容的 Base64，仓库与
     workflow 日志均不落明文）；
  2. 本机 scripts/credentials.json 存在 → 交由官方生成器 generate_credentials.py 处理；
  3. 均无 → CI 构建（编译/单测）以 credentials.example.json 占位值生成，仅满足构建门禁；
     发布构建设置 REQUIRE_REAL_CREDENTIALS=1 时**直接失败**，绝不把占位凭据发给用户。

防覆盖：仅第 3 种情形且 credentials.dat 已存在时跳过生成，避免占位值覆盖发行方
本机已生成的真实凭据。
"""

import base64
import json
import os
import sys
from pathlib import Path

# GitHub 的英文 Windows runner stdout 默认 cp1252，中文打印会抛 UnicodeEncodeError
# （本机中文 GBK 反而不触发）；强制 UTF-8 并兜底替换，含 import 进来的生成器打印
for _stream in (sys.stdout, sys.stderr):
    _stream.reconfigure(encoding="utf-8", errors="replace")

SCRIPTS_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPTS_DIR))

import generate_credentials as gen  # noqa: E402

SECRET_ENV = "CREDENTIALS_JSON_B64"
REQUIRE_REAL_ENV = "REQUIRE_REAL_CREDENTIALS"
EXAMPLE_INPUT = SCRIPTS_DIR / "credentials.example.json"


def require_real_credentials() -> bool:
    """发布构建必须注入真实凭据：缺 Secret 时直接失败，避免把占位凭据发给用户。"""
    return os.environ.get(REQUIRE_REAL_ENV, "").strip().lower() in ("1", "true", "yes", "on")


def _status(data: dict, key: str) -> str:
    if key not in data:
        return "(已移除，运行时回退注册表/配置)"
    v = str(data[key])
    return f"已配置(len={len(v)})" if v else "缺失"


def masked_summary(data: dict) -> None:
    # 仓库与 Actions 日志都是公开的：只输出存在性/长度，绝不输出任何密钥片段
    print("     字段摘要（仅状态，不输出密钥片段）:")
    print(f"       APP_ID            = {'已配置' if data.get('APP_ID') else '缺失'}")
    print(f"       ACCESS_KEY_ID     = {_status(data, 'ACCESS_KEY_ID')}")
    print(f"       ACCESS_KEY_SECRET = {_status(data, 'ACCESS_KEY_SECRET')}")
    print(f"       manbo_api_key     = {_status(data, 'manbo_api_key')}")
    print(f"       chat_provider     = {data.get('chat_provider', '')}")
    print(f"       chat_api_key      = {_status(data, 'chat_api_key')}")


def main() -> int:
    b64 = os.environ.get(SECRET_ENV, "").strip()

    if b64:
        try:
            data = json.loads(base64.b64decode(b64).decode("utf-8-sig"))
        except (ValueError, UnicodeDecodeError, json.JSONDecodeError) as e:
            print(f"[ERROR] Secret {SECRET_ENV} Base64 解码或 JSON 解析失败: {e}")
            return 1
        missing = [k for k in gen.REQUIRED_KEYS if not str(data.get(k, "")).strip()]
        if missing:
            print(f"[ERROR] Secret 注入的凭据必填字段缺失或为空: {', '.join(missing)}")
            return 1
        bad_required = [k for k in gen.REQUIRED_KEYS if gen.is_placeholder(data.get(k))]
        if bad_required:
            print(
                f"[ERROR] Secret 注入的凭据必填字段仍是模板占位值: {', '.join(bad_required)}"
            )
            return 1
        # 可选密钥的占位值不能写进产物：Manbo 会拿占位文本请求服务端并得到 401，
        # 再静默降级本地语音 —— 运行时回退注册表/配置才是正确姿态
        for k in ("manbo_api_key", "chat_api_key"):
            if gen.is_placeholder(data.get(k)):
                data.pop(k, None)
                print(
                    f"[WARN] Secret 中的 {k} 仍是模板占位值：已从产物移除"
                    "（运行时回退注册表/配置镜像）"
                )
        gen.write_info_file(
            json.dumps(data, ensure_ascii=False, separators=(",", ":")),
            gen.DEFAULT_OUTPUT,
        )
        print(f"[OK] 已由 Secret {SECRET_ENV} 生成 {gen.DEFAULT_OUTPUT}")
        masked_summary(data)
        return 0

    if gen.DEFAULT_INPUT.exists():
        print(f"[SKIP] 检测到本机 {gen.DEFAULT_INPUT.name}，交由官方生成器处理")
        return gen.main()

    if require_real_credentials():
        print(
            f"[ERROR] 发布构建要求真实凭据，但未配置 Secret {SECRET_ENV}，"
            f"且本机不存在 {gen.DEFAULT_INPUT.name}"
        )
        print("        拒绝生成占位凭据（否则安装包内的语音/开播能力会静默失效）。")
        return 1

    if gen.DEFAULT_OUTPUT.exists():
        print(
            f"[SKIP] {gen.DEFAULT_OUTPUT} 已存在且未配置 {SECRET_ENV}，"
            "跳过占位生成以免覆盖真实凭据"
        )
        return 0

    data = json.loads(EXAMPLE_INPUT.read_text(encoding="utf-8-sig"))
    gen.write_info_file(
        json.dumps(data, ensure_ascii=False, separators=(",", ":")),
        gen.DEFAULT_OUTPUT,
    )
    print(
        f"[OK] 未配置 {SECRET_ENV}，已由占位模板 {EXAMPLE_INPUT.name} 生成 "
        f"{gen.DEFAULT_OUTPUT}（随包凭据为无效占位，仅满足构建门禁）"
    )
    masked_summary(data)
    return 0


if __name__ == "__main__":
    sys.exit(main())
