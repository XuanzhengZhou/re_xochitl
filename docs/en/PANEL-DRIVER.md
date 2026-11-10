# Our Own Panel Driver (Rust Version)

> English translation of [`../PANEL-DRIVER.md`](../PANEL-DRIVER.md) (Chinese original).

> **rmdev no longer depends on xochitl / rm2fb**: `src/panel/` parses `.wbf` itself,
> encodes waveforms itself, and drives the LCDIF page flips itself.

## Why rm2fb was unavoidable before

This unit **has no EPDC** (`epdc@306f0000` is `disabled`), so everything that reaches the
screen goes through LCDIF, which serializes one page of "32-bit cell codes" per frame.
That logic exists only in the official xochitl's `EPFramebufferSwtcon`; rm2fb parasitizes
it and calls `sendUpdate`. That is why "the server dies, the screen freezes, and the
process is perfectly healthy" (see `RM2FB.md`) is structural rather than the deployment
script's fault.

That logic is now rewritten in `src/panel/`, ported from `epd.c` (the C version, already
verified item by item on the device).

## Layering

| File | Responsibility | unsafe |
|---|---|---|
| `panel/wbf.rs` | `.wbf` header / temperature bounds / xwi (RLE) / mode table parsing, fingerprint-based file selection | none |
| `panel/encode.rs` | template slots, command bytes, `(target, previous frame) → 2-bit code`, cell encoding | none |
| `panel/raw.rs` | open `/dev/fb0`, change geometry, mmap, `FBIOPAN_DISPLAY`, whole-word writes | **all of it lives here** |
| `panel/mod.rs` | `Panel`: initialize / partial refresh / full refresh / state file | none |
| `fb.rs` | `put/row_mut/blit_gray/fill_rect/get/refresh` exposed to the UI | none |

`main.rs`, `canvas.rs`, `chrome.rs`, `tagsview.rs` **were not changed by a single line** —
the signature and behavior of `refresh()` are unchanged; only the layer underneath is now
our own driver.

## Three hard rules (violating any one gives a white screen or ghosting)

1. **A waveform phase must occupy exactly one frame period** (11.76 ms).
   Measured: `pan()` **absorbs** the time spent writing the fb into the wait —
   `write 2.6 + wait 9.1 = 11.76`.
   So as long as "encode + write fb" fits into one frame, streaming (encoding the next
   frame while playing) is safe:
   * small blocks (strokes, encode < 1 ms): stream;
   * large blocks (encode > 9 ms, 9.6 ms/phase for a full screen): first **encode all
     phases into RAM**, then play them frame by frame.
   Encoding while playing and failing to keep up stretches phases past 1.5 frames; the
   symptom is "all white + ghosting" — the C version and the Rust version **each hit this
   once**, both for this reason.
2. **Every update must end with one frame of all `0` (no drive).** LCDIF keeps scanning the
   last page forever.
3. **`prv` must stay in sync with the actual on-screen state.** Once `prv` is "whiter"
   than the screen, encoding decides "target = white, previous frame = white → no drive"
   and whatever should be covered stays visible (this is exactly how the toolbar's white
   background fails to cover notes). So an image counts as settled, and `prv = img` is
   allowed, only when **a whole waveform round was not interrupted by a new target**.

## Dirty region during a waveform run: the union, not the latest block

During one waveform run the driven area is "**the union of every rectangle that has
received ink since this round started**". This is neither an optimization nor feel tuning;
it is a physical fact of the panel: waveform phases advance globally, what has already been
scanned out cannot be changed within a frame, so any ink laid down during this round has to
stay inside the driven region to have a chance of being pushed.

