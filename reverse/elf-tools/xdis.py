#!/usr/bin/env python3
"""Annotated linear disassembler for xochitl.

Reads a VA range, resolves PIC anchors to rodata, prints the string each
`ldr rX,[pc,#imm]` / `add rX,pc,rY` pair materialises, names `bl` targets from
stringrefs.tsv (single string) or a curated table, and can hide the ARM
atomic-refcount boilerplate so Qt code becomes readable.

  python dis.py 0x5563b0 0x5567d0
  python dis.py 0x552f74 0x5530e0 --quiet
"""
import os
import re
import sys

import capstone

from elfview import Elf

HERE = os.path.dirname(os.path.abspath(__file__))
RO = (0x589030, 0x74eb74)

NAMES = {
    0x552ec0: "swtconLogCategory()", 0x552f74: "EPFramebuffer::sendUpdate",
    0x55324c: "EFSwtcon::init/ctor", 0x553f04: "EFSwtcon::clearScreen?",
    0x5560b0: "EFSwtcon::copyToFb/threads?",
    0x5563b0: "EFSwtcon::loadCsvTable",
    0x55d3d4: "exportTableToCsv", 0x55d044: "??? 0x55d044",
    0x55f7dc: "parseCsvFile", 0x55aee4: "loadWaveforms()",
    0x55a2e0: "findWbfFile()", 0x5594e0: "readWaveformTable?",
    0x54ae3c: "?", 0x54fbfc: "operator delete",
    0x3b248: "QTextStream<<QString", 0x413c8: "QString::fromUtf8_helper",
    0x41518: "QString::fromAscii_helper", 0x3c598: "QMessageLogger::debug",
    0x3c7a8: "QMessageLogger::warning", 0x3df6c: "QDebug::~QDebug",
    0x3ca9c: "QArrayData::deallocate", 0x3cf34: "QImage::QImage(int,int,Fmt)",
    0x3dc30: "QImage::fill", 0x3eb9c: "QImage::bits",
    0x3c994: "strlen", 0x3e974: "printf", 0x404c8: "QRect::operator&",
}


def load_sym():
    d = {}
    p = os.path.join(HERE, "stringrefs.tsv")
    if os.path.exists(p):
        for line in open(p, errors="replace"):
            a, nm, s = line.rstrip("\n").split("\t", 2)
            d[int(a, 16)] = (nm, s)
    return d


SYM = load_sym()


def symname(t):
    if t in NAMES:
        return NAMES[t]
    if t in SYM:
        nm, s = SYM[t]
        first = s.split(" | ")[0][:40]
        return f"{nm} [{first}]"
    return f"FUN_{t:08x}"


def main():
    lo, hi = int(sys.argv[1], 0), int(sys.argv[2], 0)
    quiet = "--quiet" in sys.argv
    e = Elf()
    md = capstone.Cs(capstone.CS_ARCH_ARM, capstone.CS_MODE_ARM | capstone.CS_MODE_LITTLE_ENDIAN)
    md.skipdata = True
    regs = {}
    ALIAS = {"sb": "r9", "sl": "r10", "fp": "r11", "ip": "r12"}
    code = e.read(lo, hi - lo)
    for ins in md.disasm(code, lo):
        a, m, o = ins.address, ins.mnemonic, ins.op_str
        if quiet and m in ("dmb", "ldrex", "strex", "clrex", "dsb", "isb"):
            continue
        note = ""
        line = f"{m} {o}"
        mm = re.match(r"^(ldr|ldrb|ldrh)\s+(r\d+|sb|sl|fp|ip),\s*\[pc,\s*#(-?(?:0x)?[0-9a-f]+)\]$", line)
        if mm:
            base = (a + 8) & ~3
            off = int(mm.group(3), 0)
            tgt = base + off
            val = e.u32(tgt)
            reg = ALIAS.get(mm.group(2), mm.group(2))
            regs[reg] = ("lit", val)
            if RO[0] <= val < RO[1]:
                note = f"= {val:#x} (abs?) {e.cstr(val)[:60]!r}"
            else:
                note = f"= {val:#x}"
            print(f"{a:08x}  {line:40s} ; {note}")
            continue
        mm = re.match(r"^add\s+(r\d+|sb|sl|fp|ip),\s*pc,\s*(r\d+|sb|sl|fp|ip)$", line)
        if mm:
            d, s = ALIAS.get(mm.group(1), mm.group(1)), ALIAS.get(mm.group(2), mm.group(2))
            if s in regs:
                val = (regs[s][1] + a + 8) & 0xffffffff
                regs[d] = ("pcadd", val)
                if RO[0] <= val < RO[1]:
                    note = f"-> {val:#x} {e.cstr(val)[:70]!r}"
                else:
                    note = f"-> {val:#x}"
            print(f"{a:08x}  {line:40s} ; {note}")
            continue
        mm = re.match(r"^(add|sub)\s+(r\d+|sb|sl|fp|ip),\s*(r\d+|sb|sl|fp|ip),\s*#(-?(?:0x)?[0-9a-fA-F]+)$", line)
        if mm:
            d = ALIAS.get(mm.group(2), mm.group(2))
            s = ALIAS.get(mm.group(3), mm.group(3))
            imm = int(mm.group(4), 0)
            if mm.group(1) == "sub":
                imm = -imm
            if s in regs:
                val = (regs[s][1] + imm) & 0xffffffff
                regs[d] = ("add", val)
                if RO[0] <= val < RO[1]:
                    note = f"-> {val:#x} {e.cstr(val)[:70]!r}"
            print(f"{a:08x}  {line:40s} ; {note}")
            continue
        mm = re.match(r"^(mov|movw)\s+(r\d+|sb|sl|fp|ip),\s*#(-?(?:0x)?[0-9a-fA-F]+)$", line)
        if mm:
            regs[ALIAS.get(mm.group(2), mm.group(2))] = ("mov", int(mm.group(3), 0))
        mm = re.match(r"^movt\s+(r\d+|sb|sl|fp|ip),\s*#(-?(?:0x)?[0-9a-f]+)$", line)
        if mm:
            reg = ALIAS.get(mm.group(1), mm.group(1))
            old = regs.get(reg)
            regs[reg] = ("movt", (int(mm.group(2), 0) << 16) | (old[1] & 0xffff if old else 0))
        mm = re.match(r"^(bl|blx|b)\s+(0x[0-9a-f]+)$", line)
        if mm:
            t = int(mm.group(2), 16)
            if mm.group(1) != "b":
                note = symname(t)
                for r in ("r0", "r1", "r2", "r3", "r12", "lr"):
                    regs.pop(r, None)
        print(f"{a:08x}  {line:40s}" + (f" ; {note}" if note else ""))


if __name__ == "__main__":
    main()
