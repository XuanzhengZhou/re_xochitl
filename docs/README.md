# docs/ — reverse-engineering notes

These are **working notes**, not a specification with any guarantee. They were written
for the author, so expect informal prose, "measured" markers, and observations that
were never conclusively resolved.

English translations of every note live in [`en/`](en/).
中文原版就在本目录下，文件名相同。

## Panel and display

| Document | Topic |
|---|---|
| [`PANEL-HARDWARE.md`](en/PANEL-HARDWARE.md) | Full rM2 screen model: EPD + software TCON, timing, phases, refresh modes |
| [`SWTCON.md`](en/SWTCON.md) | No EPDC, only user-space swtcon: how a frame actually gets written |
| [`SWTCON-POLICY.md`](en/SWTCON-POLICY.md) | xochitl refresh policy and the waveform-table import path |
| [`WBF.md`](en/WBF.md) | Field-by-field analysis of the E Ink `.wbf` binary format |
| [`ZSPLASH.md`](en/ZSPLASH.md) | Running the stock waveform engine (zplash) in user space |
| [`WHITE.md`](en/WHITE.md) | White background, partial refresh, grey linearity, temperature bins |
| [`PANEL-DRIVER.md`](en/PANEL-DRIVER.md) | How the Rust panel driver is organised (raw / wbf / encode) |

## Device and boot

| Document | Topic |
|---|---|
| [`BOOT.md`](en/BOOT.md) | Boot chain, systemd units, panel INIT pitfalls |
| [`RM2FB.md`](en/RM2FB.md) | Root cause of “screen frozen but the system is alive” |
| [`POWER.md`](en/POWER.md) | Power key, sleep, wake, lock screen |

## Documents and pen input

| Document | Topic |
|---|---|
| [`FORMATS.md`](en/FORMATS.md) | E-book format support matrix and trade-offs |
| [`MEASURED.md`](en/MEASURED.md) | Correcting pen-width formulas against real `.rm` files |
| [`PERF.md`](en/PERF.md) | Smoothness: what was measured, what changed, where it stands |

## Cloud and sync

| Document | Topic |
|---|---|
| [`CLOUD.md`](en/CLOUD.md) | Self-hosted cloud: protocol facts, interface contract, conflict policy |

---

## Notes

- IPs in the documents are placeholders (`100.64.0.x`, `10.0.0.x`, `192.168.1.x`);
  `10.11.99.1` is the official fixed address of the reMarkable USB network interface.
- Bare paths such as `src/...` and `tools/...` refer to `../rmdev/` unless stated otherwise.
- `xochitl`, the QML tree and `.wbf` samples come from a device the author owns and are
  **not** distributed with this repository; obtain them from your own device.

---

## 中文

中文原版与英文翻译一一对应：中文在 `docs/NAME.md`，英文在 `docs/en/NAME.md`。

| 文档 | 讲什么 |
|---|---|
| [FORMATS](FORMATS.md) | 电子书格式支持矩阵与取舍 |
| [PANEL-HARDWARE](PANEL-HARDWARE.md) | rM2 屏幕完整模型：EPD + 软件 TCON |
| [PANEL-DRIVER](PANEL-DRIVER.md) | Rust 面板驱动怎么组织：raw / wbf / encode |
| [SWTCON](SWTCON.md) | 没有 EPDC，只有用户态 swtcon：一幅画面怎么被写出去 |
| [SWTCON-POLICY](SWTCON-POLICY.md) | xochitl 的刷新策略与波形表导入入口 |
| [WBF](WBF.md) | E Ink `.wbf` 二进制格式逐字段分析 |
| [ZSPLASH](ZSPLASH.md) | 官方波形引擎 zplash 在用户态跑起来的过程 |
| [WHITE](WHITE.md) | 白底、局部刷新、灰阶线性度、温度档 |
| [RM2FB](RM2FB.md) | 「屏幕卡死但系统活着」的根因与修复 |
| [BOOT](BOOT.md) | 开机链、systemd 服务编排、面板 INIT 的实测坑 |
| [POWER](POWER.md) | 电源键、睡眠、唤醒、锁屏 |
| [MEASURED](MEASURED.md) | 用真机 `.rm` 反推笔迹公式，逐项校正 |
| [PERF](PERF.md) | 流畅度：量了什么、改了什么、现在多少 |
| [CLOUD](CLOUD.md) | 自建云服务：官方协议事实、接口契约、冲突策略 |
