#!/bin/sh
set -u
restore() { systemctl restart remux; }
trap restore EXIT INT TERM
export RM_TIMING=1
/home/root/dev/rmdev.sh
