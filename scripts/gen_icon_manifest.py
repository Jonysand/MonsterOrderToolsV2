# -*- coding: utf-8 -*-
"""生成图标清单：扫描 public/monster_icons/**/*.png → src/generated/iconManifest.ts

后端在打包态看不到前端静态目录，图标选择器的数据源只能在构建期固化。
产物提交入库（保证 CI 与本地一致），编码为 UTF-8 with BOM（见 check_encoding.py）。
"""
import io
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ICON_DIR = os.path.join(ROOT, "public", "monster_icons")
OUT_FILE = os.path.join(ROOT, "src", "generated", "iconManifest.ts")
BOM = b"\xef\xbb\xbf"


def collect():
    paths = []
    for dirpath, dirnames, filenames in os.walk(ICON_DIR):
        dirnames.sort()
        for name in sorted(filenames):
            if not name.lower().endswith(".png"):
                continue
            rel = os.path.relpath(os.path.join(dirpath, name), ICON_DIR)
            paths.append(rel.replace(os.sep, "/"))
    return sorted(paths)


HEADER = """/* 本文件由 scripts/gen_icon_manifest.py 自动生成，请勿手工编辑。
   数据源：public/monster_icons 下的全部 PNG（随包图标），条目与磁盘文件一一对应。 */

"""


def render(paths):
    lines = [HEADER]
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


def main():
    if not os.path.isdir(ICON_DIR):
        print("图标目录不存在: %s" % ICON_DIR)
        return 1

    paths = collect()
    if not paths:
        print("未找到任何图标: %s" % ICON_DIR)
        return 1

    body = render(paths)
    # py2 下 body 为 bytes（str），py3 下为 str —— 两者都要能拼出同一份字节流
    data = BOM + (body if isinstance(body, bytes) else body.encode("utf-8"))

    old = None
    if os.path.exists(OUT_FILE):
        with open(OUT_FILE, "rb") as f:
            old = f.read()

    if old == data:
        print("iconManifest.ts 无需更新（%d 张图标）" % len(paths))
        return 0

    out_dir = os.path.dirname(OUT_FILE)
    if not os.path.isdir(out_dir):
        os.makedirs(out_dir)
    text = body.decode("utf-8") if isinstance(body, bytes) else body
    # utf-8-sig：写出 UTF-8 with BOM（.ts 编码规范见 check_encoding.py）
    with io.open(OUT_FILE, "w", encoding="utf-8-sig") as f:
        f.write(text)
    print("已生成 iconManifest.ts：%d 张图标" % len(paths))
    return 0


if __name__ == "__main__":
    sys.exit(main())
