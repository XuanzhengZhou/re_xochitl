# re_xochitl

Reverse engineering and a clean-room rewrite of the reMarkable 2 note app.

**English** | [中文](README.zh-CN.md)

The project reverse engineers the display pipeline, pen model, document formats and
sync protocol of the stock `xochitl`, then reimplements the everyday note-taking
experience in Rust without Qt: `rmdev` on the device, `rmcloud` on a self-hosted server.

> No reMarkable binaries, fonts, QML resources or firmware images are included.
> The goal of the reverse engineering is interoperability and running our own software
> on hardware we own. See [NOTICE.md](NOTICE.md).

---

## Layout

| Path | What it is | Size |
|---|---|---|
| [`rmdev/`](rmdev/) | The note app (Rust, device side) | ~94k lines |
| [`rmcloud/`](rmcloud/) | Self-hosted server: sync / render / OCR / AI / web UI | ~13k lines |
| [`reverse/`](reverse/) | Reverse-engineering tools: panel driver, WBF waveforms, QML probe, ELF analysis | ~5.8k lines |
| [`docs/`](docs/) | Technical notes (Chinese originals + [English translations](docs/en/)) | ~5k lines |

Source comments are written in Chinese; the documents above have English
translations. Issues and pull requests are welcome in either language.

---

## What works

### Pen input (`rmdev/src/pen/`)

- Reads the Wacom I2C digitizer straight from evdev, no Qt in the path.
- On-disk `Point` is 24 bytes / 6 floats, matching the official `.rm` format field by field.
- 23 tools and 9 colors; width is a function of pressure, speed and direction, derived
  from the stock binary and calibrated against real handwriting
  (see [`docs/en/MEASURED.md`](docs/en/MEASURED.md)).

### Display (`rmdev/src/panel/`)

- **No rm2fb, no EPDC**: parses `.wbf` waveform files, encodes the phases and drives
  the LCDIF directly.
- GC16 / DU / A2 refresh modes; partial updates merged into dirty rectangles.
- Boot-time INIT, slot probing and double-slot writes are documented in
  [`docs/en/BOOT.md`](docs/en/BOOT.md) and [`docs/en/PANEL-HARDWARE.md`](docs/en/PANEL-HARDWARE.md).

### Documents (`rmdev/src/doc/`)

PDF, EPUB, FB2, MOBI, DOCX, CBZ/comics, TXT (GBK) and the native `.rm`.
Reflow, page cache, TOC, full-text search and annotations.

### UI (`rmdev/src/panels/`, `src/ui/`, `src/settings/`)

Library, drawer, toolbar, page selector, keyboard; Wi-Fi scan/connect (`wpa_cli`),
battery, storage, lock-screen passcode, language, developer page, experimental
toggles, factory reset; landscape mode and left/right-handed mirrored layouts.

### AI and sync (`rmdev/src/panels/ai.rs`, `src/cloud.rs`, `rmcloud/`)

Selection-based Q&A, Live Note, answers written back into an `AI` layer, a mistakes
archive, reference PDFs. Devices pair with the server and sync incrementally
(manifest / blob / commit), resumable, with a quiet auto-sync every 10 minutes.

### Reverse-engineering tools (`reverse/`)

- `c/epd.c`, `c/wr.c`, `c/wr2.c`: user-space panel driver experiments.
- `python/wbf.py` + `c/wbf_lut.h`: E Ink `.wbf` waveform parser and LUT export.
- `c/qmldump*.c` + `sh/deploy_qmldump*.sh`: inject a probe into the running Qt process
  and dump the whole QML object tree.
- `python/rm2png.py`, `send_ink.py`, `ingest_ink.py`: `.rm` rendering and pen-stream transfer.
- `python/carve_qml.py`: carve embedded QML resources out of a stripped binary.
- `python/rm2_envtool.py`: u-boot environment (`ENV_IS_IN_FAT`) reader/writer.
- `elf-tools/`: small ARM ELF static-analysis helpers (PLT, exception index, constant scan).
- `m1/`, `rust-probe/`: early prototype and cross-compilation probes.

