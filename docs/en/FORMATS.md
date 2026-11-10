# Ebook Format Support Matrix

> English translation of [`../FORMATS.md`](../FORMATS.md) (Chinese original).

> Test environment: reMarkable 2 (armv7l / Cortex-A7 / 994 MB / 1404×1872 grayscale).
> Every "on device" column was confirmed by deploying to the real tablet and checking a
> `tools/shot.sh` screenshot — not by a local unit test.

## 1. Supported formats

| Format | Status | Page count | On device | Parsing method | Known limitations |
|---|---|---|---|---|---|
| `.rm` notebooks | ✅ | — | ✅ | Native format | — |
| **PDF** | ✅ | reported by the document | ✅ | Device's own `/usr/lib/libpdfium.so` (`dlopen`, not bundled) | ✅ Text layer + outline (`FPDFText_*` / `FPDFBookmark_*`, degrades when symbols are missing) |
| **EPUB** | ✅ | after reflow | ✅ | Hand-written zip central directory + OPF + XHTML tag stream | ✅ Images (JPEG/PNG, grayscale); tables flattened; CSS not implemented |
| **TXT** | ✅ | after reflow | ✅ | BOM → full UTF-8 validation → **built-in GBK table** → Latin-1 | — |
| **Markdown** | ✅ | after reflow | ✅ | Line-level markup stripping | `*`/`_` always removed (`a*b` loses characters) |
| **HTML** | ✅ | after reflow | ✅ | Tag-stream scan, no tree is built | `<pre>`/tables lose their structure |
| **RTF** | ✅ | after reflow | ✅ | Control-word scan, groups skipped | Surrogate pairs get split; tables/images not handled |
| **FB2** | ✅ | after reflow | ✅ | XML tag stream | TOC entries of nested sections land on the same page; `<binary>` images not decoded |
| **DOCX** | ✅ | after reflow | ✅ | Hand-written zip + `word/document.xml` | List numbering lost; styles.xml inheritance not read |
| **CBZ** (comics) | ✅ | zip entry count | ✅ | `zune-jpeg` + `zune-png` + 16.16 fixed-point area-average scaling | — |
| **MOBI** | ✅ | after reflow | ✅ | PalmDB + PalmDOC header + PalmDOC LZ77 / HUFF-CDIC | Only the KF7 part of a collection is rendered |
| **AZW3** (KF8) | ✅ | after reflow | ⚠️ local verification | Same as above + KF8 body / FDST flow 0 | No INDX reassembly; KV8 embedded images not decoded |
| `.cbr` (RAR) | ❌ | — | — | No usable RAR implementation in pure Rust | Suggests converting to CBZ |

Adding a format only requires producing a `reflow::Book` (`Vec<Chapter>` + `Vec<TocEntry>`) in
`src/doc/<fmt>.rs`, wrapping it in `reflow::ReflowDoc`, and adding one dispatch line to
`open()` in `src/doc/mod.rs` — line breaking, pagination, font sizes, page cache, TOC and
search are all inherited.

## 2. Why not use an existing reader engine

Surveyed (including sizes actually measured):

| Candidate | Reason for rejection |
|---|---|
| KOReader | AGPL-3.0; the official armhf package is 27.3 MB (80.6 MB unpacked); it is a Lua application, not a library, and also needs LuaJIT + SDL3 |
| crengine-ng | GPL-2+ (viral); 4.58 MiB for the full set, and cross-compiling for armv7 additionally requires 8 C libraries: freetype/harfbuzz/fribidi/unibreak/utf8proc/zstd/z/png/jpeg |
| MuPDF | AGPL-3.0 (a commercial license must be purchased); trimmed build 3.09 MiB, full Alpine build 45.1 MiB |
| calibre | Python + Qt6; `ebook-convert` is simply its Python pipeline, there is no embeddable C library |
| fb2 / docx-rs / html2text | Pull in quick-xml/html5ever and a pile of other dependencies, while the logic actually needed is only a few hundred lines |

**Conclusion**: all format parsing is written in-house, borrowing only leaf crates that are
pure Rust. The sole exception is PDF — the device already carries pdfium, so it is used
directly through `dlopen` and not a single byte has to be bundled.

