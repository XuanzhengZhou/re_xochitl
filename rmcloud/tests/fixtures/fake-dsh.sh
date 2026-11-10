#!/bin/sh
# 测试用假 dsh：模拟 `dsh --profile headless --json` 的 NDJSON 输出。
# 不管传什么 prompt 都回同一串事件，不联网、不跑模型。
printf '%s\n' \
  '{"type":"session","sessionId":"session-fake"}' \
  '{"type":"status","phase":"turn_start","turn":1}' \
  '{"type":"thinking","text":""}' \
  '{"type":"text","text":"你好"}' \
  '{"type":"status","phase":"step_end","step":1,"usage":{"inputTokens":100,"outputTokens":7,"cacheReadTokens":42}}' \
  '{"type":"status","phase":"turn_end","reason":{"kind":"completed"}}' \
  '{"type":"final","text":"你好"}'
