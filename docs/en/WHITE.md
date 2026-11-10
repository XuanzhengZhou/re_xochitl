# White Background Solved — Next: Partial Refresh / Grey-Level Linearity / Temperature Bands

> English translation of [`../WHITE.md`](../WHITE.md) (Chinese original).

> The white background is solved on the device: `code2` = push white, phase stretching was the
> culprit, LUT interleaving is understood, and document-page rendering passes.
> This note is offline computation plus testable experiment design only — **no hardware operations**.
> Every number can be recomputed from the `zplash` tables (`zplash_tables.json`).
>
> **Code**: the C fragments below follow the structure of `zw.c` (the zplash test harness) and were
> compiled together with it for armv7 (`file` = ELF 32-bit ARM EABI5; `GLIBC_2.17` / `GLIBC_2.4`,
> not 2.34). The older white-background test harness `zm_white.c` is kept for reference and is
> superseded by this document.

## 0. Provenance of Each Conclusion

| Conclusion | Source |
|---|---|
| code2 = push white / code1 = push black / code0 = no drive | **measured on the device** (full screen, 24 frames, saturated) |
| a 7× phase stretch is what made the white background grey | **measured on the device** (78 ms → 11.6 ms per phase) |
| LUT interleaving `g<8→raw[2g]`, `g≥8→raw[2(g-8)+1]` | **measured on the device**; re-checked against the tables, consistent |
| 46 phases at 22 °C; grey level 15 gets code 0 in **all 14 bands** | computed (see §1.2) |
| band7=[21,23] ⇒ 21 and 22 are in the same band | computed (hard-coded in the tables) |
| (5,6) is **the closest pair in the whole table** (2 frames apart only) | computed (§B.4), consistent with the visual observation |
| partial refresh is electrically sound / g15 is the only exception | computed, plus already implied by the checkerboard run (§A.2) |
| a unipolar white flash accumulates DC imbalance | raised as a risk here, **not measured** (§1.1) |

---

## 1. Two Re-Checks That Affect What Follows

### 1.1 The white flash is unipolar, the official one is bipolar (risk; keeping it bipolar is recommended)

`run_flat(2,24)` in `zw.c` = 24 frames of push-white only. The official 22 °C byte table is:

```
[1×24][0×4][2×24][0×5][1×24][0×4][2×24][0×2]    111 steps total = 1.32 s
```

**Net impulse = 0** (24 black + 24 white + 24 black + 24 white). The current implementation only
performs the last 24 white steps. Unipolar pushing leaves a one-directional residual charge in the
electrophoretic layer; it does not show up short-term (white really does saturate), but long-term it
shows as: the white background slowly greying, ghosting coming back, and "the same sequence getting
less clean every time".

- This is **not** saying the current white background is wrong, but that it may become wrong after a
  few hundred consecutive unipolar white flashes.
- Compromise: `1×12 0×4 2×24 0×2` (DC is not strictly balanced, but at least there is a reverse push)
  or simply the official 111 steps (1.32 s; the current full refresh is 823 ms, so +0.5 s).
- Criterion (measurable): after 200 unipolar white flashes, has the white-background luminance
  dropped compared with the first one — see L0 in §D.

### 1.2 Grey level 15 is all-zero code in **all 14 temperature bands**

All 14 bands were scanned (`for band in bands: count(g15 != 0)`), result:

```
band0..band13: non-zero frame count for g15 = 0 / 0 / 0 / ... / 0
```

That is: **this waveform table treats "pure white" as a background colour by design and never drives
it**. Switching temperature bands does not help, and repeating GC16 does not help either (repeat it N
times and a g15 pixel is not driven for a single frame — its optical state does not change by one
bit). This directly decides whether the partial refresh in §A works.

(Aside: g0 gets 24 frames of code1 and 0 frames of code2 and is pushed hardest; g1..g14 all have the
shape `reset push white a frames → main push black b frames → tail push white c frames`, see §B.3.)

---

## A. Feasibility Boundaries of Partial Refresh

### A.0 Conclusion

1. **It is electrically sound**, and strictly speaking **the "full-screen refresh" is already
   partial** — because the LUT maps every white pixel to code 0, when a white document page is
   refreshed full-screen, 90% of the screen area is never driven at all. Every page refresh already
   verifies "nothing moves outside the rectangle".
2. **The only hard boundary is grey level 15**: pixels inside the rectangle that must go from dark to
   white are not driven for a single GC16 frame. So partial refresh must be paired with a **local
   white flash** (`local_flat(...,2,24)`), otherwise the white background of the new content keeps the
   old black.
3. **8px alignment does not affect correctness**, only the loop granularity: write code 0 for
   out-of-rectangle pixels in the boundary cell and it is pixel-exact. Coarse trimming by cell is not
   wrong either (it just drives ≤7 extra pixels to the grey level they were supposed to be at).
4. **Partial refresh saves no time**: waveform length is set by the phase count (46×11.9 ms) and is
   independent of area. What it saves is encoding/bandwidth, plus "black text that did not change is
   not pushed again" (less flicker, less ghost accumulation, less panel wear). Real speed needs short
   waveforms (A2/DU), and zplash only has GC16 + the white-flash byte table — the short waveforms live
   in `/usr/share/remarkable/*.wbf` (13 found in the rootfs, including `ED103TC2M1`, see §C.5).

### A.1 Decoding the control-bit template: it is a **static mask over column position**, not a "drive this frame or not" switch

