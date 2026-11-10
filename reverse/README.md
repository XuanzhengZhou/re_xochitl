# reverse/ — Reverse-engineering tools and experimental code

**English** | [中文](README.zh-CN.md)

This directory is the "work out what the official software actually does" half of the
project: driver experiments, format parsing, process probes and analysis scripts. The
production code lives in [`../rmdev/`](../rmdev/); what is here is the scaffolding that
existed before it could be built.

> ⚠️ These programs read `/dev/mem` directly, write `/dev/fb0`, and inject `.so` files
> into a running Qt process. Get it wrong and you get a garbled screen, a hang, or even
> a bricked device. Only run them on hardware you own.

## Contents

| Path | What is in it |
|---|---|
| `c/` | User-space panel driver experiments (`epd.c` / `wr.c` / `wr2.c`), QML dump probes (`qmldump*.c`), waveform LUT headers |
| `python/` | `.wbf` parsing, `.rm` → PNG rendering, ink injection/transfer, QML carving, u-boot env tools, device-side HTTP proxy |
| `sh/` | Deploy scripts, on-device agent bridge, campus-network portal auto-login template + systemd units |
| `panel/` | swtcon analysis scripts (phase/band extraction, constant scanning, FDT dump) |
| `elf-tools/` | Small ARM ELF static-analysis tools: PLT / exception index / constant references / disassembly wrapper |
| `m1/` | Milestone 1 prototype: ink capture + canvas |
| `rust-probe/` | armv7 cross-compiled probe (static and dynamic builds) |

## A few caveats

- `c/wbf_lut.h` and `c/zm334.h` are tables generated from device waveform data. They
  compile, but they are **not covered by the project licence**; see
  [`../NOTICE.md`](../NOTICE.md). If you need them, regenerate them from your own
  device's `.wbf` files with `python/wbf.py`.
- `sh/campus_wlan_login.sh` is a **generic template** for Dr.COM / eportal portals:
  domains, ports and parameter names differ between schools, so edit `HOST` / `PORT`
  and the query string inside `login()` before using it. Keep credentials in
  `/home/root/.campus_cred` (`chmod 600`) and do not commit them.
- `sh/deploy_qmldump*.sh` default to device address `10.11.99.1` (the official address
  of the USB network interface).
- `sh/ask-ink.sh` / `sh/device_ask.sh` are the experimental "handwritten page → server
  agent" path. They depend on `dsh` running on the server; before running them, point
  `PATH` and `$HOME/.dsh/web.env` at your own setup.
