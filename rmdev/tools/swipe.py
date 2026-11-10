#!/usr/bin/env python3
"""从 (x0,y0) 划到 (x1,y1)：python3 swipe.py x0 y0 x1 y1"""
import fcntl, struct, sys, time
RAW_X_MAX, RAW_Y_MAX = 20966, 15725
FB_W, FB_H = 1404, 1872
fd = open("/dev/input/event1", "wb", buffering=0)
EVIOCGABS = (2 << 30) | (24 << 16) | (0x45 << 8) | 0x40
def ev(t, c, v): fd.write(struct.pack("<iiHHi", 0, 0, t, c, v))
def cur(axis):
    try:
        b = bytearray(24); fcntl.ioctl(fd, EVIOCGABS + axis, b); return struct.unpack_from("<i", b, 0)[0]
    except OSError: return None
def j(axis, v):
    c = cur(axis)
    return v + 1 if c == v else v
def dev(x, y): return int(round((1 - y / FB_H) * RAW_X_MAX)), int(round(x / FB_W * RAW_Y_MAX))
x0, y0, x1, y1 = (int(v) for v in sys.argv[1:5])
rx, ry = dev(x0, y0)
ev(1, 0x140, 1); ev(0, 0, 0); time.sleep(0.05)
ev(3, 0, j(0, rx)); ev(3, 1, j(1, ry)); ev(3, 0x18, j(0x18, 2400)); ev(1, 0x14A, 1); ev(0, 0, 0)
time.sleep(0.05)
N = 25
for i in range(1, N + 1):
    t = i / N
    x = x0 + (x1 - x0) * t; y = y0 + (y1 - y0) * t
    rx, ry = dev(x, y)
    ev(3, 0, j(0, rx)); ev(3, 1, j(1, ry)); ev(0, 0, 0)
    time.sleep(0.012)
ev(1, 0x14A, 0); ev(0, 0, 0); ev(1, 0x140, 0); ev(0, 0, 0)
print("swiped", x0, y0, "->", x1, y1)
