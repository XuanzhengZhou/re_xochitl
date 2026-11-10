import os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from elf32 import Elf32
import capstone, struct

def scan(path, lo=None, hi=None):
    """Find all (insn_addr, materialized_addr) for `add rX,pc,rY` after ldr rY,[pc,#imm]."""
    e=Elf32(path)
    md=capstone.Cs(capstone.CS_ARCH_ARM, capstone.CS_MODE_ARM|capstone.CS_MODE_LITTLE_ENDIAN)
    md.skipdata=True; md.detail=True
    out=[]
    for a,b,nm in e.text_ranges():
        if nm!='.text': continue
        data=e.read(a,b-a)
        lits={}   # reg -> (addr_of_literal_value, site)
        prev=None; prev2=None
        for ins in md.disasm(data,a):
            if ins.id==0:
                prev2=prev; prev=None; continue
            ops=ins.operands
            if ins.mnemonic=='ldr' and len(ops)==2 and ops[0].type==capstone.arm.ARM_OP_REG and ops[1].type==capstone.arm.ARM_OP_MEM and ops[1].mem.base==capstone.arm.ARM_REG_PC:
                imm=ops[1].mem.disp
                la=ins.address+8+imm
                try: val=struct.unpack('<I', e.read(la,4))[0]
                except Exception: val=None
                lits[ins.reg_name(ops[0].reg)]=(la,val,ins.address)
            elif ins.mnemonic=='add' and len(ops)==3 and ops[1].type==capstone.arm.ARM_OP_REG and ops[1].reg==capstone.arm.ARM_REG_PC and ops[2].type==capstone.arm.ARM_OP_REG:
                src=ins.reg_name(ops[2].reg)
                if src in lits and lits[src][1] is not None:
                    tgt=(ins.address+8+lits[src][1])&0xffffffff
                    out.append((ins.address, tgt, lits[src][2], ops[0].reg))
            prev2=prev; prev=ins
    return out
