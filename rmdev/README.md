# rmdev — a reMarkable 2 note app rebuilt against the stock implementation (in progress)

**English** | [中文](README.zh-CN.md)

Not "yet another e-ink note app", but **a layer-by-layer rewrite against the
reverse-engineered specification of xochitl**: pen width, pressure, direction and the
on-disk format all match the stock implementation, while subscription handwriting
recognition, telemetry, EPUB, localisation and retail mode are cut.

The specification comes from reverse engineering the stock xochitl (pen width /
pressure / direction / on-disk format, aligned item by item); the measurement results
that can be published are in [`../docs/en/MEASURED.md`](../docs/en/MEASURED.md) and
[`../docs/en/FORMATS.md`](../docs/en/FORMATS.md).

> Note: this document was written at an early milestone, so the progress table and the
> directory tree may lag behind the code. The feature list and build instructions in the
> top-level [`README.md`](../README.md) are authoritative.

---

## Current progress

| Module | Status | Notes |
|---|---|---|
| `pen/line.rs` | ✅ | `Point` (24 B) / `Line` / 23 `Tool` values / 9 `Color` values; numbers match the stock implementation |
| `pen/width.rs` | ✅ | Width formulas for all 23 pens, including the v2 pressure remap / smoothing / damping |
| `pen/stroke.rs` | ✅ | Landing point kept out of the `Line`; speed = displacement × scale; pressure = neighbour mean + displacement decay; dir = two-segment average |
| `input/` | ✅ | evdev → normalised coordinates (axis cross + X mirror); axis ranges read via `EVIOCGABS` |
| `fb.rs` | ✅ | rm2fb framebuffer + mxcfb partial refresh |
| `canvas.rs` | ✅ | Per-point width capsule rasterisation + single-layer ink density compositing |
| `chrome.rs` | ⚠️ | Development toolbar (9 pens + 3 widths + undo); not the final UI |
| `.rm` read/write | ❌ | Next milestone |
| Pages / document four-part model | ❌ | Next milestone |
| Eraser / selection / zoom | ❌ | |
| AI / sync | ❌ | |

---

## Build

```bash
# cross-compile (armv7 + glibc; must be dynamically linked for rm2fb to take over)
rustup target add armv7-unknown-linux-gnueabihf
cargo build --release --target armv7-unknown-linux-gnueabihf
# the linker is set in .cargo/config.toml (a zigcc wrapper around zig cc)

# unit tests (pure logic, run on the host)
cargo test
```

## Running it

```bash
scp target/armv7-unknown-linux-gnueabihf/release/rmdev remarkable:/home/root/
scp tools/inject_rmdev.py tools/devtest.sh remarkable:/home/root/
ssh remarkable 'sh /home/root/devtest.sh suite'   # stop remux -> inject test strokes -> grab the framebuffer -> restore
scp remarkable:/tmp/rmdev.fb /tmp/ && python3 tools/fb2png.py /tmp/rmdev.fb /tmp/dev.png
```

Trying strokes on the real device:

```bash
ssh remarkable '/home/root/run.sh'     # needs separate preparation: stop remux + start rm2fb-client
```

* Exit: tap the bottom-left corner three times
* Undo: the leftmost button in the top bar
* Switch pen / width: tap a slot in the top bar

## Layout

```
src/
  pen/line.rs     Point / Line / Tool / Color -- one-to-one with .rm
  pen/width.rs    width formulas for the 23 pens (pure functions, prime candidates for unit tests)
  pen/stroke.rs   sample stream -> Point stream
  input/evdev.rs  16-byte input_event reads + EVIOCGABS
  input/decode.rs one frame of events -> normalised coordinates (section 4.1)
  fb.rs           rm2fb / mxcfb
  canvas.rs       rasterisation + ink density compositing
  chrome.rs       development toolbar
tools/
  inject_rmdev.py synthetic stroke injection (note: the kernel drops duplicate ABS values)
  devtest.sh      one-shot on-device test
  fb2png.py       framebuffer dump -> PNG
```

## Pitfalls (all of them are written into the code comments)

1. **`input_event` is 16 bytes.** Slicing it as `(i32,u16,u16,i32)` yields only 12 bytes,
   which shifts the whole event stream and leaves the screen completely frozen. It is now
   decoded field by field over 16 bytes.
2. **`bits_per_pixel` is not a byte stride**: 16 bpp means 2 bytes per pixel. Using it
   directly writes 1/8 of a pixel and the screen does not react at all.
3. **Pen-down is detected via `BTN_TOUCH`, not `pressure>0`** — pressure only affects width.
4. **`EVIOCGABS` reports `p_max = 4095`** (not 2047); injection scripts must follow that.
5. The kernel **drops duplicate ABS values**, so injected strokes have to dither them.
6. `cargo test` uses `src/lib.rs`, while the device entry point is `src/main.rs`; both
   share the same library.
