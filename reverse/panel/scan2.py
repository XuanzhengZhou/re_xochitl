import os, sys, collections
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
RM2_ROOT = os.environ.get('RM2_ROOT', os.path.expanduser('~/re_xochitl'))
from elf32 import Elf32
import capstone
e=Elf32(os.path.join(RM2_ROOT,'xochitl'))
md=capstone.Cs(capstone.CS_ARCH_ARM, capstone.CS_MODE_ARM|capstone.CS_MODE_LITTLE_ENDIAN)
md.skipdata=True; md.detail=True
TGT={334:'334',1405:'1405',1404:'1404',234:'234',73:'73',306:'306',260:'260',26:'26',1872:'1872',1336:'1336',1040:'1040',1408:'1408',1040:'1040',0x57c:'1404?',2080:'2080'}
hits=collections.defaultdict(list)
orrs=[]
for a,b,nm in e.text_ranges():
    if nm!='.text': continue
    for ins in md.disasm(e.read(a,b-a),a):
        if ins.id==0: continue
        if ins.mnemonic=='orr' and 'lsl #16' in ins.op_str:
            orrs.append((ins.address,ins.op_str))
        for op in ins.operands:
            if op.type==capstone.arm.ARM_OP_IMM and op.imm in TGT:
                hits[op.imm].append((ins.address,ins.mnemonic,ins.op_str))
for k in sorted(hits):
    v=hits[k]; print(f"== {TGT[k]} 0x{k:x}: {len(v)} sites")
    for s in v[:20]: print(f"   0x{s[0]:06x} {s[1]} {s[2]}")
print("== orr with lsl#16:",len(orrs))
for s in orrs[:40]: print(f"   0x{s[0]:06x} {s[1]}")
