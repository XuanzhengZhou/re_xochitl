#!/usr/bin/env python3
"""从截图里量像素。给复刻用，省得靠眼睛估。

用法:
  probe.py IMG cols Y0 Y1 [THR]   在 y∈[Y0,Y1) 这一横条里，找出"有墨"的 x 区段
  probe.py IMG rows X0 X1 [THR]   在 x∈[X0,X1) 这一竖条里，找出"有墨"的 y 区段
  probe.py IMG bbox X0 Y0 X1 Y1 [THR]   该矩形内墨迹的外接框
  probe.py IMG line Y               该行有墨的 x 区段
"""
import sys
import numpy as np
from PIL import Image

def load(p):
    a = np.array(Image.open(p).convert("L"))
    return a

def segs(mask, min_len=1):
    out, start = [], None
    for i, v in enumerate(mask):
        if v and start is None:
            start = i
        elif not v and start is not None:
            if i - start >= min_len:
                out.append((start, i - 1))
            start = None
    if start is not None:
        out.append((start, len(mask) - 1))
    return out

def main():
    img = sys.argv[1]
    cmd = sys.argv[2]
    a = load(img)
    if cmd == "cols":
        y0, y1 = int(sys.argv[3]), int(sys.argv[4])
        thr = int(sys.argv[5]) if len(sys.argv) > 5 else 128
        band = a[y0:y1]
        m = (band < thr).any(axis=0)
        print("x 区段:", segs(m))
    elif cmd == "rows":
        x0, x1 = int(sys.argv[3]), int(sys.argv[4])
        thr = int(sys.argv[5]) if len(sys.argv) > 5 else 128
        band = a[:, x0:x1]
        m = (band < thr).any(axis=1)
        print("y 区段:", segs(m))
    elif cmd == "line":
        y = int(sys.argv[3])
        thr = int(sys.argv[4]) if len(sys.argv) > 4 else 128
        m = a[y] < thr
        print(f"y={y} x 区段:", segs(m))
    elif cmd == "bbox":
        x0, y0, x1, y1 = (int(v) for v in sys.argv[3:7])
        thr = int(sys.argv[7]) if len(sys.argv) > 7 else 128
        sub = a[y0:y1, x0:x1] < thr
        ys, xs = np.where(sub)
        if len(xs) == 0:
            print("空")
            return
        print(f"bbox x=[{x0+xs.min()},{x0+xs.max()}] y=[{y0+ys.min()},{y0+ys.max()}]")
    else:
        raise SystemExit(__doc__)

main()
