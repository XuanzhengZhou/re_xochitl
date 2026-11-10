# How rM2 Actually Writes the Screen: No EPDC, Only a Userspace swtcon

> English translation of [`../SWTCON.md`](../SWTCON.md) (Chinese original).

> Real-hardware forensics. Goal: pin down how "epdc screen writing" actually works, and why it
> looks like only the official xochitl can write.
>
> Conclusion up front: **this device has no EPDC, and no kernel display-driver interface is
> usable. Pushing a frame is a purely userspace piece of code inside the official xochitl
> (`EPFramebufferSwtcon`, a software TCON)**
> **that packs the pixels and memcpy's them into `/dev/fb0`, then flips pages with `FBIOPAN_DISPLAY`.**
> rm2fb does not rewrite that code; it **hijacks the official process** so that it calls it. So
> "only the official app can write" is true — but the cost is not "there is no way out", it is
> "either reuse that process or copy swtcon out".

---

## 1. Hardware Facts (All Verified on Device)

| Item | Measured value | How it was verified |
|---|---|---|
| Device / SoC | reMarkable 2.0 / i.MX7D | `/proc/device-tree/model`, `/sys/devices/soc0/machine` |
| Kernel | `5.4.70-v1.2.6-rm11x` | `uname -a` |
| **EPDC** | **`status = disabled`** | `/proc/device-tree/soc/aips-bus@30400000/epdc@306f0000/status` |
| `/proc/fb` | **only `0 mxs-lcdif`**, no epdc | `cat /proc/fb` |
| Display driver | `mxsfb` (LCDIF), `30730000.lcdif` | `.../driver -> .../drivers/mxsfb`, dmesg `mxsfb: initialized` |
| DT flags | `prevent-frying-pan`, `lcd-supply`, `lcd2-supply` | lcdif node properties |
| `display#1` timings | `hactive=0x14e(334)`, `vactive=0x57d(1405)`, `24bpp / bus-width 24`, 40 MHz | timing0 properties |
| `/dev/fb0` | `260 x 1408 @32bpp`, `stride 1040`, `virtual_size 260,23936` | `/sys/class/graphics/fb0/*`, `dfbinfo` |
| Physical address | `0xa9d00000`, 32 MB | `dfbinfo`, `/proc/<pid>/maps` |

The key arithmetic (everything lines up):

```
page      = 1040 B/row × 1408 rows      = 1,464,320 B
17 pages  = 1,464,320 × 17              = 24,893,440 B  =  sysfs virtual_size ✔
zero page = (23936-1408) × 1040         = 23,429,120   (driver fallback target)
```

In other words, `/dev/fb0` is **17 pages of 260×1408 RGB32**, not one 1404×1872 image. The
panel's 1404×1872 image is **transposed and squeezed to 4bpp** before it fits into a single page.

---

## 2. The Official Screen Path

```
Qt Quick scene graph
   ↓  EPRenderer (software compositor, QPainter draws layer by layer)
QImage 1404×1872 RGB16          ← rm2fb hijacks this line into /dev/shm/swtfb.01
   ↓  EPFramebuffer::sendUpdate(QRect, WaveformMode, UpdateFlags)   [0x552f74]
EPFramebufferSwtcon
   ↓  FUN_005560b0  [0x5560b0]   ← the only function referencing /dev/fb0
transpose + 16bpp→4bit packing
   ↓  memcpy into one page of /dev/fb0 (17-page rotation)
framegen / vsync-flip threads (SCHED_FIFO 98 / 99)
   ↓  ioctl(FBIOPAN_DISPLAY, yoffset = page number × 1408)
LCDIF streams that page out at 85 Hz → panel TCON → e-ink
```

`EPFramebuffer` is a **pure virtual interface**, `Q_PROPERTY Backend`:

```cpp
enum Backend { Tcon = 0, Swtcon = 1, Desktop = 2 };
```

- **Tcon** = the hardware EPDC path used on rM1;
- **Swtcon** = the path rM2 takes, **the waveform is computed in userspace**;
- Desktop = the desktop simulator.

