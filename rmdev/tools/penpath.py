#!/usr/bin/env python3
"""按任意折线注入笔迹：python3 penpath.py 'x0,y0 x1,y1 x2,y2 ...'

用于测试套索/选区这类需要自由曲线的场景（swipe.py 只能划直线）。
与 tap.py / swipe.py 同一套坐标约定：参数是屏幕坐标，原点左上，y 向下。
"""
import fcntl
import struct
import sys
import time

RAW_X_MAX, RAW_Y_MAX = 20966, 15725
FB_W, FB_H = 1404, 1872
fd = open("/dev/input/event1", "wb", buffering=0)

EVIOCGABS = (2 << 30) | (24 << 16) | (0x45 << 8) | 0x40


def ev(t, c, v):
    fd.write(struct.pack("<iiHHi", 0, 0, t, c, v))


def cur(axis):
    try:
        b = bytearray(24)
        fcntl.ioctl(fd, EVIOCGABS + axis, b)
        return struct.unpack_from("<i", b, 0)[0]
    except OSError:
        return None


def j(axis, v):
    c = cur(axis)
    return v + 1 if c == v else v


def dev(x, y):
    # 笔的坐标轴相对屏幕旋转过：ABS_X 是屏幕 Y（向上），ABS_Y 是屏幕 X
    return int(round((1 - y / FB_H) * RAW_X_MAX)), int(round(x / FB_W * RAW_Y_MAX))


pts = [tuple(int(v) for v in p.split(",")) for p in sys.argv[1].split()]
if len(pts) < 2:
    sys.exit("至少两个点")

ev(1, 0x140, 1)
ev(0, 0, 0)
time.sleep(0.04)
rx, ry = dev(*pts[0])
ev(3, 0, j(0, rx))
ev(3, 1, j(1, ry))
ev(3, 0x18, j(0x18, 2400))  # 压感
ev(1, 0x14A, 1)             # BTN_TOUCH down
ev(0, 0, 0)
time.sleep(0.04)

STEP = 6  # 每段插值点数：够密让套索看起来连续，又不至于太慢
for (x0, y0), (x1, y1) in zip(pts, pts[1:]):
    for i in range(1, STEP + 1):
        t = i / STEP
        x = x0 + (x1 - x0) * t
        y = y0 + (y1 - y0) * t
        rx, ry = dev(x, y)
        ev(3, 0, j(0, rx))
        ev(3, 1, j(1, ry))
        ev(0, 0, 0)
        time.sleep(0.008)

ev(1, 0x14A, 0)  # up
ev(0, 0, 0)
time.sleep(0.05)
print("penpath", len(pts), "点")
