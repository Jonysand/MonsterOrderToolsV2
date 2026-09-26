# -*- coding: utf-8 -*-
"""credentials.dat 生成器（与原工程 scripts/generate_credentials.py 算法逐步骤对称）。

用法：
  1. 复制 scripts/credentials.example.json 为 scripts/credentials.json（明文模板，
     已被 .gitignore 排除，永不入库），填入发行方持有的真实凭据；
  2. 在仓库根目录执行：python scripts/generate_credentials.py
  3. 产物写入 MonsterOrderWilds_configs/credentials.dat，随 `npm run tauri build`
     打进安装包（tauri.conf.json bundle.resources）。

文件格式（与 MonsterOrderWilds 原工程及 V2 Rust 侧 credentials.rs 完全互通）：
  credentials.dat = Base64( "@MonsterOrderSecret@" + HMAC-SHA256_hex(64字符) + JSON明文 )
  HMAC 密钥为源码常量 SALT；该机制提供混淆与防篡改校验，非对称加密。
"""

import base64
import hashlib
import hmac
import json
import sys
from pathlib import Path

FILE_MAGIC = "@MonsterOrderSecret@"
SALT = "@M0nst3r$Alt@"

REPO_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_INPUT = Path(__file__).resolve().parent / "credentials.json"
DEFAULT_OUTPUT = REPO_ROOT / "MonsterOrderWilds_configs" / "credentials.dat"

# 必填：缺任一项 B 站开播连接即不可用（连接签名三件套）
REQUIRED_KEYS = ("APP_ID", "ACCESS_KEY_ID", "ACCESS_KEY_SECRET")


def write_info_file(json_plain: str, out_path: Path) -> None:
    """Base64( MAGIC + HMAC_hex + JSON ) 写盘，与 Rust save_credentials 完全同构。"""
    hmac_hex = hmac.new(SALT.encode(), json_plain.encode(), hashlib.sha256).hexdigest()
    combined = FILE_MAGIC + hmac_hex + json_plain
    encoded = base64.b64encode(combined.encode())
    out_path.parent.mkdir(parents=True, exist_ok=True)
    out_path.write_bytes(encoded)


def mask(value: str) -> str:
    if not value:
        return "(空)"
    if len(value) <= 8:
        return "*" * len(value)
    return value[:4] + "***" + value[-4:]


def main() -> int:
    if not DEFAULT_INPUT.exists():
        print(f"[ERROR] 未找到明文凭据模板: {DEFAULT_INPUT}")
        print("        请复制 scripts/credentials.example.json 为 scripts/credentials.json 并填入真实凭据。")
        return 1

    try:
        data = json.loads(DEFAULT_INPUT.read_text(encoding="utf-8-sig"))
    except (json.JSONDecodeError, UnicodeDecodeError) as e:
        print(f"[ERROR] 解析 {DEFAULT_INPUT} 失败: {e}")
        return 1

    missing = [k for k in REQUIRED_KEYS if not str(data.get(k, "")).strip()]
    if missing:
        print(f"[ERROR] 必填字段缺失或为空: {', '.join(missing)}（缺少则 B 站开播连接不可用）")
        return 1

    placeholders = [
        k for k, v in data.items()
        if isinstance(v, str) and ("REPLACE_ME" in v or "在此填写" in v)
    ]
    if placeholders:
        print(f"[WARN] 以下字段仍为模板占位值: {', '.join(placeholders)}（对应能力将不可用）")

    # 与 V2 Rust save_credentials 一致采用紧凑 JSON；验签对原文进行，格式差异不影响互通
    json_plain = json.dumps(data, ensure_ascii=False, separators=(",", ":"))
    write_info_file(json_plain, DEFAULT_OUTPUT)

    print(f"[OK] 已生成 {DEFAULT_OUTPUT}")
    print("     字段摘要（脱敏）:")
    print(f"       APP_ID            = {data.get('APP_ID', '')}")
    print(f"       ACCESS_KEY_ID     = {mask(str(data.get('ACCESS_KEY_ID', '')))}")
    print(f"       ACCESS_KEY_SECRET = {mask(str(data.get('ACCESS_KEY_SECRET', '')))}")
    print(f"       mimo_tts_api_key  = {mask(str(data.get('mimo_tts_api_key', '')))}")
    print(f"       manbo_api_key     = {mask(str(data.get('manbo_api_key', '')))}")
    print(f"       chat_provider     = {data.get('chat_provider', '')}")
    print(f"       chat_api_key      = {mask(str(data.get('chat_api_key', '')))}")
    print("提示: scripts/credentials.json 为明文敏感文件，请勿提交或外发。")
    return 0


if __name__ == "__main__":
    sys.exit(main())
