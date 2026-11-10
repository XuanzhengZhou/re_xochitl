#!/usr/bin/env python3
"""Minimal ELF32-LE reader for the xochitl sample.

Facts baked in (verified with objdump):
  PT_LOAD #1: file off 0x0        -> vaddr 0x10000
  PT_LOAD #2: file off 0x7906a4   -> vaddr 0x7b06a4
So:  va = off + 0x10000            (off < 0x7906a4)
     va = off + 0x7b06a4 - 0x7906a4 = off + 0x20000  (off >= 0x7906a4)
"""
import os, struct

PATH = os.environ.get("RM2_ROOT", os.path.expanduser("~/re_xochitl")) + "/xochitl"


def va2off(va):
    if va < 0x7b06a4:
        return va - 0x10000
    return va - 0x20000


def off2va(off):
    if off < 0x7906a4:
        return off + 0x10000
    return off + 0x20000


class Elf:
    def __init__(self, path=PATH):
        self.path = path
        self.data = open(path, "rb").read()
        d = self.data
        assert d[:4] == b"\x7fELF" and d[4] == 1 and d[5] == 1
        (self.e_entry, phoff, shoff) = struct.unpack_from("<III", d, 0x18)
        (self.phentsize, self.phnum, self.shentsize, self.shnum, self.shstrndx) = \
            struct.unpack_from("<HHHHH", d, 0x2a)
        self.phdrs = []
        for i in range(self.phnum):
            o = phoff + i * self.phentsize
            p_type, p_offset, p_vaddr, p_paddr, p_filesz, p_memsz, p_flags, p_align = \
                struct.unpack_from("<IIIIIIII", d, o)
            self.phdrs.append(dict(type=p_type, off=p_offset, va=p_vaddr,
                                   filesz=p_filesz, memsz=p_memsz, flags=p_flags))
        self.shdrs = []
        if shoff:
            for i in range(self.shnum):
                o = shoff + i * self.shentsize
                name, typ, flags, addr, off, size, link, info, align, entsize = \
                    struct.unpack_from("<IIIIIIIIII", d, o)
                self.shdrs.append(dict(name_off=name, type=typ, flags=flags, va=addr,
                                       off=off, size=size, link=link, info=info,
                                       entsize=entsize))
            strtab = self.shdrs[self.shstrndx]
            base = strtab["off"]
            for s in self.shdrs:
                n = s["name_off"]
                end = d.index(b"\0", base + n)
                s["name"] = d[base + n:end].decode()
        self.by_name = {s.get("name"): s for s in self.shdrs}

    def read(self, va, n):
        return self.data[va2off(va): va2off(va) + n]

    def u32(self, va):
        return struct.unpack_from("<I", self.data, va2off(va))[0]

    def cstr(self, va, maxlen=256):
        o = va2off(va)
        e = self.data.index(b"\0", o)
        return self.data[o:min(e, o + maxlen)].decode("latin1")

    def dynsyms(self):
        sym = self.by_name.get(".dynsym")
        if not sym:
            return []
        strs = self.shdrs[sym["link"]]
        base = strs["off"]
        out = []
        for i in range(sym["size"] // 16):
            o = sym["off"] + i * 16
            name, value, size, info, other, shndx = struct.unpack_from("<IIIBBH", self.data, o)
            e = self.data.index(b"\0", base + name)
            nm = self.data[base + name:e].decode("latin1")
            out.append(dict(name=nm, value=value, size=size, info=info, shndx=shndx))
        return out


if __name__ == "__main__":
    e = Elf()
    print("entry", hex(e.e_entry))
    for p in e.phdrs:
        print("PH", p["type"], hex(p["off"]), hex(p["va"]), hex(p["filesz"]), p["flags"])
    for s in e.shdrs:
        print(f'{s.get("name"):24s} va {s["va"]:#010x} off {s["off"]:#010x} size {s["size"]:#x}')