The 668 B template written by `clear()`, split by row, is two run-lengths:

```
row 0 (first row)    : 43×10 47×5 45×30 47×5 43×23 | 53×27 51×30 53×48 51×30 53×48 51×30 53×21 | 43×27
row 1..1404          : 41×29 61×12 41×32           | 53×234                                    | 43×27
                       └───── first 73 cells ────┘   └──────── content cells 73..306 ────────┘   └ tail 27 ┘
```

The only 7 values that appear are `0x41 0x43 0x45 0x47 0x51 0x53 0x61`; bit decomposition:

```
bit6 always 1   bit3 always 0   bits[5:4] ∈ {00,01,10}   bits[2:0] ∈ {001,011,101,111}
0x53 = bit4|bit1|bit0   0x51 = bit4|bit0   0x43 = bit1|bit0   0x41 = bit0
```

- **bit4 falls exactly on the 234 content cells** (73..306) → content-area marker.
- **bit1 is set exactly from cell 73 on** (including the 27 tail cells) → "valid panel span" marker
  within a row.
- bit5 appears only in one run of row0 (cells 29..40) and before cell 73 in row1, **bound to the x
  position**.

All three vary only with **column**; all 1404 rows are identical ⇒ static mask. Therefore:

> **Locality can only be expressed by the 16-bit pixel field (code 0 = no drive); it is neither
> possible nor necessary to express it through control bits.**

⚠️ An honest correction: the §9.3 claim that "the 260 template does not drive" was made together with
the 334 layout switch, so the two variables were not isolated and "bit1 = drive enable" is currently
**undetermined**. It does not matter for partial refresh (the control bits are not touched). To settle
it, run an experiment of the `zm_white ctrl 0x51 --code 2 --frames 24` kind (half screen 0x53 / half
screen 0x51).

### A.2 Why "nothing moves outside the rectangle" is in fact already verified

Look at the code distribution over the 46 phases (§B.1 full table): **grey level 15 is 0 in every
phase**. That means

> full-screen rendering of a white document page = 46 "no drive" steps for every white pixel on the
> screen.

So "drive inside the rectangle, no drive outside" and "drive full screen, white areas naturally not
driven" are **the same thing** electrically. The checkerboard (white cells stay put in an environment
where the whole row is driven) and the document page (white background next to black text is clean)
have already exercised the case of "neighbours pushed, this pixel not pushed" — that is all the
condition partial refresh needs.

**Corollary**: a partial-refresh boundary is no more dangerous than "white background right next to
black text". What remains is only physical coupling (see A.5).

### A.3 The real trap: grey level 15 is never driven

When a new page is partially refreshed, **pixels in the dirty rectangle whose new value is white** can
never come back if the old content was dark. A full-screen refresh hides this behind "290 ms of white
flash"; a partial refresh has no white flash → exposed directly.

So the correct recipe for partial refresh has two parts:

```
① local white flash: code2 × 24 frames inside the rectangle    (pull the rectangle to a definite pure white)
② local GC16: play 46 phases inside the rectangle by the LUT   (only add black, white holds)
   → code 0 outside the rectangle the whole time, not one frame driven
```

Cost: 285 + 533 = **818 ms**, almost the same as a full-screen refresh (823 ms). That is where A.0
item 4 comes from.

**Decision experiment `lw` (runs all at once, the most important one)**: first full-screen black, then
on the left half "draw the document page directly with local GC16", on the right half "local white
flash first, then local GC16 on the same page". Expectation: the left half **stays entirely black**
(the white background cannot be rescued), the right half normal. If the left half is normal too → the
§1.2 conclusion is wrong, and that is a bigger signal.

### A.4 8px alignment: the precise statement

| Claim | Correct? |
|---|---|
| "the dirty rectangle must be 8px aligned, otherwise it shifts/bleeds" | ❌ wrong. Writing code 0 for out-of-rectangle pixels in the boundary cell is enough; it is pixel-exact |
| "the dirty rectangle must be extended to cell boundaries to write a few bytes less" | ✅ right, but that is only **loop granularity**, not correctness |
| "coarse trimming by cell drives 7 extra pixels and goes wrong" | ❌ it does not go wrong; they are merely re-pushed to the grey level they were supposed to be at (there is a ≤7px transient flash) |

The only real constraint: **the target grey level of every pixel outside the rectangle must be known**,
otherwise there is no way to write "what should be written" for them. If it is unknown (the typical
"redraw a small block" case), write 0 outside the rectangle (keep the current state) — and then the
rectangle must be **extended outwards to cell boundaries with the out-of-rectangle pixels in the
boundary cells written as 0**, which both preserves and does not overdrive. That is what `render_rect()`
(§A.7) does.

### A.5 Three things still to be measured

| No. | Question | Method | Reading | Risk |
|---|---|---|---|---|
| **lp** | does local driving really leave the outside untouched | draw the 16-level ramp full-screen → local GC16 blacks out y[400,750) → look at the ramp outside the rectangle | ramp completely unchanged = holds; greyer/fainter = crosstalk | none |
| **lw** | can partial refresh turn black into white | see A.3 | left black, right normal = g15 un-driven confirmed | none |
| **lb** | boundary bleed (halo) | draw a row of 8px-wide white bands (g15) right above a black block and compare with a distant white band; add thin g13/g14 bands on both sides of the black block edge | white band near the black block indistinguishable from the distant one = no bleed | none |

