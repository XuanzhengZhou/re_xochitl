# NOTICE

[English](NOTICE.md) | **中文**

## 商标与归属

reMarkable、reMarkable 2、xochitl 是 **reMarkable AS** 的商标 / 产品名。
本项目与 reMarkable AS 没有任何隶属或背书关系。

## 不包含专有资产

本仓库**不包含**从设备上提取的任何 reMarkable 专有内容，包括但不限于：

- `xochitl` 可执行文件及其任何部分；
- `libQt5Core` / `libQt5Gui` / `libepaper` / `libqvnc` / `librm2fb_*` 等共享库；
- 设备字体（Maison Neue、icomoon 等）；
- 从二进制中 carve 出的 QML 资源（`.qml`）、对象树 dump、反汇编输出；
- 固件镜像、u-boot、设备树、根文件系统。

仓库里只有**作者自己写的代码**和**对格式/协议的分析笔记**。

## 从设备数据派生的文件

以下文件由设备上的波形数据生成，能通过编译，但**不随本项目的许可证授权**：

| 文件 | 来源 | 建议 |
|---|---|---|
| `reverse/c/wbf_lut.h` | 官方 `.wbf` 波形文件（`320_R259_AFAB21_ED103TC2M1_TC`）导出的 LUT | 用 `reverse/python/wbf.py` 从自己设备的波形文件重新生成 |
| `reverse/c/zm334.h` | zplash 波形引擎的控制位模板 + LUT | 同上 |

要发布/商用之前，请自行确认这些表在你所在司法辖区的可用性，或者干脆跑一遍
生成脚本、把生成结果换成你自己的。

## 逆向的目的

本项目的逆向工作只针对**自己拥有的设备**，目的限于：

1. 互操作性——让自己写的软件能在自己买的硬件上正确驱动屏幕和笔；
2. 数据可迁移——读懂 `.rm` 格式，保证自己的手写笔记不会因为换软件而丢失；
3. 学习——把 e-ink 波形、局部刷新这类工程问题弄清楚。

不包含破解付费功能、绕过 DRM、绕过订阅校验或分发官方软件的任何内容。

## 第三方

工作区里曾经 clone 过一些上游项目用于参考（**未包含在本仓库中**）：

- [rmkit](https://github.com/ddvk/rmkit)
- [riddle](https://github.com/ddvk/riddle)
- [reMarkable2-framebuffer](https://github.com/toltec-dev/rm2fb)
- [epdiy](https://github.com/vroland/epdiy)
- [smart_remarkable](https://github.com/awwaiid/smart_remarkable)

这些项目各有自己的许可证，如需使用请到上游获取。

## 许可证

本仓库全部内容以 MIT 许可证发布（见 [LICENSE](LICENSE)），**唯一例外**是上面列出的
两个派生物：`reverse/c/wbf_lut.h`、`reverse/c/zm334.h`。它们只为让代码编过而保留，
数据来自作者自己设备，按互操作目的提供，**不附带任何授权**。需要明确许可证的话，
用 `reverse/python/wbf.py` 从你自己的设备重新生成。

## 无担保

本项目是逆向工程的产物，涉及直接写 `/dev/mem`、`/dev/fb0`、注入 Qt 进程等
高风险操作。**可能变砖、丢数据、损坏屏幕**。作者不对任何后果负责。
