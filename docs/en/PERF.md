# Responsiveness: what was measured, what changed, and where it stands

> English translation of [`../PERF.md`](../PERF.md) (Chinese original).

> Device measurements (rM2, armv7, release) and host measurements (release) are recorded separately.
> Benchmark commands: `./target/release/rmdev --bench-ui` (host), `RM_TIMING=1` to read the device log.

## 1. The three places changed

| Change | Before | After | Evidence |
|---|---|---|---|
| **Page raster cache** (`redraw_page`) | every redraw replayed all strokes | if the content fingerprint is unchanged, blit the previous ink buffer straight back | device: `ink draw 115ms → 3ms`, full redraw `155ms → 58ms` |
| **Card thumbnail blit** (`draw_tile_sel`) | per-pixel `put` | `Fb::blit_gray`: whole-row write + 256-entry gray lookup table | host: 6 cards `1.04ms → 0.37ms`; on device row-wise writes can be vectorized, so the gap is wider |
| **Thumbnail cache switched to `Rc`** | every redraw `clone`d 242 KB × N cards | only the pointer is copied | one redraw of 6 cards saves 1.4 MB of copying |
| **Overview thumbnail cache** | every Pages open rasterized every page | reuse on a `(uuid, page) + stroke fingerprint` hit | device: 53ms → 0ms per page (second open) |

Also fixed a real bug along the way: `Fb::row_mut` lacked an `x >= width` check, so a blit
overflowing the right edge of the screen computed a negative width, which was then treated as a
huge `usize` (in release the addition wraps → index-out-of-bounds panic).

## 2. Host benchmark (release, `--bench-ui`)

```
draw_library (incl. cold thumbnail render):  62.9 ms/frame
draw_library (thumbnail cache hit):          16.2 ms/frame
open_at first time (cold):                   88~119 ms
open_at again (raster cache):                37 ms/frame
redraw_page (content unchanged):             2.1 ms/frame    ← ~119 ms before the change
6 cards, per-pixel put:                      1.04 ms
6 cards, blit_gray:                          0.37 ms
30 text segments (cache hit):                0.4~1.0 ms
canvas paint_all full screen 1:1:            1.5~3.3 ms
```

> Note: `redraw_page` throttles full-screen refreshes to 250ms (the EPDC queue cannot stack),
> so the benchmark clears `last_full`; only then is the measured number CPU time.

## 3. Device measurements (`/tmp/rmdev.log`, `RM_TIMING=1`)

```
first page open: redraw_page total=155ms (ink=115ms blit=36ms overlay=3ms)
redraw again:    redraw_page total= 58ms (ink=  3ms blit=50ms overlay=5ms)
toolbar partial refresh: repaint 2 tiles 432000px blit=9.1ms overlay=12.8ms refresh=0.4ms
overview thumbnail: 53ms per page (first) / 0ms (cache hit)
```

A full-screen redraw now costs only 58ms of CPU, 50ms of which is the one-shot blit of
2.63 million pixels (`blit_1to1`, row-wise zip writes + table lookup). The remaining bottleneck
is the e-ink panel's own refresh time, not our compute — further CPU optimization can only get
down to that 50ms floor.

### 3.1 Main thread blocked by non-drawing work: battery 440 ms + `df` 947 ms

Symptom: "the whole UI freezes for a moment, then moves again; entering Settings stalls,
no page switching". A `[slow]` log line was added first (it fires when `handle` / `flush` / `tick` in the
main loop exceed 200 ms, and `tick` logs its own segments); a single boot located two spots:

| Location | Cost | Why |
|---|---|---|
| `draw_status` → `read_battery()` → `battery::read()` | **440 ms** | reading `/sys/class/power_supply/*/uevent` goes over I2C, and `time cat` measures 0.44 s. The status bar is **drawn every frame**, so every library redraw stalls for half a second; `SettingsView::new` reads it too → half a second to enter Settings |
| `p4_check_disk` → `disk::read()` → fork `df -k` | **947 ms** | the main thread waits synchronously for the child process every 60 s; at boot both the `df` binary and the page cache are cold |

Fix:

* battery → a background thread refreshes a `Battery` snapshot every 10 s
  (`static BATT: Mutex<Option<Battery>>`), and the draw path only `Copy`s it once. One
  synchronous read at startup fills it in, so the status bar is correct on the first frame.
* disk → `statvfs(3)` (a single syscall, microseconds), with the same semantics as the Used
  column of `df -k`.

Same log after the change:

```
before: [slow] tick front half 1025ms [("redraw library", 583), ("battery", 439)]
after:  [slow] tick front half  176ms [("redraw library", 174)]
```

**Lesson**: "sluggishness" on an e-ink screen is almost never slow drawing — it is blocking
syscalls (I2C, fork) stuffed into the draw path. Before adding code that reads hardware or reads
a child process to `draw_*` / `SettingsView::new`, ask first: "will this call wait 100 ms or more?"
