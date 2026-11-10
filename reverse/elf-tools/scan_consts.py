#!/usr/bin/env python3
"""Scan xochitl .text for materialised constants (PIC anchor style).

The compiler emits, for every rodata address:
    ldr  r2, [pc, #imm]     ; r2 = <offset from the add's pc>
    add  r2, pc, r2         ; r2 = target
i.e. the literal pool holds a *relative* offset, never the absolute VA.  That is
why grepping the file for LE32(0x73d3bc) finds nothing.

This tool does a linear sweep, keeps a tiny per-register constant map, and dumps
every (insn_addr, value, reg) triple to JSON so other tools can query
"who points at 0x73xxxx".

Usage:
    python scan_consts.py            # build cache tools/consts.json
    python scan_consts.py <lo> <hi>  # query cache for refs into [lo,hi]
"""
import json
import os
import re
import sys

import capstone

from elfview import Elf

HERE = os.path.dirname(os.path.abspath(__file__))
CACHE = os.path.join(HERE, "consts.json")

RE_LDR_PC = re.compile(r"^(ldr|ldrb|ldrh)\s+(r\d+|sb|sl|fp|ip|sp|lr),\s*\[pc,\s*#(-?0x[0-9a-f]+|-?\d+)\]")
RE_ADD_PC = re.compile(r"^add\s+(r\d+|sb|sl|fp|ip),\s*pc,\s*(r\d+|sb|sl|fp|ip)$")
RE_ADD_PCIMM = re.compile(r"^add\s+(r\d+|sb|sl|fp|ip),\s*pc,\s*#(-?0x[0-9a-f]+|\d+)$")
RE_ADD_IMM = re.compile(r"^add\s+(r\d+|sb|sl|fp|ip),\s*(r\d+|sb|sl|fp|ip),\s*#(-?0x[0-9a-f]+|\d+)$")
RE_SUB_IMM = re.compile(r"^sub\s+(r\d+|sb|sl|fp|ip),\s*(r\d+|sb|sl|fp|ip),\s*#(-?0x[0-9a-f]+|\d+)$")
RE_MOV_IMM = re.compile(r"^(mov|movw)\s+(r\d+|sb|sl|fp|ip),\s*#(-?0x[0-9a-f]+|\d+)$")
RE_MOVT = re.compile(r"^movt\s+(r\d+|sb|sl|fp|ip),\s*#(-?0x[0-9a-f]+|\d+)$")
RE_DST = re.compile(r"^[a-z][a-z0-9.]*\s+(r\d+|sb|sl|fp|ip)\b")
RE_ANYREG = re.compile(r"\b(r\d+|sb|sl|fp|ip)\b")

ALIAS = {"sb": "r9", "sl": "r10", "fp": "r11", "ip": "r12"}


def rn(r):
    return ALIAS.get(r, r)


def num(s):
    return int(s, 0)


