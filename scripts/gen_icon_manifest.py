# -*- coding: utf-8 -*-
"""生成图标清单：扫描随包图标目录 → src/generated/*.ts

后端在打包态看不到前端静态目录，图标选择器与随机抽选盘的数据源只能在构建期固化。
产物提交入库（保证 CI 与本地一致），编码为 UTF-8 with BOM（见 check_encoding.py）。

当前两处清单：
  public/monster_icons/**/*.png → src/generated/iconManifest.ts    （怪物图鉴库 / 选怪面板 / 抽选盘怪物池）
  public/weapon_icons/**/*.png  → src/generated/weaponIconManifest.ts（随机抽选盘的武器池）
"""
import codecs
import io
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BOM = b"\xef\xbb\xbf"


def console_encoding():
    """stdout 直连控制台时返回其代码页；管道、重定向或代码页不可用时返回 None（此时按 UTF-8 输出）。"""
    if not sys.stdout.isatty():
        return None
    if os.name == "nt":
        try:
            import ctypes

            code_page = int(ctypes.windll.kernel32.GetConsoleOutputCP())
        except Exception:
            return None
        if code_page <= 0:
            return None
        name = "cp%d" % code_page
        try:
            codecs.lookup(name)
        except LookupError:
            # py2 不认识 cp65001 之类的名字；此时控制台本就是 UTF-8，回落正好
            return None
    else:
        name = sys.stdout.encoding or "utf-8"
    return name


def log(text):
    """输出一行日志。

    py2 的 stdout 是字节流，Windows 控制台按代码页解析写入的字节：直写 UTF-8 字节时，
    cp936 会把整段缓冲区按双字节重新切分，切出非法序列（如尾部只剩半个汉字）即写入失败，
    Python 抛 IOError [Errno 0]，足以带崩 dev/build。
    故控制台直连时按控制台代码页编码（无法表示的字符退化为 ?），管道与重定向仍写 UTF-8。
    因此本脚本要求 py2/py3 都能跑（npm 里的 `python` 未必是 py3），不能只依赖 reconfigure。
    """
    raw = text if isinstance(text, bytes) else text.encode("utf-8")
    encoding = console_encoding() if sys.version_info[0] < 3 else None
    if encoding:
        raw = raw.decode("utf-8").encode(encoding, "replace")
    stream = getattr(sys.stdout, "buffer", sys.stdout)  # py2 无 buffer，直接写字节流
    stream.write(raw + b"\n")
    stream.flush()


def collect(icon_dir):
    """扫描目录下全部 PNG，返回相对路径（正斜杠分隔、字典序）。"""
    paths = []
    for dirpath, dirnames, filenames in os.walk(icon_dir):
        dirnames.sort()
        for name in sorted(filenames):
            if not name.lower().endswith(".png"):
                continue
            rel = os.path.relpath(os.path.join(dirpath, name), icon_dir)
            paths.append(rel.replace(os.sep, "/"))
    return sorted(paths)


def normalize_newlines(data):
    """比对前统一换行符。

    Windows 检出时 .gitattributes（text=auto）会把入库的 LF 换成 CRLF，
    直接按字节比对会永远判定“有变化”，每次运行都白写一遍文件。
    """
    return data.replace(b"\r\n", b"\n")


def write_if_changed(out_file, body, changed_msg, unchanged_msg):
    """仅在内容变化时写盘，返回 0/1（1 表示有 IO 错误）。

    py2 下 body 为 bytes（str），py3 下为 str —— 两者都要能拼出同一份字节流。
    """
    data = BOM + (body if isinstance(body, bytes) else body.encode("utf-8"))

    old = None
    if os.path.exists(out_file):
        with open(out_file, "rb") as f:
            old = f.read()

    if old is not None and normalize_newlines(old) == normalize_newlines(data):
        log(unchanged_msg)
        return 0

    out_dir = os.path.dirname(out_file)
    if not os.path.isdir(out_dir):
        os.makedirs(out_dir)
    text = body.decode("utf-8") if isinstance(body, bytes) else body
    # utf-8-sig：写出 UTF-8 with BOM（.ts 编码规范见 check_encoding.py）
    # newline="\n"：不做平台换行翻译，Windows 与 macOS 产出同一份字节流（仓库统一存 LF）
    with io.open(out_file, "w", encoding="utf-8-sig", newline="\n") as f:
        f.write(text)
    log(changed_msg)
    return 0


