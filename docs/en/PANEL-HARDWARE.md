# Complete Model of the rM2 Display (EPD + Software TCON)

> English translation of [`../PANEL-HARDWARE.md`](../PANEL-HARDWARE.md) (Chinese original).

> This document supersedes the frame-geometry and command-bit sections of `ZSPLASH.md`.
> Evidence levels: **[measured]** verified on the device hardware · **[official]** official binary/source · **[inferred]** not directly verified

---

## 0. In One Sentence

The rM2 **has no EPDC**. The i.MX7D's **LCDIF drives the panel as a passive-matrix dot-matrix display**:
on every pixel clock the SoC puts one `{8-bit command, 16-bit drive code}` word on the 24-bit parallel bus.
The panel side is nothing but shift registers plus source and gate drivers — **all waveform timing is
clocked in frame by frame from user space**. So "refresh time = number of phases × frame period", with
no further magic.

---

## 1. Hardware Path

```
xochitl ──draws 8-bit grayscale──> /dev/shm/swtfb.01 (1872×1404×2 B)
                              │
                        SWTCON encoder (inside xochitl, plain user space)
                              │ writes 32bpp cell
                              ▼
                        /dev/fb0 (mxs-lcdif)
                              │ LCDIF 24-bit RGB dotclk mode, 40 MHz
                              ▼
                    ED103TC2 panel (1872×1404, TTL parallel, 40-pin FPC)
```

- The kernel `mxsfb` carries an rM2-specific patch: `Driver set to prevent frying pan mode`.
- `/proc/fb` lists only `0 mxs-lcdif`; `epdc@306f0000` is disabled in the DT.
- The touch device `pt_mt` reports `min X 0..1403 / min Y 0..1871` → the panel is **1404 × 1872**.

### 1.1 The Bus Has Only 22 Data Lines **[official]**

The `fsl,pins` list of `lcdifgrp` has only 27 pads = **22 data + CLK + DE(ENABLE) + HSYNC + VSYNC + RESET**.
It is not one data line that is missing but two, muxed to GPIO:

| Pin | Muxed as | Purpose |
|---|---|---|
| `LCD_DATA16` (mux 0x108) | ALT5 GPIO3_IO21 | `regulator-gdoe` EN (Gate Driver Output Enable) |
| `LCD_DATA22` (mux 0x120) | ALT5 GPIO3_IO27 | `regulator-sdoe` EN (Source Driver Output Enable) |

**This exactly explains the fixed bits of the command byte**: the only commands ever observed are
`0x41/0x43/0x45/0x47/0x51/0x53/0x61`, all of which satisfy **bit0=1, bit6=1**
— and bit0/bit6 happen to land on DATA16/DATA22, held high as constants on the panel side.
Only **bit5..bit1** (6 lines) are left as effective command bits. **[inferred, high confidence]**

---

## 2. Frame Geometry (the easiest thing to get wrong)

```
one pan-buffer row        = one panel column (1404 columns)
a cell within a row       = 8 panel pixel rows (8 consecutive panel rows)
a row of 260 cells        = 26 command cells + 234 content cells
234 cells × 8 px          = 1872 px  ← exactly the panel height
a pan buffer of 1408 rows = 4 preamble rows + 1404 content rows
```

Panel coordinates ↔ user coordinates (both axes flipped):

```
panel_col = 1403 - user_x
panel_row = 1871 - user_y
```

One cell (4 B, little-endian):

| Bits | Meaning |
|---|---|
| `[15:0]` | 2-bit drive code for 8 panel rows, lane K → bit `(7-K)*2` |
| `[23:16]` | **8-bit command word** |
| `[31:24]` | not transmitted (the 24-bit bus carries only the low 3 bytes) |

Drive codes: **`0` = no drive, `1` = push black, `2` = push white**.

### 2.0 ⚠️ Cells Must Be Written with 32-bit Read-Modify-Write (the biggest pitfall in this project)

The mmap of `/dev/fb0` is a **write-combining** mapping. Writing **2 bytes** at a 4-byte stride
(a `memcpy` of half a cell, a `uint16_t` store, `strh`) is **silently swallowed** by the
write-combining buffer:

