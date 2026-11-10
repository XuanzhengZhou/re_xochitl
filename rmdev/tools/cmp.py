#!/usr/bin/env python3
"""把「我们的渲染」和「官方截图」叠在一起比。

用法：
  python3 tools/cmp.py ours.png official.png out.png [x y w h]

输出三栏（左：我们 / 中：官方 / 右：叠加）：
  右图里 **红色** = 我们有墨、官方没有；**蓝色** = 官方有墨、我们没有；
  重合的墨是黑色。看红蓝错位就能量出"差几像素"。

注意官方那些 png 其实是 JPEG 内容，边缘有压缩噪点，所以按阈值 128 二值化再比。
"""
import sys
from PIL import Image, ImageDraw

def ink(im, thr=128):
    g = im.convert("L")
    return g.point(lambda v: 255 if v < thr else 0, mode="L")

def main():
    a, b, out = sys.argv[1], sys.argv[2], sys.argv[3]
    crop = None
    if len(sys.argv) >= 8:
        crop = tuple(int(v) for v in sys.argv[4:8])
    A = Image.open(a).convert("L")
    B = Image.open(b).convert("L")
    if crop:
        A = A.crop((crop[0], crop[1], crop[0] + crop[2], crop[1] + crop[3]))
        B = B.crop((crop[0], crop[1], crop[0] + crop[2], crop[1] + crop[3]))
    W, H = A.size
    ia, ib = ink(A), ink(B)
    # 统计
    na = ia.histogram()[255]
    nb = ib.histogram()[255]
    # 差：膨胀 1px 容忍字体渲染的亚像素差
    from PIL import ImageFilter
    da = ia.filter(ImageFilter.MaxFilter(3))
    db = ib.filter(ImageFilter.MaxFilter(3))
    # 我们的墨但官方附近没有
    only_a = Image.new("L", (W, H))
    only_a.putdata([255 if (pa and not pb) else 0 for pa, pb in zip(ia.getdata(), db.getdata())])
    only_b = Image.new("L", (W, H))
    only_b.putdata([255 if (pb and not pa) else 0 for pb, pa in zip(ib.getdata(), da.getdata())])
    comp = Image.new("RGB", (W, H), (255, 255, 255))
    px = comp.load()
    sa = list(only_a.getdata())
    sb = list(only_b.getdata())
    for i in range(W * H):
        x, y = i % W, i // W
        if sa[i] and sb[i]:
            px[x, y] = (255, 0, 255)
        elif sa[i]:
            px[x, y] = (255, 0, 0)
        elif sb[i]:
            px[x, y] = (0, 0, 255)
        elif ia.getpixel((x, y)):
            px[x, y] = (0, 0, 0)
    print(f"墨量：ours={na} official={nb}  仅我们={sa.count(255)} 仅官方={sb.count(255)}")
    out_im = Image.new("RGB", (W * 3 + 20, H), (128, 128, 128))
    out_im.paste(A.convert("RGB"), (0, 0))
    out_im.paste(B.convert("RGB"), (W + 10, 0))
    out_im.paste(comp, (W * 2 + 20, 0))
    out_im.save(out)
    print(out)

if __name__ == "__main__":
    main()
