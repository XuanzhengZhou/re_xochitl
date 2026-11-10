#!/bin/sh
# 部署 qmldump.so → 重启 xochitl → 抓取对象树 → 无条件回滚
set -u

SO=/home/root/qmldump.so
DROPIN_DIR=/etc/systemd/system/rm2fb.service.d
DROPIN=$DROPIN_DIR/zz-qmldump.conf
OUT=/home/root/qmldump.txt

restore() {
    rm -f "$DROPIN"
    systemctl daemon-reload
    systemctl restart rm2fb
}

trap restore EXIT INT TERM

mkdir -p "$DROPIN_DIR"
cat > "$DROPIN" <<EOF
[Service]
Environment="LD_PRELOAD=$SO:/opt/lib/librm2fb_server.so.1"
EOF

rm -f /tmp/qmldump.txt
systemctl daemon-reload
systemctl restart rm2fb || { echo "restart failed"; exit 1; }

echo "waiting for dump..."
echo "pids with qmldump.so:"
for m in /proc/[0-9]*/maps; do grep -q qmldump "$m" 2>/dev/null && echo "  $(echo $m | cut -d/ -f3) $(cat $(dirname $m)/comm)"; done
i=0
while [ $i -lt 150 ]; do
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
else
    echo "no dump file produced"
fi
