import struct, os, time
fd = os.open("/dev/input/event1", os.O_WRONLY)
def ev(t,c,v): return struct.pack("<IIHHi",0,0,t,c,v)
SYN,KEY,ABS=0,1,3
BTN_TOOL_PEN,ABS_X,ABS_Y,ABS_PRESSURE=0x140,0,1,0x18
def send(evs): os.write(fd, b"".join(evs))
def point(x,y,p=2000):
    send([ev(ABS,ABS_X,x),ev(ABS,ABS_Y,y),ev(ABS,ABS_PRESSURE,p),ev(SYN,0,0)])
send([ev(KEY,BTN_TOOL_PEN,1),ev(SYN,0,0)])
for i in range(80):
    t=i/79
    x=int(5000+(15000-5000)*t); y=int(4000+(12000-4000)*t)
    point(x,y)
    time.sleep(0.004)
send([ev(KEY,BTN_TOOL_PEN,0),ev(SYN,0,0)])
os.close(fd)
print("injected 80 points")
