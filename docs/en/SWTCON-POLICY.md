# SWTCON-POLICY — xochitl's E-Ink Refresh Policy / Waveform-Table Export Switch

> English translation of [`../SWTCON-POLICY.md`](../SWTCON-POLICY.md) (Chinese original).

> Target sample: `xochitl` extracted from the device (ELF32 ARM ET_EXEC, 8,027,240 B).
> Every conclusion is stated with an address; anything without evidence is marked "undetermined".
> Re-run scripts are in `tools/`; see §6 at the end.

**Address convention**: every address in this note is a **vaddr** (link-time address).
`va = file_off + 0x10000` (file_off < 0x7906a4), `va = file_off + 0x20000` (≥ 0x7906a4).
Example: the string `/usr/share/remarkable/speedy-gonzales.csv` is at file_off `0x72d3bc` → vaddr **`0x73d3bc`**.

---

## 0. Conclusions at a Glance (TL;DR)

| # | Question | Conclusion | Confidence |
|---|---|---|---|
| Q1 | CSV export switch | **It does not exist. Nothing in this build ever writes `/usr/share/remarkable/speedy-gonzales.csv`.** That path is only ever **read** (`QFile::open(ReadOnly)`). Every occurrence of "export" means "feed a parsed CSV **into** the in-memory waveform table", not "write the in-memory table out as CSV". The only way to obtain the waveform table is to patch the binary and call `0x55ef48` (the stdout dumper) | **High** |
| Q2 | `WaveformMode` enum | Valid values are `{0,1,2,3,6,8,10}`; `{4,5,7,9}` are rejected as invalid and return immediately; `sendUpdate` holds a mapping table `→ internal wbf mode` | see §2 |
| Q3 | INIT table | `clearScreen` / power-on clear go through a separate entry; `Could not fetch init waveform` lives in `FUN_00553f04` | see §3 |
| — | **Side finding (useful for a replacement implementation)** | That CSV path is a **waveform-table override entry point**: put a valid CSV at that path on the device and xochitl parses it at startup and sets `this+0x3a=1` (skipping the wbf) | high, see §1.4 |

---

## 1. Q1 — the waveform-table CSV "export" switch: it does not exist (but there is an **import** entry point)

### 1.1 First, a correction to a premise: the string is not referenced by an absolute address because the code uses PIC anchor style

Searching the file for the LE32 form of the string's vaddr yields no hits. That is **normal**, not missing evidence.
Every rodata reference in this binary is "literal pool holds a relative offset + `add rX, pc, rX`":

```
 5532cc: e59f0358   ldr  r0, [pc, #0x358]     @ 0x55362c → 0x001ea0e0   (offset, not an address)
 5532d0: e3a01029   mov  r1, #0x29            (41 = strlen)
 5532d4: e08f0000   add  r0, pc, r0           → 0x5532dc + 0x1ea0e0 = 0x73d3bc
```

`0x73d3bc` = `"/usr/share/remarkable/speedy-gonzales.csv"` (41 bytes, `0x72d3bc`+0x10000).
A linear scanner (`tools/scan_consts.py`, `.text` 1,384,065 instructions → 48,878 constants) resolves every such
`ldr`+`add pc` pair back into a constant. **There is exactly one site in the whole binary** that constructs this
string address:

```
0x5532d4  -> 0x73d3bc   (/usr/share/remarkable/speedy-gonzales.csv)
```

It sits in `FUN_0055324c` (the `EPFramebufferSwtcon` initialization, see §1.2).

### 1.2 The only call chain

```
FUN_0055324c  (init/ctor, 0x3f0 B)          ← only caller 0x5496bc
  └─ 0x553300: bl FUN_005563b0(char* path, int 8)
        FUN_005563b0 (0x49c B)              ← only caller 0x553300
          ├─ 0x556404: bl FUN_0055f7dc(&tbl, QString(path), 1)   ← CSV parser
          ├─ 0x5567d8: bl 0x55d044(tbl, 6)   ← validation/statistics (hot section of FUN_0055cd24)
          ├─ 0x5567e8: bl FUN_0055d3d4(tbl, 6)  ← "export to table"
          └─ 0x556810: printf("Unable to export waveform table.")
```

`0x5496bc` is the construction point of `EPFramebufferSwtcon`; this chain **runs unconditionally once at boot**
(the `Unable to load waveform from file: /usr/share/remarkable/speedy-gonzales.csv` line in the device journal comes
from exactly this chain — the file does not exist, parsing fails, and the printf is reached via `0x556464`).

### 1.3 Decisive evidence: there is **no** file-writing code here

**(a) The open mode is ReadOnly.** The only `QFile::open` call in `FUN_0055f7dc`:

```
 55f810: bl   0x3d6b4     ; QFile::QFile(QString const&)
 55f818: mov  r1, #1      ; ← QIODevice::ReadOnly
 55f81c: bl   0x3c730     ; QFile::open(QFlags<QIODevice::OpenModeFlag>)
```

`QIODevice::ReadOnly = 0x0001`. The rest of the function contains no second `open`.

**(b) All three functions that "look like they export" write to stdout.**
Resolving `bl` targets to PLT names (`tools/calls.py` + `tools/plt.tsv`):

