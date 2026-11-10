import struct, sys

def be32(b,o): return struct.unpack('>I', b[o:o+4])[0]

class FDT:
    def __init__(self, data):
        self.d = data
        magic, totalsize, off_struct, off_strings, off_rsv, ver, lastcomp, bootcpu, size_strings, size_struct = struct.unpack('>10I', data[:40])
        assert magic == 0xd00dfeed, hex(magic)
        self.strings = data[off_strings:off_strings+size_strings]
        self.struct = data[off_struct:off_struct+size_struct]
        self.ver = ver
    def name(self, off):
        e = self.strings.index(b'\0', off)
        return self.strings[off:e].decode('utf-8','replace')
    def nodes(self):
        p = self.struct; i = 0; path = []
        while i < len(p):
            tok = be32(p,i); i += 4
            if tok == 1:  # BEGIN_NODE
                e = p.index(b'\0', i); nm = p[i:e].decode('utf-8','replace'); i = (e+1+3)&~3
                path.append(nm)
                yield ('BEGIN', '/'.join(path), None)
            elif tok == 2:  # END_NODE
                yield ('END', '/'.join(path), None)
                path.pop()
            elif tok == 3:  # PROP
                ln, noff = struct.unpack('>II', p[i:i+8]); i += 8
                val = p[i:i+ln]; i = (i+ln+3)&~3
                yield ('PROP', '/'.join(path), (self.name(noff), val))
            elif tok == 4:  # NOP
                pass
            elif tok == 9:
                break
            else:
                raise SystemExit('bad tok %x at %d' % (tok, i-4))

def fmt(val):
    if len(val) == 0: return 'true'
    try:
        s = val.rstrip(b'\0').decode('ascii')
        if all(32 <= ord(c) < 127 for c in s) and len(s) > 1:
            return '"%s"' % s
    except Exception: pass
    if len(val) % 4 == 0:
        return '<' + ' '.join('%08x' % x for x in struct.unpack('>%dI' % (len(val)//4), val)) + '>'
    return '[' + ' '.join('%02x' % b for b in val) + ']'

if __name__ == '__main__':
    data = open(sys.argv[1],'rb').read()
    f = FDT(data)
    want = sys.argv[2] if len(sys.argv) > 2 else None
    show = want is None
    depth = 0
    for kind, path, prop in f.nodes():
        if kind == 'BEGIN':
            depth = path.count('/') + 1
            show = (want is None) or (want in path)
        if not show: continue
        if kind == 'BEGIN':
            if want is not None or True:
                print('  '*(depth-1) + path.rsplit('/',1)[-1] + ' {')
        elif kind == 'END':
            pass
        else:
            if prop[0] in ('phandle','linux,phandle'): continue
            print('  '*depth + '%-28s = %s' % (prop[0], fmt(prop[1])))
