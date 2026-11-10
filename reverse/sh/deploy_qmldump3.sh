#!/bin/sh
# 目标进程是 remux.service 起的 xochitl --system（真正的 Qt UI）。
# 用 /etc/ld.so.preload 注入，抓完后无条件撤除并恢复 UI。
set -u

SO=/home/root/qmldump.so
OUT=/home/root/qmldump.txt

restore() {
    rm -f /etc/ld.so.preload
    systemctl restart remux
}

trap restore EXIT INT TERM

rm -f /etc/systemd/system/rm2fb.service.d/zz-qmldump.conf
systemctl daemon-reload

echo "installing /etc/ld.so.preload -> $SO"
echo "$SO" > /etc/ld.so.preload

rm -f /tmp/qmldump.txt
echo "restarting remux (this restarts the UI)..."
systemctl restart remux || { echo "restart failed"; exit 1; }

sleep 4
echo "processes holding the shim:"
for m in /proc/[0-9]*/maps; do
    grep -q qmldump "$m" 2>/dev/null && echo "  $(basename $(dirname $m)) $(cat $(dirname $m)/comm)"
done

echo "waiting for dump..."
i=0
while [ $i -lt 90 ]; do
    sleep 2
    i=$((i + 1))
    if grep -q "TREE END" /tmp/qmldump.txt 2>/dev/null; then
        echo "dump complete after $((i * 2))s"
        break
    fi
    if [ $((i % 10)) -eq 0 ]; then
        echo "  ... $((i * 2))s, size=$(wc -c < /tmp/qmldump.txt 2>/dev/null || echo 0)"
    fi
done

if [ -f /tmp/qmldump.txt ]; then
    cp /tmp/qmldump.txt "$OUT"
    echo "saved $OUT ($(wc -c < "$OUT") bytes, $(wc -l < "$OUT") lines)"
fi