| Function | Size | External symbols it calls | Verdict |
|---|---|---|---|
| `FUN_0055ef48` (`0,%i,%i` / `15,%i,%i` / `%i,0,%i` / `%i,15,%i`) | 0x380 | `memset`, **`printf` ×11** | CSV text goes to stdout; no `fopen`/`QFile::open` |
| `FUN_0055a148` (`Printing mode %d, temp %d:` / `%x->%x: ` / ANSI color) | 0x198 | `printf` ×6, `puts` | same as above |
| `FUN_0055e464` | 0x6c4 | `printf` ×5, `puts`, `QString::operator=` | same as above |

**(c) `"Unable to export to table: some waveforms are not valid"` means "import", not "export".**
It lives in `FUN_0055d3d4` (0x8a4 B), and the other two strings in the same family are
`"Exporting to table without appending zero!"` (`0x73d9f0`, ref `0x55dabc`).
"export **to table**" = write CSV entries into the internal waveform table; "without appending zero" = the internal
table requires a trailing 0 terminator.
The caller's line `"Unable to export waveform table."` (ref `0x5534f8`, `QMessageLogger::warning`) is the
externally visible wording for **the same failure**. Both messages mean "CSV → internal table" failed,
not "internal table → file" failed.

**(d) The three dumpers have no static cross-references at all.** `FUN_0055ef48` / `FUN_0055a148` / `FUN_0055e464`:
- no `bl`/`blx` targets them (all 130,260 `bl` sites were scanned);
- no `b`/`bcc` targets them (every conditional/unconditional branch in `.text` was scanned);
- their LE32 addresses do not appear anywhere in the file (.data.rel.ro / .got / .data were all searched);
- that region has no `add pc, pc, rX, lsl #2` or `ldr pc` jump table either.

In other words: they were compiled in, but the call sites were compiled out.
**This means there is no "runtime switch"**: command-line arguments, environment variables, Qt logging categories
and config options are all dead ends — there is nothing to turn it on with.

### 1.4 But: this CSV path is a live **waveform-table override entry point** (useful for the replacement)

Before calling `FUN_005563b0`, `FUN_0055324c` sets `this+0x3a = 0`, and sets it to 1 when the call succeeds:

```
 553278: strb r1, [r4, #0x3a]     ; r1 = 0, clear the flag first
 ...
 553300: bl   FUN_005563b0        ; parse the CSV (returns 0 on success)
 553358: cmp  r6, #0
 55335c: moveq r3, #1
 553360: strbeq r3, [r4, #0x3a]   ; ← CSV loaded → this[0x3a] = 1 (override flag)
 553364: bne  0x5534b4            ; failure → qWarning("Unable to export waveform table.") then return
```

That is: **put a valid CSV at `/usr/share/remarkable/speedy-gonzales.csv`, `this+0x3a` gets set to 1, and the
official implementation uses the waveforms from the CSV instead of the `.wbf`.**
The CSV row format can be reconstructed from the format strings in `FUN_0055ef48` (see §1.5); the parser is
`FUN_0055f7dc` (error strings `Corrupt data at line` / `Corrupt current value at line` /
`Corrupt target value at line` / `Corrupt waveform value at line`).

> Note: `/usr/share` lives on the rootfs, which is mounted read-only; to use this hook you have to
> `mount -o remount,rw /` first.
> This one **needs no binary patching** — it is a backdoor-grade debug entry point the official build already
> ships, far less work than reverse engineering the wbf.

### 1.5 CSV format (derived from the format strings, not yet verified on hardware)

The format strings of `FUN_0055ef48`:

```
"0,%i,%i\n"     "15,%i,%i\n"     "%i,0,%i\n"     "%i,15,%i\n"     ","  "%4x" "%4d"
```

This matches the classic 16×16 layout of an E Ink waveform table: row = current gray (0..15), column = target
gray (0..15), and each cell is a run of frame drive codes (4 bit/frame). `0,` / `15,` / `,0` / `,15` are
specialized spellings for the four boundary rows/columns (brackets and quotes omitted). `^0+` / `0+$`
(`0x73d9b0` / `0x73d9b4`, ref `0x55c30c` / `0x55c3f8`) are `QRegularExpression`s used to strip leading/trailing
zeros.

> Undetermined: the exact separator between frame codes within one row (`%4x` run together, or comma-separated)
> needs one real dump before it can be pinned down.

### 1.6 Hypotheses already ruled out

| Hypothesis | Evidence against it |
|---|---|
| Command-line arguments | the only point in `.text` that constructs the CSV path is init (0x5532d4); no `argv` comparison chain reaches it |
| Environment variables | same as above; also `0x5563b0` takes only one `char*` plus the int constant 8 |
| Qt logging categories | `rm.framebuffer.swtcon` (inside `FUN_00552ec0`) is only used for `qCDebug` and does not affect control flow |
| Config options | no `QSettings`/conf read around `0x5534b4`; the `bne` only tests the CSV parse return value |
| Only present in debug builds | **No.** The code and strings of all three dumpers are in the release binary; nothing calls them |

**Practical recommendation (the usable conclusion for Q1)**:
Abandon the "find the export switch" approach. Either (a) call `FUN_0055ef48` as a function directly (patch a `bl`
into init — `LD_PRELOAD` will not work, it is in the static symbols, so the ELF has to be modified), or (b) use the
CSV **import** entry point from §1.4, or (c) write a `.wbf` parser.
**Reason: the export feature has no switch — it does not exist.**

---

## 6. Scripts
