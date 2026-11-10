#!/usr/bin/env python3
"""Recover exact function boundaries from .ARM.exidx.

Each entry is 8 bytes:  word0 = prel31 -> function start
                        word1 = prel31 -> EHABI unwind data, or 1 = CANTUNWIND
prel31(v, addr) = addr + sign_extend(v & 0x7fffffff, 31)
"""
import json
import os
import struct
import sys

from elfview import Elf, va2off

HERE = os.path.dirname(os.path.abspath(__file__))
CACHE = os.path.join(HERE, "funcs.json")
EXIDX = (0x7906fc, 0xf800)


def prel31(word, addr):
    v = word & 0x7fffffff
    if v & 0x40000000:
        v -= 0x80000000
    return (addr + v) & 0xffffffff


def build():
    e = Elf()
    va, size = EXIDX
    data = e.read(va, size)
    starts = []
    for i in range(0, size, 8):
        w0, w1 = struct.unpack_from("<II", data, i)
        if w0 == 0:
            continue
        starts.append(prel31(w0, va + i))
    starts = sorted(set(starts))
    funcs = []
    for a, b in zip(starts, starts[1:]):
        funcs.append([a, b - a])
    funcs.append([starts[-1], 0x589030 - starts[-1]])
    json.dump(funcs, open(CACHE, "w"))
    print(f"{len(funcs)} functions -> {CACHE}")
    return funcs


def load():
    if not os.path.exists(CACHE):
        return build()
    return json.load(open(CACHE))


def find(addr, funcs=None):
    funcs = funcs or load()
    lo, hi = 0, len(funcs) - 1
    while lo <= hi:
        mid = (lo + hi) // 2
        a, n = funcs[mid]
        if a <= addr < a + n:
            return a, n
        if addr < a:
            hi = mid - 1
        else:
            lo = mid + 1
    return None, None


if __name__ == "__main__":
    if sys.argv[1:] and sys.argv[1] == "find":
        for x in sys.argv[2:]:
            a, n = find(int(x, 0))
            print(f"{x}: inside FUN_{a:08x} (size {n:#x})")
    elif sys.argv[1:] and sys.argv[1] == "list":
        lo, hi = int(sys.argv[2], 0), int(sys.argv[3], 0)
        for a, n in load():
            if lo <= a < hi:
                print(f"FUN_{a:08x} size {n:#06x}")
    else:
        build()
