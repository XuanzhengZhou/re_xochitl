#!/usr/bin/env python3
"""手指长按。用法: touchhold.py X Y [毫秒]"""
import fcntl, struct, sys, time

DEV = "/dev/input/event2"
SYN, KEY, ABS = 0, 1, 3
BTN_TOUCH = 0x14A
MT_SLOT, MT_TRACK, MT_X, MT_Y, MT_PRESSURE = 0x2F, 0x39, 0x35, 0x36, 0x3A
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


x, y = int(sys.argv[1]), int(sys.argv[2])
ms = int(sys.argv[3]) if len(sys.argv) > 3 else 900
_seq[0] += 1
ev(ABS, MT_SLOT, 0)
ev(ABS, MT_TRACK, _seq[0])
# 面板 Y 轴原点在下方：要落在屏幕 (x,y)，得发 1871-y。
#
# 先抖一个像素再落到目标值：内核会把"和上次相同"的 ABS 值当重复事件丢掉，
# 连点同一位置时第二次的 MT_X 会被整个吞掉（实测 rmdev 收到 `x=0`，长按
# 落在屏幕最左边，永远选不中卡片）。touchhold 原来用 `j()` 只兜住了"恰好
# 相等"，兜不住"这一轴根本没发出去"。
yy = 1871 - y
ev(ABS, MT_X, min(x + 1, 1403))
ev(ABS, MT_Y, min(yy + 1, 1871))
ev(ABS, MT_X, x)
ev(ABS, MT_Y, yy)
ev(ABS, MT_PRESSURE, j(MT_PRESSURE, 60, 3))
ev(SYN, 0, 0)
time.sleep(0.05)
ev(KEY, BTN_TOUCH, 1)
ev(SYN, 0, 0)
time.sleep(ms / 1000.0)
ev(ABS, MT_TRACK, -1)
ev(KEY, BTN_TOUCH, 0)
ev(SYN, 0, 0)
time.sleep(0.4)
print("held", x, y, ms)