MONSTER_DIR = os.path.join(ROOT, "public", "monster_icons")
MONSTER_OUT = os.path.join(ROOT, "src", "generated", "iconManifest.ts")

WEAPON_DIR = os.path.join(ROOT, "public", "weapon_icons")
WEAPON_OUT = os.path.join(ROOT, "src", "generated", "weaponIconManifest.ts")


def render_monsters(paths):
    lines = [
        """/* 本文件由 scripts/gen_icon_manifest.py 自动生成，请勿手工编辑。
   数据源：public/monster_icons 下的全部 PNG（随包图标），条目与磁盘文件一一对应。 */

"""
    ]
    lines.append("/** 全部随包图标（相对 /monster_icons/ 的路径，形如 `MHRS/MHRS-Rathalos_Icon.png`） */\n")
    lines.append("export const ICON_LIST: string[] = [\n")
    for p in paths:
        lines.append('  "%s",\n' % p)
    lines.append("];\n\n")
    lines.append(
        """/** 图标目录 → 作品分组（与 monster_list.json 的图标地址前缀一致） */
export function iconGroup(path: string): string {
  const prefix = path.split("/")[0];
  if (prefix.startsWith("MHWilds")) return "MHWilds";
  if (prefix.startsWith("MHWI")) return "MHWI";
  if (prefix.startsWith("MHWorld")) return "MHWorld";
  if (prefix.startsWith("MHRS")) return "MHRS";
  if (prefix.startsWith("MHRise")) return "MHRise";
  return "other";
}
"""
    )
    return "".join(lines)


def render_weapons(paths):
    lines = [
        """/* 本文件由 scripts/gen_icon_manifest.py 自动生成，请勿手工编辑。
   数据源：public/weapon_icons 下的全部 PNG（随包图标），条目与磁盘文件一一对应。 */

"""
    ]
    lines.append(
        "/** 全部随包武器图标（相对 /weapon_icons/ 的路径，形如 `MHWilds/MHWilds-Bow_Icon_Base.png`） */\n"
    )
    lines.append("export const WEAPON_ICON_LIST: string[] = [\n")
    for p in paths:
        lines.append('  "%s",\n' % p)
    lines.append("];\n")
    return "".join(lines)


def main():
    failed = False

    if not os.path.isdir(MONSTER_DIR):
        log("怪物图标目录不存在: %s" % MONSTER_DIR)
        failed = True
    else:
        paths = collect(MONSTER_DIR)
        if not paths:
            log("未找到任何怪物图标: %s" % MONSTER_DIR)
            failed = True
        else:
            rc = write_if_changed(
                MONSTER_OUT,
                render_monsters(paths),
                "已生成 iconManifest.ts：%d 张图标" % len(paths),
                "iconManifest.ts 无需更新（%d 张图标）" % len(paths),
            )
            failed = failed or bool(rc)

    if not os.path.isdir(WEAPON_DIR):
        log("武器图标目录不存在: %s" % WEAPON_DIR)
        failed = True
    else:
        paths = collect(WEAPON_DIR)
        if not paths:
            log("未找到任何武器图标: %s" % WEAPON_DIR)
            failed = True
        else:
            rc = write_if_changed(
                WEAPON_OUT,
                render_weapons(paths),
                "已生成 weaponIconManifest.ts：%d 张图标" % len(paths),
                "weaponIconManifest.ts 无需更新（%d 张图标）" % len(paths),
            )
            failed = failed or bool(rc)

    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
