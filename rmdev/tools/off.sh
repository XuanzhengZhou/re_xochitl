#!/bin/sh
# 驱动**官方** UI 来取基准截图 / 校准坐标。
#   sh tools/off.sh up             启动官方 xochitl（先停掉 rmdev）
#   sh tools/off.sh down           回到 remux
#   sh tools/off.sh tap X Y [X2 Y2]  手指点一下（屏幕坐标，左上角为原点）
#   sh tools/off.sh snap NAME      截图到 shots/official/NAME.png
set -eu
cd "$(dirname "$0")/.."
HOST=remarkable

case "${1:-}" in
up)
    ssh $HOST 'killall rmdev 2>/dev/null; sleep 1; systemctl start remux; sleep 1;
       /opt/bin/launcherctl start-app xochitl >/dev/null 2>&1; sleep 8' 2>/dev/null
    echo "官方 UI 已启动"
    ;;
down)
    ssh $HOST 'systemctl restart remux; sleep 1' 2>/dev/null
    echo "已切回 remux"
    ;;
tap)
    # 手指（MT 协议 B）。官方 xochitl 认这个。
    X=${2:?需要 X}; Y=${3:?需要 Y}
    A=${4:-0}; B=${5:-0}
    ssh $HOST "/opt/bin/python3 /home/root/touchtap.py $X $Y $A $B" 2>/dev/null || true
    sleep "${SLEEP:-1.2}"
    ;;
pen)
    # 笔。这个对官方最稳（手指事件官方有时不认）。
    X=${2:?需要 X}; Y=${3:?需要 Y}
    D=${DIST:-1}
    O=$((X + D)); P=$((Y + D))
    ssh $HOST "/opt/bin/python3 /home/root/tap.py $O $P" 2>/dev/null || true
    sleep "${SLEEP:-1.0}"
    ;;
snap)
    NAME=${2:?需要名字}
    mkdir -p "$PWD/shots/official"
    ssh $HOST 'dd if=/dev/shm/swtfb.01 of=/tmp/cur.fb bs=65536 2>/dev/null' 2>/dev/null
    scp -q $HOST:/tmp/cur.fb /tmp/cur.fb 2>/dev/null
    "$HOME/myenv/bin/python3" tools/fb2png.py /tmp/cur.fb "$PWD/shots/official/$NAME.png" >/dev/null
    echo "shots/official/$NAME.png"
    ;;
*)
    sed -n '2,7p' "$0"; exit 1;;
esac
