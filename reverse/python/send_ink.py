#!/usr/bin/env python3
"""设备端：把最近的手写页编码成"量化笔画流"推到 stdout。

用法:
    python3 send_ink.py --question "帮我解这道题" [--doc 片段] [--page N]

输出格式（二进制，供服务器 ingest_ink.py 解析）:
    第一行: JSON（\\n 结尾）
    其余:   gzip 压缩的笔画二进制
              uint16 n_strokes
              每条: uint16 n_points, int32 x0, int32 y0,
                    然后 (n_points-1) 个 int16 dx, int16 dy
    坐标已乘 10 取整（0.1 单位精度）。
"""
import argparse
import glob
import gzip
import json
import os
import struct
import sys

BASE = "/home/root/.local/share/remarkable/xochitl"
SCALE = 10


def find_docs():
    docs = []
    for meta in glob.glob(os.path.join(BASE, "*.metadata")):
        try:
            with open(meta, encoding="utf-8") as f:
                name = json.load(f).get("visibleName", "")
        except Exception:
            name = ""
        uuid = meta[:-len(".metadata")]
        n_pages = len(glob.glob(os.path.join(BASE, uuid, "*.rm")))
        docs.append((uuid, name, os.path.getmtime(meta), n_pages))
    return sorted(docs, key=lambda d: -d[2])


def pick_doc(fragment):
    docs = find_docs()
    if not docs:
        sys.exit("设备上没有文档")
    if fragment:
        hit = [d for d in docs if fragment.lower() in d[1].lower()]
        if not hit:
            sys.exit(f"没找到名称包含 {fragment!r} 的文档")
        return hit[0]
    # 只考虑真的有手写页的文档（PDF 之类的 metadata 更新会排在前面）
    writable = [d for d in docs if d[3] > 0]
    if not writable:
        sys.exit("没有包含手写页的文档")
    return writable[0]


def pick_page(uuid, index):
    pages = sorted(glob.glob(os.path.join(BASE, uuid, "*.rm")))
    if not pages:
        sys.exit("这个文档没有手写页")
    if index is None:
        return pages[-1], len(pages)
    if not (1 <= index <= len(pages)):
        sys.exit(f"页码超出范围（共 {len(pages)} 页）")
    return pages[index - 1], len(pages)


def encode(rm_path):
    from rmscene import read_tree
    from rmscene.scene_items import Line, Pen

    with open(rm_path, "rb") as f:
        tree = read_tree(f)
    strokes = []
    for item in tree.walk():
        if not isinstance(item, Line):
            continue
        if item.tool in (Pen.ERASER, Pen.ERASER_AREA):
            continue          # 擦除记录，不是墨迹
        if len(item.points) < 1:
            continue
        strokes.append(item.points)

    out = bytearray()
    out += struct.pack("<H", len(strokes))
    for pts in strokes:
        out += struct.pack("<H", len(pts))
        x0 = int(round(pts[0].x * SCALE))
        y0 = int(round(pts[0].y * SCALE))
        out += struct.pack("<ii", x0, y0)
        px, py = x0, y0
        for p in pts[1:]:
            x = int(round(p.x * SCALE))
            y = int(round(p.y * SCALE))
            out += struct.pack("<hh", max(-32768, min(32767, x - px)),
                               max(-32768, min(32767, y - py)))
            px, py = x, y
    return bytes(out), len(strokes), sum(len(s) for s in strokes)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--question", required=True)
    ap.add_argument("--doc", default=None)
    ap.add_argument("--page", type=int, default=None)
    args = ap.parse_args()

    uuid, name, _, _ = pick_doc(args.doc)
    rm, total_pages = pick_page(uuid, args.page)
    raw, n_strokes, n_points = encode(rm)
    if n_strokes == 0:
        sys.exit("这一页没有墨迹")

    header = {
        "version": 1,
        "question": args.question,
        "doc": name,
        "page": os.path.basename(rm),
        "pages": total_pages,
        "strokes": n_strokes,
        "points": n_points,
    }
    sys.stdout.write(json.dumps(header, ensure_ascii=False) + "\n")
    sys.stdout.flush()
    sys.stdout.buffer.write(gzip.compress(raw, 6))
    sys.stdout.buffer.flush()


if __name__ == "__main__":
    main()
