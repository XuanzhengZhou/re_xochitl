import os, sys, struct
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
RM2_ROOT = os.environ.get('RM2_ROOT', os.path.expanduser('~/re_xochitl'))
from elf32 import Elf32
from unicorn import Uc, UC_ARCH_ARM, UC_MODE_ARM, UC_MODE_LITTLE_ENDIAN
from unicorn.arm_const import *

PATH=os.path.join(RM2_ROOT,'xochitl')
e=Elf32(PATH)
BASE=0x10000
SIZE=0x940000
def run(func, bufva, r1=0, fill=0x1000, n=200000):
    mu=Uc(UC_ARCH_ARM, UC_MODE_ARM|UC_MODE_LITTLE_ENDIAN)
    # map full image range
    mu.mem_map(0x10000, 0x940000)
    # copy all PT_LOAD file content
    for (t,off,va,pa,fs,ms,fl,al) in e.phdrs:
        if t!=1: continue
        mu.mem_write(va, e.d[off:off+fs])
    STACK=0x8f0000
    mu.mem_write(STACK, b'\x00'*0x1000)
    mu.mem_write(bufva, bytes([fill&0xff])*0x1000)
    mu.reg_write(UC_ARM_REG_SP, STACK+0x800)
    mu.reg_write(UC_ARM_REG_LR, 0xDEAD0000)
    mu.reg_write(UC_ARM_REG_R0, bufva)
    mu.reg_write(UC_ARM_REG_R1, r1)
    try:
        mu.emu_start(func, 0xDEAD0000, count=n)
    except Exception as ex:
        if 'DEAD' not in str(ex) and 'invalid' not in str(ex).lower():
            pass
    return mu.mem_read(bufva, 0x410)
if __name__=='__main__':
    for name,fn,arg in [('A(0x5644e4)',0x5644e4,0),('B(0x564640)',0x564640,0),('B r1=1',0x564640,1)]:
        b=run(fn,0x600000,arg)
        print(f"=== {name}")
        cmds=[]; data=[]
        for w in range(260):
            v=struct.unpack_from('<I',b,w*4)[0]
            cmds.append((v>>16)&0xff); data.append(v&0xffff)
        # compress runs
        s=0
        while s<260:
            t=s
            while t+1<260 and cmds[t+1]==cmds[s]: t+=1
            print(f"   words {s:3d}..{t:3d}  cmd=0x{cmds[s]:02x}  data=0x{data[s]:04x}")
            s=t+1
