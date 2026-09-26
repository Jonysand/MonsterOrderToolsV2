# -*- coding: utf-8 -*-
"""CI/Release 构建前 credentials.dat 准备脚本（GitHub Actions 专用）。

tauri.conf.json 将 credentials.dat 声明为随包资源，tauri-build 在编译期校验其存在；
但该加密凭据按安全规范不入库（.gitignore），CI 检出后不存在，故构建前须先生成。

取值优先级：
  1. 环境变量 CREDENTIALS_JSON_B64 非空 → Base64 解码为明文凭据（发布真实凭据通道：
     在 GitHub Secrets 配置 scripts/credentials.json 明文内容的 Base64，仓库与
     workflow 日志均不落明文）；
  2. 本机 scripts/credentials.json 存在 → 交由官方生成器 generate_credentials.py 处理；
  3. 均无 → 以 credentials.example.json 占位值生成（仅满足构建门禁，随包凭据为无效
     占位，不影响单测与安装包产出）。

防覆盖：仅第 3 种情形且 credentials.dat 已存在时跳过生成，避免占位值覆盖发行方
本机已生成的真实凭据。
"""

import base64
import json
import os
import sys
from pathlib import Path

SCRIPTS_DIR = Path(__file__).resolve().parent
sys.path.insert(0, str(SCRIPTS_DIR))

import generate_credentials as gen  # noqa: E402

SECRET_ENV = "CREDENTIALS_JSON_B64"
EXAMPLE_INPUT = SCRIPTS_DIR / "credentials.example.json"


def masked_summary(data: dict) -> None:
    print("     字段摘要（脱敏）:")
    print(f"       APP_ID            = {data.get('APP_ID', '')}")
    print(f"       ACCESS_KEY_ID     = {gen.mask(str(data.get('ACCESS_KEY_ID', '')))}")
    print(f"       ACCESS_KEY_SECRET = {gen.mask(str(data.get('ACCESS_KEY_SECRET', '')))}")
    print(f"       manbo_api_key     = {gen.mask(str(data.get('manbo_api_key', '')))}")
    print(f"       chat_provider     = {data.get('chat_provider', '')}")
    print(f"       chat_api_key      = {gen.mask(str(data.get('chat_api_key', '')))}")


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
