# re_xochitl

reMarkable 2 笔记应用的逆向工程与干净重写。

[English](README.md) | **中文**

这个项目把官方 `xochitl` 的**显示链路、笔迹模型、文档格式、同步协议**逆向出来，
然后用 Rust 把日常记笔记这件事重新实现了一遍，不依赖 Qt：设备端是 `rmdev`，
服务端是自托管的 `rmcloud`。

> 仓库里不含任何 reMarkable 的专有二进制、字体、QML 资源或固件镜像。
> 逆向的目的只有互操作性和在自己拥有的硬件上跑自己的软件。详见 [NOTICE.zh-CN.md](NOTICE.zh-CN.md)。

---

## 目录

| 路径 | 内容 | 规模 |
|---|---|---|
| [`rmdev/`](rmdev/) | 笔记应用（Rust，设备端） | ~94k 行 |
| [`rmcloud/`](rmcloud/) | 自托管服务端：同步 / 渲染 / OCR / AI / 网页 | ~13k 行 |
| [`reverse/`](reverse/) | 逆向工具：面板驱动、WBF 波形、QML 探针、ELF 分析 | ~5.8k 行 |
| [`docs/`](docs/) | 技术笔记（中文原版 + [英文翻译](docs/en/)） | ~5k 行 |

源码注释用中文写；上面的文档都有英文版。Issue 和 PR 用中文或英文都可以。

---

## 已经做出来的东西

### 笔迹（`rmdev/src/pen/`）

- 直接读 Wacom I2C digitizer 的 evdev，不经过 Qt。
- 磁盘上的 `Point` 是 24 字节 / 6 个 float，和官方 `.rm` 格式逐字段对齐。
- 23 种笔、9 种颜色；宽度 = f(压力, 速度, 方向)，公式由官方二进制反推，
  再用真机笔迹校准（见 [`docs/MEASURED.md`](docs/MEASURED.md)）。

### 屏幕（`rmdev/src/panel/`）

- **不用 rm2fb、不用 EPDC**：自己解析 `.wbf` 波形文件、自己编码相位、自己驱动 LCDIF。
- 支持 GC16 / DU / A2 等刷新模式，局部刷新按脏矩形合并。
- 开机 INIT、槽位探测、双槽写入这些坑记在
  [`docs/BOOT.md`](docs/BOOT.md) 和 [`docs/PANEL-HARDWARE.md`](docs/PANEL-HARDWARE.md)。

### 文档与阅读（`rmdev/src/doc/`）

PDF、EPUB、FB2、MOBI、DOCX、CBZ / 漫画、TXT（GBK），以及原生的 `.rm`。
带排版重排、分页缓存、TOC、全文搜索和批注。

### 界面与交互（`rmdev/src/panels/`, `src/ui/`, `src/settings/`）

库、抽屉、工具栏、翻页器、键盘；Wi-Fi 扫描与连接（`wpa_cli`）、电池、存储、
锁屏密码、语言、开发者页、实验开关、恢复出厂；横屏模式、左右手镜像布局。

### AI 与云（`rmdev/src/panels/ai.rs`, `src/cloud.rs`, `rmcloud/`）

框选提问、Live Note、回答写回 `AI` 图层、错题档案、引用 PDF。
设备与服务端配对后走增量同步（manifest / blob / commit），断线可续，
每 10 分钟安静自动同步一次。

### 逆向工具（`reverse/`）

- `c/epd.c`、`c/wr.c`、`c/wr2.c`：用户态面板驱动实验。
- `python/wbf.py` + `c/wbf_lut.h`：E Ink `.wbf` 波形格式解析与 LUT 导出。
- `c/qmldump*.c` + `sh/deploy_qmldump*.sh`：往运行中的 Qt 进程注入探针，
  抓出完整的 QML 对象树。
- `python/rm2png.py`、`send_ink.py`、`ingest_ink.py`：`.rm` 渲染与笔迹流传输。
- `python/carve_qml.py`：从 stripped 二进制里 carve 出内嵌 QML 资源。
- `python/rm2_envtool.py`：u-boot 环境变量（`ENV_IS_IN_FAT`）读写工具。
- `elf-tools/`：ARM ELF 静态分析小工具（PLT、异常索引、常量扫描）。
- `m1/`、`rust-probe/`：早期原型与交叉编译探针。

---

