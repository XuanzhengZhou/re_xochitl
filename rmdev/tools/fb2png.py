#!/usr/bin/env python3
"""把 /dev/shm/swtfb.01 的裸 dump 转成 PNG：1404×1872 @16bpp RGB565 小端。

用法：python3 fb2png.py /tmp/rmdev.fb out.png [裁剪区域 x0 y0 x1 y1]
"""
import sys

import numpy as np
from PIL import Image

W, H = 1404, 1872


def main():
    src = sys.argv[1]
    dst = sys.argv[2] if len(sys.argv) > 2 else "fb.png"
    raw = np.fromfile(src, dtype="<u2", count=W * H)
    if raw.size < W * H:
        raise SystemExit(f"{src}: 只有 {raw.size} 像素")

    r = (raw >> 11) & 0x1F
    g = (raw >> 5) & 0x3F
    b = raw & 0x1F
    # 灰度：面板只有 16 级灰，直接按官方量化 level = gray8 >> 4 再放大回 8 位，
    # 这样看到的条纹/灰阶与设备上一致。
    gray8 = ((r << 3) | (r >> 2)) & 0xFF
    level4 = gray8 >> 4
    view = (level4 * 17).astype(np.uint8).reshape(H, W)

    img = Image.fromarray(view, "L")
    if len(sys.argv) >= 7:
        x0, y0, x1, y1 = (int(v) for v in sys.argv[3:7])
        img = img.crop((x0, y0, x1, y1))
    img.save(dst)
    uniq = np.unique(level4)
    print(f"{dst}: {img.size[0]}x{img.size[1]}  灰阶档位={list(uniq)}")


if __name__ == "__main__":
    main()
