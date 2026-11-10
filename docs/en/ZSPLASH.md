# zplash (the Official Waveform Engine) Runs in User Space

> English translation of [`../ZSPLASH.md`](../ZSPLASH.md) (Chinese original).

> A pure in-memory experiment: **not a single byte written to `/dev/fb0`, not one pan.**

---

## 0. Conclusion

**The official 19 KB waveform engine (`zplash`) can be loaded and called directly from Linux user space.**
That was the precondition for route R (reuse the official waveform, get completely off xochitl), and it now holds.

The price is three small changes, all verified:

| Change | Reason | How |
|---|---|---|
| NOP out 2 **privileged instructions** | SIGILL in user space | `0x10: mcr p15,0,r0,c1,c0,2` (writes CPACR)<br>`0x18: vmsr fpexc` (PL1-only; Linux's VFP emulator does not accept it) |
| **Relocate 97 `movt` instructions** | it is not position-independent code | rewrite every `movt rX,#0x8030` / `#0x8031` to the new base |
| Map **192 KB** (not 19 KB) | its bss runs up to `base+0x1ffff` | `mmap` a 64 KB-aligned RWX region whose low 16 bits are 0 |

---

## 1. Load recipe (reusable code)

```python
raw = gzip.decompress(open("/var/lib/uboot/zplash","rb").read())   # 2,910 B -> 19,032 B
buf = bytearray(raw)
struct.pack_into("<I", buf, 0x10, 0xE1A00000)   # mcr p15  -> mov r0,r0
struct.pack_into("<I", buf, 0x18, 0xE1A00000)   # vmsr fpexc -> mov r0,r0

base = 0x60000000        # must be 64K-aligned; low 16 bits 0 is what lets us patch only movt
size = 0x30000
libc.mmap(base, size, PROT_READ|WRITE|EXEC, MAP_PRIVATE|MAP_ANON|MAP_FIXED_NOREPLACE, -1, 0)

# relocation: imm16 of movt = 0x8030 + (off >> 16)
for each movt (imm16 ∈ [0x8030, 0x803F]):
    new = imm16 - 0x8030 + (base >> 16)

memmove(base, buf, len(buf))
```

Three pitfalls, hit one at a time:

1. `0x80300000` **is outside user space** (this machine uses a 2G/2G split), so `mmap` returns `ENOMEM` outright.
2. Patching only `movt #0x8030` is not enough — there are another **61 `#0x8031`** (bss sits at `base+0x14a58`),
   and missing them gives a `SIGSEGV` from writing off into nowhere. The rule therefore has to be computed from the offset.
3. `vmsr fpexc` is not flagged as privileged by capstone, but Linux's `vfp_undef` only emulates FPSCR;
   any write to FPEXC is SIGILL. NOP it out — Linux maintains FPEXC.EN itself.

---

## 2. The ABI all works

```
init_entry(&fns) = 0        ← entry at offset 0, fills in 5 function pointers
  set_temp  = 0x600001bc
  clear     = 0x60000610
  fill      = 0x60000768
  init_tbl  = 0x600007f0
  blit_gc   = 0x60000814

set_temp(22)  -> 0
init_tbl(&tbl) -> phases = 111,  table = 0x60000c8c
phase table = [1×24, 0×4, 2×24, 0×5, 1×25, 0×4, 2×24, 0×4, ...]   # only 0 / 1 / 2
```

It matches what U-Boot does:

```c
splash.fill(frame1, 0x5555);
splash.fill(frame2, 0xAAAA);
int phases = splash.init_tbl(&wf_init);
for (i = 0; i < phases; i++) mxs_pan(wf_init[i]);   // ← 111 iterations, hopping between the 0/1/2 slots
```

`frame0 = clear()`, `frame1 = 0x5555`, `frame2 = 0xAAAA`, 111 phases. **This is exactly what runs on every boot.**

---

## 3. The strongest piece of evidence: `clear()` reproduces the on-device idle pattern

Distribution of what `clear()` writes into a scratch buffer:

```
0x00 : 1,407,810      0x53 : 328,680      0x41 : 85,644
0x43 :    37,968      0x61 :  16,848      0x51 :    90
```

And the idle table that the official swtcon writes into `/dev/fb0` on the device is:

```
0x41 / 0x43 / 0x45 / 0x47 / 0x51 / 0x53 / 0x61
```

**The same set of values.** So `zplash` and `EPFramebufferSwtcon` inside xochitl are the same engine
(or at least the same waveform data source). That moves the legitimacy of route R from "inferred" to "measured".

It also solves a side mystery: the reason the content of `/dev/fb0` on the device never changes is that it is
stuck on the **idle frame produced by `clear()`** — swtcon considers the screen already clean, with nothing to drive.

---

## 4. A geometry mismatch that has to be solved first

The frame size the engine measures for itself:

```
clear() write span = 0 .. 1,877,079  =  334 × 1405 × 4
fill(v) write pattern = one 16-bit value every 4 bytes (32bpp, one pixel per word)
```

**So zplash was compiled for U-Boot's `334 × 1405 @32bpp`.**

Linux-side xochitl uses **`260 × 1408 @32bpp`** (one page is 1,464,320 B). The two do not match:

| | Width | Height | Per pixel | One frame |
|---|---|---|---|---|
| zplash (U-Boot) | 334 | 1405 | 4 B | 1,877,080 |
| Linux fb page | 260 | 1408 | 4 B | 1,464,320 |

Cramming the engine's frame straight into a Linux page gives shifted lines.

**The fix (and the next step): switch the fb back to `334×1405`.**
This is not "inventing a new mode": it is the one already listed under `modes` (`U:334x1405p-84`),
and exactly the mode the bootloader uses on every boot — known to work.

Constraints worked out: `line_length = 334×4 = 1336`,
`yres_virtual ≤ 24,893,440 / 1336 = 18,632`, so there is room for **13 aligned full frames**
(`13 × 1405 = 18,265`) — enough for the 4 that the U-Boot sequence uses.

---

## 5. Still open: the parameter semantics of `blit_gc`

A 16×16 / 8×8 bitmap was drawn at `(100,100)`, and `blit_gc` barely writes anything
(the return value is always 1; 0–16 bytes change inside the window).

Possible reasons (to be verified):
- it expects a **bitmap of the whole panel**, with `x0/y0/w/h` being only a clip region;
- the bitmap is not 1 byte per pixel but 4-bit packed;
- it needs "the previous frame" to be in some particular state.

Fortunately **this does not block the main line**: the INIT sequence only uses `clear` / `fill` / `init_tbl` / pan,
all verified working. `blit_gc` is what "draw a static image" needs, and belongs to the next stage.

---

## 7. First on-device INIT replay

### 7.1 What was done

Switch the fb from `260×1408 @34.72MHz/85Hz` to **`334×1405 @40MHz`** (the U-Boot set),
then run U-Boot's INIT sequence entirely through the zplash engine, **with no xochitl anywhere**:

```
xres=334 yres=1405 xres_virtual=334 yres_virtual=18265 (13 full frames)
pixclock=25000(40MHz)  margins all 1  hsync/vsync=1
stride=1336  frame=1,877,080

clear(f0)=0  fill(f1,0x5555)=0  fill(f2,0xAAAA)=0  clear(f3)=0
init_tbl -> phases=111, table only uses slot 0/1/2
111 phases × 2 rounds of pan, finally panning to idle frame 3
```

### 7.2 What the screen showed (visual observation)

> "It became lots of diagonal lines, like a raster, then it shifted, then a black-and-white flash."

**The whole chain is connected**: engine → frame → pan → panel, with xochitl not involved.
Diagonal lines → phase switch → black-and-white flash maps exactly to idle frame → phase loop → alternating
`0x5555`/`0xAAAA` frames.

### 7.3 But it exposed a geometry problem (important)

Spreading the frame produced by `clear()` out as 1405×334:

```
r0: 43 45 43 53 51 53 51 53 53 51 53 43
r1: 41 61 41 53 53 53 53 53 53 53 53 43
r2: 41 61 41 53 53 53 53 53 53 53 53 43
r3: ... (rows 1..1404 are byte-identical, varying only with column)
```

**Frame content depends only on the column, not on the row.** Yet the screen shows **diagonal stripes**.

Reasoning: a pattern that varies only with column appears as diagonal lines on screen ⇒ **every row is shifted
horizontally by a fixed amount** ⇒ the row pitch the engine assumes and the row pitch the panel/LCDIF actually
scans do not agree.

Another piece of supporting evidence: `fill()` **does not start writing at offset 0** —

```
clear() write span (0, 1877079)        ← starts at 0, length exactly 1,877,080
fill()  write span (292, 1875633)      ← starts at 292, 1447 bytes short at the end
```

292 = 73 words. So `fill` has its own idea of the frame origin internally (possibly with left/right margins).

### 7.4 Three things that need decompiler confirmation

The questions (by importance):

1. **Row pitch constant**: in `clear`(0x610) / `fill`(0x768) / `blit_gc`(0x814), how much is added
   when advancing one row? Is it `334*4=1336`? Or some other constant (336 words, 340 words)?
2. **Frame-origin offset**: why does `fill` start at 292? Is there a constant such as `origin = 73 words`?
3. **Which byte lane the data lands in**: confirm whether it is `+2` (the R channel) or whether it depends on
   24/32bpp byte packing.
4. The parameter semantics of `blit_gc`.

Answers to 1 and 2 give the correct pitch and the diagonal lines should disappear immediately;
an answer to 4 is what lets real images be drawn.

---

## 8. Follow-up measurement: the frame's row structure (pure memory, no risk)

### 8.1 Method

`fill(v)` writes only the "pixel payload", not row padding. Hand it a buffer full of `0xCC` and count the
**gaps between write offsets**; the row pitch falls straight out.

### 8.2 Result

```
fill(0x5555): wrote 657,072 bytes = 1404 rows × 468 B
             gap between rows 403 B × 1403 times
clear():      wrote 1,877,080 bytes, contiguous from 0 to 1,877,079 (full frame)
```

Solving it: per-row payload `468 B = 234 words`, row pitch `1336 B = 334 words`, and the payload starts at
**word 73 of the row** (off=292).

```
1872 px × 4 bit = 7488 bit = 234 words  ✔ matches exactly
1336 = 73(before) + 234(payload) × 4 + 27(after)  ✔ no remainder
```

So the structure of one frame is:

```
1405 rows × 334 words (1336 B)
  word 0   .. 72   ← row header (clear writes it, fill does not touch it)
  word 73  .. 306  ← 234 words of pixel drive payload = 1872 px @ 4bpp
  word 307 .. 333  ← row trailer
row 0 content differs from the other rows (frame header)
```

**This explains why the rows of `clear()` are almost byte-identical**: it is the drive pattern for "uniform white",
and every row should naturally be the same.

### 8.3 Two things confirmed in passing

1. **The mode switch really took effect**, it was not a no-op:

   ```
   before switch:  line_length=1040   (260×4)
   after switch:   line_length=1336   (334×4)
   after restore:  line_length=1040
   ```

   So the diagonal lines in the previous INIT run were **not** caused by `set_par` being skipped.

2. `smem_len` is actually **33,554,432 (32 MB)**, not the 24.9 MB assumed all along.
   In 334×1405 mode that holds at most **17 aligned full frames** (32 MB / 1,877,080 = 17.8) —
   more headroom than previously judged.

### 8.4 Still waiting on the decompiler (additions to the earlier list)

Against the numbers measured above, these need confirming:

1. Is the row-pitch constant literally `1336`, or is it computed from some field (e.g. `words_per_line * 4`)?
2. **Why `fill` does not write the first 73 words** — are those 73 words row command/timing, or a header that
   only `clear()` initialises?
3. Is the payload really 4 bit/pixel with one dot per clock? This decides "whether the diagonal lines are a
   running shift": if the panel consumes only 234 words per row while 334 are sent, the extra 100 words shift the
   image row by row → diagonal lines; if the panel resets the row count on HSYNC it will not.
   **This one question settles the diagonal lines.**
4. Whether `blit_gc` writes into the "payload area" or a whole row (including the row header).

---

## 9. Decompiler results applied: the whole chain runs on hardware

The decompilation report (24 KB, including byte-by-byte differential verification with Unicorn) was turned into
a C program and run on the device.

### 9.1 Three key conclusions of the report, independently re-checked

| Conclusion | Independent verification |
|---|---|
| Frame = 1405 rows × 334 cells, low 16 bits of a cell = 8 px × 2bit | the write-gap distribution of `fill` yields 234 words of payload per row / pitch 1336 B, matching the report |
| **334 layout content start = cell 73** | in zplash's own template cell 72 = `0x41` and from cell 73 = `0x53` ✔ |
| **260 layout content start = cell 26** | in the device idle frame cell 25 = `0x41` and from cell 26 = `0x51` ✔ |
| 22 °C = 46 phases | band7 from `zplash_tables.json` gives a 46-phase LUT, and drive codes 0/1/2 are the only ones present ✔ |

### 9.2 What already works

```
C program: zigcc cross-compile -> armv7, runs directly on the device
input: 1872×1404 8-bit grayscale bitmap (checkerboard)
process: switch fb to 334×1405 @40MHz -> 46-phase GC16 -> build one frame per phase -> FBIOPAN_DISPLAY
result: **a correct black-and-white checkerboard appeared on screen, upright (top-left = black, bottom-right = white)**
```

Visually confirmed: *"Now it is a checkerboard."*
**That means bitmap → LUT → drive code → frame → pan → panel is correct end to end.**

The code is `/tmp/zm334.c` on the device (with `/tmp/zm334.h`).

### 9.3 Two big pitfalls (both subtle)

**Pitfall 1: the control-bit template of the 260 mode is the "idle" state and does not drive the panel.**

We first extracted the control bits (`0x41/0x43/0x51/0x53/0x61`) from the device's `/dev/fb0` idle frame;
after splicing on the pixel codes the **screen showed nothing at all**. Switching to zplash's own 668-byte
template (a valid frame) and moving the fb to 334×1405 produced an image immediately.

Conclusion: **the control bits are not purely static "wiring information" — they carry the semantics of
"does this frame drive".** Using zplash's template therefore forces the 334 layout.

**Pitfall 2: `remux` brings the rm2fb server back up by itself; `systemctl is-active rm2fb` is a false signal.**

To really confirm that nobody is grabbing the screen you must:

```sh
systemctl stop remux          # this is the one that matters
# then confirm no process holds /dev/fb0
```

Stopping only `rm2fb.service` leaves `remux` free to pull up an xochitl carrying `librm2fb_server`, which pans in
an 85 Hz loop and overwrites our pan every 12 ms — the symptom being "wrote something, nothing happened".

### 9.4 Still unsolved: not white enough, plus ghosting

The final behaviour: the checkerboard is there, but **the white squares are greyish, the whole thing looks dull,
and ghosting from the old UI is still present**.

Cause analysis (what to look into next):

- Of the 46 LUTs at 22 °C, **grayscale 15 (white) gets drive code 0 (no drive) in the vast majority of phases**,
  and only grayscale 0 (black) is actively pushed. In other words this waveform assumes "white is the background
  state", so how white the white squares end up depends on what state the panel was in **before** the drive.
- The 111-step cleanup (byte table `[1×24][0×4][2×24]…`, pushing and pulling the whole screen with
  `fill` semantics) did run to completion, but did not bring the panel to a clean white background.
- Possible reasons: the cleanup sequence ends on a code 2 push, which does not land on white; or a settling time
  is needed between "cleanup → draw"; or a different temperature table should be used.

### 9.5 Measured performance

| Scenario | Time |
|---|---|
| 46 phases, full screen (334 layout) | **3.6 s** (encoding dominates; pan is only ~0.55 s) |
| 46 phases, full frame (260 layout) | 0.8 s |

Encoding is the bottleneck, not the panel. The route given in §13 of the report (phase deduplication −41%,
constant phases via `fill`, dirty rectangles, switching to the short DU/Glf waveforms) is exactly what to do next.

### 9.6 Conclusion

> **Getting off xochitl is now technically verified**: our own process can drive this screen completely,
> using the official waveform data itself, with the right orientation and a correct image.
>
> What is left is not "can it" but "how well": white-level baseline, ghosting removal, encoding performance.
> All three are tuning problems, no longer unknown reverse engineering.

---

## 10. White background, grayscale and timing fixed together (v2, on device)

The three symptoms in §9.4 have **one common root cause**; once it is fixed they all disappear.

### 10.1 Drive-code polarity (measured)

| code | Effect | Evidence |
|---|---|---|
| 0 | **no drive** | content of the idle frame produced by `clear()`; panning it up leaves the screen unchanged |
| 1 | **push black** | full screen of code 1 ×24 frames → all black |
| 2 | **push white** | full screen of code 2 ×24 frames → **whole screen pure white, saturated** (290 ms is enough) |

This confirms the "white is the background state" hypothesis: **in the 46-phase LUT grayscale 15 gets code 0
throughout**, so "draw a white page" by itself drives no pixels at all — white has to be actively painted with
code 2.

### 10.2 The real bug: phases were stretched 7×

The v1 loop was "encode one frame (**78 ms**) → pan (13 ms)". While encoding, the previous frame is still being
scanned out, so **each phase actually stays on screen for ~91 ms ≈ 7 panel frames**. The whole waveform timeline
is stretched 7×, so of course the grayscale does not come out — this is not a problem with the waveform table.

Fix: compute only **one row** per phase (334 u32), then `memcpy` it across all 1405 rows.

```
before fix: 78 ms encoding + 13 ms playback = 91 ms/phase   → 46 phases 4.2 s, grayscale smears into a blur
after fix: 11.6 ms/phase (= 1 panel frame)                  → 46 phases 533 ms, grayscale normal
```

**Criterion**: `pan()` returns "remaining jiffies", and the interval between serial pans is the panel frame
period (measured 11.6–13.3 ms). Any encoding cost that pushes a single phase beyond that pollutes the waveform
timeline.

### 10.3 The LUT index is interleaved

The 16 bytes per phase are **not** arranged in grayscale order:

```
gray g <  8  →  row[2*g]
gray g >= 8  →  row[2*(g-8) + 1]
```

Reading in raw order misreads the whole waveform. With the interleaved mapping:

- phases 3–20: grayscale 1–14 get code 2 (overshoot to white), grayscale 0 and 15 get 0
- phases 21–35: grayscale 0–14 all get code 1 (pushed to black)
- phases 36–44: different **lengths of code 2 tail** depending on the target gray — this is the grayscale shaping section
- phase 45: all 0 (tail)

The sequences for grayscale 5 and 6 differ by only 2 frames — so in practice those two levels **must** merge;
that is a property of the waveform itself.

### 10.4 v2 measured numbers

| Item | Result |
|---|---|
| Polarity and saturation | code 2 ×24 frames = **290 ms** → whole screen pure white |
| 46-phase full-screen playback | **533 ms** (11.59 ms/phase) |
| Grayscale staircase (16 horizontal bands) | by eye: **15+ levels distinguishable**, only gray5/gray6 merge |
| Document page (white background + black header + 30 lines of text) | **clean background, no ghosting of the previous image** |
| Total full-screen refresh time | white flash 290 + GC16 533 = **823 ms** |
| Encoding cost (with per-row content) | 1027 ms / 46 phases (22 ms/phase) → needs scratch pre-generation + `memcpy` |
| Geometry | **1872×1404 exact 1:1, no cropping on any edge**; portrait is the natural orientation |

### 10.5 The full-screen refresh path available now

```
① flash the whole screen with code 2 for 24 frames  (290 ms)  —— drive the panel to a definite white background and wipe the previous image
② play the 46-phase GC16                            (533 ms)  —— pixels other than white are pushed to black/grey; white pixels do not move
```

**Why the ghosting disappears**: not because the waveform erases by itself, but because step ① resets the whole
panel to white. Conversely this shows the current table **cannot self-erase** — grayscale 15 always gets code 0.

### 10.6 What is still missing (updated list)

1. **Partial refresh / DU**: the current table has no previous-state dimension; whether a local region can be held
   still purely by filling code 0 outside the rectangle is unverified; and without a short waveform, any update has
   to take the 823 ms full-screen path.
2. **The full (prev,cur) waveform tables**: in `/usr/share/remarkable/*.wbf` (42 files).
   §11 records progress on this line.
3. **Temperature band**: the real temperature source is `/sys/class/hwmon/hwmon1/temp0` (`sy7636a_temperature`),
   **not** `thermal_zone0`. Measured 21 °C, which falls in band7 [21,23].
4. **Encoding performance**: a page with real content encodes at 22 ms/phase; it must be pre-generated into scratch
   and then played back.

### 10.7 The boundary of partial refresh (measured, important)

Two ways of "drive only part of the area, fill the remaining cells with code 0" were tried:

| Approach | Result |
|---|---|
| **Whole-row band**: `y ∈ [600,800)` full width pushed white, all other rows code 0 | ✅ **clean**. The calibration image is intact, only one full-width white band was added, nothing else moved |
| **Rectangle inside rows**: `x ∈ [80,400) × y ∈ [80,400)`, part of the cells in the same row pushed white | ❌ **crosstalk**. Changes appeared outside the target area, along with stripes at the cell pitch |

**Conclusion: partial refresh must be done per whole row (full width); you cannot drive only a segment of a row.**

A likely physical reason: mixing code 0 and code 2 within one row disturbs the load/charge balance of the source
drivers and produces crosstalk, whereas driving a whole row uniformly does not. The control bits that vary per
column, `0x53`/`0x51` (differing only in bit1), are very possibly the odd/even or polarity configuration of the
source channels; that is not decoded yet.

This has a direct UI implication: **on e-ink, dirty regions for partial refresh must be organised as horizontal
strips**. For text-oriented UI (lines, paragraphs, status bar, keyboard) that is actually natural.

### 10.8 The scan window: looks like the answer, is a trap in practice

The frame period is indeed strictly proportional to the number of scanned pixels (measured, `pixclock=25000`):

| xres | yres | Frame period |
|---|---|---|
| 334 | 1405 | **11.86 ms** |
| 334 | 400 | 3.40 ms |
| 334 | 200 | 1.71 ms |
| 200 | 200 | 1.03 ms |
| 150 | 200 | 0.78 ms |

`pixclock` scales linearly too: full frame 25000→11.87 ms, 15000→7.25, 10000→5.27.

**But shrinking `yres` does not shrink the driven area — the content gets tiled.**

Measured: with `yres=200`, drawing four 50-row bands (black/white/black/white) inside the window makes
**that set of bands repeat seven or eight times across the whole screen**. So the panel gate lines still scan all
1405 rows; LCDIF merely feeds the 200 rows in a loop.

**Shrinking `pixclock` too far underdrives the waveform.** At `pixclock=5000` (5× faster scan) the same document
page turns into **a uniform grey plus horizontal texture**, and the header splits into white lines. The waveform
pulse widths are **calibrated in time**, so shrinking the frame period shrinks the drive time of every phase with
it — not enough energy.

**So the correct formula for drive time is:**

```
refresh time = number of phases × frame period (fixed by the waveform's calibrated frame rate; not freely changeable)
```

The `frame_rate = 133 Hz` (7.5 ms/frame) in the wbf header is the calibrated value, corresponding to
`pixclock ≈ 15850`. We currently use 25000 (≈84 Hz), 1.6× slower than calibration — mild overdrive, but
acceptable image quality.

**Real partial refresh is not a smaller window; it is a full-frame scan that writes non-zero codes only into the
dirty area** (§10.7 already verified that whole-row bands are clean). The cost is that no time is saved.

> Conclusion: the mechanism behind the official 21 ms partial refresh **has still not been found**. Paths ruled out:
> shrinking `yres`/`xres` (tiling), raising `pixclock` (underdrive).
> Remaining candidates: a gate-line mask in the control bits (undecoded), or 21 ms referring to input latency
> rather than panel refresh.

---

## 11. Rendering with the official wbf 2-D tables (done)

### 11.1 The waveform data no longer comes from zplash

The device has 42 official E Ink waveform files under `/usr/share/remarkable/*.wbf` (~282 KB each).
The parser `wbf.py` already decodes them completely (self-check described in `WBF.md`):

- header 48 B, `mc+1=8` modes, `trc+1=14` temperature bands, `luts=0x04` (5-bit LUT, 32×32 transitions)
- **256 B per phase = a 32×32 `[prev][cur]` table, 2 bit per cell, 4 cells per byte**
- target-axis mapping `dst_index = 2 × gray`

zplash's 16 B/phase table is a **flattened** version: the phase count is kept, the previous-state dimension is lost.
That is the root of "you must flash white to pre-clean".

### 11.2 Decode rules (pinned down by measurement)

```
idx = cur*32 + prev      (state = 2*gray)
bit order = LSB-first    code 1 = push black, code 2 = push white, code 0 = no drive
```

Verification (DU @ 21–23 °C, 25 phases):

```
white(30)→black(0):  24×code1 + 1×code0
black(0)→white(30):  24×code2 + 1×code0
unchanged:           all 0
```

**GC16's non-zero diagonal is by design** — GC16 is a "flash everything" refresh: every pixel goes through a reset
cycle, so it removes ghosting inherently; DU is the mode that "does not drive if unchanged".

### 11.3 Phase counts per mode (21–23 °C)

| Mode | Phases | Grayscales | Purpose |
|---|---|---|---|
| INIT | 111 | — | power-up / cleanup |
| DU | **25** | 2 (black/white) | fast partial |
| GC16 | **46** | 16 | full grayscale refresh |
| GL16 ×3 | 46 | 16 | between the two |
| A2 | **12** | 2 | fastest, heavy ghosting |
| DU4 | 29 | 4 | compromise |

### 11.4 Already working

`wr.c` (plus `wbf_lut.h`, C tables generated from the wbf) on the device:

```
white flash 24 frames                     281 ms
full-screen GC16 46 phases (2-D tables)   playback 537 ms + encoding 1323 ms
result: white background + black header bar + 30 lines of black text, clean
```

**Arbitrary grayscale patterns can be rendered now.** What remains is purely encoding performance
(1323 ms is pure CPU cost, which pre-generation / caching / phase dedup can bring down).

### 11.5 Open

1. The mechanism behind the official 21 ms partial refresh (candidates in §10.8)
2. Encoder speed-up: 46-phase full screen 1323 ms → target <200 ms
3. Actual image quality / ghosting per mode (DU/A2/DU4) has not been measured systematically
4. Temperature switching: `wbf_lut.h` currently embeds only band7 (21–23 °C)
