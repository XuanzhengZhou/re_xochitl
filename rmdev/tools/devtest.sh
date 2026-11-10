#!/bin/sh
# 在设备上跑 rmdev：停掉官方 UI → 独占屏幕 → 注入测试笔迹 → 抓帧缓冲 → 恢复。
#   ssh remarkable 'sh /home/root/devtest.sh basic'
set -u
SCENE=${1:-all}
BIN=${BIN:-/home/root/rmdev}

restore() {
    killall rmdev 2>/dev/null
    sleep 1
    systemctl start remux
}
trap restore EXIT INT TERM

killall rmdev 2>/dev/null
systemctl stop remux
sleep 1

nohup /opt/bin/rm2fb-client "$BIN" > /tmp/rmdev.log 2>&1 &
sleep 3

/opt/bin/python3 /home/root/inject_rmdev.py "$SCENE" || echo "!! 注入失败"
sleep 0.6

dd if=/dev/shm/swtfb.01 of=/tmp/rmdev.fb bs=65536 2>/dev/null
echo "--- 应用日志 ---"
cat /tmp/rmdev.log
echo "--- fb ---"
wc -c < /tmp/rmdev.fb
