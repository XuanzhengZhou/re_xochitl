#!/usr/bin/env python3
"""从官方 .wbf 生成多温度档波形 blob (panel/wbf_bands.bin)。

用法: python3 panel/mkbands.py <file.wbf> [out.bin]

布局(小端):
    "MXWF" | u16 ver(=1) | u8 nband | u8 nmode | u16 npool
    u16 bounds[nband+1]          # 温度边界, 末位是上界
    u16 nph[nmode*nband]         # 每 (模式,档) 的相位数 —— 注意本区固定 nmode*nband 个 u16
    u16 rows[sum(nph)]           # 每相位的去重行号
    u8  pool[npool][256]         # 32x32 LUT 的 4-code/byte 打包行

模式顺序: 0=DU 1=GC16 2=A2 3=DU4
"""
import sys, struct, os
sys.path.insert(0, os.path.join(os.path.dirname(os.path.abspath(__file__)), '..'))
from wbf import Wbf

MODES = [(1, 'DU'), (2, 'GC16'), (6, 'A2'), (7, 'DU4')]

def main(argv):
    if len(argv) < 2:
        print(__doc__); return 1
    src = argv[1]
    out = argv[2] if len(argv) > 2 else os.path.join(os.path.dirname(os.path.abspath(__file__)), 'wbf_bands.bin')
    w = Wbf(src, want_lut=False)
    bounds = list(w.temp_bounds)
    nband = len(bounds) - 1
    data = {}
    for mi, nm in MODES:
        bands = []
        for j in range(nband):
            tid = w.modes[mi]["temps"][j]["table_id"]
            d = w.tables[tid]["data"]
            bands.append([bytes(d[p * 256:(p + 1) * 256]) for p in range(len(d) // 256)])
        data[nm] = bands
    pool, idx = [], {}
    for _, nm in MODES:
        for b in data[nm]:
            for row in b:
                if row not in idx:
                    idx[row] = len(pool); pool.append(row)
    assert len(pool) < 65535
    buf = bytearray(b'MXWF' + struct.pack('<HBBH', 1, nband, len(MODES), len(pool)))
    buf += struct.pack('<%dH' % len(bounds), *bounds)
    for _, nm in MODES:
        for b in data[nm]:
            buf += struct.pack('<H', len(b))
    for _, nm in MODES:
        for b in data[nm]:
            buf += struct.pack('<%dH' % len(b), *[idx[r] for r in b])
    for r in pool:
        buf += r
    open(out, 'wb').write(buf)
    print("%s -> %s  (%d B, %d 档 x %d 模式, 去重相位 %d)" % (src, out, len(buf), nband, len(MODES), len(pool)))
    for _, nm in MODES:
        print("  %-5s %s" % (nm, [len(b) for b in data[nm]]))
    return 0

if __name__ == '__main__':
    sys.exit(main(sys.argv))
