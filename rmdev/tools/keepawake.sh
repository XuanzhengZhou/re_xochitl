#!/bin/sh
while true; do
  echo 0 > /sys/class/graphics/fb0/blank 2>/dev/null
  sleep 20
done
