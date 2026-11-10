#!/usr/bin/env python3
"""rm2png —— 把 reMarkable 手写笔记（.rm v6）渲染成 PNG。

用法:
    ./rm2png.py                # 取设备上最近修改的一份笔记，渲染所有页面
    ./rm2png.py <文档名片段>    # 按 visibleName 匹配文档
    ./rm2png.py --list         # 只列出设备上的文档
"""
import glob
import json
import os
import subprocess
import sys

SSH = ["ssh", "-o", "BatchMode=yes", "remarkable"]
BASE = "/home/root/.local/share/remarkable/xochitl"
OUTDIR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "note")


def sh(cmd):
    return subprocess.run(cmd, capture_output=True, text=True).stdout


def list_docs():
    """返回 [(uuid, visibleName, mtime)]"""
    raw = sh(SSH + [f'cd {BASE} && for f in *.metadata; do '
                    f'printf "%s\\t%s\\t%s\\n" "${{f%.metadata}}" '
                    f'"$(stat -c %Y "$f")" "$f"; done'])
    docs = []
    for line in raw.strip().splitlines():
        parts = line.split("\t")
        if len(parts) != 3:
            continue
        uuid, mtime, metafile = parts
        name = ""
        try:
            meta = json.loads(sh(SSH + [f"cat {BASE}/{metafile}"]))
            name = meta.get("visibleName", "")
        except Exception:
            pass
        docs.append((uuid, name, int(mtime)))
    return sorted(docs, key=lambda d: -d[2])


def fetch_pages(uuid):
    os.makedirs(OUTDIR, exist_ok=True)
    raw = sh(SSH + [f'ls {BASE}/{uuid}/*.rm 2>/dev/null'])
    pages = [p.strip() for p in raw.strip().splitlines() if p.strip()]
    local = []
    for p in pages:
        dest = os.path.join(OUTDIR, os.path.basename(p))
        subprocess.run(["scp", "-q", f"remarkable:{p}", dest], check=False)
        local.append(dest)
    return sorted(local)


def render(rm_path, out_path, scale=3, pad=30, line_width=5):
    from PIL import Image, ImageDraw
    from rmscene import read_tree
    from rmscene.scene_items import Line, Pen

    with open(rm_path, "rb") as f:
        tree = read_tree(f)
    # .rm v6 里 ERASER(6)/ERASER_AREA(8) 是"擦除记录"而非墨迹：
    # 被擦掉的笔迹已从场景中删除，这些记录只用于撤销，绝不能再画出来，
    # 否则会出现"圈选线""划改线"等幽灵笔画。
    items = [x for x in tree.walk() if isinstance(x, Line)]
    ink = [l for l in items if l.tool not in (Pen.ERASER, Pen.ERASER_AREA)]
    pts = [p for l in ink for p in l.points]
    if not pts:
        return None
    minx = min(p.x for p in pts)
    maxx = max(p.x for p in pts)
    miny = min(p.y for p in pts)
    maxy = max(p.y for p in pts)
    w = int((maxx - minx) + 2 * pad) * scale
    h = int((maxy - miny) + 2 * pad) * scale
    img = Image.new("L", (w, h), 255)
    d = ImageDraw.Draw(img)
    erased = sum(1 for l in items
                 if l.tool in (Pen.ERASER, Pen.ERASER_AREA))
    for l in ink:
        p = [(((q.x - minx) + pad) * scale, ((q.y - miny) + pad) * scale)
             for q in l.points]
        if not p:
            continue
        if len(p) >= 2:
            d.line(p, fill=0, width=line_width, joint="curve")
        else:
            x, y = p[0]
            d.ellipse([x - 2, y - 2, x + 2, y + 2], fill=0)
    img.save(out_path)
    return len(ink), img.size, erased


def main():
    args = [a for a in sys.argv[1:] if a != "--list"]
    docs = list_docs()
    if "--list" in sys.argv:
        for uuid, name, mtime in docs:
            print(f"{uuid}  {name}")
        return

    if args:
        key = args[0].lower()
        target = [d for d in docs if key in d[1].lower()]
        if not target:
            print(f"没找到名称包含 {key!r} 的文档")
            return
        doc = target[0]
    else:
        if not docs:
            print("设备上没有文档")
            return
        doc = docs[0]

    uuid, name, _ = doc
    print(f"文档: {name or '(未命名)'}  [{uuid}]")
    pages = fetch_pages(uuid)
    if not pages:
        print("这个文档没有手写页（可能是 PDF 或空文档）")
        return
    for i, rm in enumerate(pages, 1):
        out = os.path.splitext(rm)[0] + ".png"
        info = render(rm, out)
        if info:
            n_ink, size, n_erase = info
            print(f"  第 {i} 页 -> {out}  ({n_ink} 条墨迹, "
                  f"{n_erase} 次擦除, {size[0]}x{size[1]})")
        else:
            print(f"  第 {i} 页: 空页")


if __name__ == "__main__":
    main()
