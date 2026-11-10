#!/bin/sh
set -u
restore() { killall rmdev 2>/dev/null; sleep 1; systemctl start remux; }
trap restore EXIT INT TERM
killall rmdev 2>/dev/null; systemctl stop remux; sleep 1
nohup /opt/bin/rm2fb-client /home/root/rmdev > /tmp/rmdev.log 2>&1 &
sleep 4
/opt/bin/python3 /home/root/tap.py 700 300      # 打开第一条
sleep 2
dd if=/dev/shm/swtfb.01 of=/tmp/t_open.fb bs=65536 2>/dev/null
/opt/bin/python3 /home/root/tap.py 1350 125     # 点"收起"
sleep 2
dd if=/dev/shm/swtfb.01 of=/tmp/t_hide.fb bs=65536 2>/dev/null
/opt/bin/python3 /home/root/tap.py 1372 30      # 点右上角把手
sleep 2
dd if=/dev/shm/swtfb.01 of=/tmp/t_show.fb bs=65536 2>/dev/null
/opt/bin/python3 /home/root/tap.py 50 125       # 返回
sleep 2
dd if=/dev/shm/swtfb.01 of=/tmp/t_back.fb bs=65536 2>/dev/null
cat /tmp/rmdev.log
