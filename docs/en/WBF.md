# E Ink `.wbf` Waveform File Format Analysis (reMarkable 2)

> English translation of [`../WBF.md`](../WBF.md) (Chinese original).

Sample: `/tmp/320_R259_AFAB21_ED103TC2M1_TC.wbf` (282 610 B, taken from a real device
under `/usr/share/remarkable/`, one of 42 files).
Parser: [`reverse/python/wbf.py`](../../reverse/python/wbf.py).

---

## 0. Findings at a Glance

| Item | Result | Confidence |
|---|---|---|
| Header structure | 48 B fixed + temperature-band table + xwi block, every field/offset determined | **Certain** (all four checksums pass) |
| `mc` / `trc` | `mc = 7` → **8 modes**; `trc = 13` → **14 temperature bands** | **Certain** |
| Table organisation | mode table (8×4B) → per-mode temperature table (14×4B) → waveform table (RLE) | **Certain** (all 112 references pass checksum) |
| Waveform table encoding | `0xFC` toggles RLE/raw, `0xFF` terminates, 1 B checksum trailer | **Certain** (all 79 tables pass checksum) |
| Phase geometry | **32 src × 32 dst**, 2 bit per cell, 4 cells per byte → **256 B per phase** | **Certain** (phase counts match the zplash ladders value by value) |
| **Is there a previous-frame state dimension?** | **Yes**, the LUT is two-dimensional `[prev][cur]` | **Certain** |
| Effective grayscale | only **even** dst indices are used; `dst_index = 2 × grayscale (0..15)` | High (cross-confirmed three ways: DU4={0,10,20,30}, A2/DU={0,30}, GC16=all even 0..30) |
| Mode names | mode_version=0x19 → INIT, DU, GC16, GL16×3, A2, DU4 | High (public tool's mode table + panel model ES103TC1 match) |
| Phase code semantics | only three values 0/1/2 occur (3 never appears); 0 = no drive | Medium (the meaning of 0 is certain, the polarity of 1/2 is undetermined) |
| Intra-byte bit order LSB/MSB | **undetermined** (both are self-consistent, evidence in §11) | Low |

**Self-check (strongest signal) — both ladders match:**

```
zplash phase_count : 131,116,102,90,77,66,55,46,38,38,38,38,38,38
wbf  mode 2 (GC16) : 131,116,102,90,77,66,55,46,38,38,38,38,38,38   ✔ 14/14 identical

zplash byte_count  : 149,133,117,103,89,159,135,111,99,87,79,71,63,59
wbf  mode 0 (INIT) : 149,133,117,103,89,159,135,111,99,87,79,71,63,59 ✔ 14/14 identical
```

The two **mutually independent** ladders agree value by value, which means header
parsing, mode table, temperature-band table, RLE decoding and phase/byte geometry are
**all correct**.

---

## 1. Overall File Layout

```
0x000  48 B   fixed header (waveform_data_header)
0x030  16 B   temperature-band table: 14 upper bounds + 1 final bound + 1 B checksum  (trc+1 bounds + final bound + cs)
0x040  31 B   xwi block: 1 B length (0x1D=29) + 29 B ASCII + 1 B checksum
0x05F  32 B   mode table: 8 × {u24 address, u8 checksum}
0x07F 0x380   mode blocks: 8 × 0x70 B, each = 14 × {u24,u8} temperature pointers + 14 × {u24,u8} second bank
0x3FF  16 B   fixed 16 B record (see §8)
0x40F  ...    waveform table area: 79 distinct tables, each [RLE data][0xFF][checksum][16 B record]
0x44AD1 ...   last table of the last mode (DU4)
0x44E90 ...   descriptors pointed to by the second pointer bank + 8×8 dither matrix (1313 B, semantics undetermined)
0x44FF2  = end of file = file_length in the header ✔
```

> Two earlier anchor points were wrong. The measured layout is as follows:
> * The xwi **length byte is at `0x40`**, the string at `0x41..0x5D` (29 B), the checksum at `0x5E`.
>   `xwia = 0x40` in the header points at the length byte, not at the string.
> * `0x5E` is not the start of a u32 offset table. The real table is at **`0x5F`** (= `wmta` in the header),
>   each entry **4 B = u24 address + u8 checksum** (checksum = sum of the first 3 bytes & 0xFF),
>   8 entries total. The `0x7f88,0xef7f,...` seen at `0x5E` is a one-byte-shifted pseudo-u32.

---

## 2. The 48-Byte Header (Complete Field Table)

Packed, little-endian. The print order in `xochitl` corresponds exactly to the offsets:

| Offset | Size | Field (xochitl print name) | Measured value | Notes |
|---|---|---|---|---|
| 0x00 | u32 | `checksum` | `0x734762BC` | **CRC32**: `crc32(4×0x00 \|\| file[4:file_length])`, verified ✔ |
| 0x04 | u32 | `file_length` | 282610 | = actual file size ✔ |
| 0x08 | u32 | `serial_number` | 7802 | production serial number |
| 0x0C | u8 | `run_type` | 0x11 | run type |
| 0x0D | u8 | `fpl_platform` | 0x00 | FPL platform code |
| 0x0E | u16 | `fpl_lot` | **259 (0x0103)** | = the `R259` in the filename ✔ |
| 0x10 | u8 | `mode_version` | **0x19** | selects the mode-name table (see §10) |
| 0x11 | u8 | `wf_version` | 0xAB | |
| 0x12 | u8 | `wf_subversion` | 0x21 | |
| 0x13 | u8 | `wf_type` | 0x51 | |
| 0x14 | u8 | `fpl_size` | 0x00 | panel size code |
| 0x15 | u8 | `mfg_code` | 0xCE | panel vendor code; xochitl's `tft_vid` is most likely derived from it/barcode (see §4) |
| 0x16 | u8 | `wfm_rev` | 0x00 | |
| 0x17 | u8 | `frame_rate_hex` / `frame_rate` | **0x85 = 133** | frame rate 133 Hz (xochitl prints it twice, as hex and as %d) |
| 0x18 | u8 | (not printed by xochitl) | 0x55 | |
| 0x19 | u8 | `vcom_offset` | 0x00 | |
| 0x1A | u16 | (not printed by xochitl) | 0x0000 | |
| 0x1C | u24 | `xwia` | **0x40 (64)** | xwi block offset (points at the length byte) |
| 0x1F | u8 | `cs1` | 0xCB | semantics unknown (not a simple sum, see §11) |
| 0x20 | u24 | `wmta` | **0x5F (95)** | mode table offset ✔ consistent with `48+16+1+29+1` |
| 0x23 | u8 | `fvsn` | 0x01 | |
| 0x24 | u8 | `luts` | **0x04** | LUT type; 0x04 = 5-bit / 32×32 / 4 codes per byte |
| 0x25 | u8 | `mc` | **7** | mode count − 1 |
| 0x26 | u8 | `trc` | **13** | temperature-band count − 1 |
| 0x27 | u8 | `awv` | 0x03 | advanced waveform flags |
| 0x28 | u8 | `eb` | 0xFF | |
| 0x29 | u8 | `sb` | 0xFC | |
| 0x2A | 1 | (reserved) | 0x01 | |
| 0x2B..0x2E | 4 | (reserved) | 0 | |
| 0x2F | u8 | `cs2` | 0x77 | semantics unknown |

`luts` value → geometry (consistent with the public `wbf_waveform_dump` tool):

| luts | States | Codes/byte | Unidirectional | Notes |
|---|---|---|---|---|
| 0x00 | 16 | 4 | no | 4-bit LUT, 16×16 |
| **0x04** | **32** | **4** | no | **5-bit LUT, 32×32 (this sample)** |
| 0x05 | 32 | 2 | no | multi-voltage LUT |
| 0x15 | 32 | 2 | no | multi-voltage LUT v2 |
| 0x1D | 32 | 2 | yes | unidirectional LUT (only 32 destinations per phase) |

---

## 3. Temperature-Band Table (0x30..0x3F)

`trc+1 = 14` lower bounds + final bound + 1 B checksum:

```
bounds      = 00 03 06 09 0c 0f 12 15 18 1b 1e 21 26 2b 30   (0,3,6,9,12,15,18,21,24,27,30,33,38,43)
final bound = 0x3E = 0x30 = 48
checksum    = 0x3F = 0x47 = (sum(bounds)+48)&0xFF ✔
```

The 14 temperature bands (half-open intervals):

```
[0,3) [3,6) [6,9) [9,12) [12,15) [15,18) [18,21) [21,24)
[24,27) [27,30) [30,33) [33,38) [38,43) [43,48)
```

These correspond one-to-one to zplash's `tmin/tmax`, except that zplash clamps the two
ends to `-64` and `+200`: zplash `[-64,2] [3,5] ... [43,200]` ↔ wbf band 0 `[0,3)` and band
13 `[43,48)`; the extrapolation rule is zplash's own choice.
**All 14 bands map one-to-one, unambiguously.**

---

## 4. The xwi Block (Extra Waveform Information)

`xwia=0x40`: `[u8 len][len B ASCII][u8 cs]`, `cs = (len + Σchars) & 0xFF`.

In this sample: `len=0x1D(29)`, string `320_R259_AFAB21_ED103TC2M1_TC`, `cs=0x88` ✔

This is the filename itself, and it explains the three xochitl fields
`xwi: %s` / `xwi_size: %d` / `xwi_cs: 0x%02X`. `tft_vid: %s` is the panel VID derived from
mfg_code/barcode (xochitl also contains strings such as `U2/M1/U3/C5/C6` and
`Unable to decode TFT_VID from barcode.`).

---

## 5. Mode Table and Mode Blocks

```
mode table @ wmta(0x5F): 8 × {u24 table address, u8 = (b0+b1+b2)&0xFF}
  mode0 -> 0x07F      mode4 -> 0x23F
  mode1 -> 0x0EF      mode5 -> 0x2AF
  mode2 -> 0x15F      mode6 -> 0x31F
  mode3 -> 0x1CF      mode7 -> 0x38F
```

Each mode occupies **0x70 B** and holds **two** 14-entry pointer banks:

```
+0x00  14 × {u24 waveform table address, u8 cs}    ← primary temperature table (all valid in this file)
+0x38  14 × {u24 address, u8 cs}                   ← second bank (content in mode4/mode5, all zero elsewhere)
```

For mode4/mode5 the second bank points at the **8 B / 12 B records** starting at
0x44E90 / 0x44F00 near the end of the file (`01 00 01 02 <ptr> <cs>`,
`02 00 02 04 <ptr><cs> <ptr><cs>`), and those records in turn point at the
**8×8 dither matrices with values in 0..7** after 0x44FA8.
This is reported verbatim in the JSON as `modes[i].extra_bank`, but **its semantics are
not interpreted** (see §11). It is most likely the GL16 dither / multi-grayscale mixing
channel and deserves a separate investigation later.

---

## 6. Waveform Tables: RLE + Phase Geometry

### 6.1 RLE (verified against the checksums of all 79 tables)

```
rle = True
loop:
    c = *p++
    cs += c
    if c == 0xFC: rle = not rle          # toggle raw/RLE
    elif c == 0xFF: break                # end of table
    elif not rle: emit(c)                # raw, 1 byte
    else:  n = *p++; cs += n; emit(c × (n+1))   # RLE: repeat n+1 times
assert *p == cs & 0xFF                   # 1 B checksum trailer
```

Note that the repeat count is `n+1` (a length of 0 means one repetition), and the checksum
**includes** `0xFC`/`n`/`0xFF` themselves.
All 79 tables in this sample pass (`wbf.py --check` prints this).

### 6.2 A Phase Is a 32×32 Grid

```
bytes_per_phase = states*states / codes_per_byte = 32*32/4 = 256
phases          = len(expanded) / 256
```

Each byte holds four 2-bit codes; byte order is **outer dst (0..31), inner src in groups of 4**.

In `wbf.py`, `phase_grid()` returns `grid[src][dst]`, which makes it convenient to look up a
waveform by `(prev,cur)`.

### 6.3 Only 0/1/2 Occur as 2-Bit Codes; 3 Never Appears

Measured distribution (temp band 0):

| Mode | Phase code 0 | 1 | 2 | 3 |
|---|---|---|---|---|
| INIT | 5 120 | 73 728 | 73 728 | 0 |
| DU | 72 528 | 1 482 | 742 | 0 |
| GC16 | 97 240 | 17 712 | 19 192 | 0 |
| A2 | 35 636 | 102 | 102 | 0 |
| DU4 | 80 364 | 3 166 | 2 486 | 0 |

INIT's 1/2 counts are perfectly symmetric (73 728 / 73 728), resembling positive/negative
polarity drive; 0 = no drive. Which of 1 and 2 drives toward black or toward white is
**undetermined**.

### 6.4 A Fixed 16 B Record Follows Each Table

In 78 of 79 tables the 16 B immediately after "RLE data + 0xFF + checksum" is **identical**:

```
01 00 | b0 04 | b0 04 | c0 08 | 40 06 | 00 00 | 00 00 | 00 77
       1200    1200    2240    1600
```

The same record also appears after the mode table area (0x3FF). It looks like waveform/timing
constants (1200/2240/1600), **semantics undetermined**; the parser simply skips it.

---

## 7. Self-Check Results (Directly Reproducible with the Script)

`python3 wbf.py <file> --check`:

```
self-check OK: mode 0 (INIT) phases == zplash byte_count (INIT)
self-check OK: mode 2 (GC16) phases == zplash phase_count (GC16)
self-check OK: mode 3 (GL16) phases == zplash phase_count (GC16)
self-check OK: mode 4 (GL16) phases == zplash phase_count (GC16)
self-check OK: mode 5 (GL16) phases == zplash phase_count (GC16)
```

Phase ladder per mode (14 temperature bands, B0 coldest):

| mode | name | phase ladder |
|---|---|---|
| 0 | INIT | 149,133,117,103,89,159,135,111,99,87,79,71,63,59 |
| 1 | DU | 73,65,57,50,43,37,31,25,22,19,17,15,13,12 |
| 2 | GC16 | **131,116,102,90,77,66,55,46,38,38,38,38,38,38** ← zplash phase_count |
| 3 | GL16 | 131,116,102,90,77,66,55,46,38,38,38,38,38,38 |
| 4 | GL16 | 131,116,102,90,77,66,55,46,38,38,38,38,38,38 |
| 5 | GL16 | 131,116,102,90,77,66,55,46,38,38,38,38,38,38 |
| 6 | A2 | 35,31,28,24,21,17,15,12,10,10,10,10,10,10 |
| 7 | DU4 | 84,75,65,58,49,42,35,29,24,24,24,24,24,24 |

Note that mode3/4/5 point at the **same set of table addresses** (0x20B7C…), i.e. the three
GL16 modes share a single LUT set — which matches GL16 appearing three times in a row in the
mode-name table.

The other three classes of checksum also all pass (the fields are in `wbf.py`'s `to_json()`):
* global CRC32 = header `checksum` ✔
* temperature-band table checksum ✔
* xwi checksum ✔
* checksums of all 79 waveform tables ✔ (112 references, 79 distinct tables after dedup)

---

## 8. What the Modes Are / Correspondence with GC16, DU, A2, GL16

Evidence:
1. `mode_version = 0x19`; in the mode-name table of the public tool
   [`wbf_waveform_dump`](https://cdn.jsdelivr.net/gh/zephray/glider@master/utils/wbf_waveform_dump/main.c),
   `versions={0x19,0x43}` maps to
   `{"INIT","DU","GC16","GL16","GL16","GL16","A2","DU4"}`, **exactly 8 entries = mc+1**;
   the sample panel for that entry is **ES103TC1**, the same 10.3" family as
   **ED103TC2M1** in this file's xwi.
2. `mc+1 = 8` matches the length of that table.
3. mode3/4/5 share tables → GL16 three times.
4. The destination (dst) index sets fully confirm the semantics:

| mode | dst index set | semantics |
|---|---|---|
| 0 INIT | all of 0..31 | full-flash initialisation, any previous state → any target |
| 1 DU | **{0, 30}** | drives only to black/white → binary direct update |
| 2 GC16 | **{0,2,4,…,30}** | full 16-grayscale refresh |
| 3/4/5 GL16 | {0,2,…,30} | 16 grayscale, shorter waveform (reuses the GC16 tables) |
| 6 A2 | **{0, 30}** + src∈{0,1,3,29,30,31} | binary high-speed refresh |
| 7 DU4 | **{0, 10, 20, 30}** | **4-grayscale** direct update |

DU4 uses only the 4 targets `0/10/20/30` — exactly twice the grayscales `0/5/10/15`, which
is the hardest evidence for `dst_index = 2 × grayscale`:
* GC16 uses all 16 grayscales → even indices `0,2,…,30` ✔
* DU4 uses 4 grayscales `{0,5,10,15}` → `{0,10,20,30}` ✔
* A2/DU use `{black, white} = {0,15}` → `{0,30}` ✔

So: **the wbf destination axis has 32 slots, but the panel has only 16 grayscales; the
mapping is `2×g`** (odd slots are always 0, confirmed by measurement).

---

## 9. The Most Important Conclusion for This Project: wbf Has a Previous-State Dimension

* Each waveform table in wbf is **two-dimensional `[prev 0..31][cur 0..31]`**,
  2 bit per phase cell, 256 B per phase.
* What is compiled into zplash is **16 bytes per phase, destination grayscale only, no
  previous state** (`phase_tables`), plus a set of `byte_tables`.
* Yet the two **agree value by value in phase counts** (GC16 ladder / INIT ladder),
  which shows that zplash's tables were "flattened" out of this official wbf:
  phase lengths preserved, the 32×32 previous-state dimension dropped, only the 16
  destination grayscales kept.

**Implication**: a genuinely partial refresh / ghosting removal requires wbf's
two-dimensional tables — the waveform for the same destination grayscale differs with a
different previous state, and zplash's 1D tables cannot express that.
Also, `DU` (mode 1) writes only the two targets `{black, white}` in wbf, so the rM2's
"fast partial refresh" is inherently binary at the waveform level; intermediate grays come
from dithering.

---

## 10. On xochitl's 16×16 CSV Export (a Point of Tension, Now Explained)

xochitl's export format strings `0,%i,%i` / `15,%i,%i` / `%i,0,%i` / `%i,15,%i` / `%1,%2,%3`
do look like a **16×16** grid. But the file structure forces 32×32:
at 16×16 (64 B/phase), GC16 would yield 524 phases, 4× off zplash's 131;
at 32×32 (256 B/phase) all 14 values **match exactly**.

Conclusion: **the file is 32×32**; xochitl's 16×16 export is its own post-conversion
representation (most likely it exports only the 16 grayscales `dst=2×g` × 16 prev samples).
This item is a "reasonable inference", not direct evidence.

---

## 11. What Could Not Be Determined (Explicitly Flagged)

1. **Intra-byte bit order of the 2-bit codes (LSB-first vs MSB-first)**
   The reference implementation (`wbf_waveform_dump`) uses LSB-first; but decoded MSB-first,
   GC16's diagonal (previous state = target state) has 28/32 entries that are entirely 0,
   versus only 16/32 for LSB-first. Both pass every checksum (checksums are independent of
   bit order). **Undetermined**; the default follows the reference implementation
   (LSB-first), and `phase_grid(..., msb_first=)` in `wbf.py` switches it.
2. **Whether `src` or `dst` is the outer loop**
   The reference implementation uses outer dst, inner src in groups of 4. Because the pairing
   is coupled to bit order, this cannot be proven independently from the file alone.
   Defaults to the reference implementation.
3. **The polarity of phase codes 1 and 2** (which drives toward black, which toward white).
   All that is known: 0 = no drive, and 3 never appears.
4. **The exact meaning of `cs1`, `cs2`, `eb`, `sb`, `awv`, `fvsn`.**
5. **The meaning of the fixed 16 B record after each table** (`01 00 b0 04 b0 04 c0 08 40 06 …77`);
   identical after 78 of 79 tables, suspected timing/voltage constants.
6. **The semantics of the second pointer bank + trailing descriptors + 8×8 dither matrix**
   (populated for mode4/mode5). Reported in the JSON as `extra_bank`, not interpreted.
7. **The mechanism behind `dst_index = 2×grayscale`** (a "5-bit controller", or 1 bit reserved
   for something else?).
8. **Why the `src` axis uses 0,1,2,3,4 and the even values 6..28 plus 29,30,31** (20 values)
   rather than the 16 pure evens. Possibly a special encoding for "uninitialised/edge states".
9. Only one sample exists, so **the differences across the 42 files** (three TFT VIDs,
   different FPL lots, whether variants with `luts=0x00/0x05/0x15/0x1D` exist) are all
   inferred from code paths, not measured.

---

## 12. `wbf.py` Usage

```bash
python3 wbf.py /tmp/320_R259_AFAB21_ED103TC2M1_TC.wbf -o /tmp/wbf.json --check
python3 wbf.py FILE.wbf --summary            # human-readable summary + list of all tables
python3 wbf.py FILE.wbf --grid -o out.json   # expand each phase into a 32×32 integer grid (large)
python3 wbf.py FILE.wbf --no-lut             # header/table structure only
python3 wbf.py dir/*.wbf                     # batch, one .json per file
```

Output structure (with additional extension fields):

```json
{
  "file": "...", "file_size": 282610,
  "header": { "checksum": ..., "mc": 7, "trc": 13, "luts": 4, ...,
              "_field_layout": [{"name":"checksum","offset":0,"size":4}, ...],
              "_declared_crc32_ok": true },
  "lut_geometry": {"states":32, "codes_per_byte":4, "unidirectional":false,
                   "transitions":1024, "bytes_per_phase":256, "note":"..."},
  "temperature_bounds": [0,3,6,...,43,48],
  "xwi": {"string": "320_R259_AFAB21_ED103TC2M1_TC", "checksum_ok": true, ...},
  "modes": [
    {"index":0, "name":"INIT", "temp_table_addr":127, "extra_bank":[],
     "temps":[
       {"index":0, "range":[0,2], "bound":0, "next_bound":3,
        "table_id":0, "table_addr":1039, "phases":149,
        "expanded_bytes":38144, "checksum_ok":true,
        "lut": ["55ff55...", ...]        // each entry = 256 B (hex) of one phase
       }, ...]}, ...]
}
```

* By default `lut` is **512 hex characters per phase** (lossless, compact): about 3.3 MB for
  the whole file.
* With `--grid`, `lut[phase]` becomes a 32×32 integer array of `grid[src][dst]` (about 12 MB).
* `phase_grid()` can also be imported on its own for `(prev,cur) -> waveform` lookups.
* Robustness: truncated files / garbage data / all-zero files never crash; they only return
  non-zero or carry `warnings`. This sample produces no warnings and no errors.

---

## 14. External References (for Cross-Validation, Not Artifacts of This Repository)

* `wbf_waveform_dump` (glider / NekoInk) — authoritative implementation of the header
  structure, RLE and phase geometry:
  <https://cdn.jsdelivr.net/gh/zephray/glider@master/utils/wbf_waveform_dump/main.c>
* `python-waveform-parser` (aceisace) — Python reference for the same format:
  <https://raw.githubusercontent.com/aceisace/python-waveform-parser/main/wbf-parser.py>
* `waveform_data_header` in the i.MX EPDC kernel driver (field names of the earlier 48 B
  version):
  <https://lore.kernel.org/linux-arm-kernel/20220206080016.796556-4-andreas@kemnade.info/raw>

**Every numeric value** in this document was measured from the sample; external material was
used only to cross-check field names and the decoding algorithm. Every checksum from both
sides matches.
