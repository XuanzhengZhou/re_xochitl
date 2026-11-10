#!/usr/bin/env python3
"""向 rM2 的 /dev/input/event1 注入合成笔迹，用于验证 rmdev 的笔宽/压力/映射。

用法：在设备上 `python3 inject_rmdev.py <场景名>`

注意两个坑：
  * 内核会丢掉重复的 ABS 值（input_handle_abs_event），所以坐标必须抖动；
  * 落笔判定是 BTN_TOUCH，不是 pressure——压力只在落笔后影响宽度。
"""
import struct
import sys
import time

SYN, KEY, ABS = 0, 1, 3
BTN_TOOL_PEN, BTN_TOUCH = 0x140, 0x14A
AX_X, AX_Y, AX_P, AX_TX, AX_TY = 0, 1, 0x18, 0x1A, 0x1B

RAW_X_MAX, RAW_Y_MAX = 20966, 15725
FB_W, FB_H = 1404, 1872

fd = open("/dev/input/event1", "wb", buffering=0)
_jit = [0]


def ev(t, c, v):
    fd.write(struct.pack("<iiHHi", 0, 0, t, c, v))


_k = [0]


def jitter(v):
    """内核丢重复 ABS 值。用轮转偏移，既不重复也不用每个点去 ioctl（那样太慢）。"""
    _k[0] = (_k[0] + 1) % 3
    return v + _k[0]


def to_dev(x, y):
    """屏幕像素 → 设备原始值（§4.1 的逆向：y 来自 ABS_X 且镜像）。"""
    rx = int(round((1.0 - y / FB_H) * RAW_X_MAX))
    ry = int(round(x / FB_W * RAW_Y_MAX))
    return rx, ry


def hover(on=True):
    ev(KEY, BTN_TOOL_PEN, 1 if on else 0)
    ev(SYN, 0, 0)


def down(x, y, pressure=2400, tilt=(0, 0)):
    rx, ry = to_dev(x, y)
    ev(ABS, AX_X, rx)
    ev(ABS, AX_Y, ry)
    ev(ABS, AX_P, pressure)
    ev(ABS, AX_TX, tilt[0])
    ev(ABS, AX_TY, tilt[1])
    ev(KEY, BTN_TOUCH, 1)
    ev(SYN, 0, 0)


def move(x, y, pressure=2400, tilt=(0, 0)):
    rx, ry = to_dev(x, y)
    ev(ABS, AX_X, jitter(rx))
    ev(ABS, AX_Y, jitter(ry))
    ev(ABS, AX_P, jitter(pressure))
    ev(ABS, AX_TX, tilt[0])
    ev(ABS, AX_TY, tilt[1])
    ev(SYN, 0, 0)


def up():
    ev(KEY, BTN_TOUCH, 0)
    ev(SYN, 0, 0)


def stroke(points, dt=0.008, tilt=(0, 0)):
    """points = [(x, y, pressure), ...]"""
    x0, y0, p0 = points[0]
    hover(True)
    time.sleep(0.05)
    down(x0, y0, p0, tilt)
    time.sleep(dt)
    for x, y, p in points[1:]:
        move(x, y, p, tilt)
        time.sleep(dt)
    time.sleep(dt)
    up()
    time.sleep(0.05)
    hover(False)
    time.sleep(0.1)


def tap(x, y, pressure=2400):
    hover(True)
    time.sleep(0.05)
    down(x, y, pressure)
    time.sleep(0.06)
    up()
    time.sleep(0.05)
    hover(False)
    time.sleep(0.25)


def line(x0, y0, x1, y1, n=40, p0=2400, p1=2400, tilt=(0, 0)):
    pts = []
    for i in range(n):
        t = i / (n - 1)
        pts.append(
            (
                x0 + (x1 - x0) * t,
                y0 + (y1 - y0) * t,
                int(p0 + (p1 - p0) * t),
            )
        )
    stroke(pts, tilt=tilt)


# ---------------------------------------------------------------- 场景

def slot(i):
    """顶栏第 i 个笔槽的中心（槽宽 156，行高 90）。"""
    return i * 156 + 78, 45


def scene_basic():
    """默认笔（FinelinerV2，中号）：一条恒定粗细的横线。"""
    line(200, 400, 1200, 400)


def scene_pressure():
    """同一支笔，压力从 0 爬到最大——Fineliner 应该**完全不变**。"""
    line(200, 700, 1200, 700, p0=100, p1=4090)


def scene_marker():
    """切到 MarkerV2（槽 4），压力从 0 爬到最大——应该明显变粗。"""
    tap(*slot(4))
    line(200, 1000, 1200, 1000, p0=100, p1=4090)


def scene_highlighter():
    """槽 5 = HighlighterV2，恒定 30 单位宽。"""
    tap(*slot(5))
    line(200, 1300, 1200, 1300)


def scene_eraser():
    """槽 8 = Eraser，10×thickness。"""
    tap(*slot(8))
    line(200, 1600, 1200, 1600)


def scene_thickness():
    """三档厚度对比（FinelinerV2）：三个槽位分别画一条。"""
    tap(*slot(0))
    for by, tx in [(300, 78), (600, 702), (900, 1326)]:
        tap(tx, 120)     # 厚度档：3 等分的中间
        line(200, by, 1200, by)


def scene_undo():
    line(200, 500, 1200, 500)
    line(200, 700, 1200, 700)
    line(200, 900, 1200, 900)
    time.sleep(0.3)
    tap(32, 32)  # 撤销按钮


def thick(tier):
    """顶栏第二行：三档厚度。"""
    tap(78 + (tier - 1) * 468, 120)


def scene_suite():
    """一次跑完九支笔 + 三档厚度，每支笔一条压力斜坡，方便逐条量宽度。"""
    # FinelinerV2，三档厚度
    tap(*slot(0))
    for tier, by in [(1, 260), (2, 400), (3, 540)]:
        thick(tier)
        line(200, by, 1200, by, p0=200, p1=4000)
    # BallpointV2
    thick(2)
    tap(*slot(1))
    line(200, 700, 1200, 700, p0=200, p1=4000)
    # PencilV2
    tap(*slot(3))
    line(200, 850, 1200, 850, p0=200, p1=4000)
    # MarkerV2，带倾角
    tap(*slot(4))
    line(200, 1000, 1200, 1000, p0=200, p1=4000, tilt=(18, 0))
    # HighlighterV2
    tap(*slot(5))
    line(200, 1150, 1200, 1150, p0=200, p1=4000)
    # Calligraphy，带倾角
    tap(*slot(7))
    line(200, 1300, 1200, 1300, p0=200, p1=4000, tilt=(18, 18))
    # Eraser
    tap(*slot(8))
    line(200, 1450, 1200, 1450, p0=200, p1=4000)
    # SharpPencilV2（不吃压力）
    tap(*slot(2))
    line(200, 1600, 1200, 1600, p0=200, p1=4000)


SCENES = {
    "suite": scene_suite,
    "basic": scene_basic,
    "pressure": scene_pressure,
    "marker": scene_marker,
    "highlighter": scene_highlighter,
    "eraser": scene_eraser,
    "thickness": scene_thickness,
    "undo": scene_undo,
}

if __name__ == "__main__":
    name = sys.argv[1] if len(sys.argv) > 1 else "all"
    if name == "all":
        scene_basic()
        time.sleep(0.2)
        scene_pressure()
    else:
        SCENES[name]()
    print("injected:", name)