The C code for `lp`/`lw` is in §A.7 of this document; `lb` can be assembled from the same
`render_rect` plus a custom `page[]` and is not given separately (its value is lower than the other
two).

### A.6 The timing budget (why partial refresh saves no time)

| Action | Frames | Time |
|---|---|---|
| local white flash, 24 frames | 24 | **285 ms** |
| local GC16, 46 phases | 46 | **533 ms** |
| total | 70 | **818 ms** |
| full screen (the current one) | 24+46 | 823 ms |

The only savings are encoding (the rectangle is small → `render_rect` encoding drops from ~1.5 s to
tens of ms) and panel wear outside the rectangle. **Phase duration is a property of the waveform, not of the
area** — this also explains why "refresh only a small block" is not faster.

### A.7 C fragments (following the structure of `zw.c`, compiled together with it)

Place after `band_flash()` and before `resp_test()`:

```c
static void page_fill(int g){
    for(int y=0;y<PH;y++) for(int x=0;x<PW;x++) page[y][x]=(unsigned char)g;
}

/* Drive only the pixels inside the rectangle [x0,x1)x[y0,y1); code 0 (no drive) outside it.
 * Pre-encode into scratch, then play, so that every phase is exactly 1 panel frame
 * (the same timing discipline as render_page).
 * Boundary cells are encoded too: pixels outside the rectangle within the same cell are
 * written as 0, which makes it pixel-exact. */
static void render_rect(int x0,int y0,int x1,int y1){
    if(x0<0)x0=0; if(y0<0)y0=0; if(x1>PW)x1=PW; if(y1>PH)y1=PH;
    int cx0=x0>>3, cx1=(x1+7)>>3;
    unsigned char *scratch=malloc((size_t)FRAME*NPHASES);
    if(!scratch){ fprintf(stderr,"scratch malloc failed\n"); return; }
    double t0=ms();
    for(int ph=0;ph<NPHASES;ph++){
        unsigned char *dst=scratch+(size_t)ph*FRAME;
        unsigned int *cells=(unsigned int*)dst;
        fprintf(stderr,"phase %d/%d\r",ph,NPHASES);
        for(int y=0;y<ROWS;y++){
            const unsigned char *t=tmpl[y<1?0:1];
            unsigned int *row=cells+(size_t)y*CELLS334;
            for(int c=0;c<CELLS334;c++) row[c]=(unsigned int)t[c]<<16;
            if(y<y0||y>=y1) continue;
            const unsigned char *src=page[y];
            for(int c=cx0;c<cx1&&c<234;c++){
                unsigned int pk=0;
                for(int l=0;l<8;l++){
                    int px=c*8+l;
                    if(px>=x0&&px<x1)                       /* outside the rectangle write 0 = keep */
                        pk|=(unsigned int)(lut[ph][src[px]&15]&3)<<((7-l)*2);
                }
                row[CX0+c]=((unsigned int)tmpl[y<1?0:1][CX0+c]<<16)|pk;
            }
        }
    }
    fprintf(stderr,"\n");
    double t1=ms();
    int a=0;
    for(int ph=0;ph<NPHASES;ph++){
        memcpy(fb+(size_t)a*FRAME,scratch+(size_t)ph*FRAME,FRAME);
        pan(a); a^=1;
    }
    printf("rect render x[%d,%d) y[%d,%d): encode %.0f ms, play %.0f ms\n",
           x0,x1,y0,y1,t1-t0,ms()-t1);
    free(scratch);
}

/* Push n frames of a single grey level inside the rectangle (code 0 outside) — this is the "local white flash" */
static void local_flat(int x0,int y0,int x1,int y1,int code,int n){
    if(x0<0)x0=0; if(y0<0)y0=0; if(x1>PW)x1=PW; if(y1>PH)y1=PH;
    int cx0=x0>>3, cx1=(x1+7)>>3;
    unsigned int r0[CELLS334],r1[CELLS334];
    memcpy(r0,rowimg[0],LINE); memcpy(r1,rowimg[1],LINE);
    unsigned int v=(unsigned int)(code*0x5555);
    for(int c=cx0;c<cx1&&c<234;c++){ r0[CX0+c]|=v; r1[CX0+c]|=v; }
    int a=0;
    for(int i=0;i<n;i++){
        unsigned int *cells=(unsigned int*)(fb+(size_t)a*FRAME);
        memcpy(cells,(y0<=0)?r0:rowimg[0],LINE);
        for(int y=1;y<ROWS;y++)
            memcpy(cells+(size_t)y*CELLS334,(y>=y0&&y<y1)?r1:rowimg[1],LINE);
        pan(a); a^=1;
    }
}
```

Add two stages to `main()`:

```c
    if(!strcmp(st,"lp")){
        mark("white flash"); run_flat(2,24); settle();
        mark("full-screen GC16, 16-level ramp"); run_ramp();
        mark("local GC16 blacks out y[400,750) (nothing outside driven)");
        page_fill(0); render_rect(0,400,PW,750);
        settle();
        mark("check: the ramp outside must stay exactly the same, the inside must go pure black");
        return 0;
    }
    if(!strcmp(st,"lw")){
        mark("white flash"); run_flat(2,24); settle();
        mark("full-screen black"); run_flat(1,24); settle();
        mark("left half: document page by direct local GC16 (no white flash)");
        make_page(0); render_rect(0,0,PW/2,PH);
        mark("right half: 24-frame local white flash, then local GC16 on the same page");
        local_flat(PW/2,0,PW,PH,2,24);
        make_page(0); render_rect(PW/2,0,PW,PH);
        settle();
        mark("check: the left half should still be black (white background not recoverable), the right half normal black on white");
        return 0;
    }
```