---

## Build

### rmdev (device)

```bash
cd rmdev
cargo test                    # pure-logic unit tests, runs on the host

rustup target add armv7-unknown-linux-gnueabihf
cargo build --release --target armv7-unknown-linux-gnueabihf
```

The cross linker is configured in `rmdev/.cargo/config.toml` (a `zigcc` wrapper around
`zig cc`, which ships a glibc 2.17 sysroot). See the comments there; switch to
`arm-linux-gnueabihf-gcc` if you prefer.

**The binary must be dynamically linked** to draw on screen: a static musl build is not
picked up by `LD_PRELOAD` and cannot reach the rm2fb canvas. The default configuration
is dynamic.

`rmdev/tools/deploy.sh` assumes a `remarkable` host in `~/.ssh/config` and deploys in
three steps: copy to `.new`, `systemctl stop`, then `mv` into place and start
(overwriting a running executable fails with `ETXTBSY`).

### rmcloud (server)

```bash
cd rmcloud
cargo build --release
cargo test
```

Single-user by design, binds to a Tailscale address by default. Configuration,
dependencies and deployment are described in [`rmcloud/README.md`](rmcloud/README.md)
and [`docs/en/CLOUD.md`](docs/en/CLOUD.md).

---

## Documentation

Chinese originals live at `docs/NAME.md`, English translations at `docs/en/NAME.md`.

| Document | Topic |
|---|---|
| [FORMATS](docs/en/FORMATS.md) | E-book format support matrix and trade-offs |
| [PANEL-HARDWARE](docs/en/PANEL-HARDWARE.md) | Full screen model: EPD plus software TCON |
| [PANEL-DRIVER](docs/en/PANEL-DRIVER.md) | How the Rust panel driver is organised: raw / wbf / encode |
| [SWTCON](docs/en/SWTCON.md) | No EPDC, only user-space swtcon: how a frame actually gets out |
| [SWTCON-POLICY](docs/en/SWTCON-POLICY.md) | xochitl refresh policy and the waveform import path |
| [WBF](docs/en/WBF.md) | Field-by-field analysis of the E Ink `.wbf` format |
| [ZSPLASH](docs/en/ZSPLASH.md) | Running the stock waveform engine (zplash) in user space |
| [WHITE](docs/en/WHITE.md) | White background, partial refresh, grey linearity, temperature bins |
| [RM2FB](docs/en/RM2FB.md) | Root cause of "screen frozen but system alive" |
| [BOOT](docs/en/BOOT.md) | Boot chain, systemd units, panel INIT pitfalls |
| [POWER](docs/en/POWER.md) | Power key, sleep, wake, lock screen |
| [MEASURED](docs/en/MEASURED.md) | Correcting the pen-width formulas against real `.rm` files |
| [PERF](docs/en/PERF.md) | Smoothness: what was measured, what changed, where it stands |
| [CLOUD](docs/en/CLOUD.md) | Self-hosted cloud: protocol facts, interface contract, conflict policy |

---

## Privacy and legal

This repository is a sanitized export of a private workspace:

- **No private git history.** The original repository's commit history contained internal
  addresses and key references; this tree starts from a single, clean commit.
- Private keys, device serial numbers, MAC addresses, student IDs, personal e-mail
  addresses, Tailscale addresses and campus-network details were removed or replaced
  with placeholders.
- No device-extracted proprietary binaries, fonts or QML resources.
- Tables derived from device waveform data (`reverse/c/wbf_lut.h`, `reverse/c/zm334.h`)
  are kept because the code needs them, but they are **not covered by the project
  licence**; regenerate them with `reverse/python/wbf.py` from your own device.

See [NOTICE.md](NOTICE.md) for trademarks, third-party projects and the warranty disclaimer.

---

## Licence

**Not chosen yet.** Before adding a `LICENSE`, decide whether `reverse/` (reverse-engineering
tools and notes) uses the same licence as `rmdev/` and `rmcloud/`, and keep the
device-derived tables excluded as described in NOTICE. Until then, all rights reserved.
