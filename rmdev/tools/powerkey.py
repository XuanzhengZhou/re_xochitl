#!/usr/bin/env python3
"""往 /dev/input/event0 注入电源键（`snvs-powerkey`，KEY_POWER = 116）。

用法（设备上）：
    /opt/bin/python3 /home/root/tools/powerkey.py tap        # 短按 → 睡 / 醒
    /opt/bin/python3 /home/root/tools/powerkey.py hold 4000  # 按住 4 s → 关机框

注意：写 evdev 节点等于"从内核注入"，和真按一模一样（不需要 uinput，
也不需要物理按钮）。设备睡着后 SSH 会断，注入前先设个 RTC 闹钟兜底：
    echo +240 > /sys/class/rtc/rtc0/wakealarm   # 用完记得 echo 0 清掉
"""
import struct, sys, time
PATH="/dev/input/event0"
KEY_POWER=116
def ev(t,c,v):
    return struct.pack("llHHi",0,0,t,c,v)  # 32-bit ARM: timeval 8 bytes each
def send(seq):
    with open(PATH,"wb") as f:
        for t,c,v in seq:
            f.write(ev(t,c,v))
        f.write(ev(0,0,0))
        f.flush()
if __name__=="__main__":
    cmd=sys.argv[1] if len(sys.argv)>1 else "tap"
    if cmd=="tap":
        send([(1,KEY_POWER,1)]); time.sleep(0.12); send([(1,KEY_POWER,0)])
    elif cmd=="hold":
        ms=int(sys.argv[2]) if len(sys.argv)>2 else 4000
        send([(1,KEY_POWER,1)]); time.sleep(ms/1000.0); send([(1,KEY_POWER,0)])
    print("sent",cmd)
