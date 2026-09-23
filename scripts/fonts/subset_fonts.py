#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""点怪悬浮窗字体子集化。

输入：设计稿目录里的原始字体（Source Han Serif/Sans VF + DM Serif Display）
输出：src/assets/fonts/ 下随包分发的子集 woff2

字符集策略（都不含完整 CJK 字表，避免 18MB 全量进包）：
  serif（怪物名/面板标题/钤印）：怪物表规范名 + 全部别名 + 前端文案用字 + 拉丁数字标点
  sans （昵称/正文/徽章）      ：GB2312 一级常用字 + 前端文案 + 怪物表 + 分词词典 + 拉丁数字标点
  DM Serif Display             ：拉丁整包（仅 30KB，无需子集化）

用法（Windows / macOS / Linux 通用，先准备 venv）：
  python -m venv .venv
  .venv/bin/python -m pip install "fonttools[woff]" brotli      # macOS/Linux
  .venv\\Scripts\\python -m pip install "fonttools[woff]" brotli  # Windows
  .venv/bin/python scripts/fonts/subset_fonts.py                 # macOS/Linux
  .venv\\Scripts\\python scripts/fonts/subset_fonts.py            # Windows

换用外部常用字表（如《通用规范汉字表》一级字表）：
  .venv/bin/python scripts/fonts/subset_fonts.py --extra-file path/to/charset.txt
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

try:
    from fontTools import subset
except ImportError:  # pragma: no cover - 环境未准备时的友好提示
    sys.exit(
        "缺少 fontTools。请先创建 venv 并安装：\n"
        '  python -m venv .venv\n'
        '  .venv/bin/python -m pip install "fonttools[woff]" brotli   # macOS/Linux\n'
        '  .venv\\Scripts\\python -m pip install "fonttools[woff]" brotli   # Windows'
    )

REPO_ROOT = Path(__file__).resolve().parents[2]

# 拉丁可打印字符 + 数字
ASCII_PRINTABLE = "".join(chr(c) for c in range(0x20, 0x7F))

# 前端与日志里出现的标点/符号（GB2312 之外的部分，缺了会退化成替代字形）
PUNCTUATION = (
    "　、。〃々〈〉《》「」『』【】〔〕〖〗！＂＃％＆＇（）＊＋，－．／：；＜＝＞？＠［］＾＿｀｛｜｝～"
    "·—–…‘’“”„†‡•‰′″‹›⁄€￥°±×÷≈≡≤≥√∞∝∴∵⊥∠△○●★☆◆◇□■▲▼►◄←→↑↓⇒⇔"
    "✓✕✗✔✘☑☐⚠※§¶©®™№"
)

# 前端固定文案会被衬线体渲染的部分（面板标题、完成钤印）；
# 其余衬线用字由怪物表与前端文案扫描自动覆盖
SERIF_FIXED_TEXT = "狩猎点单队列討伐完了"


def gb2312_level1() -> str:
    """GB2312 一级汉字（3755 个常用字），与《通用规范汉字表》一级字表高度重合。"""
    chars = []
    for hi in range(0xB0, 0xD8):
        for lo in range(0xA1, 0xFF):
            try:
                chars.append(bytes((hi, lo)).decode("gb2312"))
            except UnicodeDecodeError:
                continue
    return "".join(chars)


def scan_corpus(globs: list[str]) -> set[str]:
    """扫描仓库语料，收集非 ASCII 字符（UI 文案、怪物名、分词词典）。"""
    found: set[str] = set()
    for pattern in globs:
        for path in REPO_ROOT.glob(pattern):
            if not path.is_file():
                continue
            try:
                text = path.read_text(encoding="utf-8", errors="ignore")
            except OSError:
                continue
            found.update(c for c in text if ord(c) > 0x2000)
    return found


def monster_chars() -> set[str]:
    """怪物表规范名 + 全部别名（别名可能被原样回显，故一并收）。"""
    import json

    path = REPO_ROOT / "MonsterOrderWilds_configs" / "monster_list.json"
    if not path.is_file():
        raise SystemExit(f"找不到怪物表：{path}")
    data = json.loads(path.read_text(encoding="utf-8"))
    chars: set[str] = set()
    for name, meta in data.items():
        chars.update(c for c in name if ord(c) > 0x2000)
        for alias in meta.get("别称", []) or []:
            chars.update(c for c in str(alias) if ord(c) > 0x2000)
    return chars


