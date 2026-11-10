#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""
wbf.py -- E Ink ".wbf" waveform file parser (i.MX EPDC / reMarkable 2 flavour).

Parses the complete waveform library shipped on reMarkable 2 under
/usr/share/remarkable/*.wbf and dumps every mode x temperature LUT to JSON.

--------------------------------------------------------------- format summary
Header (48 B, packed, little endian) is the classic E Ink / i.MX EPDC
"waveform_data_header"; it is followed by

  0x30 : temperature range table  trc+1 bounds + 1 checksum byte
  xwia : xwi block: 1 length byte, len chars, 1 checksum byte   (xwia==0 -> absent)
  wmta : mode table: (mc+1) x {u24 address, u8 checksum}
  ...  : one temperature table per mode, (trc+1) x {u24 address, u8 checksum}
  ...  : waveform tables: RLE-compressed phase data, terminated by 0xFF,
         followed by one checksum byte.

Waveform table RLE (reverse engineered from the on-disk data, matches the
public `wbf_waveform_dump` tool and the Rockchip EBC driver):

    rle_mode = True
    loop:
        c = next byte
        if c == 0xFC: rle_mode = not rle_mode
        elif c == 0xFF: break
        elif not rle_mode: emit c
        else: n = next byte; emit c, (n+1) times
    checksum = sum of every byte consumed above, incl. the 0xFF, & 0xFF

The expanded stream is a sequence of *phases*; each phase holds
`states*states` 2-bit codes, packed 4 codes per byte, so

    bytes_per_phase = states*states / codes_per_byte
    phases          = len(expanded) / bytes_per_phase

`states` is 16 for a 4-bit LUT and 32 for a 5-bit LUT (header field `luts`),
except for "unidirectional" LUTs (luts==0x1D) which carry only `states` cells
per phase.  Byte order inside a phase: dst-major, src-minor, LSB-first inside
each byte (see `phase_grid()`; the bit order is flagged UNCONFIRMED in WBF.md).

This parser was validated against the sample
    320_R259_AFAB21_ED103TC2M1_TC.wbf
- all 112 waveform-table checksums match,
- the temperature-table and xwi checksums match,
- the global CRC32 in the header matches,
- the GC16 ladder comes out as 131,116,102,90,77,66,55,46,38 x6,
  which is exactly the ladder compiled into the zplash U-Boot blob,
- the INIT ladder comes out as 149,133,117,103,89,159,135,111,99,87,79,71,63,59,
  which is exactly zplash's other per-band count ladder.

Usage:
    python3 wbf.py FILE.wbf [-o out.json] [--grid] [--no-lut] [--summary] [--quiet]
"""

from __future__ import annotations

import argparse
import json
import os
import struct
import sys
import zlib

HEADER_SIZE = 48

# 48-byte packed header: (name, offset, size)
HEADER_FIELDS = [
    ("checksum",                 0x00, 4),   # CRC32 over file[4:file_length]
    ("file_length",              0x04, 4),
    ("serial_number",            0x08, 4),
    ("run_type",                 0x0C, 1),
    ("fpl_platform",             0x0D, 1),
    ("fpl_lot",                  0x0E, 2),
    ("mode_version",             0x10, 1),
    ("wf_version",               0x11, 1),
    ("wf_subversion",            0x12, 1),
    ("wf_type",                  0x13, 1),
    ("fpl_size",                 0x14, 1),
    ("mfg_code",                 0x15, 1),
    ("wfm_rev",                  0x16, 1),
    ("frame_rate",               0x17, 1),
    ("unknown0",                 0x18, 1),
    ("vcom_offset",              0x19, 1),
    ("unknown1",                 0x1A, 2),
    ("xwia",                     0x1C, 3),   # offset of the xwi block
    ("cs1",                      0x1F, 1),
    ("wmta",                     0x20, 3),   # offset of the mode table
    ("fvsn",                     0x23, 1),
    ("luts",                     0x24, 1),   # LUT flavour, see LUT_KINDS
    ("mc",                       0x25, 1),   # mode count - 1
    ("trc",                      0x26, 1),   # temperature range count - 1
    ("awv",                      0x27, 1),   # "advanced waveform flags"
    ("eb",                       0x28, 1),
    ("sb",                       0x29, 1),
    ("reserved0_1",              0x2A, 1),
    ("reserved0_2",              0x2B, 1),
    ("reserved0_3",              0x2C, 1),
    ("reserved0_4",              0x2D, 1),
    ("reserved0_5",              0x2E, 1),
    ("cs2",                      0x2F, 1),
]

# header->luts -> (states, codes_per_byte, unidirectional, note)
LUT_KINDS = {
    0x00: (16, 4, False, "4-bit LUT, bidirectional, 16x16 transitions"),
    0x04: (32, 4, False, "5-bit LUT, bidirectional, 32x32 transitions"),
    0x05: (32, 2, False, "5-bit multi-voltage LUT, 32x32, 2 codes/byte"),
    0x15: (32, 2, False, "5-bit multi-voltage LUT (v2), 32x32, 2 codes/byte"),
    0x1D: (32, 2, True,  "5-bit multi-voltage unidirectional LUT, 32 targets"),
}

# mode_version -> mode names.  Taken from the public `wbf_waveform_dump` tool,
# which calls this "partially derived from drm_epd_helper.c".
MODE_NAMES = {
    0x01: ["INIT", "DU", "GL8", "GC8"],
    0x03: ["INIT", "DU", "GL16", "GL16", "A2"],
    0x09: ["INIT", "DU", "GC16", "A2"],
    0x12: ["INIT", "DU", "NULL", "GC16", "A2", "GL16", "GL16", "DU4"],
    0x15: ["INIT", "DU", "GC16", "GL16", "A2", "DU4", "GC4"],
    0x16: ["INIT", "DU", "GC16", "GL16", "GL16", "GC16", "A2"],
    0x18: ["INIT", "DU", "GC16", "GL16", "GL16", "GL16", "A2"],
    0x20: ["INIT", "DU", "GC16", "GL16", "GL16", "GL16", "A2"],
    0x19: ["INIT", "DU", "GC16", "GL16", "GL16", "GL16", "A2", "DU4"],
    0x43: ["INIT", "DU", "GC16", "GL16", "GL16", "GL16", "A2", "DU4"],
    0x23: ["INIT", "DU", "GC16", "GL16", "A2", "DU4"],
    0x54: ["INIT", "DU", "GC16", "GL16", "GL16", "A2"],
    0x48: ["INIT", "DU", "GC16", "GL16", "GL16", "NULL", "A2"],
    0x83: ["INIT", "DU", "GC16", "GL16", "A2", "GL16", "GL16"],
}


class WbfError(Exception):
    pass


def _u24(buf, off):
    if off < 0 or off + 3 > len(buf):
        raise WbfError("pointer read out of bounds at 0x%X" % off)
    return buf[off] | (buf[off + 1] << 8) | (buf[off + 2] << 16)


def read_pointer(buf, off):
    """Read a {u24 address, u8 checksum} table entry."""
    addr = _u24(buf, off)
    if off + 3 >= len(buf):
        raise WbfError("pointer checksum read out of bounds at 0x%X" % off)
    cs = buf[off + 3]
    ok = ((buf[off] + buf[off + 1] + buf[off + 2]) & 0xFF) == cs
    return addr, cs, ok


def parse_header(buf):
    if len(buf) < HEADER_SIZE:
        raise WbfError("file smaller than the 48 byte header (%d B)" % len(buf))
    hdr = {}
    for name, off, size in HEADER_FIELDS:
        if size == 1:
            hdr[name] = buf[off]
        elif size == 2:
            hdr[name] = struct.unpack_from("<H", buf, off)[0]
        elif size == 3:
            hdr[name] = _u24(buf, off)
        else:
            hdr[name] = struct.unpack_from("<I", buf, off)[0]
    hdr["_field_layout"] = [
        {"name": n, "offset": o, "size": s} for n, o, s in HEADER_FIELDS
    ]
    return hdr


def lut_geometry(luts):
    states, cpb, uni, note = LUT_KINDS.get(
        luts, (16, 4, False, "UNKNOWN luts value 0x%02X, assuming 4-bit" % luts)
    )
    transitions = states if uni else states * states
    bytes_per_phase = transitions // cpb
    return {
        "states": states,
        "codes_per_byte": cpb,
        "unidirectional": uni,
        "transitions": transitions,
        "bytes_per_phase": bytes_per_phase,
        "note": note,
        "known": luts in LUT_KINDS,
    }


def decode_waveform_table(buf, addr, max_bytes=64 << 20, max_phases=4096):
    """
    RLE-expand one waveform table starting at `addr`.

    Returns dict: data (bytes), checksum_ok, stored_checksum, end (offset of the
    byte after the checksum), consumed (raw bytes touched), error.
    """
    out = bytearray()
    cs = 0
    p = addr
    n = len(buf)
    rle = True
    while True:
        if p >= n:
            return {
                "data": bytes(out),
                "checksum_ok": False,
                "stored_checksum": None,
                "end": p,
                "consumed": p - addr,
                "error": "ran off the end of the file (no 0xFF terminator)",
            }
        c = buf[p]
        p += 1
        cs = (cs + c) & 0xFF
        if c == 0xFC:
            rle = not rle
        elif c == 0xFF:
            stored = buf[p] if p < n else None
            return {
                "data": bytes(out),
                "checksum_ok": stored == cs,
                "stored_checksum": stored,
                "end": p + 1,
                "consumed": p + 1 - addr,
                "error": None,
            }
        elif not rle:
            out.append(c)
        else:
            if p >= n:
                continue
            ln = buf[p]
            p += 1
            cs = (cs + ln) & 0xFF
            out.extend(bytes((c,)) * (ln + 1))
        if len(out) > max_bytes:
            return {
                "data": bytes(out),
                "checksum_ok": False,
                "stored_checksum": None,
                "end": p,
                "consumed": p - addr,
                "error": "decoded stream exceeded %d bytes, aborting" % max_bytes,
            }


def phase_grid(phase_bytes, states, codes_per_byte, msb_first=False):
    """
    Unpack one phase into grid[src][dst] (2-bit codes).

    Layout assumed: dst-major / src-minor, codes packed LSB-first inside a byte.
    """
    grid = [[0] * states for _ in range(states)]
    p = 0
    for dst in range(states):
        for src in range(0, states, codes_per_byte):
            if p >= len(phase_bytes):
                return grid
            v = phase_bytes[p]
            p += 1
            for k in range(codes_per_byte):
                code = (v >> ((codes_per_byte - 1 - k) * 2)) & 3 if msb_first \
                    else (v >> (2 * k)) & 3
                s = src + k
                if s < states:
                    grid[s][dst] = code
    return grid


class Wbf:
    def __init__(self, path, grid=False, want_lut=True):
        self.path = path
        with open(path, "rb") as f:
            self.buf = f.read()
        self.grid = grid
        self.want_lut = want_lut
        self.warnings = []
        self.errors = []

        self.header = parse_header(self.buf)
        h = self.header
        self.file_size = len(self.buf)

        # ---- sanity -------------------------------------------------------
        if h["file_length"] != self.file_size:
            self.warnings.append(
                "header file_length %d != actual size %d"
                % (h["file_length"], self.file_size)
            )
        self.declared_crc_ok = None
        if 0 < h["file_length"] <= self.file_size:
            crc = zlib.crc32(self.buf[4:h["file_length"]], zlib.crc32(b"\0" * 4))
            self.declared_crc_ok = crc == h["checksum"]

        self.geom = lut_geometry(h["luts"])
        self.mode_count = h["mc"] + 1
        self.trt_entries = h["trc"] + 1

        # ---- temperature range table -------------------------------------
        t0 = HEADER_SIZE
        tend = t0 + self.trt_entries + 2
        if tend > self.file_size:
            raise WbfError("temperature range table runs past EOF")
        self.temp_bounds = list(self.buf[t0:t0 + self.trt_entries + 1])
        self.temp_cs = self.buf[tend - 1]
        self.temp_cs_ok = (sum(self.temp_bounds) & 0xFF) == self.temp_cs
        self.temp_table_end = tend

        # ---- xwi block ----------------------------------------------------
        self.xwi = None
        self.xwi_cs_ok = None
        if h["xwia"]:
            o = h["xwia"]
            if o < self.file_size:
                ln = self.buf[o]
                raw = self.buf[o + 1:o + 1 + ln]
                csb = self.buf[o + 1 + ln] if o + 1 + ln < self.file_size else None
                self.xwi = {
                    "offset": o,
                    "size": ln,
                    "string": raw.decode("latin-1"),
                    "checksum": csb,
                    "checksum_ok": csb == ((ln + sum(raw)) & 0xFF),
                }
                self.xwi_cs_ok = self.xwi["checksum_ok"]
                if not self.xwi_cs_ok:
                    self.warnings.append("xwi checksum mismatch")

        # ---- where does the mode table start? -----------------------------
        computed = HEADER_SIZE + (self.trt_entries + 2)
        if self.xwi:
            computed += 1 + self.xwi["size"] + 1
        self.mode_table_off = h["wmta"] if h["wmta"] else computed
        if h["wmta"] and h["wmta"] != computed:
            self.warnings.append(
                "wmta 0x%X != computed mode table offset 0x%X"
                % (h["wmta"], computed)
            )
        if not (0 < self.mode_table_off
                and self.mode_table_off + 4 * self.mode_count <= self.file_size):
            self.warnings.append(
                "mode table offset 0x%X is unusable, falling back to 0x%X"
                % (self.mode_table_off, computed)
            )
            self.mode_table_off = computed
        if self.mode_count > 64 or self.trt_entries > 64:
            raise WbfError(
                "implausible header: mc+1=%d, trc+1=%d"
                % (self.mode_count, self.trt_entries)
            )

        # ---- mode table ---------------------------------------------------
        self.mode_addrs = []
        for i in range(self.mode_count):
            off = self.mode_table_off + 4 * i
            addr, cs, ok = read_pointer(self.buf, off)
            if not ok:
                self.warnings.append("mode pointer %d checksum mismatch" % i)
            if addr >= self.file_size:
                self.errors.append(
                    "mode %d points past EOF (0x%X)" % (i, addr)
                )
            self.mode_addrs.append(addr)

        # ---- per mode temperature table -----------------------------------
        self.mode_tables = []
        self.extra_tables = []
        for i, base in enumerate(self.mode_addrs):
            row = []
            for j in range(self.trt_entries):
                off = base + 4 * j
                addr, cs, ok = read_pointer(self.buf, off)
                if not ok:
                    self.warnings.append(
                        "temp pointer m%d/t%d checksum mismatch" % (i, j)
                    )
                row.append(addr)
            self.mode_tables.append(row)

            # Some files carry a second bank of trt_entries pointers right
            # after the first one (still inside the 0x70 byte mode block).
            # Its meaning is not known; report it, do not interpret it.
            extra = []
            for j in range(self.trt_entries):
                off = base + 4 * (self.trt_entries + j)
                if off + 4 > self.file_size:
                    break
                try:
                    addr, cs, ok = read_pointer(self.buf, off)
                except WbfError:
                    break
                extra.append(addr if ok else None)
            while extra and extra[-1] in (None, 0):
                extra.pop()
            self.extra_tables.append(extra)

        # ---- decode every waveform table ----------------------------------
        self.table_ids = {}          # addr -> table id
        self.tables = []             # [ {addr, data, phases, ...} ]
        self.modes = []
        names = MODE_NAMES.get(h["mode_version"])
        for i in range(self.mode_count):
            temps = []
            for j in range(self.trt_entries):
                addr = self.mode_tables[i][j]
                tid = self.table_ids.get(addr)
                if tid is None:
                    tid = len(self.tables)
                    self.table_ids[addr] = tid
                    self.tables.append(
                        self._decode_table(addr, len(self.tables))
                    )
                tab = self.tables[tid]
                lo = self.temp_bounds[j]
                nxt = self.temp_bounds[j + 1] if j + 1 < len(self.temp_bounds) else None
                entry = {
                    "index": j,
                    "range": [lo, (nxt - 1) if nxt is not None else None],
                    "bound": lo,
                    "next_bound": nxt,
                    "table_id": tid,
                    "table_addr": addr,
                    "phases": tab["phases"],
                    "expanded_bytes": len(tab["data"]),
                    "checksum_ok": tab["checksum_ok"],
                }
                if tab["error"]:
                    entry["error"] = tab["error"]
                if self.want_lut and tab["data"]:
                    entry["lut"] = self._lut_json(tab)
                temps.append(entry)
            self.modes.append(
                {
                    "index": i,
                    "name": names[i] if names and i < len(names) else None,
                    "temp_table_addr": self.mode_addrs[i],
                    "extra_bank": self.extra_tables[i],
                    "temps": temps,
                }
            )

    # ------------------------------------------------------------------ utils
    def _decode_table(self, addr, tid):
        tab = {"id": tid, "addr": addr}
        if addr >= self.file_size:
            tab.update(
                data=b"", phases=0, checksum_ok=False, error="address past EOF"
            )
            self.errors.append("table %d address 0x%X past EOF" % (tid, addr))
            return tab
        res = decode_waveform_table(self.buf, addr)
        bpp = self.geom["bytes_per_phase"]
        phases = len(res["data"]) // bpp if bpp else 0
        if bpp and len(res["data"]) % bpp:
            self.warnings.append(
                "table %d @0x%X: expanded size %d is not a multiple of %d "
                "(phases truncated)" % (tid, addr, len(res["data"]), bpp)
            )
        tab.update(
            data=res["data"],
            phases=phases,
            checksum_ok=res["checksum_ok"],
            stored_checksum=res["stored_checksum"],
            error=res["error"],
            raw_bytes=res["consumed"],
        )
        if not res["checksum_ok"]:
            self.warnings.append("table %d @0x%X checksum mismatch" % (tid, addr))
        return tab

    def _lut_json(self, tab):
        bpp = self.geom["bytes_per_phase"]
        states = self.geom["states"]
        cpb = self.geom["codes_per_byte"]
        data = tab["data"]
        out = []
        for p in range(tab["phases"]):
            phase = data[p * bpp:(p + 1) * bpp]
            if self.grid:
                out.append(phase_grid(phase, states, cpb))
            else:
                out.append(phase.hex())
        return out

    # ------------------------------------------------------------------- dump
    def to_json(self):
        h = dict(self.header)
        h["_file_size"] = self.file_size
        h["_declared_crc32_ok"] = self.declared_crc_ok
        return {
            "file": os.path.basename(self.path),
            "file_size": self.file_size,
            "header": h,
            "lut_geometry": self.geom,
            "temperature_bounds": self.temp_bounds,
            "temperature_table_checksum": {
                "stored": self.temp_cs,
                "computed": sum(self.temp_bounds) & 0xFF,
                "ok": self.temp_cs_ok,
            },
            "xwi": self.xwi,
            "mode_table_offset": self.mode_table_off,
            "table_count": len(self.tables),
            "modes": self.modes,
            "warnings": self.warnings,
            "errors": self.errors,
        }

    def summary(self):
        h = self.header
        geo = self.geom
        lines = []
        a = lines.append
        a("file            : %s (%d B)" % (self.path, self.file_size))
        a("header checksum : 0x%08X  crc32 ok=%s"
          % (h["checksum"], self.declared_crc_ok))
        a("serial_number   : %d" % h["serial_number"])
        a("fpl_lot         : 0x%04X (=%d)" % (h["fpl_lot"], h["fpl_lot"]))
        a("mode_version    : 0x%02X   wf_version 0x%02X sub 0x%02X type 0x%02X"
          % (h["mode_version"], h["wf_version"], h["wf_subversion"], h["wf_type"]))
        a("frame_rate      : 0x%02X (%d)" % (h["frame_rate"], h["frame_rate"]))
        a("luts            : 0x%02X -> %s" % (h["luts"], geo["note"]))
        a("states          : %d x %d, %d codes/byte, %d B per phase"
          % (geo["states"], geo["states"], geo["codes_per_byte"],
             geo["bytes_per_phase"]))
        a("modes (mc+1)    : %d" % self.mode_count)
        a("temp ranges     : %d  bounds=%s"
          % (self.trt_entries, self.temp_bounds))
        a("mode table      : 0x%X" % self.mode_table_off)
        if self.xwi:
            a("xwi             : %r (%d chars, cs ok=%s)"
              % (self.xwi["string"], self.xwi["size"], self.xwi["checksum_ok"]))
        a("distinct tables : %d" % len(self.tables))
        a("")
        a("%-4s %-6s %5s %9s %7s %9s  %s"
          % ("mode", "name", "temp", "addr", "phases", "bytes", "checksum"))
        for m in self.modes:
            for t in m["temps"]:
                a("%-4d %-6s %5d %9s %7d %9d  %s"
                  % (m["index"], m["name"] or "?",
                     t["index"], hex(t["table_addr"]), t["phases"],
                     t["expanded_bytes"], "ok" if t["checksum_ok"] else "BAD"))
        if self.warnings:
            a("")
            a("warnings:")
            for w in self.warnings:
                a("  - " + w)
        if self.errors:
            a("")
            a("errors:")
            for e in self.errors:
                a("  - " + e)
        return "\n".join(lines)


def self_check(wbf):
    """
    Compare the parsed ladders against the two ladders compiled into the
    zplash U-Boot waveform blob (independent ground truth).
    """
    GC16 = [131, 116, 102, 90, 77, 66, 55, 46, 38, 38, 38, 38, 38, 38]
    INIT = [149, 133, 117, 103, 89, 159, 135, 111, 99, 87, 79, 71, 63, 59]
    res = {"gc16_ladder": GC16, "init_ladder": INIT, "matches": []}
    for m in wbf.modes:
        phases = [t["phases"] for t in m["temps"]]
        if phases == GC16:
            res["matches"].append(
                {"mode": m["index"], "name": m["name"], "ladder": "zplash phase_count (GC16)"}
            )
        if phases == INIT:
            res["matches"].append(
                {"mode": m["index"], "name": m["name"], "ladder": "zplash byte_count (INIT)"}
            )
    return res


def main(argv=None):
    ap = argparse.ArgumentParser(description="Parse E Ink .wbf waveform files")
    ap.add_argument("wbf", nargs="+")
    ap.add_argument("-o", "--out", help="output JSON (default: <input>.json)")
    ap.add_argument("--grid", action="store_true",
                    help="emit each phase as a states x states int grid "
                         "(large) instead of a hex string")
    ap.add_argument("--no-lut", action="store_true", help="skip LUT payloads")
    ap.add_argument("--summary", action="store_true",
                    help="print a human readable summary to stdout")
    ap.add_argument("--check", action="store_true",
                    help="verify the zplash ladder self-check")
    ap.add_argument("--quiet", action="store_true")
    args = ap.parse_args(argv)

    rc = 0
    for path in args.wbf:
        try:
            w = Wbf(path, grid=args.grid, want_lut=not args.no_lut)
        except Exception as exc:                       # never crash on a file
            print("FAILED %s: %s" % (path, exc), file=sys.stderr)
            rc = 1
            continue
        doc = w.to_json()
        if args.check:
            doc["self_check"] = self_check(w)
        out = args.out or (path + ".json")
        if len(args.wbf) > 1 and args.out:
            base, ext = os.path.splitext(args.out)
            out = "%s_%s%s" % (base, os.path.splitext(os.path.basename(path))[0], ext)
        with open(out, "w") as f:
            json.dump(doc, f)
        if not args.quiet:
            print("%s -> %s  (%d modes, %d temps/mode, %d tables, "
                  "phases %s)"
                  % (os.path.basename(path), out, w.mode_count,
                     w.trt_entries, len(w.tables),
                     [m["temps"][0]["phases"] for m in w.modes]))
        if args.summary:
            print(w.summary())
        if args.check:
            sc = doc["self_check"]
            if sc["matches"]:
                for hit in sc["matches"]:
                    print("self-check OK: mode %d (%s) phases == %s"
                          % (hit["mode"], hit["name"], hit["ladder"]))
            else:
                print("self-check: no ladder matched (sample is not an "
                      "rM2/ED103TC2 wbf?)")
    return rc


if __name__ == "__main__":
    sys.exit(main())
