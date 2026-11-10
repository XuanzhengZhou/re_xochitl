#!/bin/sh
set -eu
NAME=${1:-shot}
PID=$(ssh remarkable 'pidof rmdev' 2>/dev/null | tr -d '\r')
ssh remarkable "kill -USR1 $PID" 2>/dev/null
sleep 1
scp -q remarkable:/tmp/rmdev.shot.pgm /tmp/rmdev.shot.pgm
"$HOME/myenv/bin/python3" "$(dirname "$0")/pgm2png.py" /tmp/rmdev.shot.pgm "$PWD/shots/$NAME.png"
echo "shots/$NAME.png"
