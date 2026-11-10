#!/bin/sh
# 从官方 UI 切到 rmdev；rmdev 退出后自动把官方 UI 拉回来。
# 由 rmdev-gate（--watch）在 journal 里看到 DEV 条目被打开时拉起。
set -u
# 先把刚打开的那个文档关掉，否则 xochitl 会记一笔「closed unexpectedly」，
# 下次启动在库里挂一条崩溃横幅。（顶栏左上角的返回键）
/opt/bin/python3 /home/root/tap.py 66 90 >/dev/null 2>&1
sleep 2
systemctl stop remux
sleep 1
/opt/bin/rm2fb-client /home/root/rmdev
sleep 1
systemctl start remux
