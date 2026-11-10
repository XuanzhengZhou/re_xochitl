import os, sys, struct, collections
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
RM2_ROOT = os.environ.get('RM2_ROOT', os.path.expanduser('~/re_xochitl'))
from elf32 import Elf32
import capstone

TARGETS = {0x41,0x43,0x45,0x47,0x51,0x53,0x61,
           0x410000,0x430000,0x450000,0x470000,0x510000,0x530000,0x610000,
           0x4100,0x4300,0x4500,0x4700,0x5100,0x5300,0x6100,
           0x41000000,0x43000000,0x53000000,0x61000000}
FILES = [os.path.join(RM2_ROOT, p) for p in [
    'xochitl',
    'libs/libepaper.so',
    'libs/librm2fb_server.so.1.0.1',
    'libs/librm2fb_client.so.1.0.1',
    'libs/libqvnc.so',
]]
md = capstone.Cs(capstone.CS_ARCH_ARM, capstone.CS_MODE_ARM|capstone.CS_MODE_LITTLE_ENDIAN)
md.detail=True
for p in FILES:
    e=Elf32(p)
    print('#'*20, p)
    hits=collections.defaultdict(list)
    for a,b,nm in e.text_ranges():
        if nm!='.text': continue
        data=e.read(a,b-a)
        for ins in md.disasm(data,a):
            for op in ins.operands:
                if op.type==capstone.arm.ARM_OP_IMM and op.imm in TARGETS:
                    hits[op.imm].append((ins.address, ins.mnemonic, ins.op_str))
                    break
    for k in sorted(hits):
        v=hits[k]
        print(f"  imm 0x{k:x}: {len(v)} sites")
        for s in v[:12]: print(f"     0x{s[0]:06x}  {s[1]} {s[2]}")
