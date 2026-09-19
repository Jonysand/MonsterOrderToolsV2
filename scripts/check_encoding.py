# -*- coding: utf-8 -*-
"""E5: 编码规范核查 —— .rs/.ts/.tsx/.css/.md 需 UTF-8 with BOM；JSON 需 UTF-8 无 BOM。"""
import os
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BOM = b"\xef\xbb\xbf"

REQUIRE_BOM = {".rs", ".ts", ".tsx", ".css", ".md"}
REQUIRE_NO_BOM = {".json"}
SKIP_DIRS = {"node_modules", "target", "dist", ".git", "gen", "icons"}

problems = []
scanned = {"bom": 0, "nobom": 0}


def check(path, ext):
    with open(path, "rb") as f:
        head = f.read(3)
    has_bom = head == BOM
    if ext in REQUIRE_BOM:
        scanned["bom"] += 1
        if not has_bom:
            problems.append("MISSING BOM: " + path)
    elif ext in REQUIRE_NO_BOM:
        scanned["nobom"] += 1
        if has_bom:
            problems.append("UNEXPECTED BOM: " + path)


for dirpath, dirnames, filenames in os.walk(ROOT):
    dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
    for name in filenames:
        ext = os.path.splitext(name)[1].lower()
        if ext in REQUIRE_BOM or ext in REQUIRE_NO_BOM:
            check(os.path.join(dirpath, name), ext)

print("scanned: bom-required={bom} json={nobom}".format(**scanned))
if problems:
    print("\n".join(problems))
    sys.exit(1)
print("OK: all files follow encoding rules")