Cost and payoff: the only new dependencies are `zune-jpeg` / `zune-png` (about +210 KB, pure
Rust, zero C), which buys support for EPUB/TXT/MD/HTML/RTF/FB2/DOCX/CBZ; pulling in muPDF
instead would be at least +3 MB and would turn the whole project AGPL.

## 3. Two pitfalls (hit before, recorded here)

### 3.1 `iconv` is unusable on the device

`/usr/lib/gconv/` is trimmed down to just `IBM437.so` and `gconv-modules`:

```
$ ls /usr/lib/gconv/
IBM437.so
gconv-modules
```

So `iconv_open("UTF-8", "GB18030")` necessarily returns `(iconv_t)-1`, and any GBK path that
goes through the system iconv fails silently on the device and falls back to Latin-1
(garbage all over the screen).

Fix: `src/doc/gbk.rs` embeds a GBK double-byte table (126 lead × 190 trail = 23940 entries,
21791 of which are valid code points, a 47 KB `u16` array). The decode order is
**BOM → full UTF-8 validation → built-in GBK table → system iconv → Latin-1**: the first two
steps cover 99% of files, and the table catches the on-device case.

### 3.2 `-C target-cpu=cortex-a7` brings no gain on this machine

armv7 targets enable **no `target_feature`** by default (`rustc --print cfg` prints 0 lines),
so explicitly turning on NEON is a reasonable hypothesis. Measured (`--bench-ui` on the
device):

| Item | NEON off | NEON on |
|---|---|---|
| `open_at` (raster cache) | 49.48 ms | 49.34 ms |
| `redraw_page` (content unchanged) | 43.48 ms | 43.64 ms |
| 6 cards `blit_gray` | 7.19 ms | **7.74 ms (+7%)** |
| `canvas paint_all`, full screen | 30.83 ms | 30.91 ms |

The hot paths are all memory-bandwidth-bound (memcpy-like), not compute-bound; NEON on
Cortex-A7 is a 64-bit implementation with low throughput to begin with, and LLVM's
auto-vectorization generated worse code. **Conclusion: do not enable it**; a comment
explaining this is left in the config file.

The path that would genuinely benefit is JPEG/PNG decoding, but `zune-jpeg` only enables
NEON for `aarch64` (the first line of `idct/neon.rs` is `#![cfg(target_arch = "aarch64")]`),
so armv7 runs the scalar path anyway.

## 4. Annotating ebooks

Officially, PDF annotations are stored as "one `.rm` per page, with the page id recorded in
`cPages.pages` of `.content`". This structure is copied as-is:

- When a document is opened, `Library::ensure_pages(uuid, n)` pads `cPages.pages` up to the
  document's page count (**it only adds, never deletes** — the page count of a reflowed
  format changes with font size, and deleting the surplus pages would throw the annotations
  away with them);
- One `.rm` per page, using exactly the same ink codec, tools and undo stack as notebooks;
- Turning a page just switches to the corresponding page id in `cPages`.

On-device check: draw a stroke on page 1 of an EPUB → go to page 2 → go back to page 1, and
the ink is still there; `cPages` in `.content` is padded from 0 to 3 pages, and 2 `.rm` files
appear under the directory.

Changing the font size changes page content but **not page numbers** (`set_reading_layout`
does not go through `goto_page` — its first check is "target page == current page, return
immediately", and a font-size change is exactly the "same page number, entirely different
content" case). Also, the page raster cache (`page_fingerprint`) used to look only at ink, so
after a font-size change it would hit the stale image; the background version number `bg_gen`
is now mixed into the fingerprint as well.

## 6. On-device verification record (7 formats on one screen)

`shots/lib8.png`: MOBI (5 pages) / comic CBZ (10 pages) / FB2 (2 pages) / Markdown (1 page) /
Chinese GBK novel (1 page) / EPUB (3 pages) / PDF (Page 7 of 41) — all rendered the correct
cover and page count.

## 5. Reading settings

There are 5 font-size steps (`reflow::FONT_SIZES`); the entry point is the `Text size` row of
the **document settings panel** (the four-square icon in the left toolbar), one tap per step.
Changing the size reflows → page count changes → the current page is re-rendered, while the
page number stays put (adjusting the text size halfway through a book does not bounce you
back to the first page).

Line spacing/margins/font size are already wired into the reading UI (document settings →
`Text size` → `Text settings` sheet, persisted per document to
`~/.config/rmdev/readcfg.json`). `Font` / `Justification` are still greyed out.
