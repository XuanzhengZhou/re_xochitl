import struct
class Elf32:
    def __init__(self, path):
        self.path=path; self.d=open(path,'rb').read(); d=self.d
        assert d[:4]==b'\x7fELF'
        (self.e_entry, phoff, shoff)=struct.unpack_from("<III",d,0x18)
        (self.phentsize,self.phnum,self.shentsize,self.shnum,self.shstrndx)=struct.unpack_from("<HHHHH",d,0x2a)
        self.phdrs=[]
        for i in range(self.phnum):
            o=phoff+i*self.phentsize
            self.phdrs.append(struct.unpack_from("<IIIIIIII",d,o))
        self.secs=[]
        shstr=self.phdrs and None
        raw=[]
        for i in range(self.shnum):
            o=shoff+i*self.shentsize
            raw.append(struct.unpack_from("<IIIIIIIIII",d,o))
        st=raw[self.shstrndx][4]
        def nm(n):
            e=d.index(b'\0',st+n); return d[st+n:e].decode('latin1')
        for r in raw:
            self.secs.append(dict(name=nm(r[0]),typ=r[1],flags=r[2],addr=r[3],off=r[4],size=r[5],link=r[6],info=r[7],align=r[8],entsize=r[9]))
        self.by_name={s['name']:s for s in self.secs}
        # exec segments
        self.exec=[p for p in self.phdrs if (p[6]&1) and p[4]]
    def read(self, va, n):
        for (t,off,vaddr,paddr,filesz,memsz,flags,align) in self.phdrs:
            if t==1 and vaddr<=va<vaddr+filesz:
                k=min(n, vaddr+filesz-va)
                return self.d[off+(va-vaddr):off+(va-vaddr)+k]
        raise ValueError(hex(va))
    def va2off(self, va):
        for (t,off,vaddr,paddr,filesz,memsz,flags,align) in self.phdrs:
            if t==1 and vaddr<=va<vaddr+memsz:
                return off+(va-vaddr)
        return None
    def text_ranges(self):
        out=[]
        for s in self.secs:
            if s['flags']&4 and s['size']:
                out.append((s['addr'],s['addr']+s['size'],s['name']))
        return out
