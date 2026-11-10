# 流畅度：量到哪、改了什么、现在多少

> 设备实测（rM2，armv7，release）与本机实测（M 系列，release）分开记。
> 基准命令：`./target/release/rmdev --bench-ui`（本机）、`RM_TIMING=1` 看设备日志。

## 1. 改过的三处

| 改动 | 原来 | 现在 | 依据 |
|---|---|---|---|
| **页面光栅缓存**（`redraw_page`） | 每次重绘都把全部笔迹重画一遍 | 内容指纹没变就直接贴回上次的墨迹缓冲 | 设备：`画墨 115ms → 3ms`，整次重绘 `155ms → 58ms` |
| **卡片缩略图贴图**（`draw_tile_sel`） | 逐像素 `put` | `Fb::blit_gray`：整行写 + 256 项灰度查表 | 本机 6 张卡 `1.04ms → 0.37ms`；设备上按行写能向量化，差距更大 |
| **缩略图缓存换 `Rc`** | 每次重绘 `clone` 242 KB × N 张卡 | 只拷指针 | 6 张卡一次重绘省 1.4 MB 拷贝 |
| **总览缩略图缓存** | 每次开 Pages 都把每页光栅化 | `(uuid,页) + 笔迹指纹` 命中就复用 | 设备：单页 53ms → 0ms（二次打开） |

顺手修掉一个真 bug：`Fb::row_mut` 缺 `x >= width` 检查，屏幕右侧越界的贴图会算出负宽度
再被当成巨大 `usize`（release 下加法回绕 → 索引越界 panic）。

## 2. 本机基准（release，`--bench-ui`）

```
draw_library（含缩略图冷渲染）:  62.9 ms/帧
draw_library（缩略图命中缓存）:  16.2 ms/帧
open_at 首次（冷）:             88~119 ms
open_at 再次（光栅缓存）:        37 ms/帧
redraw_page（内容没变）:         2.1 ms/帧      ← 改动前 ~119 ms
6 张卡逐像素 put:                1.04 ms
6 张卡 blit_gray :               0.37 ms
30 段文字（缓存命中）:            0.4~1.0 ms
canvas paint_all 整屏 1:1:       1.5~3.3 ms
```

> 注：`redraw_page` 里有 250ms 的整屏刷新限速（EPDC 队列不能叠），
> 基准里把 `last_full` 清掉了，量到的才是 CPU 时间。

## 3. 设备实测（`/tmp/rmdev.log`，`RM_TIMING=1`）

```
首次开页：redraw_page 总计=155ms（画墨=115ms 贴图=36ms 浮层=3ms）
再次重绘：redraw_page 总计= 58ms（画墨=  3ms 贴图=50ms 浮层=5ms）
工具条局部刷新：repaint 2 块 432000px 贴图=9.1ms 浮层=12.8ms 刷新=0.4ms
总览缩略图：单页 53ms（首次）/ 0ms（缓存命中）
```

现在一次整屏重绘的 CPU 只剩 58ms，其中 50ms 是 263 万像素的一次性贴图
（`blit_1to1`，按行 zip 写 + 查表）。剩下的瓶颈是 e-ink 面板自己的刷新时间，
不是我们的计算——再优化 CPU 也只能省到 50ms 这个地板。

### 3.1 主线程被「非绘制」卡住：电池 440 ms + `df` 947 ms

2026-11-09 修「整个界面像冻住一会儿，然后才动；一进 Settings 就卡，不分页」。
先加了一条 `[slow]` 日志（主循环里 `handle` / `flush` / `tick` 超过 200 ms 就打，
`tick` 内部再按段打），一次开机就定位到两处：

| 位置 | 耗时 | 为什么 |
|---|---|---|
| `draw_status` → `read_battery()` → `battery::read()` | **440 ms** | 读 `/sys/class/power_supply/*/uevent` 走 I2C，`time cat` 实测就是 0.44 s。状态栏**每帧都画**，等于每次重绘库都卡半秒；`SettingsView::new` 也读一次 → 进设置卡半秒 |
| `p4_check_disk` → `disk::read()` → fork `df -k` | **947 ms** | 主线程每 60 s 同步等子进程；开机时 `df` 的二进制和页缓存全是冷的 |

改法：

* 电池 → 后台线程每 10 s 刷一份 `Battery` 快照（`static BATT: Mutex<Option<Battery>>`），
  绘制路径只 `Copy` 一次。启动时同步读一次填好，第一帧状态栏就是真值。
* 磁盘 → `statvfs(3)`（一个 syscall，微秒级），口径和 `df -k` 的 Used 列一致。

改完同一份日志：

```
改前: [slow] tick 前段 1025ms [("重画库", 583), ("电池", 439)]
改后: [slow] tick 前段  176ms [("重画库", 174)]
```

**教训**：墨水屏上「卡」几乎都不是绘制慢，而是绘制路径里塞了会阻塞的系统调用
（I2C、fork）。往 `draw_*` / `SettingsView::new` 里加读硬件、读子进程的代码之前，
先问一句"这次调用会不会等 100 ms 以上"。