---

## B. Grey-Level Linearity

### B.1 Full 46-phase table (de-interleaved into `g0..g15` per the interleaving rule)

```text
phase g0 g1 g2 g3 g4 g5 g6 g7 g8 g9 10 11 12 13 14 15
   0   0  0  0  0  0  0  0  0  0  0  0  0  0  0  0  0
   1   0  0  0  0  0  0  0  0  0  0  0  0  0  0  0  0
   2   0  0  0  0  0  0  0  0  0  0  0  0  0  0  0  0
   3   0  0  0  2  0  0  0  0  0  0  0  0  2  0  0  0
   4   0  2  2  2  0  0  0  0  0  0  0  2  2  0  0  0
   5   0  2  2  2  2  0  0  0  0  0  0  2  2  0  0  0
   6   0  2  2  2  2  0  0  0  0  0  2  2  2  0  0  0
   7   0  2  2  2  2  0  0  2  0  0  2  2  2  2  0  0
   8   0  2  2  2  2  2  2  2  0  0  2  2  2  2  0  0
   9   0  2  2  2  2  2  2  2  0  0  2  2  2  2  0  0
  10   0  2  2  2  2  2  2  2  0  2  2  2  2  2  0  0
  11   0  2  2  2  2  2  2  2  0  2  2  2  2  2  0  0
  12   0  2  2  2  2  2  2  2  0  2  2  2  2  2  0  0
  13   0  2  2  2  2  2  2  2  0  2  2  2  2  2  0  0
  14   0  2  2  2  2  2  2  2  0  2  2  2  2  2  0  0
  15   0  2  2  1  2  2  2  2  0  2  2  2  2  2  0  0
  16   0  1  2  1  2  2  2  2  0  2  2  2  1  2  2  0
  17   0  1  1  1  2  2  2  2  0  2  2  1  1  1  2  0
  18   0  1  1  1  2  2  2  2  0  2  1  1  1  1  2  0
  19   0  1  1  1  1  2  1  2  0  2  1  1  1  1  2  0
  20   0  1  1  1  1  1  1  1  2  2  1  1  1  1  2  0
  21   1  1  1  1  1  1  1  1  2  2  1  1  1  1  1  0
  22   1  1  1  1  1  1  1  1  2  1  1  1  1  1  1  0
  23   1  1  1  1  1  1  1  1  2  1  1  1  1  1  1  0
  24   1  1  1  1  1  1  1  1  2  1  1  1  1  1  1  0
  25   1  1  1  1  1  1  1  1  2  1  1  1  1  1  1  0
  26   1  1  1  1  1  1  1  1  2  1  1  1  1  1  1  0
  27   1  1  1  1  1  1  1  1  2  1  1  1  1  1  1  0
  28   1  1  1  1  1  1  1  1  1  1  1  1  1  1  1  0
  29   1  1  1  1  1  1  1  1  1  1  1  1  1  1  1  0
  30   1  1  1  1  1  1  1  1  1  1  1  1  1  1  1  0
  31   1  1  1  1  1  1  1  1  1  1  1  1  1  1  1  0
  32   1  1  1  1  1  1  1  1  1  1  1  1  1  1  1  0
  33   1  1  1  1  1  1  1  1  1  1  1  1  1  1  1  0
  34   1  1  1  1  1  1  1  1  1  1  1  1  1  1  1  0
  35   1  1  1  1  1  1  1  1  1  1  1  1  1  1  1  0
  36   1  1  1  1  1  1  1  1  1  1  0  1  1  2  2  0
  37   1  1  1  1  1  1  1  1  1  1  0  0  0  2  2  0
  38   1  1  1  1  1  1  2  0  1  1  0  0  2  2  2  0
  39   1  1  1  1  1  1  1  2  1  1  0  2  2  2  2  0
  40   1  1  1  2  1  0  0  2  1  2  2  2  2  2  2  0
  41   1  0  1  1  2  0  0  2  2  2  2  2  2  2  2  0
  42   1  0  1  0  2  2  2  2  2  2  2  2  2  2  2  0
  43   1  0  2  2  2  2  2  0  2  2  2  2  2  2  2  0
  44   1  2  2  2  0  2  2  0  2  2  2  2  2  0  2  0
  45   0  0  0  0  0  0  0  0  0  0  0  0  0  0  0  0
```

How to read: code 2 = push white, 1 = push black, 0 = no drive; each row = one phase = one panel
frame (11.9 ms).

The structure is obvious at a glance: **phases 3..20 are the reset section** (most levels get 2 = push
white), **21..35 is the main black-push section** (all 1 except g15), **36..44 is the tail section**
(each level switches back to 2 at a different moment), **45 is all 0** (the closing frame).

### B.2 Drive sequence per level (RLE) + counts