def scan(elf, lo=0x41638, hi=0x58903c, verbose=True):
    text = elf.read(lo, hi - lo)
    md = capstone.Cs(capstone.CS_ARCH_ARM, capstone.CS_MODE_ARM | capstone.CS_MODE_LITTLE_ENDIAN)
    md.skipdata = True
    regs = {}
    out = []
    n = 0
    last_report = 0
    for ins in md.disasm(text, lo):
        a = ins.address
        m = ins.mnemonic
        ops = ins.op_str
        line = f"{m} {ops}".strip()
        n += 1
        if verbose and a - last_report > 0x100000:
            last_report = a
            print(f"  {a:#x}  insn={n}", file=sys.stderr)

        # --- materialise constants -------------------------------------
        mm = RE_LDR_PC.match(line)
        if mm:
            base = (a + 8) & ~3
            off = num(mm.group(3))
            tgt = base + off
            if 0 <= tgt < len(elf.data):
                val = elf.u32(tgt)
                regs[rn(mm.group(2))] = ("lit", val)
                if 0x10000 <= val <= 0x7e0000:
                    out.append([a, val, rn(mm.group(2)), "litabs"])
            else:
                regs.pop(rn(mm.group(2)), None)
            continue
        mm = RE_ADD_PC.match(line)
        if mm:
            d, s = rn(mm.group(1)), rn(mm.group(2))
            src = regs.get(s)
            if src:
                val = (src[1] + a + 8) & 0xffffffff
                regs[d] = ("pcadd", val)
                if 0x10000 <= val <= 0x7e0000:
                    out.append([a, val, s, "pcadd"])
            else:
                regs.pop(d, None)
            continue
        mm = RE_ADD_PCIMM.match(line)
        if mm:
            d = rn(mm.group(1))
            val = (a + 8 + num(mm.group(2))) & 0xffffffff
            regs[d] = ("pcadimm", val)
            if 0x10000 <= val <= 0x7e0000:
                out.append([a, val, d, "pcadimm"])
            continue
        mm = RE_ADD_IMM.match(line) or RE_SUB_IMM.match(line)
        if mm:
            d, s = rn(mm.group(1)), rn(mm.group(2))
            imm = num(mm.group(3))
            if m.startswith("sub"):
                imm = -imm
            src = regs.get(s)
            if src:
                val = (src[1] + imm) & 0xffffffff
                regs[d] = ("addimm", val)
                if 0x10000 <= val <= 0x7e0000:
                    out.append([a, val, s, "addimm"])
            else:
                regs.pop(d, None)
            continue
        mm = RE_MOV_IMM.match(line)
        if mm:
            d = rn(mm.group(2))
            regs[d] = ("mov", num(mm.group(3)))
            continue
        mm = RE_MOVT.match(line)
        if mm:
            d = rn(mm.group(1))
            old = regs.get(d)
            val = (num(mm.group(2)) << 16) | (old[1] & 0xffff if old else 0)
            regs[d] = ("movt", val)
            if 0x10000 <= val <= 0x7e0000:
                out.append([a, val, d, "movt"])
            continue

        # --- plain ldr Rt,[pc,#imm] whose literal is itself a VA ---------
        if mm is None and m in ("ldr", "ldrb") and "[pc," in ops:
            try:
                d = rn(ops.split(",")[0].strip())
                off = num(re.search(r"#(-?0x[0-9a-f]+|-?\d+)", ops).group(1))
                val = elf.u32((a + 8) & ~3 + 0) if False else elf.u32((a + 8) + off)
                if val is not None:
                    regs[d] = ("litvalue", val)
                    if 0x10000 <= val <= 0x7e0000:
                        out.append([a, val, d, "litabs"])
            except Exception:
                pass
            continue

        # --- anything else: invalidate written registers ----------------
        mm = RE_DST.match(line)
        if mm:
            regs.pop(rn(mm.group(1)), None)
        else:
            # e.g. push/pop/stm/ldm: be conservative but cheap
            pass
        if m in ("bl", "blx"):
            for r in ("r0", "r1", "r2", "r3", "r12", "lr"):
                regs.pop(r, None)
    if verbose:
        print(f"scanned {n} insns, {len(out)} constants", file=sys.stderr)
    return out


def main():
    e = Elf()
    if len(sys.argv) >= 2 and sys.argv[1] == "--query":
        db = json.load(open(CACHE))
        lo, hi = int(sys.argv[2], 0), int(sys.argv[3], 0)
        hits = [r for r in db if lo <= r[1] <= hi]
        hits.sort(key=lambda r: r[0])
        for a, v, reg, kind in hits:
            print(f"{a:#08x} -> {v:#08x}  ({reg},{kind})")
        print(f"# {len(hits)} hits in [{lo:#x},{hi:#x}]", file=sys.stderr)
        return
    out = scan(e)
    json.dump(out, open(CACHE, "w"))
    print(f"wrote {CACHE}: {len(out)} entries")


if __name__ == "__main__":
    main()
