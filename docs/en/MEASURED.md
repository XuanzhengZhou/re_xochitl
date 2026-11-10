# Measured Corrections (Deriving the Formulas Back from Real Official .rm Files)

> English translation of [`../MEASURED.md`](../MEASURED.md) (Chinese original).

> Decompilation can recover the **shape** of a formula, but the constants are often
> lost inside jump tables or register reuse. What this step does: take one page of
> `.rm` genuinely handwritten on the device, **read the raw float32 values directly**,
> and check them item by item against the formula table. Three entries agree, two do not.
>
> Data source: `/home/root/.local/share/remarkable/xochitl/1554f45c-.../c6199893-....rm`
> (331,322 B, 292 strokes: 282×tool15 + 8×tool6 + 2×tool8)

---

## 1. Confirmed (formulas agree with the official implementation point by point)

### 1.1 On-disk point layout = 24 bytes, 6 floats (v1 point format)

Decoded directly with `struct.unpack('<6f', chunk)`, the first point is:

```
x=-267.1155  y=356.1505  speed=0.2504  dir=0.0000  width=57.6000  pressure=0.0083
```

Six-field order, stride and float32 encoding all check out. **This device writes
v1 (24 B), not v2 (14 B).**

### 1.2 `pressure` stores a normalized value

0.0083 / 0.0215 / 0.0303 all the way up to 1.0 — consistent with the `0..1` of the
in-memory model. `rmscene`'s `*255` is its own v1 compatibility layer; **do not** take
it as the on-disk truth.

### 1.3 `dir` is stored in radians

Adjacent points measure 0.0000 → 1.5708 → 2.8989, i.e. 0 → π/2 → 2.9 rad.
`rmscene`'s `255·θ/2π` is likewise only its display conversion.

### 1.4 `width` agrees with the BallpointV2 formula

For tool 15 / thickness 2.0 (every point of the 282 strokes): **width ∈ [3.0, 4.5]**

Formula `w = max(p'·3, 2.0) × (2/1.35)` ⇒ **[2.96, 4.44]** — agreement down to the
second decimal place. (`p'` is the pressure after v2 remapping; `2/1.35 = 1.4815` is
`THICK_DIV`.)

---

## 2. Corrections needed (the spec table disagrees with measurement)

| Tool | Spec text | Measured | Conclusion |
|---|---|---|---|
| 6 Eraser | `w = 10.0·t` | t=2.4 → **57.6**; t=1.2 → **14.4** | **`w = 10·t²`** |
| 8 EraseSection | `w = 2.0·t` | t=2.0 → **6.0** | **`w = 3·t`** |

Verification: `10 × 2.4² = 57.6` ✓, `10 × 1.2² = 14.4` ✓, `3 × 2.0 = 6.0` ✓.

This also retroactively explains the comment in the QML about the eraser having
"a historical squared mapping" — the squaring really does land in the point expansion
inside `line.cpp`, and the final value is exactly `10·t²`.

> The code has been corrected to match the measurements: `Tool::Eraser` /
> `Tool::EraseSection` in `rmdev/src/pen/width.rs`. The three anchors above are pinned
> by unit tests.

---

## 3. Still open (does not affect inking, only the two legacy pens)

`speed` is not linearly related to the displacement between adjacent points. Measured
within the same page:

| i | displacement d | on-disk speed |
|---|---|---|
| 6 | 4.27 | 1.93 |
| 7 | 3.63 | 2.03 |
| 22 | 6.72 | 0.69 |
| 25 | 4.72 | 2.19 |

The ratio drifts between 0.1 and 0.56. Three possibilities, no verdict yet:

1. The official code low-pass filters the coordinates: `speed` uses the displacement
   **before** filtering, while `.rm` stores the filtered coordinates;
2. `speed` actually carries the sampling interval (the `dt` register looking "passed
   but unused" in the decompilation may just mean it was not read);
3. Sampling is not equidistant, and `speed` is the per-segment instantaneous value,
   then smoothed.

**Impact**: only the `Paintbrush / Pencil` width formulas consume `speed` (the other
tools do not). Run with `d/100` normalization for now; fit against the same page's
data once tuning the feel of these two pens actually begins.

---

## 4. How to settle V1 (the pixel semantics of `width`)

In `renderSolidLine`: `pen_width = base(+0x98) × point.width`, where `point.width` is
the 3.0–4.5 above and `base` is the scene→view scale.

⇒ **at a 1:1 view, `pixel width == point.width`** (k=1).
Corroborating evidence: under the official renderer this page's strokes look 3–4 px
thick, the same order of magnitude as the 3.0–4.5 of `width`. Anything more conclusive
needs a pixel-by-pixel "official render vs our render" comparison of the same stroke,
which is deferred to the A/B phase.

---

## 5. Reproduction

```bash
# 1) Pull one page of .rm from the device
scp remarkable:'/home/root/.local/share/remarkable/xochitl/<uuid>/<page>.rm' /tmp/official.rm

# 2) Bypass rmscene's scaling layer and read the raw float32 directly
python3 - <<'PY'
import rmscene, struct
for b in rmscene.read_blocks(open('/tmp/official.rm','rb')):
    if type(b).__name__ == 'SceneLineItemBlock' and b.item.value:
        L = b.item.value
        print(L.tool, L.thickness_scale, len(L.points))
PY

# 3) Monotonic print: read the 24 bytes around stream.data.data.tell() inside point_from_stream
```