| Grey level | Drive sequence (46 phases, `code×frames`) | Black frames | White frames | Tail white pulses c |
|---|---|---|---|---|
| ** 0** | `0×21  1×24  0×1` | 24 | 0 | 0 |
| ** 1** | `0×4  2×12  1×25  0×3  2×1  0×1` | 25 | 13 | 1 |
| ** 2** | `0×4  2×13  1×26  2×2  0×1` | 26 | 15 | 2 |
| ** 3** | `0×3  2×12  1×25  2×1  1×1  0×1  2×2  0×1` | 26 | 15 | 2 |
| ** 4** | `0×5  2×14  1×22  2×3  0×2` | 22 | 17 | 3 |
| ** 5** | `0×8  2×12  1×20  0×2  2×3  0×1` | 20 | 15 | 3 |
| ** 6** | `0×8  2×11  1×19  2×1  1×1  0×2  2×3  0×1` | 20 | 15 | 3 |
| ** 7** | `0×7  2×13  1×18  0×1  2×4  0×3` | 18 | 17 | 4 |
| ** 8** | `0×20  2×8  1×13  2×4  0×1` | 13 | 12 | 4 |
| ** 9** | `0×10  2×12  1×18  2×5  0×1` | 18 | 17 | 5 |
| **10** | `0×6  2×12  1×18  0×4  2×5  0×1` | 18 | 17 | 5 |
| **11** | `0×4  2×13  1×20  0×2  2×6  0×1` | 20 | 19 | 6 |
| **12** | `0×3  2×13  1×21  0×1  2×7  0×1` | 21 | 20 | 7 |
| **13** | `0×7  2×10  1×19  2×8  0×2` | 19 | 18 | 8 |
| **14** | `0×16  2×5  1×15  2×9  0×1` | 15 | 14 | 9 |
| **15** | `0×46` | 0 | 0 | 0 |

Adjacent-level differences:
| Pair | Differing frames | Phases where they differ | Net impulse difference |
|---|---|---|---|
| 0→1 | 21 | [4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 41, 42, 43, 44] | +12 |
| 1→2 | 4 | [16, 41, 42, 43] | +1 |
| 2→3 | 5 | [3, 15, 16, 40, 42] | +0 |
| 3→4 | 10 | [3, 4, 15, 16, 17, 18, 40, 41, 42, 44] | +6 |
| 4→5 | 7 | [5, 6, 7, 19, 40, 41, 44] | +0 |
| 5→6 | 2 | [19, 38] | +0 |
| 6→7 | 8 | [7, 19, 38, 39, 40, 41, 43, 44] | +4 |
| 7→8 | 26 | [7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 38, 39, 40, 43, 44] | +0 |
| 8→9 | 17 | [10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 22, 23, 24, 25, 26, 27, 40] | +0 |
| 9→10 | 12 | [6, 7, 8, 9, 18, 19, 20, 21, 36, 37, 38, 39] | +0 |
| 10→11 | 5 | [4, 5, 17, 36, 39] | +0 |
| 11→12 | 3 | [3, 16, 38] | +0 |
| 12→13 | 8 | [3, 4, 5, 6, 16, 36, 37, 44] | +0 |
| 13→14 | 14 | [7, 8, 9, 10, 11, 12, 13, 14, 15, 17, 18, 19, 20, 44] | +0 |
| 14→15 | 29 | [16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44] | +1 |

`tail white pulses c` = counting back from phase 44 and skipping the trailing 0s, the number of
consecutive 2s — this is **the dose by which the tail section pushes the pixel back to white**.

### B.3 Structural model

Except for g0 and g15, every level is three-part:

```
[n leading frames of 0 = wait]  2×a (reset: push white)  1×b (main push: black)  [maybe a 0 gap]  2×c (tail: push white)  [trailing 0]
```

- `c` increases monotonically with grey level: 0,1,2,2,3,3,3,4,4,5,5,6,7,8,9 — **only 10 distinct
  values**, i.e. 4 groups of the 16 levels share a value.
- `b` varies irregularly between 13 and 26 (g8 is only 13, g2/g3 are 26).
- The net impulse `n2-n1` is completely non-monotonic: `-24,-12,-11,-11,-5,-5,-5,-1,-1,-1,-1,-1,-1,-1,-1,0`,
  and from g7 to g14 it is -1 throughout — **so "net charge decides grey level" is the wrong model;
  do not use it for mapping**.

What really distinguishes adjacent levels is **trajectory shape** (b and c moving in opposite
directions), not net impulse. That also explains why the raw frame difference between adjacent levels
can reach 10~26 frames while the net impulse difference is often 0.

### B.4 Sticking prediction: Hamming distance between adjacent levels

| 1 | 2 | 3 | 4 | 5 | 6 |
|---|---|---|---|---|---|
| **(5,6) 2 frames apart** ← smallest in the whole table | (11,12) 3 | (1,2) 4 | (1,3) 5 | (2,3) 5 | (10,11) 5 |

Other adjacent pairs: 6→7: 8, 7→8: 26, 8→9: 17, 9→10: 12, 12→13: 8, 13→14: 14, 4→5: 7, 3→4: 10, 0→1: 21,
14→15: 29.

**This ranking reproduces the observation directly**:
- (5,6) differ by only 2 frames (phase 19: g5=2/g6=1; phase 38: g5=0/g6=1) — the only pair in the
  table that "barely moves in opposite directions" → sticking.
- every other adjacent pair is ≥4 frames → distinguishable.
- **(11,12) with 3 frames is the second most dangerous pair** (phases 3, 16, 38); if the ramp ever
  looks muddy at 11/12 too, that is it.

This gives a rule of thumb that can be used directly:

> **Adjacent levels whose waveforms differ by < 4 frames ⇒ indistinguishable to the eye; ≥ 4 ⇒
> distinguishable.**
> The rule also explains the observed 15 levels + (5,6) sticking, with no optical calibration needed.

If 11/12 turn out **clearly separated** in measurement while 5/6 stick, then a difference located in
the tail section is more fatal than the frame count (the 5/6 difference is at phase 38, in the tail;
2 of the 3 frames of 11/12 are in the reset section) — that is itself a model constraint worth
recording.

### B.5 Nominal 16 levels ≠ 16 independently distinguishable levels

| Quantity | Value |
|---|---|
| nominal levels | 16 |
| distinct values of the tail pulse c | 10 (0,1,2,3,4,5,6,7,8,9) |
| extra spread from b/a differences | 5 pairs ((2,3)(4,5)(7,8)(9,10)(11,12) etc., each 3~26 frames apart) |
| actually distinguishable | **15** ((5,6) coincide) |
| uniformity | **not equidistant**; spacing set by 4~26 frame differences, sparse at both ends and dense in the middle |

So "resolving 15" and "16 uniform levels" are two different things. Uniformity cannot be achieved by
remapping (remapping can only select, not change luminance) — only by changing the LUT or adding
dithering.

### B.6 Correction: two paths

**Path 1 (no LUT change, usable immediately): treat the 16 levels as 15 and fill in with dithering.**

After merging `(5,6)` there are 15 distinguishable levels left. When rendering 8-bit grey, use this
table (input level 0..15 → LUT index):

| Input level | 0 | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | 11 | 12 | 13 | 14 | 15 |
|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|
| LUT index | 0 | 1 | 2 | 3 | 4 | **5** | **5** | 7 | 8 | 9 | 10 | 11 | 12 | 13 | 14 | 15 |

(i.e. map 6 to 5 as well. The output luminance of input 6 will be slightly below ideal; 2D dithering
later compensates for it.) This only prevents 6 from producing a band identical to 5; **it does not
improve uniformity**.

**Path 2 (change the LUT, the only real uniformity): calibrate the optical curve first, then invert
it.**

Key fact: `c` has only 10 steps at 1-frame granularity. If saturation is around 9~12 frames, then 1
frame of c ≈ 1 visible level — so 6 of the 16 levels must necessarily be spread by b, and 1 frame of b
contributes < 1 visible level (that is why 5/6 stick). **Conclusion: (5,6) is structural; shifting 1~2
frames into neighbouring phases cannot fix it** (moving g6 pushes it into g7's or g4's hole). Real
16-level equidistance requires re-arranging the whole (b,c) allocation and a calibrated curve — see
`respc` (§B.7).

**Why there is no "two-byte patch" here (the first version of this was wrong; correction follows)**

The first idea was "change g6's phase 19 from 1 to 2 and phase 38 from 1 to 0", which would make
exactly the two entries **completely identical** — the wrong direction. In reality the two frames
**cancel each other**:

```
phase 19: g5=2, g6=1      (g6 pushes white 1 frame less)
phase 38: g5=0, g6=1      (g6 pushes black 1 frame more, and is sandwiched between 1 and 0, essentially useless)
net impulse difference = 0  ⇒  the two trajectories nearly coincide  ⇒  sticking
```

To separate them there are only two directions, and **both are already taken**:

| Direction | Change | Collides with |
|---|---|---|
| push g6 brighter +2 | only effective if moved into the tail white pulses | g7 (c=4); the difference drops from 8 frames to ~4, marginal |
| push g5 darker +2 | can only be added to b | g4 (b=22, c=3 same as g5), direct collision |

That is: **the `c=3` step packs g4/g5/g6, and between them only 1~2 frames of b are available**. This
is a density problem of this table's design, not a misalignment. Real 16-level equidistance needs the
whole (b,c) group re-arranged (with `respc` calibration first), or a band with more phases — but that
would overdrive, see §C.2.

**So the practical recommendation is path 1: merge 5/6 and use 15 levels + dithering.** Zero risk,
usable immediately.

(Aside: a sub-frame knob worth trying — the 0 gap in the tail. A `0` frame = 0 V, the ink particles
stop, so "3 frames white + 1 frame pause" is a slightly smaller dose than "3 consecutive white
frames". g10's `0×4` and g7's `0×1` in the table use this knob for fine tuning; g6's isolated white
frame (phase 38) sits between 1 and 0, i.e. it pushed white for one frame but never got into the final
position — that is the physical reason it does not separate from g5. If it is to be touched at all,
the direction is "move this white frame into the last run" rather than delete it, but that is still
only 1 frame. **Unverified.**)

### B.7 Two verification experiments

| No. | Purpose | Method | Reading |
|---|---|---|---|
| **pair** | measure all 15 adjacent pairs in one go | draw 0..14 in the left column and 1..15 in the right, same row = one pair (`make_pairs()`) | any row where left and right are equal ⇒ that pair sticks. **This single image gives the complete input to the correction table** |
| **respc** | calibrate the `c → luminance` curve | after a black background, 10 bands each pushed code2 for 0..9 frames (exactly the c values in the table) | the first band that stops getting whiter = the saturation point; also yields the luminance per frame of c |

`respc`'s frame sequence is deliberately `0,1,2,3,4,5,6,7,8,9` (not the original
`1,2,3,4,6,8,12,16,24,32`), because it **corresponds one-to-one** to the 14 c values in the 16-level
table — one experiment translates the tail section of the whole LUT into luminance.