```c
/* ✗ completely ineffective: reads back looking correct, yet the panel does not react at all */
memcpy(buf + (4+pc)*1040 + 26*4, codes, 468);

/* ✓ the only reliable form */
uint32_t *l = (uint32_t*)(buf + (4+pc)*1040);
for (int s = 0; s < 234; s++)
    l[26+s] = (l[26+s] & 0xFFFF0000u) | codes[s];
```

This pitfall once made every attempt at per-pixel rendering fail silently (read-back compared
identical, yet not one pixel on screen moved), while "fill the whole row with one code" kept
working because it is a 32-bit write by nature — so the symptom looked like "it can only flush
solid colours; as soon as you draw, nothing happens".

**Rule: never write cells into the fb with `memcpy` or half-word stores; either do a 32-bit
read-modify-write, or copy whole 32-bit words (`volatile uint32_t`).**

**[measured]** After the fix, the official INIT (111 frames flushing white) + pushing black for the
502,120 dark pixels of an image (40 frames) = a white background + black header bar + 22 lines of
black body text on screen, with geometry and coordinates exactly right.

### 2.1 Two Layouts

| Layout | xres | Row stride | yres | Content cells | Used by |
|---|---|---|---|---|---|
| 260 cell | 260 | 1040 B | 1408 | **26..259** | official kernel / xochitl |
| 334 cell | 334 | 1336 B | 1405 | 73..306 | U-Boot zplash |

Both have a content width of **234 cells = 1872 px**; the two buffers **cannot be mixed**.

---

## 3. Command-Word Template (official, byte for byte) **[official + matches measurement]**

The official code does not look anything up; it builds the word bit by bit with OR/BIC masks
(xochitl `0x5644e4` / `0x564640`):

```
fill_first_line (pan buffer row 0):
   whole row = 0x0043_0000
   cells 20..142  |= 0x0004_0000     -> 0x47
   cells 40..102  &= ~0x0002_0000    -> 0x45

fill_line (all other rows):
   whole row = 0x0041_0000
   cells 8..18    |= 0x0020_0000     -> 0x61
   cells 55..254  |= 0x0002_0000     -> 0x43
   cells 26..259  |= 0x0010_0000     -> 0x53 / 0x51   (only on content rows)
```

Expanded (260 words per row):

| Row | Command layout |
|---|---|
| 0 | `0..19:43  20..39:47  40..102:45  103..142:47  143..259:43` |
| 1,2 | `0..7:41  8..18:61  19..54:41  55..254:43  255..259:41` |
| ≥3 | `0..7:41  8..18:61  19..25:41  26..54:51  55..254:53  255..259:51` |

**`0..25` are the 26 command-only words, `26..259` are the 234 content words.**

Key fact: **xochitl writes the command bits only at initialisation and never rewrites them afterwards**
(the generator has a single caller, `buildShadow 0x564b44`, with just 4 call sites: the three power-on
frames with data=`0000/5555/AAAA`, plus opening `/dev/fb0` to build the shadow framebuffer).
⇒ **The command bytes are not what makes a partial refresh fast or slow.**

---

## 4. Pixel Encoding

The phase table is indexed two-dimensionally by `(prev, tgt)` over 16 grey levels:

```c
idx   = (prev << 4) | tgt;        /* prev/tgt are both 0..15, 15 = white */
code  = T[idx];                   /* 0 / 1 / 2 */
cell  = OR over lane K of (code << ((7-K)*2));
```

- Each phase writes only the **low 16 bits** of a cell; the command bits in the high 16 bits are left
  untouched (volatile u16 write).
- The change-tracking buffer is initialised to `0xFF` (= grey level 15 = white), because the panel is
  white after power-on.
- Partial update = the `partial` table (zeroing the `src==tgt` diagonal of `idx` → no change, no drive).

---

## 5. Timing

| Layout | htotal | vtotal | pixclock | Frame period |
|---|---|---|---|---|
| 260 | 263 | 1553 | 28800 ps (34.72 MHz) | **11.76 ms** |
| 334 | 337 | 1408 | 25000 ps (40 MHz) | **11.86 ms** |

