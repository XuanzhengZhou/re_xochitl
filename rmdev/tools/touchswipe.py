#!/usr/bin/env python3
"""手指横划（MT 协议 B）。用法: touchswipe.py X0 Y0 X1 Y1 [时长ms]"""
import fcntl, struct, sys, time

DEV = "/dev/input/event2"
SYN, KEY, ABS = 0, 1, 3
BTN_TOUCH = 0x14A
MT_SLOT, MT_TRACK, MT_X, MT_Y, MT_PRESSURE = 0x2F, 0x39, 0x35, 0x36, 0x3A

fd = open(DEV, "wb", buffering=0)
EVIOCGABS = (2 << 30) | (24 << 16) | (0x45 << 8) | 0x40
_seq = [700]


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


def main():
    x0, y0, x1, y1 = (int(v) for v in sys.argv[1:5])
    ms = int(sys.argv[5]) if len(sys.argv) > 5 else 500
    n = max(8, ms // 16)
    _seq[0] += 1
    tid = _seq[0]
    # 面板 Y 轴原点在下方
    def sy(v):
        return 1871 - v

    ev(ABS, MT_SLOT, 0)
    ev(ABS, MT_TRACK, tid)
    ev(ABS, MT_X, j(MT_X, x0, 1))
    ev(ABS, MT_Y, j(MT_Y, sy(y0), 2))
    ev(ABS, MT_PRESSURE, j(MT_PRESSURE, 60, 3))
    ev(KEY, BTN_TOUCH, 1)
    ev(SYN, 0, 0)
    for i in range(1, n + 1):
        f = i / n
        x = int(x0 + (x1 - x0) * f)
        y = int(y0 + (y1 - y0) * f)
        ev(ABS, MT_X, x)
        ev(ABS, MT_Y, sy(y))
        ev(SYN, 0, 0)
        time.sleep(ms / 1000.0 / n)
    ev(ABS, MT_TRACK, -1)
    ev(KEY, BTN_TOUCH, 0)
    ev(SYN, 0, 0)
    time.sleep(0.4)
    print("swiped", x0, y0, "->", x1, y1)


main()
