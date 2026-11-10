#!/bin/sh
# 模拟模型侧失败：先吐一个 token，再 turn_end error（不发 done）。
printf '%s\n' \
  '{"type":"text","text":"写到一半"}' \
  '{"type":"status","phase":"turn_end","reason":{"kind":"error","message":"模型超时了"}}'
exit 1
