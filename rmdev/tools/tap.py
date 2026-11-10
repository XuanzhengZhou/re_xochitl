#!/usr/bin/env python3
"""在设备上点一下屏幕：python3 tap.py X Y [X2 Y2 ...]（屏幕像素坐标）

横屏模式下坐标是**逻辑**的（1872×1404，工具栏在左），加 RM_LAND=1 会先转成
物理坐标再注入（逻辑 (x, y) 上屏成物理 (1403-y, x)，见 `fb::rect_to_phys`）。
"""
import fcntl
import os
import struct
import sys
import time

RAW_X_MAX, RAW_Y_MAX = 20966, 15725
FB_W, FB_H = 1404, 1872

fd = open("/dev/input/event1", "wb", buffering=0)

# EVIOCGABS(axis) = _IOR('E', 0x40+axis, struct input_absinfo[24])
EVIOCGABS = (2 << 30) | (24 << 16) | (0x45 << 8) | 0x40


def current(axis):
    """读该轴当前值。内核会丢掉与当前值相同的 ABS 事件，所以必须知道它。
    （这个坑踩了两次：连点同一个位置，事件被内核静默吞掉。）"""
    try:
        buf = bytearray(24)
        fcntl.ioctl(fd, EVIOCGABS + axis, buf)
        return struct.unpack_from("<i", buf, 0)[0]
    except OSError:
        return None


_jit = [0]


def ev(t, c, v):
    fd.write(struct.pack("<iiHHi", 0, 0, t, c, v))


def jit(axis, v):
    """确保送出去的值和内核当前记录的不同，否则这条事件不会产生。"""
    cur = current(axis)
    if cur is not None and cur == v:
        _jit[0] += 1
        v = v + 1 + (_jit[0] % 3)
    return v


def land(x, y):
    """横屏：逻辑坐标 → 物理坐标。

    `RM_LAND=1` 或 `left` = 逆时针（充电口在左，默认）；`RM_LAND=cw`/`right` = 顺时针。
    逆时针：px = y、py = 1871 - x；顺时针：px = 1403 - y、py = x。
    """
    v = os.environ.get("RM_LAND")
    if v in ("cw", "right"):
        return (1403 - y, x)
    return (y, 1871 - x)


def tap(x, y):
    if os.environ.get("RM_LAND"):
        x, y = land(x, y)
    rx = int(round((1.0 - y / FB_H) * RAW_X_MAX))
    ry = int(round(x / FB_W * RAW_Y_MAX))
    ev(1, 0x140, 1)  # BTN_TOOL_PEN
    ev(0, 0, 0)
    time.sleep(0.05)
    ev(3, 0, jit(0, rx))
    ev(3, 1, jit(1, ry))
    ev(3, 0x18, jit(0x18, 2400))
    ev(1, 0x14A, 1)  # BTN_TOUCH
    ev(0, 0, 0)
    time.sleep(0.06)
    ev(1, 0x14A, 0)
    ev(0, 0, 0)
    ev(1, 0x140, 0)
    ev(0, 0, 0)
    time.sleep(0.3)


if __name__ == "__main__":
    coords = [int(v) for v in sys.argv[1:]]
    for i in range(0, len(coords), 2):
        tap(coords[i], coords[i + 1])
    print("tapped", coords)
