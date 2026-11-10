#!/usr/bin/env python3
"""服务器端：从 stdin 读「头部 JSON + 量化笔画」，渲染成 PNG 放进 cache/。

输出一行 JSON：{"png": 路径, "question": ..., "doc": ..., "page": ...}
"""
import gzip
import json
import os
import struct
import sys
import time

CACHE = os.path.join(os.path.dirname(os.path.abspath(__file__)), "cache")
SCALE = 10


def decode(data):
    off = 0
    n_strokes, = struct.unpack_from("<H", data, off)
    off += 2
    strokes = []
    for _ in range(n_strokes):
        n_pts, = struct.unpack_from("<H", data, off)
        off += 2
        x, y = struct.unpack_from("<ii", data, off)
        off += 8
        pts = [(x / SCALE, y / SCALE)]
        for _ in range(n_pts - 1):
            dx, dy = struct.unpack_from("<hh", data, off)
            off += 4
            x += dx
            y += dy
            pts.append((x / SCALE, y / SCALE))
        strokes.append(pts)
    return strokes


def render(strokes, out_path, scale=3, pad=30, line_width=5):
    from PIL import Image, ImageDraw

    xs = [p[0] for s in strokes for p in s]
    ys = [p[1] for s in strokes for p in s]
    minx, maxx = min(xs), max(xs)
    miny, maxy = min(ys), max(ys)
    w = int((maxx - minx) + 2 * pad) * scale
    h = int((maxy - miny) + 2 * pad) * scale
    img = Image.new("L", (w, h), 255)
    d = ImageDraw.Draw(img)
    for s in strokes:
        pts = [(((p[0] - minx) + pad) * scale,
                ((p[1] - miny) + pad) * scale) for p in s]
        if len(pts) >= 2:
            d.line(pts, fill=0, width=line_width, joint="curve")
        elif pts:
            x, y = pts[0]
            d.ellipse([x - 2, y - 2, x + 2, y + 2], fill=0)
    img.save(out_path)
    return img.size


def main():
    header = json.loads(sys.stdin.buffer.readline())
    strokes = decode(gzip.decompress(sys.stdin.buffer.read()))
    if not strokes:
        sys.exit("没有可渲染的笔画")

    os.makedirs(CACHE, exist_ok=True)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    page = (header.get("page") or "page").replace(".rm", "")
    out = os.path.join(CACHE, f"{stamp}-{page}.png")
    size = render(strokes, out)
    print(json.dumps({
        "png": out,
        "question": header.get("question", ""),
        "doc": header.get("doc", ""),
        "page": header.get("page", ""),
        "size": size,
    }, ensure_ascii=False))


if __name__ == "__main__":
    main()