### B.8 C fragments

Place next to `resp_test()`:

```c
/* 15 adjacent grey-level pairs side by side: left column 0..14, right column 1..15, same row = one pair */
static void make_pairs(void){
    for(int y=0;y<PH;y++){
        int j=(y*15)/PH; if(j>14) j=14;
        for(int x=0;x<PW;x++) page[y][x]=(unsigned char)((x<PW/2)? j : j+1);
    }
}

/* c -> luminance calibration: after a black background each band pushes code2 for n frames,
   n being the tail white pulse count from the 16-level table */
#define CBAND 140
static void resp_c(void){
    static const int CN[10]={0,1,2,3,4,5,6,7,8,9};
    int a=0;
    for(int i=0;i<10;i++){
        int y0=i*CBAND, y1=y0+CBAND; if(y1>ROWS) y1=ROWS;
        unsigned int rb[CELLS334];
        memcpy(rb,rowimg[1],LINE);
        for(int c=0;c<234;c++) rb[CX0+c]|=0xAAAAu;      /* code2 = push white */
        for(int j=0;j<CN[i];j++){
            unsigned int *cells=(unsigned int*)(fb+(size_t)a*FRAME);
            memcpy(cells,rowimg[0],LINE);
            for(int y=1;y<ROWS;y++)
                memcpy(cells+(size_t)y*CELLS334,(y>=y0&&y<y1)?rb:rowimg[1],LINE);
            pan(a); a^=1;
        }
        printf("band %d (y %d..%d) = code2 x %d frames\n",i,y0,y1,CN[i]);
    }
}
```

```c
    if(!strcmp(st,"pair")){
        mark("white flash"); run_flat(2,24); settle();
        make_pairs(); render_rect(0,0,PW,PH);
        mark("check: the row whose left and right halves are equal is the pair that sticks");
        return 0;
    }
    if(!strcmp(st,"respc")){
        mark("black background"); run_flat(1,24); settle();
        resp_c(); settle();
        mark("check: band 0 is the pure-black reference; see which band starts to saturate");
        return 0;
    }
```

---

## C. Temperature Bands

### C.1 21 and 22 are in the same band (certain)

`zplash`'s 14 bands are split by whole degrees; 22 °C falls in band7:

```
band7: tmin=21 tmax=23   46 phases   111-step byte table     ← both 21 °C and 22 °C are here
band6: tmin=18 tmax=20   55 phases   135-step byte table     ← 20 °C falls here
band8: tmin=24 tmax=26   38 phases    99-step byte table     ← 24 °C rises here
```

21 °C was measured using the 46-phase table — **entirely correct, no band error**.

(Aside: band7's 46 phases match the 46 Gc16 phases that the community swtcon measured at 24 °C, which
suggests this table's "nominal temperature" may be slightly above 22; but since 21 falls in band7, use
band7.)

### C.2 Cost and risk of switching bands

| Band | Phases | Play time | vs band7 | Risk |
|---|---|---|---|---|
| band6 (18..20) | 55 | 652 ms | +20% | overdrive: more reset, white may be cleaner, but black text edges get "harder" and flicker is more visible |
| **band7 (21..23)** | **46** | **545 ms** | — | current |
| band8 (24..26) | 38 | 451 ms | **−17%** | underdrive: the table is designed for a warmer panel; at 21 °C it underdrives → lower contrast, ghosting |

Only band8 has speed value (saves 94 ms / full screen). But **the cost of a wrong band is mushy grey
levels**, which conflicts with the white background that was just fixed. Unless measurement shows
band8 does not lose image quality at 21 °C, do not switch bands for 17%.

**Grey-level density per band (computed; the hardest evidence for or against switching)**

Parsing `c` out of each band's LUT as in §B.3 (`c` = tail white pulse frames, the main scale of grey
level):

| Band | Temp | Phases | c(g1..g14) | Distinct c values | Min. difference between adjacent levels |
|---|---|---|---|---|---|
| band0 | ≤2 | 131 | 3,7,9,11,13,13,15,16,18,20,21,23,27,35 | **13** | 8 |
| band5 | 15..17 | 66 | 1,2,3,4,4,5,6,6,7,7,8,9,10,14 | 11 | 3 |
| band6 | 18..20 | 55 | 1,2,3,3,4,4,4,5,5,5,7,7,9,12 | 8 | 2 |
| **band7** | **21..23** | **46** | **1,2,2,3,3,3,4,4,5,5,6,7,8,9** | **9** | **2 (5→6)** |
| band8 | 24..26 | 38 | 1,1,2,2,3,3,3,3,4,4,5,6,7,9 | 8 | 3 |
| band11-13 | ≥33 | 38 | 0,0,2,2,0,0,0,0,0,0,0,0,8,0 | 4 | 2 |

Two readings:

1. **The colder, the wider the range of c** (band0 up to 35 frames, band7 only up to 9) — cold weather
   needs longer pulses, so the design has more "steps" to split. This does not mean "a cold band gives
   finer levels"; quite the opposite: **using a cold band at room temperature = overdrive**, every
   level is pushed into saturation and the grey levels are flattened into a few.
2. **The three bands at ≥33 °C are degenerate** (c is mostly 0, the structure has changed completely);
   **never use band11-13 at room temperature**. This also hints that the "high-temperature section" of
   this table may not follow the same design rules but is another waveform mode.