### 2.1 Where the Waveform Data Lives

It is not firmware "hidden inside a chip":

```
xochitl[4839]: Reading waveforms from /usr/share/remarkable/320_R355_AF0A11_ED103TC2U2_VB3300-KCD_TC.wbf
xochitl[4839]: Unable to load waveform from file: /usr/share/remarkable/speedy-gonzales.csv
```

- `/usr/share/remarkable/` holds **42 `.wbf` files** (E Ink waveform libraries, about 280 KB each, named after the panel part number);
- The binary still carries an old `speedy-gonzales.csv` path; **this firmware does not have that file**, so the fallback branch is taken;
- What actually loads on this device is **`320_R355_AF0A11_ED103TC2U2_VB3300-KCD_TC.wbf`**.

That log line comes from **the xochitl process that really is the server**, so it is this machine's
panel waveform. The DT property `parade,panel_id_support` shows the part number is read back from
the Parade TCON.

### 2.2 Pixel Format (Inline Loop of 0x5560b0, Decompiled)

Source: 1404×1872, RGB565, stride 2808 B (= 1404×2).
Destination: first allocate a `malloc(0x281ac0)` block = **2,628,288 B = 1404×1872 bytes** (1 byte/pixel).

```c
// Each round takes 8 pixels from 8 rows in the same column and writes them transposed into 8 consecutive bytes
nib = (px_low_byte >> 1) & 0xF;      // equivalent to gray8 >> 4 (for grayscale, blue = g>>3)
out = (nib << 4) | nib;              // nibble duplication: high and low nibble are identical
```

That is:

- **16 gray levels**, `level = (rgb565 low byte >> 1) & 0xF`;
- Each byte holds the effective value of exactly one pixel, **both nibbles filled with the same value** (so it reads correctly whichever half the hardware uses);
- **Transposed**: output rows of 1872 B, 1404 rows in total, columns↔rows swapped;
- framegen then squeezes this 8bpp transposed buffer into the fb page as **packed 4bpp**:
  `1872 px × 4 bit = 936 B ≤ 1040 B`, `1404 rows ≤ 1408 rows`, and the remainder is padding.

> One point not yet nailed down: 1040−936 = 104 B/row and 1408−1404 = 4 rows — whether that padding
> sits at the end of each row or is interleaved needs a **captured live panel page** to
> reverse-engineer (see §6).

### 2.3 `prevent_frying_pan`: only one frame is displayed at a time

