#!/bin/sh
set -u
restore() { systemctl restart remux; }
trap restore EXIT INT TERM
/opt/bin/launcherctl start-app rmdev >/dev/null 2>&1
sleep 6
/opt/bin/python3 /home/root/tap.py 700 300 >/dev/null
sleep 3
dd if=/dev/shm/swtfb.01 of=/tmp/e_open.fb bs=65536 2>/dev/null
# 画一条横线（默认 FinelinerV2）
/opt/bin/python3 /home/root/inject_rmdev.py basic >/dev/null 2>&1
sleep 2
dd if=/dev/shm/swtfb.01 of=/tmp/e_drawn.fb bs=65536 2>/dev/null
# 选橡皮（第 9 个槽）
/opt/bin/python3 /home/root/tap.py 1326 45 >/dev/null
sleep 2
# 竖着擦过去
/opt/bin/python3 /home/root/swipe.py 700 350 700 470 >/dev/null
sleep 2
dd if=/dev/shm/swtfb.01 of=/tmp/e_erased.fb bs=65536 2>/dev/null
echo "done"
