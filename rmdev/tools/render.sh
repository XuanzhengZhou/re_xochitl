#!/bin/sh
# 本机无头渲染一屏，输出 PNG。用法：sh tools/render.sh <what> [out.png]
#
#   what: library | sidebar | favorites | tags | trash | notebooks | pdfs | ebooks
#         settings[:<tab>] | search[:<query>] | doc:<uuid>[:<page>] | lock
#         settings 的 tab：0..7 或 wifi/battery/storage/security/language/
#         accessibility/help（另：about/licenses/langlist/pin）
#         docsearch:<uuid>[:<page>]:<query>[:<cmd>]   （cmd: type/enter/next/prev/pageonly/close）
#         nbsettings:<uuid>[:<page>][:<cmd>]          （cmd: name/template/save）
#         pagesel:<uuid>[:<page>][:<cmd>]             （页总览多选；cmd: all/none/delete/move/tag）
#         列表模式 / 排序下拉：library:list、library:sort
#
# 库数据默认用 ~/re_xochitl/fixture（从真机拷来的几本笔记 + PDF 条目）。
set -eu
cd "$(dirname "$0")/.."

WHAT=${1:-library}
NAME=$(printf '%s' "$WHAT" | tr ':/' '__')
OUT=${2:-shots/render-$NAME.png}

export RMDEV_LIB_ROOT=${RMDEV_LIB_ROOT:-$HOME/re_xochitl/fixture}
cargo build -q 2>/dev/null
./target/debug/rmdev --render "$WHAT" /tmp/render.pgm 2>/dev/null

"$HOME/myenv/bin/python3" - "$OUT" <<'PY'
import sys
from PIL import Image
im = Image.open("/tmp/render.pgm")
im.save(sys.argv[1])
print(sys.argv[1], im.size)
PY
