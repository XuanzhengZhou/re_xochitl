#!/bin/sh
# 测试用假 dsh（图 prompt 验收）：把收到的全部参数落盘到 $FAKE_DSH_PROMPT。
# 最后一个参数就是 prompt；测试读这个文件断言里面给了哪些图的绝对路径。
# 落盘之后再吐一串正常 NDJSON，让 `/api/v1/agent/ask` 正常跑完。
if [ -n "$FAKE_DSH_PROMPT" ]; then
  printf '%s\n' "$@" >> "$FAKE_DSH_PROMPT"
fi
printf '%s\n' \
  '{"type":"text","text":"看到了"}' \
  '{"type":"status","phase":"step_end","step":1,"usage":{"inputTokens":9000,"outputTokens":12}}' \
  '{"type":"status","phase":"turn_end","reason":{"kind":"completed"}}'
