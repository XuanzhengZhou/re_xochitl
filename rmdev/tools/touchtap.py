#!/usr/bin/env python3
"""往 /dev/input/event2 注入一次单指点击（MT 协议 B）。
用法: touchtap.py X Y [X2 Y2 ...]     （RM_LAND=1 时坐标按横屏逻辑坐标解释）
"""
import fcntl, os, struct, sys, time

DEV = "/dev/input/event2"
SYN, KEY, ABS = 0, 1, 3
BTN_TOUCH = 0x14A
MT_SLOT, MT_TRACK, MT_X, MT_Y, MT_PRESSURE = 0x2F, 0x39, 0x35, 0x36, 0x3A

def land(x, y):
    """横屏：逻辑坐标 → 物理坐标。

    `RM_LAND=1` 或 `left` = 逆时针（充电口在左，默认）；`RM_LAND=cw`/`right` = 顺时针。
    逆时针：px = y、py = 1871 - x；顺时针：px = 1403 - y、py = x。
    """
    v = os.environ.get("RM_LAND")
    if v in ("cw", "right"):
        return (1403 - y, x)
    return (y, 1871 - x)


fd = open(DEV, "wb", buffering=0)
EVIOCGABS = (2 << 30) | (24 << 16) | (0x45 << 8) | 0x40
_seq = [100]


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
    if c is not None and c == v:
        return v + step + 1
    return v


def tap(x, y, hold=0.08):
    # 横屏：逻辑坐标 → 物理坐标
    if os.environ.get("RM_LAND"):
        x, y = land(x, y)
    # 面板 Y 轴原点在下方：要落在屏幕 (x,y)，得发 1871-y
    y = 1871 - y
    _seq[0] += 1
    tid = _seq[0]
    ev(ABS, MT_SLOT, 0)
    ev(ABS, MT_TRACK, tid)
    # 内核会把"和上次相同"的 ABS 值当重复事件丢掉，导致连点同一行时
    # Y 保留旧值（实测第二次点在 y=1375，应用收到的是 y=1871）。
    # 所以先抖一个像素再落到目标值，保证每次都是"变化"。
    ev(ABS, MT_X, min(x + 1, 1403))
    ev(ABS, MT_Y, min(y + 1, 1871))
    ev(ABS, MT_X, x)
    ev(ABS, MT_Y, y)
    ev(ABS, MT_PRESSURE, j(MT_PRESSURE, 60, 3))
    ev(KEY, BTN_TOUCH, 1)
    ev(SYN, 0, 0)
    time.sleep(hold)
    ev(ABS, MT_TRACK, -1)
    ev(KEY, BTN_TOUCH, 0)
    ev(SYN, 0, 0)
    time.sleep(0.35)


if __name__ == "__main__":
    a = [int(v) for v in sys.argv[1:]]
    for i in range(0, len(a), 2):
        tap(a[i], a[i + 1])
    print("touched", a)
