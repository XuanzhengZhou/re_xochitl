#!/bin/sh
set -u
restore() { systemctl restart remux; }
trap restore EXIT INT TERM
# 1) 官方库
systemctl restart remux; sleep 15
dd if=/dev/shm/swtfb.01 of=/tmp/cmp_official.fb bs=65536 2>/dev/null
# 2) 我们的库
systemctl stop remux; sleep 1
nohup /opt/bin/rm2fb-client /home/root/rmdev > /tmp/cmp.log 2>&1 &
sleep 8
dd if=/dev/shm/swtfb.01 of=/tmp/cmp_ours.fb bs=65536 2>/dev/null
killall rmdev; sleep 1
