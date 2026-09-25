# -*- coding: utf-8 -*-
"""E6: 孤儿字段核查 —— 统计 AppConfig 各字段在 Rust 后端与前端的使用点。"""
# io.open 而非内置 open：本机 `python` 可能解析到 Python 2.7（内置 open 不接受
# encoding 关键字，会直接抛 TypeError）；io.open 在 py2/py3 下行为一致。
import io
import os
import re

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CONFIG = os.path.join(ROOT, "src-tauri/src/config.rs")

content = io.open(CONFIG, encoding="utf-8-sig").read()
m = re.search(r"pub struct AppConfig \{(.*?)\n\}", content, re.S)
fields = re.findall(r"pub (\w+):", m.group(1))
print("AppConfig fields:", len(fields))

roots = [os.path.join(ROOT, "src-tauri/src"), os.path.join(ROOT, "src")]
files = []
for root in roots:
    for dp, dn, fn in os.walk(root):
        for name in fn:
            if name.endswith((".rs", ".ts", ".tsx")):
                files.append(os.path.join(dp, name))

cache = {}
for p in files:
    cache[p] = io.open(p, encoding="utf-8-sig", errors="ignore").read()

orphans = []
for f in fields:
    pat = re.compile(r"\b" + f + r"\b")
    hits = {}
    for p, txt in cache.items():
        c = len(pat.findall(txt))
        if os.path.normpath(p) == os.path.normpath(CONFIG):
            c -= 2  # struct 定义 + Default 初始化
        if c > 0:
            hits[os.path.relpath(p, ROOT)] = c
    total = sum(hits.values())
    flag = "ORPHAN" if total == 0 else ("WEAK" if total <= 1 else "ok")
    if flag != "ok":
        orphans.append((f, total, hits))
    print("{:32s} {:3d}  {}".format(f, total, ",".join(sorted(hits.keys()))[:110]))

print()
if orphans:
    print("!! fields needing review:")
    for f, total, hits in orphans:
        print("   {} total={} {}".format(f, total, hits))
else:
    print("OK: all fields are wired")
