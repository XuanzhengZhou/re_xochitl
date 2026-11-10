# M1 — rm-sketch 骨架

一个从零写的 Rust 笔记程序，跑在 rm2fb 上，**不依赖 xochitl 的任何代码**。

## 现在能做什么

- 接管整块屏幕（1404×1872 @16bpp）
- 以硬件事件率读取 Wacom 笔，压力 → 笔宽（1~4 px）
- 笔迹即时上屏，只刷新脏矩形（`WF_DU` + `UPDATE_MODE_PARTIAL`）
- 工具栏 + 撤销按钮（点一下回退一笔，全屏 `WF_GC16` 重绘）
- 左下角连点三次退出，退出后自动恢复官方 UI

## 已验证（不是"应该能跑"）

| 验证项 | 结果 |
|---|---|
| 画布所有权 | 截图确认：灰工具栏 + 白画布，md5 变化 |
| 坐标映射 | 注入 80 点合成笔画，落点 bbox `x 356..1072, y 532..1427`，理论值 `357..1071 / 533..1426` → **误差 1 px** |
| 局部刷新 | 长笔画连续无断裂，说明逐段 dirty-rect 刷新有效 |
| **悬空不落墨** | 注入 60 点悬空移动（无 `BTN_TOUCH`）→ 屏幕 0 像素；落笔后同样轨迹 → 4478 像素 |
| 撤销按钮 | 绘制正确（点击逻辑待真人验证） |
| 退出后恢复 | `systemctl start remux` 正常拉起官方 UI |

## 笔刷（软硬可调）

压力→笔宽是一条幂曲线：`r = min + (max - min) * (p/4096)^k`

| 预设 | k | 半径 | 手感 |
|---|---|---|---|
| `pencil` | 2.0 | 0 → 1.5 | 轻压是 1px 发丝线，用力才变粗 |
| `pen` | 1.7 | 0 → 2.4 | 渐进变粗（默认） |
| `fineliner` | — | 固定 1.2 | 恒宽，像签字笔 |
| `marker` | 0.9 | 1.5 → 3.6 | 一落笔就宽 |

**默认就是 `fineliner`（恒宽）**，因为实测最接近官方钢笔的手感；压力曲线仍可用 `RM_BRUSH` 切回。

**k 越大越"硬"**（要压得重才变粗），k 越小越"软"（轻碰就散开）。
`k → ∞` 等价于固定宽度，这也就是 `fineliner` 的效果。

工具栏第二个方块是切换按钮，点一下换一种，样张实时显示当前手感。

不想重编译也可以调：

```sh
RM_BRUSH=0..3      # 预设索引
RM_BRUSH_K=2.4     # 改硬度
RM_BRUSH_W=0.5,2.0 # 改最小/最大半径
RM_BRUSH=1 RM_BRUSH_W=1.4,1.4 /opt/bin/rm2fb-client /home/root/rm-sketch   # 自定义恒宽
```

压力还会做 EMA 平滑（`0.6*old + 0.4*new`），避免采样抖动导致线宽跳变。

### 实测（合成笔迹，压力 200→4000 线性上升）

| 笔刷 | x=350 | x=550 | x=800 | x=1050 |
|---|---|---|---|---|
| pencil k=2.0 | 1 | 1 | 3 | 3 |
| pen k=1.7 | 1 | 1 | 3 | 5 |
| fineliner | 3 | 3 | 3 | 3 |
| marker k=0.9 | 5 | 5 | 7 | 7 |

（单位 px，压力随 x 增大而增大）

## 跑法

```sh
# 把二进制放到设备
scp target/armv7-unknown-linux-gnueabihf/release/rm-sketch root@10.11.99.1:/home/root/
scp run.sh root@10.11.99.1:/home/root/

# 在设备上跑（会停掉官方 UI，退出后自动恢复）
ssh root@10.11.99.1 /home/root/run.sh

# 退出：左下角点三下
```

编译：

```sh
RUSTFLAGS="-C linker=$HOME/bin/zigcc" \
  cargo build --release --target armv7-unknown-linux-gnueabihf
```

**必须动态链接**（`gnueabihf`）。静态 musl 产物不会被 rm2fb 的 `LD_PRELOAD` 接管，
看到的会是 mxsfb 的原始 260×1408 线缓冲。

## 踩到的两个坑（都值得记住）

### 1. `bits_per_pixel` 不是字节数
`FBIOGET_VSCREENINFO` 的 `bits_per_pixel = 16`，我直接当成了字节步长，
于是每行只写了 1/8 的像素。表现很有迷惑性：映射正确、校验和一致、ioctl 返回 0，
但屏幕上就是不变。

**判定手法**：现在 `--selftest` 会打印"清屏后还剩多少非白像素"，
并再做一次原始 `memset` 对比。memset 后为 0、循环后为 170,812，一下就能定位到是循环本身的问题。

### 2. `BTN_TOOL_PEN` 是"笔进入范围"，不是"落笔"
一开始把它当落笔信号，结果笔悬空也会画。正确状态机：

```
BTN_TOOL_PEN  笔在感应范围内（悬空也算）    → 只更新位置，不落墨
BTN_TOUCH     笔尖接触玻璃                  → 落笔 / 抬笔
ABS_PRESSURE  压力                          → 笔宽（固件不发 BTN_TOUCH 时的降级判据）

contact = in_range && (saw_touch ? btn_touch : pressure > 40)
```

### 3. 落笔瞬间的坐标是上一次的
`BTN_TOOL_PEN 1` 先到，位置事件后到。直接用它画会在角落留一个脏点。
现在必须等本次接触的第一组 `ABS_X/ABS_Y` + `SYN` 才开始成笔。

## 已知未做（M2 起）

- 页面管理、`.rm` 存读
- 压力曲线与笔刷质感（现在只有线性 1~4 px）
- 悬停光标（现在悬空完全不显示，可以做一个跟随的小圆点）
- 橡皮、套索、缩放
- 暂停/唤醒后重绘
- 多指触摸（现在只用笔，触摸设备 `event2` 没接）

## 文件

```
m1/
├── Cargo.toml
├── src/main.rs       ~430 行，单文件
├── run.sh            设备端启动/恢复脚本
├── inject.py         合成笔迹注入（测试坐标映射用）
├── m4.png            空白画布截图
└── m6.png            注入笔画截图
```
