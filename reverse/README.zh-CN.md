# reverse/ —— 逆向工具与实验代码

这个目录是"怎么把官方行为搞清楚"的那一半：驱动实验、格式解析、进程探针、
分析脚本。生产代码在 `../rmdev/`，这里是把它做出来之前的脚手架。

> ⚠️ 这些程序会直接读 `/dev/mem`、写 `/dev/fb0`、往运行中的 Qt 进程注入 `.so`。
> 用错会花屏、死机、甚至变砖。只在你自己拥有的设备上跑。

## 目录

| 路径 | 内容 |
|---|---|
| `c/` | 用户态面板驱动实验（`epd.c` / `wr.c` / `wr2.c`）、QML dump 探针（`qmldump*.c`）、波形 LUT 头 |
| `python/` | `.wbf` 解析、`.rm` → PNG 渲染、笔迹注入/传输、QML carve、u-boot env 工具、设备侧 HTTP 代理 |
| `sh/` | 部署脚本、设备端 agent 桥、校园网门户自动认证模板 + systemd 单元 |
| `panel/` | swtcon 分析脚本（相位/band 提取、常数扫描、FDT dump） |
| `elf-tools/` | ARM ELF 静态分析小工具：PLT / 异常索引 / 常量引用 / 反汇编封装 |
| `m1/` | 里程碑 1 原型：笔迹采集 + 画布 |
| `rust-probe/` | armv7 交叉编译探针（静态 / 动态两种产物） |

## 几个注意点

- `c/wbf_lut.h`、`c/zm334.h` 是从设备波形数据生成的表，能编译，
  但**不在项目许可证范围内**，见 `../NOTICE.md`。要用就拿 `python/wbf.py`
  从自己设备的 `.wbf` 重新生成。
- `sh/campus_wlan_login.sh` 是 Dr.COM / eportal 门户的**通用模板**，不同学校的
  域名、端口和参数名不一样，用之前改 `HOST` / `PORT` 和 `login()` 里的查询串。
  凭据放 `/home/root/.campus_cred`（`chmod 600`），不要提交。
- `sh/deploy_qmldump*.sh` 默认设备地址是 `10.11.99.1`（USB 网卡的官方地址）。
- `sh/ask-ink.sh` / `sh/device_ask.sh` 是"手写页 → 服务器 agent"的实验链路，
  依赖服务器上的 `dsh`，跑之前先把 `PATH` 和 `$HOME/.dsh/web.env` 改成自己的。
