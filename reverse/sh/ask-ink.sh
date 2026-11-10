#!/bin/bash
# 服务器端入口：stdin 收「头部 JSON + 量化笔画」→ 渲染 PNG → dsh headless 回答。
#
# 环境变量：
#   KEEP=1   保留本次生成的 PNG（默认用完即删）
#
# 缓存目录：~/rm2/cache/
#   - 所有中间产物只放在这里，清理也只清这里
#   - 自动清理：3 天前的 PNG、超过 200 个时删最旧的
set -euo pipefail

DIR="$HOME/文档/remarkable2"
CACHE="$DIR/cache"
SESSION_FILE="$CACHE/session-id"
mkdir -p "$CACHE"

# --- 自动清理（只作用于 cache 目录）---
find "$CACHE" -maxdepth 1 -type f -name '*.png' -mtime +3 -delete 2>/dev/null || true
COUNT=$(find "$CACHE" -maxdepth 1 -type f -name '*.png' | wc -l)
if [ "$COUNT" -gt 200 ]; then
    find "$CACHE" -maxdepth 1 -type f -name '*.png' -printf '%T@ %p\n' \
        | sort -n | head -n "$((COUNT - 200))" | cut -d' ' -f2- | xargs -r rm -f
fi

# --- 解码 + 渲染 ---
INFO=$(python3 "$DIR/ingest_ink.py")
PNG=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["png"])' <<<"$INFO")
QUESTION=$(python3 -c 'import json,sys; print(json.load(sys.stdin)["question"])' <<<"$INFO")

cleanup() { [ "${KEEP:-0}" = "1" ] || rm -f "$PNG"; }
trap cleanup EXIT

# --- dsh 环境 ---
export PATH=/home/user/.nvm/versions/node/v24.14.1/bin:$PATH
set -a; . "$HOME/.dsh/web.env" 2>/dev/null; set +a

# --- 固定 session（长期记忆）：首次自动创建并记住 id ---
# 注意：dsh 的输出被 head 截断会触发 SIGPIPE，pipefail 下会误判失败，
# 所以这里临时关掉 pipefail 并吞掉错误。
if [ ! -s "$SESSION_FILE" ]; then
    set +o pipefail
    SID=$(timeout 180 dsh --profile headless --json "初始化" 2>/dev/null \
        | head -n 1 \
        | python3 -c 'import json,sys
try:
    print(json.loads(sys.stdin.readline())["sessionId"])
except Exception:
    pass' || true)
    set -o pipefail
    [ -n "${SID:-}" ] && echo "$SID" > "$SESSION_FILE" || true
fi
SID=$(cat "$SESSION_FILE" 2>/dev/null || true)

PROMPT="这是一页手写笔记的图片：$PNG
请先用 read_image 读它；字迹潦草时可以裁剪/放大局部核对。
然后回答用户的问题：$QUESTION"

if [ -n "$SID" ]; then
    timeout 300 dsh --profile headless --session-id "$SID" "$PROMPT" 2>/dev/null
else
    timeout 300 dsh --profile headless "$PROMPT" 2>/dev/null
fi
