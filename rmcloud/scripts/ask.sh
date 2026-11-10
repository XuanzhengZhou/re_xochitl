#!/bin/sh
# 从命令行把一张页面图丢给云端 AI，看它流式答什么。
#
#   sh cloud/scripts/ask.sh <页面图.png> [问题] [模式] [矩形]
#
#   <页面图.png>  整页渲染图（1404x1872 灰度最好；彩色也行）
#   [问题]        默认「解释这一页」
#   [模式]        select / live / review / organize，默认 live
#   [矩形]        只在 select 模式有用：x0,y0,x1,y1（设备像素）
#
# 环境变量：
#   RMCLOUD_URL   默认 http://127.0.0.1:8787
#   RMCLOUD_TOKEN 不给就从 $RMCLOUD_CONFIG / ~/rmcloud.json 的 "token" 读
#   RMCLOUD_DOC   笔记 uuid（默认空 —— 只是让模型知道上下文，可以不给）
#   RMCLOUD_PAGE  页号（默认 0）
#
# 这是**手工验收脚本**，不是生产代码路径。它做的事和真机完全一样：
#   PUT blob（带 X-Sha256）→ POST /api/v1/agent/ask → 逐行读 SSE。
set -eu

PNG=${1:?用法: ask.sh <页面图.png> [问题] [模式] [矩形]}
Q=${2:-解释这一页}
MODE=${3:-live}
RECT=${4:-}

URL=${RMCLOUD_URL:-http://127.0.0.1:8787}
CFG=${RMCLOUD_CONFIG:-$HOME/rmcloud.json}

if [ -z "${RMCLOUD_TOKEN:-}" ]; then
    # 只取 token 一个字段，绝不把整份配置打出来
    RMCLOUD_TOKEN=$(CFG="$CFG" python3 -c '
import json, os
try:
    print(json.load(open(os.environ["CFG"])).get("token", ""))
except Exception:
    print("")
')
fi
[ -n "$RMCLOUD_TOKEN" ] || { echo "没有 token：设 RMCLOUD_TOKEN 或检查 $CFG" >&2; exit 2; }

SHA=$(sha256sum "$PNG" | cut -d' ' -f1)
SZ=$(wc -c <"$PNG")

printf '[ask] 上传 %s（%s 字节, sha=%s）\n' "$PNG" "$SZ" "$SHA" >&2
CODE=$(curl -s -o /dev/null -w '%{http_code}' -X POST "$URL/api/v1/blob" \
    -H "Authorization: Bearer $RMCLOUD_TOKEN" \
    -H "X-Sha256: $SHA" \
    --data-binary "@$PNG")
case "$CODE" in
    200|204) ;;
    *) echo "[ask] 上传失败 HTTP $CODE" >&2; exit 1 ;;
esac

BODY=$(Q="$Q" MODE="$MODE" RECT="$RECT" SHA="$SHA" DOC="${RMCLOUD_DOC:-}" PAGE="${RMCLOUD_PAGE:-0}" python3 -c '
import json, os
e = os.environ
b = {"mode": e["MODE"], "doc": e["DOC"], "page": int(e["PAGE"]),
     "text": e["Q"], "image_blob": e["SHA"]}
if e["RECT"]:
    b["rect"] = [float(x) for x in e["RECT"].split(",")]
print(json.dumps(b))
')

printf '[ask] %s 模式提问：%s\n' "$MODE" "$Q" >&2
# --no-buffer：curl 逐块吐出，SSE 才能边到边显示
curl -sN -X POST "$URL/api/v1/agent/ask" \
    -H "Authorization: Bearer $RMCLOUD_TOKEN" \
    -H "Content-Type: application/json" \
    --no-buffer \
    -d "$BODY" | while IFS= read -r line; do
    case "$line" in
        "event: token"*) printf '' ;;
        "data: "*)
            # 只把 token 的正文打出来；done/error 事件原样打
            printf '%s' "${line#data: }" | python3 -c '
import json, sys
raw = sys.stdin.read().strip()
if not raw:
    sys.exit()
try:
    v = json.loads(raw)
except Exception:
    print(raw)
    sys.exit()
if "t" in v:
    sys.stdout.write(v["t"]); sys.stdout.flush()
else:
    print("\n[ask] " + json.dumps(v, ensure_ascii=False))
'
            ;;
    esac
done
echo