`drivers/video/fbdev/mxsfb.c` (reMarkable's fork):

```c
static int mxsfb_pan_display(struct fb_var_screeninfo *var, struct fb_info *fb_info)
{
    ...
    writel(fb_info->fix.smem_start + offset, host->base + host->devdata->next_buf);
    writel(CTRL1_CUR_FRAME_DONE_IRQ_EN, ...);
    ret = wait_for_completion_timeout(&host->flip_complete, HZ / 2);   // wait one frame
    ...
    if (host->prevent_frying_pan) {
        /* Next frame will be the zero frame (last frame in buffer) by default. */
        offset = fb_info->fix.line_length * (var->yres_virtual - var->yres);
        writel(fb_info->fix.smem_start + offset, host->base + host->devdata->next_buf);
    }
    return ret;      // ← note: returns "remaining jiffies", not 0
}
```

Three consequences that must be known:

1. **One pan = the panel receives exactly one frame, then LCDIF is pointed straight back at the last page** (to stop DC bias from "frying" the panel);
   **so every frame needs a fresh pan** — do not expect "write the memory once and it stays".
2. **`FBIOPAN_DISPLAY` returns 49 / 48 (remaining jiffies) on success, not 0.**
   Any code that tests failure with `if (ioctl(...) != 0)` will misjudge it. It must be `< 0`.
3. Each pan **blocks for roughly one frame** (85 Hz ≈ 11.8 ms). Serial screen refresh pays exactly that cost.

---

## 3. The Truth Behind "Only the Official App Can Write"

`librm2fb_server.so` is not a driver, it is a **parasite**. It does exactly three things:

1. **Overrides `__libc_start_main`** — the moment the process starts, `main` is swapped for `server_main`,
   `argv[0]` is rewritten to `"rm2fb-server"`, and `prctl(PR_SET_NAME)` is changed as well.
   (So `ps` shows `rm2fb-server` while `/proc/<pid>/exe` still points at `/usr/bin/xochitl`.)
2. **Hooks `QImage::QImage(int,int,Format)`** — once the official "framebuffer mirror" QImage is
   allocated at 1404×1872, its pixel pointer is replaced with `/dev/shm/swtfb.01`.
   The `REPLACING THE IMAGE with shared memory` log line comes from here.
3. **Calls the official methods through Qt meta-object reflection**:

```cpp
// src/shared/swtfb.cpp (rm2fb upstream)
void initQT() {
    auto ptr = f_getInstance();            // ← address is read from the config file
    instance = reinterpret_cast<QObject *>(ptr);
    img = (QImage *)(ptr + 8);             // offset +8 is its QImage
}
void SendUpdate(const QRect &rect, int waveform, int flags) const {
    QGenericArgument argWaveform("EPFramebuffer::WaveformMode", &waveform);
    QGenericArgument argUpdateMode("EPFramebuffer::UpdateFlags", &flags);
    QMetaObject::invokeMethod(instance, "sendUpdate", Qt::DirectConnection,
                              Q_ARG(QRect, rect), argWaveform, argUpdateMode);
}
```

The `EPFramebuffer::sendUpdate` signature (dug out of the moc metadata, 14 methods in total):

```
moc @ 0x7ae84c / static_metacall @ 0x547c88
 1  clearScreen()
 2  sendUpdate(QRect, WaveformMode, UpdateFlags)
 4  waitForLastUpdate()
 9  clearGhosting()                 // goes through sendUpdate(fullRect, 3) internally
10  addFastUpdateRect(QRect)
13  fastUpdateRegion()
```

**This explains every symptom**:

- Why preloading `librm2fb_server.so` into your own program SIGSEGVs —
  it needs `getInstance` to get the singleton living inside xochitl, and a non-Qt, non-xochitl
  address space has nothing;
- Why "only the official libepaper can write the screen" — `EPFramebufferSwtcon`, the code that
  actually touches the panel, and that whole framegen pipeline **exist only inside the xochitl
  binary**, and the waveform also requires its own copy of the `.wbf`;
- The observed "screen frozen but clients all fine" is exactly `invokeMethod` hitting a process in
  the wrong state (or one that never gets as far as submitting to swtcon).

---

## 4. Three Wrong Assumptions in the Existing Code

The constants at the top of `rmdev/src/fb.rs` are **all dead code** on this machine:

| Assumption in the code | Reality |
|---|---|
| `MXCFB_SEND_UPDATE = 0x4048462e` is used to push frames | **There is no EPDC driver on this device, nobody handles it.** Only the rm2fb client shim intercepts it and turns it into a 24 B IPC message |
| `WAVEFORM_MODE_DU/GC16/GL16`, `temp = 0x18`, `dither_mode` | Same as above, all discarded by the shim; swtcon uses its own `WaveformMode` enum + `.wbf` |
| `/dev/fb0` is `1404×1872 @16bpp` | Bare `/dev/fb0` is `260×1408 @32bpp`. The 1404×1872 view appears **only under the rm2fb client shim** — the shim fakes it |

The line in `fb.rs` saying "bare `/dev/fb0` is a 260×1408@32bpp line buffer, draw into it and nothing
shows" is **correct**, but the conclusion drawn from it is wrong: it is not "you must go through
rm2fb", it is "you must write into the 17 pages in swtcon's format".

Also, the three tuning knobs `RM_TEMP` / `RM_DITHER` / `RM_WAIT` **never took effect**; every
performance and grayscale conclusion previously based on them should be discarded and re-run.

---

## 5. Three Paths, Ordered by Cost

### A. Keep the Official xochitl as an "Invisible Display Daemon" (Status Quo, 0 Risk)

This is what rm2fb does today, and it already works in the project. Its value is underrated:
that process **does not display its own UI** (`main` was swapped out); it is just a 25 MB display
service.

Cost: dependence on the official binary + re-verification after every OTA + the `HOME` isolation
pitfall (see [`RM2FB.md`](./RM2FB.md) §5.4).

**It is an acceptable product shape.** The only hard problems are "official updates will touch it"
and a fragile startup order.

### B. Implement swtcon (the Real Decoupling, Estimated 2–4 Person-Weeks)

What has to be copied is already clear:

| What to copy | Location | Status |
|---|---|---|
| Transpose + 4bit packing | inline loop in `0x5560b0` | decompiled; formula in §2.2 |
| Waveform table loading | `FUN_0055aee4` (an `fread` loop) | decompiled; the data is that `.wbf` |
| Submit/drive sequence generation | `FUN_00555084` (3960 B, **no ioctl or file operation anywhere in the function body**) | decompiled to 621 lines of pseudo-C; **understanding it is the real barrier** |
| fb open + pan | hidden around `FUN_00554d18` / `FUN_00554f58` | not pinned down one by one |
| Waveform mapping | `0x552f74`: `0→2, 1→0, 2→1, 3→2, 6→3, 8→1, 10→(flag?8:0)`; 4/5/7/9 report invalid | recovered |

Good news: `.wbf` is the standard E Ink waveform container (on this device
`320_R355_AF0A11_ED103TC2U2_VB3300-KCD_TC.wbf`), so no waveform synthesis is needed. Bad news: the
framegen path runs at real-time priority 98, and a timing slip causes an LCDIF underflow (corrupted
screen).

### C. Compromise: Take Over Only "Write Page + Flip Page", Still Borrow the Waveform Once (Recommended First)

If what `FUN_00555084` produces is actually **a reusable per-frame drive sequence**, then: let the
official process run once to obtain the sequence, and afterwards do the memcpy + pan yourself.
Whether this path works depends on where `FUN_00555084`'s output goes and **needs one more round of
decompilation to confirm**; it is the step with the smallest investment and the largest payoff.

---

## 7. Reproduction Commands

```sh
# 1. Confirm there is no EPDC
ssh remarkable 'cat /proc/fb; \
  cat /proc/device-tree/soc/aips-bus@30400000/epdc@306f0000/status; echo'

# 2. Real fb geometry
ssh remarkable 'for f in name stride bits_per_pixel virtual_size modes pan; do \
  printf "%-16s %s\n" "$f" "$(cat /sys/class/graphics/fb0/$f)"; done'

# 3. Who actually holds /dev/fb0
ssh remarkable 'for p in $(pidof xochitl); do echo "== $p"; \
  tr "\0" " " < /proc/$p/cmdline; echo; ls -l /proc/$p/fd | grep -E "fb0|shm"; done'

# 4. Watch it flip pages (not write pixels)
ssh remarkable 'sh -c "strace -f -p $(pidof xochitl | cut -d\  -f1) \
  -e trace=ioctl -s 120 -o /tmp/t.tr & sleep 4; kill %1; head -n 20 /tmp/t.tr"'

# 5. Panel waveform file
ssh remarkable 'journalctl -a | grep -iE "wbf|waveform" | tail -n 5'
```

---

## 8. One Paragraph for Whoever Writes the Code

> Stop looking for `MXCFB_SEND_UPDATE`; this machine does not have it.
> Pushing a frame = take the 1404×1872 grayscale image, **transpose and squeeze it to 4bpp**,
> memcpy it into one of the 17 pages of `/dev/fb0`, then call
> `FBIOPAN_DISPLAY(yoffset = page number × 1408)`; one frame later the driver automatically points
> LCDIF back at the zero page, so **every frame must be flipped**, and it returns 49 on success, not 0.
> The genuinely hard part (waveform → drive sequence) is inside `EPFramebufferSwtcon` in the official
> xochitl, and the waveform data is
> `/usr/share/remarkable/320_R355_AF0A11_ED103TC2U2_VB3300-KCD_TC.wbf`.