def build_charsets(extra_file: Path | None) -> dict[str, str]:
    ui = scan_corpus(
        [
            "src/**/*.tsx",
            "src/**/*.ts",
            "src/**/*.css",
            "index.html",
            "MonsterOrderWilds_configs/dict/*",
        ]
    )
    monsters = monster_chars()
    extra = set(extra_file.read_text(encoding="utf-8")) if extra_file else set()
    extra = {c for c in extra if ord(c) > 0x2000}

    common = set(gb2312_level1())
    tail = set(ASCII_PRINTABLE) | set(PUNCTUATION)
    print(f"  语料用字 {len(ui)} · 怪物名/别名 {len(monsters)} · GB2312 一级 {len(common)}")
    if extra:
        print(f"  外部字表补充 {len(extra)}")

    sans = common | ui | monsters | extra | tail
    # 衬线体只服务怪物名与固定衬线文案（设计说明 §七.7）：缺字由 CSS 回落链交给内嵌黑体
    serif = monsters | extra | tail | set(SERIF_FIXED_TEXT)
    return {"sans": "".join(sorted(sans)), "serif": "".join(sorted(serif))}


def rename_family(font, family: str, subfamily: str = "Regular") -> None:
    """把修改版字体的可写名称改成自有字族名。

    思源系列与 DM Serif Display 的 OFL 声明了保留字名（Reserved Font Name 'Source'）；
    子集属于修改版，按 OFL 1.1 §3 不得沿用该名，故重写 nameID 1/2/3/4/6/16/17。
    版权与许可记录（nameID 0 / 13 / 14）原样保留，满足 OFL 随附声明的要求。
    """
    name = font["name"]
    ps_name = f"{family}-{subfamily}".replace(" ", "")
    patches = {
        1: family,
        2: subfamily,
        3: f"{ps_name};subset",
        4: f"{family} {subfamily}",
        6: ps_name,
        16: family,
        17: subfamily,
    }
    for name_id, value in patches.items():
        for record in [r for r in name.names if r.nameID == name_id]:
            name.names.remove(record)
        name.setName(value, name_id, 3, 1, 0x409)
    if "CFF " in font:  # 非 CFF2 的静态 OTF 需要在 CFF 里同步 PostScript 名
        font["CFF "].cff.fontNames = [ps_name]


def subset_font(src: Path, dst: Path, text: str, family: str) -> None:
    options = subset.Options()
    options.flavor = "woff2"
    options.layout_features = ["*"]
    # 保留版权与许可等 name 记录（思源系列为 SIL OFL 1.1，再分发需随附声明）
    options.name_IDs = ["*"]
    options.name_legacy = True
    options.name_languages = ["*"]
    options.notdef_outline = True

    font = subset.load_font(str(src), options)
    try:
        subsetter = subset.Subsetter(options=options)
        subsetter.populate(text=text)
        subsetter.subset(font)
        rename_family(font, family)
        dst.parent.mkdir(parents=True, exist_ok=True)
        subset.save_font(font, str(dst), options)
    finally:
        font.close()


def main() -> int:
    parser = argparse.ArgumentParser(description="点怪悬浮窗字体子集化")
    parser.add_argument(
        "--src-dir",
        type=Path,
        default=REPO_ROOT.parent / "mg-queue-ui-optimize" / "fonts",
        help="原始字体目录（默认取同级设计稿目录）",
    )
    parser.add_argument(
        "--out-dir",
        type=Path,
        default=REPO_ROOT / "src" / "assets" / "fonts",
        help="子集输出目录",
    )
    parser.add_argument(
        "--extra-file",
        type=Path,
        default=None,
        help="额外字符集文本（如《通用规范汉字表》一级字表），逐字符并入",
    )
    parser.add_argument("--dump-charset", type=Path, default=None, help="导出字符集以便人工审阅")
    args = parser.parse_args()

    if not args.src_dir.is_dir():
        print(f"找不到原始字体目录：{args.src_dir}")
        return 1

    print("字符集统计：")
    charsets = build_charsets(args.extra_file)

    if args.dump_charset:
        args.dump_charset.write_text(
            "# serif\n" + charsets["serif"] + "\n\n# sans\n" + charsets["sans"] + "\n",
            encoding="utf-8",
        )
        print(f"  字符集已导出：{args.dump_charset}")

    jobs = [
        ("SourceHanSerifCN-VF.otf.woff2", "MH-Serif-SC-MonsterNames.woff2", charsets["serif"], "MH Serif SC"),
        ("SourceHanSansCN-VF.otf.woff2", "MH-Sans-SC-Common.woff2", charsets["sans"], "MH Sans SC"),
        # DM Serif Display 原样分发（未修改，无需改名）
        ("DMSerifDisplay-Regular.woff2", "DMSerifDisplay-Regular.woff2", None, None),
    ]

    print("子集化：")
    for src_name, dst_name, text, family in jobs:
        src = args.src_dir / src_name
        dst = args.out_dir / dst_name
        if not src.is_file():
            print(f"  缺失源文件：{src}")
            return 1
        before = src.stat().st_size
        if text is None:
            dst.parent.mkdir(parents=True, exist_ok=True)
            dst.write_bytes(src.read_bytes())
        else:
            subset_font(src, dst, text, family)
        after = dst.stat().st_size
        print(
            f"  {dst_name}: {before / 1024:.0f}KB -> {after / 1024:.0f}KB"
            f"（{len(text) if text else 0} 字符）"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
