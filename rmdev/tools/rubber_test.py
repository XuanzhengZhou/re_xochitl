#!/usr/bin/env python3
"""模拟"笔翻过来"：用 BTN_TOOL_RUBBER(0x141) 走一遍，验证笔尾=橡皮。"""
import struct, time
RAW_X_MAX, RAW_Y_MAX = 20966, 15725
FB_W, FB_H = 1404, 1872
fd = open("/dev/input/event1", "wb", buffering=0)
_k = [0]
def ev(t, c, v): fd.write(struct.pack("<iiHHi", 0, 0, t, c, v))
def j(v):
    _k[0] = (_k[0] + 1) % 3
    return v + _k[0]
def dev(x, y): return int(round((1 - y / FB_H) * RAW_X_MAX)), int(round(x / FB_W * RAW_Y_MAX))
def stroke(y, x0=200, x1=1200, n=30):
    rx, ry = dev(x0, y)
    ev(1, 0x141, 1); ev(0, 0, 0); time.sleep(0.05)          # BTN_TOOL_RUBBER
    ev(3, 0, j(rx)); ev(3, 1, j(ry)); ev(3, 0x18, j(2400)); ev(1, 0x14A, 1); ev(0, 0, 0)
    time.sleep(0.02)
    for i in range(1, n):
        t = i / (n - 1)
        x = x0 + (x1 - x0) * t
        rx, ry = dev(x, y)
        ev(3, 0, j(rx)); ev(3, 1, j(ry)); ev(3, 0x18, j(2400)); ev(0, 0, 0)
        time.sleep(0.012)
    ev(1, 0x14A, 0); ev(0, 0, 0); ev(1, 0x141, 0); ev(0, 0, 0)
    time.sleep(0.2)
stroke(400)
print("rubber stroke done")
