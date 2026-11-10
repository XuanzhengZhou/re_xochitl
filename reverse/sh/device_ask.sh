#!/bin/sh
# 设备端入口：把最近手写页的墨迹推给 home-server 上的 agent。
#
# 用法:
#   ask.sh "帮我解这道题"                 # 最近修改的文档、最后一页
#   ask.sh "解释一下" --doc Quick          # 按文档名片段
#   ask.sh "看看第 2 页" --page 2
#
# 中间产物只在服务器 cache/ 里，用完即删。
set -e
export PATH=/opt/bin:$PATH
export PYTHONPATH=/home/root/rm2agent/vendor

[ $# -ge 1 ] || { echo "用法: ask.sh \"问题\" [--doc 片段] [--page N]"; exit 1; }
Q="$1"; shift

python3 /home/root/rm2agent/send_ink.py --question "$Q" "$@" \
  | /opt/bin/ssh -i /home/root/.ssh/id_ed25519 -p 2222 \
      -o BatchMode=yes -o StrictHostKeyChecking=no \
      user@127.0.0.1 '/home/user/rm-agent/ask-ink.sh'