So the answer for C is: **at 21 °C use band7, do not switch.** band8 saves 94 ms but has lower
grey-level density (8 distinct c values); band6 costs 107 ms more and has lower density too.
Switching bands saves no time, it only trades away image quality.

**Decision experiment `tv` (no code given; use the existing half-screen A/B approach)**: draw the ramp
on the left half with `lut` (46 phases) and the same ramp on the right half with band8's 38-phase LUT
— but the phase sequences differ, so both must run in the same pan loop (take max(46,38); the shorter
one is padded with 8 frames of code 0). See whether both halves resolve 15 of the 16 levels and
whether the top-end white matches. This requires band8's LUT to be compiled into the program as well
(only band7 is embedded at present); the full 14-band header file can be generated on request.

### C.3 Reading the temperature + choosing a band

```c
/* the real temperature source on the rM2 is the sy7636a PMIC, not thermal_zone0 */
static int read_temp(void){
    static const char *p[] = {
        "/sys/class/hwmon/hwmon1/temp0",
        "/sys/class/hwmon/hwmon0/temp0",
        "/sys/class/thermal/thermal_zone0/temp",
    };
    for (unsigned i=0;i<sizeof(p)/sizeof(p[0]);i++){
        int f2=open(p[i],O_RDONLY); if(f2<0) continue;
        char b[32]={0}; int n=read(f2,b,31); close(f2);
        if(n<=0) continue;
        int v=atoi(b);
        if(v>1000) v/=1000;              /* thermal_zone0 reports millidegrees */
        return v;
    }
    return -999;
}
/* zplash's 14 bands: tmin,tmax,phases,byte-table length (from zplash_tables.json) */
static const struct { int tmin,tmax,ph,bt; } BANDS[14] = {
 {-64,  2,131,149},{  3,  5,116,133},{  6,  8,102,117},{  9, 11, 90,103},
 { 12, 14, 77, 89},{ 15, 17, 66,159},{ 18, 20, 55,135},{ 21, 23, 46,111},
 { 24, 26, 38, 99},{ 27, 29, 38, 87},{ 30, 32, 38, 79},{ 33, 37, 38, 71},
 { 38, 42, 38, 63},{ 43,200, 38, 59},
};
static int band_of(int t){
    for(int i=0;i<14;i++) if(t>=BANDS[i].tmin && t<=BANDS[i].tmax) return i;
    return t< -64 ? 0 : 13;
}
```

**One point that must be stated clearly**: `set_temp()` is not reachable from user-space C (the
zplash blob is not loaded). So "switching bands" on this side = **swapping an embedded LUT**, with no
hardware state. In other words, the current `zw.c` is always band7's waveform, whatever the real panel
temperature is. To match official behaviour, all 14 band LUTs must be compiled in and `read_temp()` must
pick the table before every refresh. The generator behind `zm_white.h` can emit all 14 bands in one
pass (~15 KB of table data).

### C.4 Temperature drift and partial refresh (aside, important)

The LUT does not look at the previous state (`blit_gc` never reads the target frame), so **partial
refresh cannot correct the luminance change outside the rectangle caused by temperature drift**.
Content outside the rectangle slowly drifts with temperature/time; only a full-screen refresh pulls it
back. Practical implication: partial refresh needs a policy of "one full refresh every N updates or
every T seconds".

### C.5 Short waveforms (A2/DU) are in the .wbf files, not in zplash

13 waveform file names were found in the rootfs (`rootfs_p3.img.gz`), including:

```
_AFEF21_ED103TC2M1_VB3300-KCD_TC.wbf
_AFEC21_ED103TC2M1_VB3300-KCD_TC.wbf
320_R292_AFBC21_ED103TC2M1_TC.wbf
_AFEE21_ED103TC2U2_VB3300-KCD_TC.wbf ...
```

`ED103TC2M1` is the panel covered in the report. xochitl reads its waveform from here (the
`Unable to load INIT tables!` message also comes from this path). zplash only has GC16 + the
white-flash byte table; **the short waveforms A2/DU/GLF are in the wbf files**. For "fast partial
refresh" that is the only way — but the wbf format is not parsed yet, which is new work and outside
the scope of this note.

---

## D. Recommended Execution Order (sorted by information per unit time; ~15 minutes for the whole set)

| Step | stage | Time | Answers | If the result is the opposite, that means |
|---|---|---|---|---|
| L0 | `temp` | 0 | confirm temperature and band (printed) | — |
| **L1** | `pair` | ~1.5 s | distinguishability of all 15 adjacent pairs in one go | if 11/12 stick too ⇒ the §B.4 criterion holds; if 5/6 separate ⇒ the tables are understood incorrectly |
| **L2** | `respc` | ~1.5 s | c→luminance curve (saturation point) | if it saturates at 5 frames ⇒ all 16 levels rely on b, uniformisation has no solution; if it saturates at 12 ⇒ there is room |
| **L3** | `lw` | ~3 s | direct evidence that g15 is never driven | if the left half is normal too ⇒ §1.2 is wrong, partial refresh needs no preceding white flash |
| L4 | `lp` | ~2 s | whether local driving touches the outside of the rectangle | if the ramp changes ⇒ crosstalk, partial refresh does not hold |
| L5 | `r` | 0.6 s | ramp re-check (the current one) | — |
| optional | boundary halo | 2 s | bleed | — |

Do L1+L2 first, because their output directly decides whether the B correction table is possible; L3
is the make-or-break of A; L4 decides whether partial refresh can ship.

---
