#!/usr/bin/env python3
"""笔按住不动：penhold.py X Y [毫秒]

坐标映射和 tap.py 一致（ABS_X 对应屏幕 Y 的反向，ABS_Y 对应屏幕 X）。
按住期间不发多余事件，松手时再抬——这样应用侧可以靠"按下多久"判定长按。
"""
import fcntl
import struct
import sys
import time

DEV = "/dev/input/event1"
RAW_X_MAX, RAW_Y_MAX = 20966, 15725
FB_W, FB_H = 1404, 1872

fd = open(DEV, "wb", buffering=0)
EVIOCGABS = (2 << 30) | (24 << 16) | (0x45 << 8) | 0x40


def ev(t, c, v):
    fd.write(struct.pack("<iiHHi", 0, 0, t, c, v))


def current(axis):
    try:
        buf = bytearray(24)
        fcntl.ioctl(fd, EVIOCGABS + axis, buf)
        return struct.unpack_from("<i", buf, 0)[0]
    except OSError:
        return None


_jit = [0]


def jit(axis, v):
    cur = current(axis)
    if cur is not None and cur == v:
        _jit[0] += 1
        v = v + 1 + (_jit[0] % 3)
    return v


x, y = int(sys.argv[1]), int(sys.argv[2])
ms = int(sys.argv[3]) if len(sys.argv) > 3 else 900
rx = int(round((1.0 - y / FB_H) * RAW_X_MAX))
ry = int(round(x / FB_W * RAW_Y_MAX))

ev(1, 0x140, 1)          # BTN_TOOL_PEN
ev(0, 0, 0)
time.sleep(0.05)
ev(3, 0, jit(0, rx))
ev(3, 1, jit(1, ry))
ev(3, 0x18, jit(0x18, 2400))
ev(1, 0x14A, 1)          # BTN_TOUCH
ev(0, 0, 0)
time.sleep(ms / 1000.0)  # 按住不动
ev(1, 0x14A, 0)
ev(0, 0, 0)
ev(1, 0x140, 0)
ev(0, 0, 0)
time.sleep(0.3)
print("penheld", x, y, ms)
