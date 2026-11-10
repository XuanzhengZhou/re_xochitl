#!/usr/bin/env python3
"""双指手势注入。

用法：
  twofinger.py tap   X Y
  twofinger.py swipe X0 Y0 X1 Y1 [时长ms]

两指水平相距 120px（第二指在右）。tap 按住 120ms 就抬起（用于"双指点击 = 撤销"）；
swipe 两指同步移动（用于"双指下滑 = 收起工具条"）。
坐标约定与 tap.py 一致：屏幕坐标、原点左上、y 向下。
"""
import fcntl
import struct
import sys
import time

DEV = "/dev/input/event2"
SYN, KEY, ABS = 0, 1, 3
BTN_TOUCH = 0x14A
MT_SLOT, MT_TRACK, MT_X, MT_Y, MT_PRESSURE = 0x2F, 0x39, 0x35, 0x36, 0x3A
GAP = 120

fd = open(DEV, "wb", buffering=0)
EVIOCGABS = (2 << 30) | (24 << 16) | (0x45 << 8) | 0x40
_seq = [900]


def ev(t, c, v):
    fd.write(struct.pack("<iiHHi", 0, 0, t, c, v))


def cur(axis):
    try:
        b = bytearray(24)
        fcntl.ioctl(fd, EVIOCGABS + axis, b)
        return struct.unpack_from("<i", b, 0)[0]
    except OSError:
        return None


def j(axis, v, step):
    c = cur(axis)
    return v + step + 1 if c == v else v


def sy(v):
    # 面板 Y 轴原点在下方
    return 1871 - v


def place(slot, tid, x, y):
    ev(ABS, MT_SLOT, slot)
    ev(ABS, MT_TRACK, tid)
    ev(ABS, MT_X, j(MT_X, x, slot + 1))
    ev(ABS, MT_Y, j(MT_Y, sy(y), slot + 2))
    ev(ABS, MT_PRESSURE, j(MT_PRESSURE, 60, slot + 3))


def main():
    mode = sys.argv[1]
    _seq[0] += 1
    t0 = _seq[0]
    t1 = t0 + 1
    if mode == "tap":
        x, y = int(sys.argv[2]), int(sys.argv[3])
        place(0, t0, x, y)
        place(1, t1, x + GAP, y)
        ev(KEY, BTN_TOUCH, 1)
        ev(SYN, 0, 0)
        time.sleep(0.12)
        ev(ABS, MT_SLOT, 1)
        ev(ABS, MT_TRACK, -1)
        ev(ABS, MT_SLOT, 0)
        ev(ABS, MT_TRACK, -1)
        ev(KEY, BTN_TOUCH, 0)
        ev(SYN, 0, 0)
        time.sleep(0.4)
        print("two-finger tap", x, y)
        return

    x0, y0, x1, y1 = (int(v) for v in sys.argv[2:6])
    ms = int(sys.argv[6]) if len(sys.argv) > 6 else 300
    n = max(8, ms // 16)
    place(0, t0, x0, y0)
    place(1, t1, x0 + GAP, y0)
    ev(KEY, BTN_TOUCH, 1)
    ev(SYN, 0, 0)
    for i in range(1, n + 1):
        f = i / n
        x = int(x0 + (x1 - x0) * f)
        y = int(y0 + (y1 - y0) * f)
        ev(ABS, MT_SLOT, 0)
        ev(ABS, MT_X, x)
        ev(ABS, MT_Y, sy(y))
        ev(ABS, MT_SLOT, 1)
        ev(ABS, MT_X, x + GAP)
        ev(ABS, MT_Y, sy(y))
        ev(SYN, 0, 0)
        time.sleep(ms / 1000.0 / n)
    ev(ABS, MT_SLOT, 1)
    ev(ABS, MT_TRACK, -1)
    ev(ABS, MT_SLOT, 0)
    ev(ABS, MT_TRACK, -1)
    ev(KEY, BTN_TOUCH, 0)
    ev(SYN, 0, 0)
    time.sleep(0.4)
    print("two-finger swipe", x0, y0, "->", x1, y1)


main()
