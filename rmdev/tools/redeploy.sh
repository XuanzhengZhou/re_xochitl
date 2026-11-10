#!/bin/sh
# 一条命令完成：交叉编译 → 部署 → 通过 remux 重启 App → 截图 → 打印日志尾。
#   sh tools/redeploy.sh [截图名]
#
# 为什么这么小心：**强杀正在画屏幕的进程会把 EPDC 更新队列卡死**，
# 之后 ioctl 永不返回、输入全死，只能人工重启。所以：
#   - 绝不 scp 覆盖正在运行的二进制（Text file busy）→ 先落 .new 再 mv
#   - 重启走 launcherctl，保住 remux 里的条目
#   - 起来之后核对 "ioctl 前"/"ioctl 后" 计数是否配对
set -eu
cd "$(dirname "$0")/.."
NAME=${1:-cur}

echo "== 1/4 交叉编译 =="
cargo build --release --target armv7-unknown-linux-gnueabihf 2>&1 | tail -2

echo "== 2/4 单测 =="
cargo test --lib 2>&1 | tail -2

echo "== 3/4 部署并重启 =="
scp -q target/armv7-unknown-linux-gnueabihf/release/rmdev remarkable:/home/root/rmdev.new
ssh remarkable '
  killall rmdev 2>/dev/null || true
  sleep 4
  mv /home/root/rmdev.new /home/root/rmdev
  chmod +x /home/root/rmdev
  /opt/bin/launcherctl start-app rmdev >/dev/null 2>&1 || true
  sleep 12
' 2>/dev/null

echo "== 4/4 截图与健康检查 =="
sh tools/shot.sh "$NAME"
ssh remarkable 'printf "刷新配对 前=%s 后=%s\n" \
  "$(grep -c "ioctl 前" /home/root/dev/rmdev.log)" \
  "$(grep -c "ioctl 后" /home/root/dev/rmdev.log)"; tail -n 6 /home/root/dev/rmdev.log' 2>/dev/null
