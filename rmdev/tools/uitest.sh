#!/bin/sh
# 启动 rmdev → 抓库界面 → 点第一条笔记 → 抓笔记本界面 → 恢复
set -u
restore() { killall rmdev 2>/dev/null; sleep 1; systemctl start remux; }
trap restore EXIT INT TERM

killall rmdev 2>/dev/null
systemctl stop remux
sleep 1
nohup /opt/bin/rm2fb-client /home/root/rmdev > /tmp/rmdev.log 2>&1 &
sleep 4
dd if=/dev/shm/swtfb.01 of=/tmp/ui_lib.fb bs=65536 2>/dev/null

/opt/bin/python3 /home/root/tap.py 700 300
sleep 2
dd if=/dev/shm/swtfb.01 of=/tmp/ui_nb.fb bs=65536 2>/dev/null

echo "--- 日志 ---"
cat /tmp/rmdev.log