- `FBIOPAN_DISPLAY` **really does block for one frame period** **[measured 10.8–11.6 ms per call]**.
- The `yoffset` of `fbiopan` is simply slot index × yres; `CUR_BUF` follows it.
- The `prevent_frying_pan` patch makes the ioctl return the remaining jiffies immediately (never 0),
  but it **still waits for VSYNC**.
- ⇒ One phase = one pan = one panel frame. **There is no way to "shorten a frame"**: the LCDIF has
  MPU/8080 smart mode, BUSY handshaking and AXI throttling, but **no pause or clock-stop setting**;
  shrinking `yres` only tiles the content (the panel's gate scan runs on its own), and raising the
  pixclock underdrives the panel.

---

## 6. Where the Waveforms Come From

- 42 files in `/usr/share/remarkable/*.wbf`, with the name pattern
  `320_R<rev>_AF<FPL_LOT>_<TFT_VID>[_VB3300-KCD]_TC.wbf`.
- The panel identity (`fpl_lot` / `tft_vid` / `vcom_mv` / `vcom_offset`) is decoded from the **barcode**;
  if matching fails it degrades to matching FPL_LOT alone or TFT_VID alone; if everything fails →
  `fatal: unable to find any waveform files!`
- Temperature source: the `temp0` of `/sys/class/hwmon/*/` whose `name == sy7636a_temperature` (integer °C).
- 14 temperature bands; 22 °C falls in band 7 (21..23) → 46 phases.

### 6.1 Mode Table (official load order, 9 slots)

| Slot | Mode | wbf mode | Notes |
|---|---|---|---|
| 0 | DU full | 1 | 1 bit |
| 1 | GC16 full | 2 | 16 levels, 46 phases @22 °C |
| 2 | GC16 FAST | 3 | has ghosting |
| 3 | GLF | 6 | |
| 4 | DU4 | 7 | 4 levels |
| 5 | DU partial | 1 + skip_init | skips the leading all-zero phases |
| 6 | GC16 partial | 2 + skip_init + diagonal cleared | |
| 7 | DU4 partial | 7 + skip_init | |
| 8 | A2 | 4 | 1-bit animation waveform, the fastest |

`skip_init` = skip the phases at the start of the waveform whose target code is 0 → a partial refresh
needs fewer actual frames.

---

## 7. The Truth About the 21 ms Partial Refresh **[official]**

It is not a gate-line mask and not a raised clock; it merely **swaps in a shorter waveform table**:

```
/usr/share/remarkable/speedy-gonzales.csv
4 comma-separated fields per line:  mode , current , target , phase string
   mode / current / target : decimal 0..15
   phase string : a string of only '0' '1' '2'; one character = one frame = one phase
```

**Want 2 frames? Write 2 characters** → 2 × 11.76 ms ≈ 23 ms ≈ the observed 21 ms.

Read chain: `EPSwtcon` constructor `0x55324c` → `loadCsvTable 0x5563b0` → `parseCsvFile 0x55f7dc`.

**This unit does not have that file**, so it takes the `.wbf` fallback (A2 at 10 phases minimum,
DU at 12), which naturally cannot reach 21 ms. Drop in a valid CSV and you get a custom waveform of
any length.

---

## 8. The Real Cause of the "Frozen Screen" on This Unit **[measured]**

Comparison from `/home/root/log.txt`:

```
success (xochitl 2.6.1, Oct 11 / Nov 04 14:41):
    we're running on an epaper device
    [2.2 s loading the waveforms and building the tables]
    SWTCON initialized \o/
    EPD platform plugin loaded!

failure (xochitl "nedshimmelfinney" 30c1de8478, the same every time from Nov 04 16:22):
    we're running on an epaper device
    +7 ms  Unable to write to file: "/usr/share/remarkable/speedy-gonzales.csv"
    +8 ms  Unable to export waveform table.
    +15 ms EPD platform plugin loaded!        ← SWTCON skipped entirely
```

- **xochitl was upgraded around Nov 4, 16:22** (the `src/main.cpp` line number in the log changes
  from 285 to 167).
- At startup the new version **exports** the waveform table to `speedy-gonzales.csv`; if that write
  fails it simply abandons SWTCON initialisation.
- Consequence: no process holds `/dev/fb0` open, the panel stays on its last frame and does not move —
  that is the "freeze".
- It is unrelated to disk space (the root partition was 94% full with 14.2 MB left, yet bind-mounting
  that directory onto a 5.9 GB `/opt` still failed, and the error returns within 6 ms).

---

## 9. Driving It Ourselves (all measured and passing)

`wr2.c` + `panel/wbf_bands.bin` is the complete reference implementation. Current capabilities:

| Capability | Measured |
|---|---|
| Full-screen GC16, 16-level grey | 46 phases, 435 ms encoding + 539 ms playback |
| Full-screen A2, monochrome (fast) | 12 phases, 113 ms encoding + 133 ms playback |
| Partial refresh (DU, dirty columns only) | 310/1404 dirty columns → 6.4 ms encoding + N×11 ms |
| First power-on INIT | 111 frames / 1.32 s (only needed for the first update after boot) |

### 9.0 **Decisive Conclusion: Every Waveform Phase Must Occupy Exactly One Frame Period**

This is the root cause behind the whole problem.

Previously `full` (GC16, 46 phases) always produced a white screen plus ghosting, while `page`
(the same set of code1 repeated for 40 frames) worked fine. The wrong conclusion at the time was
"cell data that changes frame by frame does not drive the panel". The real reason is:

> `play()` writes each phase into the fb with a read-modify-write:
> `l[26+s] = (l[26+s] & 0xFFFF0000u) | code;`
> on write-combining memory every cell becomes one read plus one write → **30 ms to fill one slot**.
> 30 ms = 2.5 frame periods, so **each phase was displayed for 3–6 frames**, the waveform was
> stretched as a whole, and GC16's charge balance and settling were entirely lost.

After switching to "build the whole slot template in RAM + copy nothing but 32-bit words", the write
dropped to **1.94 ms per slot**, each phase lasted exactly one frame → GC16 immediately produced the
correct image, and 16-level grey worked too.

**[measured]** Same code, same waveform table, only the write method changed:

| Actual duration per phase | GC16 result |
|---|---|
| 34 ms (27 encoding + 6 writing) | all white + ghosting |
| 11.7 ms (pre-encoded + 1.94 ms write) | ✅ white background / black title bar / 22 lines of black text / 16-level grey ramp at the bottom all correct |

Corollaries (a driver must obey these):
- the duration of every phase must be an integer multiple of `1/frame`, **preferably 1**;
- therefore encoding must be faster than one frame (9.4 ms < 11.76 ms now, which meets the target);
- therefore you cannot "encode while playing back": either pre-encode into RAM, or guarantee
  encoding < one frame.

### 9.1 Wrong Hypotheses Ruled Out (each one measured)

| Hypothesis | How it was tested | Conclusion |
|---|---|---|
| WC drops data | 3 patterns filling a whole slot, read back via `/dev/mem` | **0 mismatched cells**, WC is fine |
| `pan()` does not change the address | read LCDIF `CUR_BUF`(0x40) directly | it changes every time, stepping exactly one slot `0x165800` |
| `pan()` is not vsync-synchronised → tearing | a thread polls `CUR_BUF` at high rate and records the change instant | pan first waits 8.8 ms for vsync, **then** writes the address and returns immediately → the switch happens on a frame boundary, no tearing |
| frame-varying content does not drive | four quadrants pushed black one after another, 24 frames each | **all four quadrants black in order** → frame changes work perfectly |
| wbf LUT index direction is reversed | 4 horizontal bands, each with a different index convention / phase direction | all four came out white → not an indexing problem |
| the checkerboard pattern itself is broken | same checkerboard driven with pure code1 for 24 frames | ✅ crisp checkerboard → the data path is fine |

### 9.2 The Real LCDIF Register Layout (correcting the earlier table)

`CTRL` was previously assumed to be at `0x10`. In fact **each register occupies 0x10 bytes and
`+4/+8/+12` are `SET/CLR/TOG` aliases** (reads all return the same value, which makes misreading easy):

| Offset | Register | Measured value |
|---|---|---|
| `0x000` | `CTRL` | `0x000a0f21` |
| `0x010` | `CTRL1` | `0x01070500` |
| `0x020` | `CTRL2` | `0x00a00000` |
| `0x030` | `TRANSFER_COUNT` | `0x05800104` (1408 rows × 260 cell) |
| `0x040` | `CUR_BUF` | changes with `pan()`, base `0xa9d00000` |
| `0x050` | `NEXT_BUF` | `0xab358000` (= base + 16 slots, constant) |
| `0x060` | `TIMING` | `0` |
| `0x070` | `VDCTRL0` | `0x11300001` |
| `0x080` | `VDCTRL1` | `0x00000611` = 1553 = vtotal |
| `0x090` | `VDCTRL2` | `0x00040107` → htotal 263, hsync 1 |
| `0x0a0` | `VDCTRL3` | `0x00020002` |
| `0x0b0` | `VDCTRL4` | `0x00040104` → 260 \| SYNC_SIGNALS_ON(bit18) |

For `fb0`, `fix.smem_start = 0xa9d00000`, `smem_len = 33,554,432`, `line_length = 1040`,
`yres_virtual = 23936` (17 slots).

### 9.3 Panel Drive-Strength Curve (measured with `ladder`)

Eight vertical bars driven with pure code1 for 1/2/3/4/6/8/12/20 frames respectively:

| Frames | 1 | 2 | 3 | 4 | 6 | 8 | 12 | 20 |
|---|---|---|---|---|---|---|---|---|
| Appearance | barely visible | very faint | visible | light grey | mid grey | dark grey | near black | full black |

The user confirmed: "white on the left to black on the right, 8 bars in total". So **frame count ≈ grey
level**, with an obvious dead zone at the low end and a saturation zone at the high end. This curve is
the only basis for designing a refresh policy, especially the frame count of a partial refresh.

### 9.4 Partial Refresh: Mechanism and Measurements

The phase structure of DU/A2/DU4 decoded from the wbf files (this is what "fast refresh" really is):

| Mode | Black←White | White←Black | Unchanged | Phases | Duration |
|---|---|---|---|---|---|
| DU | `1`×24 | `2`×24 | `0`×25 | 25 | 292 ms |
| A2 | `1`×11 | `2`×11 | `0`×12 | 12 | 141 ms |
| DU4 | `0`×4 `1`×24 | `0`×4 `2`×24 | `0`×29 | 29 | 340 ms |

In other words **DU/A2 do not "image" at all**; they only push the pixels whose polarity really changed
for N frames in one direction. The official 21 ms partial refresh = 2 frames; by the curve above,
2 frames can only give a very faint change — this is the inherent cost of "fast refresh", not an
implementation problem.

**[measured]** `pupd`: after establishing a baseline with a full-screen GC16, a 200×150 black block is
moved along x step by step, encoding only the dirty-column window at each step:

| Metric | Value |
|---|---|
| Dirty-column window | 310 / 1404 (22%) |
| Encoding | **6.4 ms** |
| Playing 24 frames (DU, full drive) | 268 ms |
| Playing 2 frames (the official fast-refresh order of magnitude) | **about 22 ms** |

The user confirmed: "only one clean black block, no trailing".

**Pitfall hit here**: `prv` (the previous frame's state) must be updated at **every step**. Updating it
only at the end of the whole run makes the step that returns the old position to white look up
`tgt==prev`, DU decides there is no change → zero drive → it cannot be erased, and a black band is left
on screen.

### 9.5 Temperature Bands (required for correctness)

Panel temperature comes from `/sys/class/thermal/thermal_zone0/type == epd-thermal`
(there is also `/sys/class/hwmon/hwmon1/name == sy7636a_temperature`, with the file `temp0`).
Measured 21 °C on this unit → band 7 (21..23).

Temperature bounds `[0 3 6 9 12 15 18 21 24 27 30 33 38 43 48]`, 14 bands in total.
Phase count per mode varies with temperature:

| Mode | Phases per band |
|---|---|
| INIT | 149 133 117 103 89 159 135 111 99 87 79 71 63 59 |
| DU | 73 65 57 50 43 37 31 25 22 19 17 15 13 12 |
| GC16 | 131 116 102 90 77 66 55 46 38 38 38 38 38 38 |
| A2 | 35 31 28 24 21 17 15 12 10 10 10 10 10 10 |
| DU4 | 84 75 65 58 49 42 35 29 24 24 24 24 24 24 |

`panel/wbf_bands.bin` (327,908 B) has been generated for all 14 bands × 4 modes (DU/GC16/A2/DU4), with
phases deduplicated to 1263 rows × 256 B. Format:

```
"MXWF" | u16 ver | u8 nband | u8 nmode | u16 npool
u16 bounds[nband+1] | u16 nph[nmode*nband]
u16 rows[sum(nph)]  | u8 pool[npool][256]
mode order 0=DU 1=GC16 2=A2 3=DU4; phase_row = pool[rows[base_sum+ph]]

Note that **the two regions have different lengths**: the `nph` region is always
`nmode*nband*2` bytes (112 here), while the `rows` region is `sum(nph)*2` bytes
(4428 here). Giving the `nph` region the length of the `rows` region in a loader
makes all phases point at the same row — the symptom is "every phase encodes to
exactly the same result".
```

Verified byte for byte: band 7 of the blob and the table previously compiled into `wbf_lut.h` agree in
**all of 46/25/12/29 phases**.

### 9.6 What Writing a Real Driver Takes (conclusion)

```
1. open /dev/fb0, FBIOPUT_VSCREENINFO set 260×1408, yres_virtual 17×1408,
   pixclock 0x7080, margins 1/1/1/143 (yielding htotal 263 / vtotal 1553 → 11.76 ms = 85 Hz)
2. build a 1.46 MB template slot in RAM (command bytes fixed), change only content cells at runtime
3. run the official INIT once before the first update after power-on (111 frames, net zero drive)
4. maintain prv (current on-screen state) and img (target); per update compute key = (tgt<<4)|prv
5. read the temperature, pick the band, fetch the waveform; pre-encode all phases into RAM (< one frame each)
6. phase by phase: copy the whole slot to the fb as 32-bit words → pan(0); exactly one pan per phase
7. partial refresh: encode only dirty columns + play only N frames; memcpy(prv, img) at every step
```

Strictly forbidden:
- `memcpy`, half-word or any other non-32-bit-whole-word access to `/dev/fb0` (it silently drops data);
- read-modify-write is legal, but it is 15× slower and stretches phases past one frame → the waveform
  fails;
- letting any phase occupy more than one frame period;
- **leaving the last frame in the fb after an update finishes** (see 9.7).

### 9.7 **Every Update Must End with One All-Zero (No-Drive) Frame**

This is the second fatal trap, and an extremely subtle one.

After `wr2` exits, the `LCDIF` is still in the `RUN` state and **keeps scanning `CUR_BUF` forever**.
That is, the last frame is **driven over and over indefinitely**, the screen is slowly pushed to wherever
that frame's codes point, and the effect of the preceding 44 phases is entirely overwritten.

**[measured]** The first run of `gscale` (16-step ramp, 46 phases) produced "white background + only the
2nd black bar from the left" — exactly the **code1 region of the last phase**. Adding two all-zero
frames immediately produced the correct 16-step ramp.

The right way: as the last step of every update sequence, write one frame in which the 16-bit code of
all content cells = 0 (no drive), then `pan` once more. Code 0 does not drive; e-ink is bistable, so
the image simply stays. Afterwards the LCDIF can scan for ten thousand years without moving the panel
again.

### 9.8 Verification Checklist (all confirmed by the user on real hardware)

| Test | Expected | Result |
|---|---|---|
| `ladder` 1/2/3/4/6/8/12/20 frames | 8 bars grading white on the left to black on the right | ✅ "white on the left to black on the right, 8 bars in total" |
| `quad` four quadrants pushed black in order | all four quadrants black | ✅ "all four quadrants black, in order" |
| `chk` checkerboard with pure code1 | black-and-white checkerboard | ✅ checkerboard visible |
| `gscale` 16-step ramp (GC16) | 16 continuous bars from black to white | ✅ "16 continuous vertical bars grading from black to white" |
| `g16` full page (GC16) | black title bar + 22 lines of black text + grey ramp at the bottom | ✅ correct |
| `pupd` partial refresh moving a black block | one clean black block, no trailing | ✅ no trailing |

---

## 10. Still Unsolved

1. **Encoding headroom**: 9.46 ms per phase full-screen against an 11.76 ms frame leaves only 2.3 ms.
   Getting under 5 ms requires NEON (the 256-entry gather over 8 lanes is the only bottleneck).
   For now this is sidestepped by "pre-encode all phases into RAM" (46 phases = 30 MB, a noticeable
   footprint for a full-screen GC16).
2. **Visual linearity of the greys**: all 16 levels are confirmed distinguishable, but adjacent levels
   were not tested for sticking (levels (5,6) are predicted to differ by only 2 frames, so they most
   likely do stick).
3. **Verifying 21 ms on hardware**: by the curve, a 2-frame partial refresh has very low contrast; the
   official `speedy-gonzales.csv` may use a different drive voltage, and without that file it cannot be
   reproduced. (Waveform-file selection is solved in §11.)
4. Where `OUTSTANDING_REQS` in `CTRL2=0x00a00000` comes from; why `NEXT_BUF` stops at 16 slots.
5. The CCM pre/post divider ratio for 40 MHz is unconfirmed.
6. The official SWTCON still does not initialise on this unit (the new xochitl skips it silently);
   reviving it requires downgrading to 2.6.1 or producing a valid `speedy-gonzales.csv`.
7. The number of source driver ICs on the panel (needs a teardown or the datasheet).
8. The fingerprint-matching rule for the other wbf panel variants (`320_R292/R299/R306/R325/...`,
   42 files in total): currently R259 is hard-coded; a real driver should select the file by
   fpl_lot/tft_vid.

---

## 11. Standalone Driver `epd.c` (own wbf parser, no xochitl dependency)

```
epd info                           list the 42 wbf files plus the decoded fingerprint, band and phase count
epd init                           official INIT sequence (111 frames) + blank frame, flushes the screen white
epd test                           draw the built-in test page
epd draw <file.pgm> [mode]         draw an arbitrary PGM (P5, 1404x1872, 8bit)
epd local <x> <y> <w> <h> <g> <n>  partial-refresh a rectangle for n frames
epd blank                          feed one all-zero frame
```
`mode` = `gc16` (default) / `du` / `a2` / `du4`.

### 11.1 Measured Results

| Case | Result |
|---|---|
| `epd draw card.pgm` (Chinese title + English body + 16-step ramp + continuous gradient + geometric shapes) | ✅ user confirmed "the card is correct" |
| two `epd local` calls right after (320×200 and 180×180 black blocks) | ✅ "the card is correct, plus two black blocks of the right size" |
| full-screen GC16 | 46 phases, 434 ms encoding + 535 ms playback |
| partial refresh 320×200, 301/1404 dirty columns | 24 frames, 296 ms (including encoding) |

### 11.2 The wbf File Format (the minimum a C implementation needs)

```
[0x00] u32 checksum        (CRC32 over file[4:file_length], seeded with CRC32("\0\0\0\0"))
[0x04] u32 file_length
[0x0E] u16 fpl_lot         ← panel lot, decides the R### in the file name
[0x10] u8  mode_version    ← version of the mode-name sequence (0x19 → INIT,DU,GC16,GL16×3,A2,DU4)
[0x11] u8  wf_version  [0x12] u8 wf_subversion   ← the AF<ver><sub> in the file name
[0x17] u8  frame_rate      (0x85)
[0x1C] u24 xwia            ← address of the string block
[0x20] u24 wmta            ← address of the mode pointer table
[0x24] u8  luts   [0x25] u8 mc   [0x26] u8 trc
[0x30] u8  bounds[trc+2]   ← temperature bounds, last entry is the upper bound   then 1 checksum byte
[xwia] u8 len + len-byte string + 1 checksum byte
       → of the form "320_R355_AF0A11_ED103TC2U2_VB3300-KCD_TC"
         take the field after the "AF…" field = TFT_VID
[wmta] one {u24 addr, u8 cs} per mode
       at each mode's addr place trc+1 more {u24 addr, u8 cs} (one per temperature band)
       each band's addr points at one RLE-compressed waveform table
```

**Table decoding (RLE)**: read byte `c`;
- `0xFC` → toggles the RLE switch (what follows is literal bytes);
- `0xFF` → end; the next byte is the running checksum;
- RLE on: read `len`, emit `len+1` copies of `c`;
- RLE off: emit `c` as is.
Expanded length ÷ 256 = number of phases (`luts=0x04` → a 32×32 LUT, 256 bytes per phase).

### 11.3 Panel Fingerprint: Where the Official Code Reads It, and What This Unit Has

**[measured]** strace of the official xochitl:

```
openat("/dev/mmcblk2boot1", O_RDONLY) = 5      ← the barcode lives in this partition
openat("/usr/share/remarkable/320_R402_...C5.....wbf")   ← try them one by one
openat("/usr/share/remarkable/320_R351_...U2.....wbf")
openat("/usr/share/remarkable/320_R355_...U2.....wbf")
openat("/usr/share/remarkable/320_R355_...U2.....wbf")   ← hit, opened again for a full parse
```

**[measured]** what xochitl itself prints (this line is the decisive evidence):

```
Reading waveforms from /usr/share/remarkable/320_R355_AF0A11_ED103TC2U2_VB3300-KCD_TC.wbf
```

So the panel fingerprint of this unit is **FPL_LOT = 0x163 (=355) / TFT_VID = ED103TC2U2**.

`/dev/mmcblk2boot1` is 2 MB and holds a single TLV record area with 5 records:

```
[0] "RM110-111-01171"
[1] "FUM211004939"
[2] "BYD210114001810"
[3] "EUFZBRBC1W1V009UBAT -1.33"
[4] "TESTS_DONE"
```

The record format is `<u32 BE length><data>` with no field id. **None of these 5 records contains the
literal 355**, which means the official code takes a fallback path on this unit, and the
`barcode → (fpl_lot, tft_vid)` mapping rule is **not yet solved**. `epd info` prints this TLV block
verbatim so it can be compared later.

`epd.c` therefore uses the precedence: environment variable `RM2EPD_WBF` > `/etc/rm2epd.conf`
(`fpl_lot=`/`tft_vid=`) > built-in verified defaults (0x163 / ED103TC2U2) > three-level fallback
(FPL_LOT only / TFT_VID only / any). On this unit `/etc/rm2epd.conf` already holds the correct values
and `epd info` reports "exact match on FPL_LOT+TFT_VID".

Incidentally: the header CRC32 of all 42 wbf files **is valid**, and the official code picked R355 not
because it is special but because it was the first file matching both FPL_LOT and TFT_VID.

### 11.4 Two More Pitfalls (both subtle, worth remembering)

**Pitfall A: 8-bit → 4-bit overflow**
```c
img[i] = (row[i] + 8) >> 4;    /* wrong! 255 -> 263>>4 = 16, outside 0..15 */
```
`kp` is `unsigned char`, so `(16<<4)|15 = 271` truncates to `15`, which is interpreted as
"target black / previously white" → **the whole white background gets pushed black**.
Correct form: `(v*15u + 127u) / 255u`. Also mask both sides with `&0xF` in `build_kp` as a backstop.

**Pitfall B: before a partial refresh, the target image must be initialised from the current on-screen state**
`img` is freshly malloc'd memory (all zeros). A partial refresh only changes one rectangle, and all other
pixels are supposed to keep `tgt == prv` (only then do DU/A2 decide "no change"). Miss out
`memcpy(img, prv, UW*UH)` and every other pixel is "target black" → **the full column height is pushed
black and a vertical black band appears**, and worse, this wrong state is written to the state file and
corrupts every update that follows. This also shows that `prv` (the on-screen state) is a first-class
citizen of the driver: it must be persisted and resynchronised after every update.
