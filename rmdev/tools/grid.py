#!/usr/bin/env python3
"""在截图上叠加坐标网格，方便肉眼读精确像素。用法: grid.py in.png out.png [步长]"""
import sys
from PIL import Image, ImageDraw

src, dst = sys.argv[1], sys.argv[2]
step = int(sys.argv[3]) if len(sys.argv) > 3 else 100
im = Image.open(src).convert("RGB")
d = ImageDraw.Draw(im)
W, H = im.size
for x in range(0, W, step):
    d.line([(x, 0), (x, H)], fill=(255, 0, 0), width=1)
    d.text((x + 3, 3), str(x), fill=(255, 0, 0))
    d.text((x + 3, H - 14), str(x), fill=(255, 0, 0))
for y in range(0, H, step):
    d.line([(0, y), (W, y)], fill=(0, 120, 255), width=1)
    d.text((3, y + 1), str(y), fill=(0, 120, 255))
    d.text((W - 40, y + 1), str(y), fill=(0, 120, 255))
im.save(dst)
print(dst)
