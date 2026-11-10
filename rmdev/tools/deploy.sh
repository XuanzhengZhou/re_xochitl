#!/bin/sh
# 把当前 release 部署到设备、重启我们的 app，然后抓一张图。
#   sh tools/deploy.sh [截图名]
#
# 2026-11-08 之后 rmdev **自己驱动面板**（`src/panel/`：自己解析 `.wbf`、
# 自己编码波形、自己写 LCDIF），所以这脚本不再有 rm2fb 那一套讲究：
# 不加载 rm2fb client、不需要 framebuffer server、也不启动 xochitl。
#
# # 仍然绝对不要碰 remux / xochitl.service / rm2fb.service
#
# 为什么：那两个服务一旦起来就会抢 `/dev/fb0`（rm2fb 的 server 就是
# xochitl 里的 `EPFramebufferSwtcon`），我们的面板线程会和他们互相踩，
# 表现是画面撕裂/波形错乱。要归还面板就 `systemctl isolate` 到别的 target，
# 别在这里顺手 start。
#
# 部署三步：拷成 .new → 停服务、mv 过去 → 起服务。
# 覆盖正在运行的可执行文件是 ETXTBSY，所以必须"停 → mv"。
set -eu
cd "$(dirname "$0")/.."
NAME=${1:-cur}
BIN=target/armv7-unknown-linux-gnueabihf/release/rmdev
if [ ! -f "$BIN" ]; then
  echo "缺少 $BIN，先跑："
  echo "  cargo build --release --target armv7-unknown-linux-gnueabihf"
  exit 1
fi
scp -q "$BIN" remarkable:/home/root/rmdev.new
ssh remarkable '
  set -e
  # 必须用 systemctl stop：rmdev.service 是 Restart=always，直接 killall
  # 会被 systemd 立刻拉起来，mv 就撞上正在运行的可执行文件（ETXTBSY）。
  systemctl stop rmdev 2>/dev/null || true
  systemctl stop rmdev-autostart 2>/dev/null || true
  killall rmdev 2>/dev/null || true
  sleep 1
  mv /home/root/rmdev.new /home/root/rmdev
  chmod +x /home/root/rmdev
  # 必须让 `fresh` 成立，否则 `Panel::open` 会跳过 INIT，实测帧在屏上
  # 完全不生效（停在上一帧/花屏）。`/tmp` 是 tmpfs，重启机器会自带清空，
  # 但 restart 服务不会——这里手动删。
  rm -f /tmp/rm2epd.state
  # 开机链现在是 systemd → rmdev.service（remux 已 stop + disable，
  # 见 docs/BOOT.md）。千万别再 start remux：它会抢 /dev/fb0，
  # 两个写者同时喷帧 = 撕裂/白屏。
  systemctl start rmdev
  sleep 8
' 2>/dev/null
sh tools/shot.sh "$NAME" || true
echo "--- 设备日志尾部 ---"
ssh remarkable 'export PATH=/opt/bin:/usr/bin:/bin:/sbin:/usr/sbin; journalctl -u rmdev --no-pager -n 25' 2>/dev/null || true
