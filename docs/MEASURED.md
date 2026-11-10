# 实测校正（用官方真实 .rm 反推公式）

> 反编译能给出公式的**形状**，但常量经常在跳转表/寄存器复用里丢掉。
> 这一步的做是：拿用户在设备上真笔写的一页 `.rm`，**直接读原始 float32**，
> 和 `M1-pen-input.md` §4.5 的公式表逐项对。结果三处对得上、两处对不上。
>
> 数据源：`/home/root/.local/share/remarkable/xochitl/1554f45c-.../c6199893-....rm`
> （331,322 B，292 条线：282×tool15 + 8×tool6 + 2×tool8）

---

## 1. 已确认（公式与官方逐点一致）

### 1.1 点的磁盘布局 = 24 字节 6 float（v1 点格式）

用 `struct.unpack('<6f', chunk)` 直接解，第一点：

```
x=-267.1155  y=356.1505  speed=0.2504  dir=0.0000  width=57.6000  pressure=0.0083
```

六字段顺序、步长、float32 —— 与 `M1 §2.4` 完全一致。**这台设备写 v1（24 B），不是 v2（14 B）。**

### 1.2 `pressure` 存的是归一化值

0.0083 / 0.0215 / 0.0303 一路到 1.0 —— 与内存模型的 `0..1` 一致。
`rmscene` 的 `*255` 是它自己的 v1 兼容层，**不要**拿它当磁盘真值。

### 1.3 `dir` 存的是弧度

相邻点实测 0.0000 → 1.5708 → 2.8989，即 0 → π/2 → 2.9 rad。
`rmscene` 的 `255·θ/2π` 同样只是它的显示换算。

### 1.4 `width` 与 BallpointV2 公式吻合

tool 15 / thickness 2.0（282 条线的全部点）：**width ∈ [3.0, 4.5]**

公式 `w = max(p'·3, 2.0) × (2/1.35)` ⇒ **[2.96, 4.44]** —— 吻合到小数第二位。
（`p'` 是 v2 重映射后的压力；`2/1.35 = 1.4815` 是 `THICK_DIV`。）

---

## 2. 需要修正（规格表与实测不符）

| Tool | 规格原文 | 实测 | 结论 |
|---|---|---|---|
| 6 Eraser | `w = 10.0·t` | t=2.4 → **57.6**；t=1.2 → **14.4** | **`w = 10·t²`** |
| 8 EraseSection | `w = 2.0·t` | t=2.0 → **6.0** | **`w = 3·t`** |

验证：`10 × 2.4² = 57.6` ✓、`10 × 1.2² = 14.4` ✓、`3 × 2.0 = 6.0` ✓。

这也反过来解释了 QML 里那段"橡皮有历史上的平方映射"的注释——
平方确实落在 `line.cpp` 的点展开里，最终值就是 `10·t²`。

> 代码已按实测修正：`rmdev/src/pen/width.rs` 的 `Tool::Eraser` / `Tool::EraseSection`。
> 单测里用上面三个锚点锁死。

---

## 3. 仍待解（不影响落墨，只影响两支老笔）

`speed` 与相邻点位移不是线性关系。实测同一页里：

| i | 位移 d | 磁盘 speed |
|---|---|---|
| 6 | 4.27 | 1.93 |
| 7 | 3.63 | 2.03 |
| 22 | 6.72 | 0.69 |
| 25 | 4.72 | 2.19 |

比值在 0.1~0.56 之间飘。三种可能，暂不判：

1. 官方对坐标做了低通，`speed` 用的是**滤波前**的位移，而 `.rm` 存的是滤波后的坐标；
2. `speed` 里其实掺了采样间隔（`dt` 寄存器在反编译里"传了没用"可能只是没读到）；
3. 采样不是等间隔，`speed` 是每段的瞬时值再平滑。

**影响面**：只有 `Paintbrush / Pencil` 的宽度公式吃 `speed`（其余工具不吃）。
先按 `d/100` 归一化跑，等真的开始调这两支笔的手感时再用同一页数据拟合。

---

## 4. V1（`width` 的像素语义）怎么定

`M2 §4.4` 的 `renderSolidLine` 里：`pen_width = base(+0x98) × point.width`，
`point.width` 就是上面那个 3.0~4.5，`base` 是场景→视图缩放。

⇒ **1:1 视图下 `像素宽 == point.width`**（k=1）。
旁证：官方这页笔迹的视觉粗细就是 3~4 px，和 `width` 的 3.0~4.5 同量级。
要更硬就需要同一条笔画"官方渲染 vs 我们渲染"的逐像素比对，留到 A/B 阶段。

---

## 5. 复现方法

```bash
# 1) 从设备取一页 .rm
scp remarkable:'/home/root/.local/share/remarkable/xochitl/<uuid>/<page>.rm' /tmp/official.rm

# 2) 绕开 rmscene 的缩放层，直接读原始 float32
python3 - <<'PY'
import rmscene, struct
for b in rmscene.read_blocks(open('/tmp/official.rm','rb')):
    if type(b).__name__ == 'SceneLineItemBlock' and b.item.value:
        L = b.item.value
        print(L.tool, L.thickness_scale, len(L.points))
PY

# 3) 单调打印：在 point_from_stream 里读 stream.data.data.tell() 附近的 24 字节
```