## 构建

### rmdev（设备端）

```bash
cd rmdev
cargo test                    # 纯逻辑单测，本机可跑

rustup target add armv7-unknown-linux-gnueabihf
cargo build --release --target armv7-unknown-linux-gnueabihf
```

交叉链接器在 `rmdev/.cargo/config.toml` 里配置（`zigcc` 包装的 `zig cc`，
自带 glibc 2.17 sysroot）。装法见该文件注释；不想用 zig 就换成
`arm-linux-gnueabihf-gcc`。

**要画屏必须动态链接**：静态 musl 产物不会被 `LD_PRELOAD` 接管，也看不到
rm2fb 画布。默认配置就是动态的。

`rmdev/tools/deploy.sh` 假设 `~/.ssh/config` 里有 `remarkable` 这个 host，
部署分三步：拷成 `.new` → `systemctl stop` → `mv` 过去再启动
（覆盖运行中的可执行文件会 `ETXTBSY`）。

### rmcloud（服务端）

```bash
cd rmcloud
cargo build --release
cargo test
```

单用户设计，默认只绑 Tailscale 地址。配置、依赖和部署细节见
[`rmcloud/README.md`](rmcloud/README.md) 与 [`docs/CLOUD.md`](docs/CLOUD.md)。

---

## 文档

中文原版在 `docs/NAME.md`，英文翻译在 `docs/en/NAME.md`。

| 文档 | 讲什么 |
|---|---|
| [FORMATS](docs/FORMATS.md) | 电子书格式支持矩阵与取舍 |
| [PANEL-HARDWARE](docs/PANEL-HARDWARE.md) | rM2 屏幕完整模型：EPD + 软件 TCON |
| [PANEL-DRIVER](docs/PANEL-DRIVER.md) | Rust 面板驱动怎么组织：raw / wbf / encode |
| [SWTCON](docs/SWTCON.md) | 没有 EPDC，只有用户态 swtcon：一幅画面怎么被写出去 |
| [SWTCON-POLICY](docs/SWTCON-POLICY.md) | xochitl 的刷新策略与波形表导入入口 |
| [WBF](docs/WBF.md) | E Ink `.wbf` 二进制格式逐字段分析 |
| [ZSPLASH](docs/ZSPLASH.md) | 官方波形引擎 zplash 在用户态跑起来的过程 |
| [WHITE](docs/WHITE.md) | 白底、局部刷新、灰阶线性度、温度档 |
| [RM2FB](docs/RM2FB.md) | 「屏幕卡死但系统活着」的根因与修复 |
| [BOOT](docs/BOOT.md) | 开机链、systemd 服务编排、面板 INIT 的实测坑 |
| [POWER](docs/POWER.md) | 电源键、睡眠、唤醒、锁屏 |
| [MEASURED](docs/MEASURED.md) | 用真机 `.rm` 反推笔迹公式，逐项校正 |
| [PERF](docs/PERF.md) | 流畅度：量了什么、改了什么、现在多少 |
| [CLOUD](docs/CLOUD.md) | 自建云服务：官方协议事实、接口契约、冲突策略 |

---

## 隐私与合规

这个仓库是从私有工作区**脱敏导出**的：

- **不含原始 git history**。原仓库的提交历史里有内网地址和密钥引用；
  这棵树从单个干净提交开始。
- 私钥、设备序列号、MAC、学号、个人邮箱、Tailscale 地址、校园网信息
  全部移除或替换成了占位符。
- 不含从设备上取出的专有二进制、字体或 QML 资源。
- `reverse/c/wbf_lut.h`、`reverse/c/zm334.h` 是从设备波形数据派生的表，代码需要它们，
  但**不在项目许可证范围内**；建议用 `reverse/python/wbf.py` 从自己设备重新生成。

商标、第三方项目和免责声明见 [NOTICE.zh-CN.md](NOTICE.zh-CN.md)。

---

## 许可证

MIT，见 [LICENSE](LICENSE)。Copyright (c) 2026 XuanzhengZhou。

MIT 覆盖本仓库全部内容，**唯一例外**是从设备数据派生的两个表
（`reverse/c/wbf_lut.h`、`reverse/c/zm334.h`）：它们只为让代码编过而保留，
不附带任何授权，说明见 [NOTICE.zh-CN.md](NOTICE.zh-CN.md#许可证)。
