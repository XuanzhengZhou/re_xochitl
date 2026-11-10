#!/usr/bin/env python3
"""Build a BL call index for .text:  {target: [caller, ...]}

Linear sweep; ARM32 only (the whole binary is ARM, no Thumb interwork).
"""
import json
import os
import re
import sys

import capstone

from elfview import Elf

HERE = os.path.dirname(os.path.abspath(__file__))
CACHE = os.path.join(HERE, "calls.json")
LO, HI = 0x41638, 0x589028

RE_BL = re.compile(r"^bl\s+(?:#)?(0x[0-9a-f]+)$")


def build():
    e = Elf()
    text = e.read(LO, HI - LO)
    md = capstone.Cs(capstone.CS_ARCH_ARM, capstone.CS_MODE_ARM | capstone.CS_MODE_LITTLE_ENDIAN)
    md.skipdata = True
    calls = {}
    srcs = {}
    n = 0
    for ins in md.disasm(text, LO):
        n += 1
        if ins.mnemonic in ("bl", "blx"):
            m = RE_BL.match(f"{ins.mnemonic} {ins.op_str}")
            if m:
                t = int(m.group(1), 16)
                calls.setdefault(t, []).append(ins.address)
                srcs.setdefault(ins.address, t)
    json.dump({"calls": {f"{k:x}": v for k, v in calls.items()},
               "srcs": {f"{k:x}": v for k, v in srcs.items()}}, open(CACHE, "w"))
    print(f"{n} insns, {len(srcs)} bl sites, {len(calls)} distinct targets -> {CACHE}")


def load():
    d = json.load(open(CACHE))
    return ({int(k, 16): v for k, v in d["calls"].items()},
            {int(k, 16): v for k, v in d["srcs"].items()})


if __name__ == "__main__":
    if sys.argv[1:] and sys.argv[1] == "callers":
        calls, _ = load()
        t = int(sys.argv[2], 0)
        print("callers of %#x:" % t, [hex(c) for c in calls.get(t, [])])
    elif sys.argv[1:] and sys.argv[1] == "callees":
        _, srcs = load()
        lo, hi = int(sys.argv[2], 0), int(sys.argv[3], 0)
        seen = {}
        for s, t in sorted(srcs.items()):
            if lo <= s < hi:
                seen.setdefault(t, []).append(s)
        for t in sorted(seen):
            print(f"  -> {t:#08x}  from {[hex(x) for x in seen[t][:4]]}")
    else:
        build()