Doing the opposite (shrinking the dirty region to each update's own small block) shows up
as: while writing one stroke the UI submits a 12×12 patch every ~10 ms, each patch is
driven for 1 frame and then dropped — DU needs 22 frames to reach full black, so the screen
shows a faint, slow line.

One companion rule: **`prepare()` (rebuilding `kp`) runs only for the newly arrived job
rectangle**, never for the union. The union easily grows to 1264×1872, recomputing it costs
2.4 ms, and the frame period is pushed from 11.76 to 16.5 ms — the waveform is stretched 40%
and the feel is "sluggish, can't keep up".

## Limits when writing the fb

Only **whole 32-bit word** writes are allowed (`write_volatile::<u32>`).
`memcpy`/`copy_from_slice` may be compiled into 64/128-bit moves, which this mapping
silently drops — the read-back looks right, the screen does not move.

`FBIOPAN_DISPLAY` returns a **positive** number on success (remaining jiffies), so failure
can only be detected with `< 0`.

**Writing only the dirty rows is not enough**: frame integrity is guaranteed by the TCON,
so the whole slot must be written (1.46 MB, 2.6 ms). Fortunately `pan` absorbs that time
into the frame period, so nothing is lost.

## State file

`/tmp/rm2epd.state`: raw `1404×1872` bytes of on-screen grayscale, x-major
(`prv[x*UH+y]`, 0 = black, 15 = white). Written incrementally, dirty rectangles only
(`FileExt::write_at`), otherwise every frame would rewrite 2.6 MB. A missing file means
"just booted" → run the official 111-frame INIT first to establish the white baseline.

Environment variables: `RM_PANEL_STATE` (alternate path), `RM2EPD_WBF` (force a waveform
file), `RM_DU_FRAMES` (play only N DU frames, default 0 = all), `RM_PANEL_TIMING=1` (print
per-block timings).

## How to verify

```sh
# 1) driver only, no UI
./paneltest          # white background + black square top-left + black bar at top + 16 gray levels + black square bottom-right + bottom ruler

# 2) layered debugging when something goes wrong
./paneldbg enc       # compute waveform codes only (no panel driving): GC16 black←white should be 21 zeros + 24 ones + 0
./paneldbg raw 1 24  # bypass encoding, push 24 "push black" frames directly -> the screen should turn black
./paneldbg slot      # read back the words of slot 0 to verify that writes landed
./paneldbg regs      # LCDIF registers (CUR_BUF should follow pan)

# 3) real run
setsid sh -c "RM_PANEL_TIMING=1 ./rmdev >/tmp/rmdev.log 2>&1 &"
```

Note: **do not go through `/opt/bin/rm2fb-client` again**, and do not start
`rm2fb.service` — that brings xochitl up and it fights over `/dev/fb0`.

## Four fixed bugs (all of them convincing impostors)

### 1. Wrong offset in phase encoding → strokes never appear

`encode_cols_range` emits "packed from `out_off`, laid out by `(pc - pclo)`", while `one`
is addressed **absolutely** by panel column (`put_col` reads `one[pc*CSTR..]`).
The two-pass path (`update()`) copies into `one[pclo*CSTR..]` itself, so full-screen
refresh was fine; but the streaming path (strokes) passed `out_off = 0`, so the encoded
frame landed at `one[0..span]`, and `put_cols` went on to read `one[pclo*CSTR..]` — exactly
the range that had just been zeroed.

Result: **every frame pushed "all `0`, no drive"**, not a single dot of ink. Full screen
fine, strokes completely absent. The diagnostic was to read the words back from the slot: a
correct phase 0 should be `0x00535555` (all 8 lanes pushing black); what was read was
`0x00530000`.

> Probe trap: content cell 26 covers y=1864..1871, not the top of the screen.
> `s = (UH-1-y)/8`, otherwise you will spend ages reading a cell that is never used.

### 2. Dirty region shrunk back to the latest block → faint, slow ink

See the previous section. After switching to the union, the same stroke went from "driven
for 1 frame" to "pushed on every frame of the stroke".

### 3. Rebuilding `kp` over the union → 16.5 ms frame period (waveform stretched 40%)

With the union at 1264×1872, one `build_kp_cols` call took 2.4 ms, pushing the frame period
from 11.76 to 16.5. Fixed by rebuilding only the rectangle of the newly arrived job (`kp`
depends only on `img`/`prv`, and those change only inside the job).

### 4. `prv` committed too early → toolbar fails to cover notes, menus go white

The streaming path used to `commit()` on exit regardless of whether it had been
interrupted. So **ink whose waveform had not finished** was recorded as "arrived", and the
next toolbar draw computed `(target = white, previous frame = white)` → no drive → the note
text stayed on screen uncovered. Now it commits only when "a full round had no new target".

## Crash hardening

`fill_rect`/`dim`/`refresh` all moved to i64 clipping. When scrolling to the bottom edge of
the screen the UI computes rectangles whose `y+h` overflows i32; this used to panic and take
the whole process down, leaving the screen on its last frame, which looks like "frozen,
navigation does nothing". The driver layer should not die because the layer above gets one
rectangle wrong.

## Measured numbers (band 8, 24 °C)

| Operation | Phases | Frames/phase | Per frame | End-to-end latency |
|---|---|---|---|---|
| Stroke `71×71 Du` | 22 | 22/22 | **11.1 ms** | ≤ 9 ms |
| Overlay `600×720 Gl16` | 38 | 38/38 | **11.7 ms** | ≤ 0 ms |
| Stroke `71×64 Du` (2 rounds) | 22 | 44/22 | **11.6 ms** | ≤ 8 ms |
| Full-screen GC16 (two-pass) | 38 | 38 | encode 522 + play 454 ms | — |

Encoding cost: **9.6 ms** per full-screen phase (after adding the "8 lanes, same code" fast
path; it was 11.7 ms before).

## From "it runs" to "it matches the official app"

The goal for this round was "final output matching the official app, and no slower than it".
In practice the bottleneck was almost never the waveform or the encoding; it was three
pieces of wrong code, plus a few places where the wrong semantics were chosen.

### 1. `clip()` return values destructured in the wrong order → rectangles enlarged thousands of times, plus a panic

`clip()` returns `(x0, x1, y0, y1)`, and `fill_rect` / `dim` destructure in that order. Only
`refresh` was written as `(x0, y0, x1, y1)`:

* `y0` got the x right edge and `x1` got the y bounds, so `w = y1 - x0`, `h = y0 - x1`;
* `11×11@720,1460` became a `740×740` square starting at `720,731` — the area was enlarged
  **4400×**;
* out of bounds, the slice start in `mark_pushed` passed the end:
  `slice index starts at 1027782 but ends at 1027728` → **panic → abort → back to the
  library screen**.

Why it was **intermittent**: with `x = 0, w = 120`, `x1` happened to equal `y`, so the
mismatched result came out right by accident. That is why toolbar refreshes always looked
fine, and **the further from the origin, the worse it got**; with `x=0` and `y=0` it even hit
`x0 >= x1` and returned immediately, dropping that refresh entirely ("wrote something,
nothing happened; tap the menu and it appears").

Fix: extracted into `Fb::region()`, destructuring changed to `(x0, x1, y0, y1)`, plus a
regression test `region_never_enlarges_the_request` (11×11 must come back unchanged,
out-of-bounds must be clipped).

### 2. The "first-time reset" branch in `RectF::include()` → `Line.bounds` is always a zero rectangle

```rust
if self.w == 0.0 && self.h == 0.0 {
    self.x = x; self.y = y;      // reset origin
}
```

`Line::push` calls `include` **twice** per sample point (top-left corner, bottom-right
corner). The first call sees `w==0`, resets, and still computes 0; the second sees it again
and resets again. **No matter how many points a stroke has, `bounds`' `w`/`h` stay at 0
forever.**

Log evidence (point count normal, bounding box all zeros; log strings translated from the
Chinese build):

```
[erase] path 1927 pts, radius box (33,54)-(100,1590), 1 hit;
        candidates [(18, 625, 0.0,  0.0,  0.0, 0.0, 31.0),
                    (15, 455, -204.75, 1430.82, 0.0, 0.0, 3.0),
                    (18, 402, 89.99, 138.48, 0.0, 0.0, 31.0)]
```

Downstream symptoms came in a crowd: the eraser's "erase whole stroke" derives its range
from `line_bbox` → only a single point remains, so it hits only when the eraser sits exactly
on those coordinates (**highlighter cannot be erased** — it is thick and sparse); the
repaint rectangle after erasing degrades to a few pixels (**erased content stays on
screen**); selection intersection and zoom clipping have the same problem.

### 3. Dirty-region diff scanned the whole screen every time → 32 ms per stroke

To catch refreshes the app failed to report, `diff_pushed` compared the 2.6 M pixels of
`plane` against `pushed` on every `refresh`: 10.4 MB read, **32 ms even with DDR bandwidth
saturated**, and completely independent of how large the request was. The app paid it per
stroke, capping the refresh rate at 16 Hz.

Changed to recording a bounding box at the **write entry points**: `plane` is a private
field and there are only five writers — `put` / `row_mut` / `fill_rect` / `dim` / `clear`;
each entry calls `mark_dirty` once and the diff scans only that box. While writing text it
is those few dozen pixels. Three companion changes:

* compare through a byte view — only `&[u8]` `==` sinks down to `bcmp`; `&[u16]` is an
  element-by-element loop, 20–50× slower;
* `put` does not mark dirty when the value is unchanged (the vast majority of pixels in an
  overlay repaint are rewritten in place);
* `fill_rect` compares row by row and skips a row that is already the target color.

### 4. Reallocating the 2.6 MB grayscale buffer every frame → 146 ms of page faults

`push_rect` used to `mem::take` and then move `levels` into the channel, so the UI had to
`malloc` the same block again next time, and the first write to each new page faulted:

```
before [pf] refresh 1404×1872@0 total 212418us = … + push 211445
after  [pf] refresh 1404×1872@0 total  65862us = … + push  64902
```

Changed so the panel thread returns the buffer when it is done (a `back_rx` return channel
plus a 4-deep free pool).

### 5. Three fixes to waveform and rendering semantics

* **Strokes use A2, not DU**: DU is 25 phases ≈ 294 ms, A2 is 12 phases ≈ 141 ms, with the
  same binary semantics (`A2 black←white` = 10 push-black frames, `white←white` /
  `black←black` = all `0`).
* **The ballpoint pen's anti-aliased gray is not driven under A2**, so lines scatter into a
  string of dots. Changed to 8×8 ordered dithering (reusing `BAYER8`) to fold the density
  into 0/255. Ordered rather than random: on e-ink a random pattern differs on every
  repaint, which reads as "grainy flicker".
* **Highlighter repaints take the true-gray path** (`tex = s >= 0.999 && !is_highlighter()`
  in `paint_line`); dithering must be skipped or flat gray turns into a checkerboard — added
  the `ink_hl` flag.

### 6. The eraser split into two tools

The `Tool` enum **must not gain new values** (it is the `u8` stored in `.rm`, and the 23
values must match the official app), so the distinction lives in the mode flag
`chrome.erase_stroke`:

* **Eraser** (paint white): erase only the pixels it sweeps over, and on pen-up leave an
  **eraser line** with `Tool::Eraser`. The official app stores it the same way and subtracts
  in order when rendering; erasing only the canvas without leaving the line means everything
  erased comes back after a page flip.
* **Erase stroke**: `commit_erase` removes the stroke from the model, and `Op::Remove`
  makes it undoable.
* The pen-tail eraser (`BTN_TOOL_RUBBER`) is always paint-white and unaffected by the
  current menu selection — `stroke_erase` is fixed at button-down.

Two companion fixes:

* Hit testing changed from "point to point" to "point to **line segment**" — highlighter
  samples are sparse, so an eraser passing between two points is far from every sample;
  `hw` also changed from `hit_width` (level × coefficient, only 2~3 px) to "the larger of
  `hit_width` and the sample's real width".
* Erasing changed from "linear falloff by coverage" to **hard erase**
  (`cov >= 0.5 ? 0 : old`). Linear falloff leaves intermediate gray `old * (1-cov)`, which
  A2 does not drive for even one frame → ghost dots that cannot be erased.

### 7. Stability

* The channel is now **bounded** (`sync_channel(8)`) with `try_send`. In an unbounded queue
  each job carries a whole block of grayscale data (2.6 MB for a full screen), so filling
  one screen can blow up memory — and Rust's allocation failure is an `abort` (systemd shows
  `status=134`, which manifests as "back to the library screen"). When a send fails, the
  block is **marked dirty again**, the buffer goes back to the pool, and **it is not
  recorded as pushed** — better a frame late than treating unsent content as sent.
* `ExecStart`'s `>` changed to `>>` so a crash site is no longer overwritten by the restart.
* The debug outline in `repaint_rects` now `mark_pushed`s only the border pixels. It used to
  mark the whole block, and once `push_rect` returned early on out-of-bounds, that block
  already equalled `plane` in `pushed`, so the dirty-region diff never saw it again ("the
  frame shrank ring by ring and finally disappeared").

### Measurements

| Metric | Before | After |
|---|---|---|
| `avg_flush` (one app-side submit) | 5000~10746 µs | **455~1889 µs** |
| `flush/s` | single digits | **57~133** |
| one diff pass | 32 ms | **35 µs** |
| one stroke refresh (`275×277`) | 62 ms | **2~4 ms** |
| full-screen GC16 `push` | 211 ms | **65 ms** |
| panic / restart | once per screenful | 0 |

End-to-end estimate: pen sampling 125 Hz ≈ 8 ms + processing < 1 ms + submit < 1 ms + first
frame ≤ 11.76 ms ≈ **21 ms**. This is summed from the link segments, **not** measured with a
high-speed camera.

### Debug switches

| Variable | Effect |
|---|---|
| `RM_PANEL_TIMING=1` | one line per round: `[panel] … frame period … over-frames …` |
| `RM_TIMING=1` | one line per second: `[t] ev/s=… flush/s=… avg_flush=…` |
| `RM_PANEL_JOBS=1` | one line per `refresh`: `[req] W×H@X,Y wf= mode= ← file:line` (very noisy) |
| `RM_VIS=1` | draw the rectangle each refresh finally settled on with a 2 px border, blocks 1/2/3+ in black/dark gray/light gray; **pollutes the picture**, remember to turn it off after diagnosing |
| `RM_ERASE_LOG=1` | when erasing a whole stroke, print the eraser path point count, the radius box, and the candidates' point counts and bounding boxes |

## Screenshots: do not use `/tmp/rm2epd.state`

The `prv` stored in `/tmp/rm2epd.state` is **not "what is currently on screen"**: in the
streaming path `prv` is "the previous frame's target" (`set_rect` writes it as the encoding
start), and only the two-pass path (`RM_PANEL_TWOPASS=1`) and the settle path update it to
the arrived value; during normal operation it is written to disk only in `Drop`/`flush_state`,
so a `SIGTERM` that kills the process writes nothing.

So screenshots take another route:

```sh
sh tools/shot.sh name      # kill -USR1 -> the app dumps the UI plane as PGM -> pull it back and convert to PNG
```

The `SIGUSR1` handler only sets an `AtomicBool`; the actual dump happens in the main loop's
`tick()` (`/tmp/rmdev.shot.pgm`), and what you get is `Fb::plane` — what we **want** on
screen.

## Large refreshes raise the clock temporarily (`src/cpu.rs`)

`ondemand` drops to 396 MHz when idle, which pushes full-screen encoding from 9 ms/phase to
17 ms and makes the shutdown dialog / sleep image 1.1 s end-to-end. Within one round
`framegen` raises `scaling_min_freq` to `cpuinfo_max_freq` **as soon as a job of ≥ 0.7 Mpx
appears**, and restores it at the end of the round. Measured: shutdown dialog 1146 ms →
548 ms (frame period 24.7 → 11.8, over-frames 45/45 → 0/45).

Note that this does not help memory-bandwidth-bound content: the sleep image (fine serif
text) is still 1100 ms and 16.4 ms/phase after raising the clock — that area is all
"mixed-content cells" and never hits the 8-lane same-code fast path. See `POWER.md` §4.2.

## Not done yet

* ~~**Inherent eraser latency**~~: solved. Erasing the black pen goes through A2 (12 phases
  ≈ 141 ms); only erasing **true gray such as highlighter** switches to GL16 on demand
  (46 phases ≈ 540 ms), because A2 cannot push flat gray back to paper white. The decision
  uses the mid-gray ratio of erased pixels tallied in `erase_segment` (it switches only when
  more than 64 are involved).
* ~~**Fast writing still slightly lags**~~: solved. Strokes use A2, end-to-end ≈ 21 ms.
* **The 522 ms two-pass encoding**: only with `RM_PANEL_TWOPASS=1`, off by default. Real
  pipelining would need encoding moved to its own thread (the official app uses two threads,
  framegen + vsync-flip).
* **The 54 ms of full-screen grayscale**: `gray_to_cols` measures ~21 ns/px over 2.6 M px
  (reads 5.2 MB, writes 2.6 MB). It only affects full-screen/large refreshes; a handwritten
  stroke is a few dozen pixels and under 1 µs, so it was not pushed further. Pushing it
  further would need vectorization (ARMv7 has NEON) or changing `img`'s row/column order.
* **End-to-end latency is not measured**: 21 ms is an estimate summed from the link
  segments. Confirming it needs a 240 fps high-speed camera (pen tip and screen in the same
  frame, count the frame difference) or a hardware probe like the official one.
* Crash safety: if `SIGTERM` lands exactly in the middle of an update, the last phase keeps
  being scanned. Signal handling should write a blank frame.
* **The `Tool` enum must not gain values**: `.rm` stores that `u8` (23 values matching the
  official app). Any future feature like an "eraser variant" can only go through mode flags,
  never the enum.
