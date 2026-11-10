#!/bin/sh
# 模拟 dsh 不认我们自己拼的 session：带 --session-id 时按真实 dsh 的样子报
# "unknown session" 并以 1 退出；去掉 --session-id（agent_api 的重试）后才正常回答。
for a in "$@"; do
  if [ "$a" = "--session-id" ]; then
    printf '%s\n' '{"type":"status","phase":"turn_end","reason":{"kind":"error","message":"unknown session: session-rm-deadbeef"}}'
    exit 1
  fi
done
printf '%s\n' \
  '{"type":"text","text":"重试成功"}' \
  '{"type":"status","phase":"step_end","step":1,"usage":{"inputTokens":11,"outputTokens":3}}' \
  '{"type":"status","phase":"turn_end","reason":{"kind":"completed"}}'
