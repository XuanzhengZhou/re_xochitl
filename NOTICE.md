# NOTICE

**English** | [中文](NOTICE.zh-CN.md)

## Trademarks and attribution

reMarkable, reMarkable 2 and xochitl are trademarks / product names of **reMarkable AS**.
This project is neither affiliated with nor endorsed by reMarkable AS.

## No proprietary assets

This repository does **not** contain any reMarkable proprietary content extracted from the
device, including but not limited to:

- the `xochitl` executable or any part of it;
- shared libraries such as `libQt5Core` / `libQt5Gui` / `libepaper` / `libqvnc` / `librm2fb_*`;
- device fonts (Maison Neue, icomoon, etc.);
- QML resources (`.qml`) carved out of the binaries, object-tree dumps, disassembly output;
- firmware images, u-boot, device trees, root filesystems.

The repository contains only **code written by the author** and
**analysis notes on formats and protocols**.

## Files derived from device data

The following files are generated from waveform data on the device. They do compile, but they
are **not licensed under this project's license**:

| File | Source | Recommendation |
|---|---|---|
| `reverse/c/wbf_lut.h` | LUT exported from the official `.wbf` waveform file (`320_R259_AFAB21_ED103TC2M1_TC`) | Regenerate it from your own device's waveform file with `reverse/python/wbf.py` |
| `reverse/c/zm334.h` | Control-bit templates + LUT for the zplash waveform engine | Same as above |

Before publishing or using this commercially, confirm on your own that these tables are usable
in your jurisdiction, or simply run the generator scripts and replace the generated results with
your own.

## Purpose of the reverse engineering

The reverse engineering in this project targets only **devices we own**, and its purpose is limited to:

1. Interoperability — so that software we write can drive the screen and the pen correctly on hardware we bought;
2. Data portability — to understand the `.rm` format so that our own handwritten notes are not lost when we switch software;
3. Learning — to figure out engineering problems such as e-ink waveforms and partial refresh.

It does not include cracking paid features, circumventing DRM, bypassing subscription checks, or
distributing official software in any form.

## Third parties

Some upstream projects were cloned at some point for reference (**not included in this repository**):

- [rmkit](https://github.com/ddvk/rmkit)
- [riddle](https://github.com/ddvk/riddle)
- [reMarkable2-framebuffer](https://github.com/toltec-dev/rm2fb)
- [epdiy](https://github.com/vroland/epdiy)
- [smart_remarkable](https://github.com/awwaiid/smart_remarkable)

Each of these projects has its own license; please get them from upstream if you want to use them.

## No warranty

This project is the output of reverse engineering. It involves high-risk operations such as
writing directly to `/dev/mem` and `/dev/fb0`, and injecting into the Qt process.
**It may brick the device, lose data, or damage the screen.** The author takes no responsibility
for any consequences.
