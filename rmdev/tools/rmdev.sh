#!/bin/sh
# remux 里的 rmdev 入口。
#
# # 一、别信 `/dev/shm/swtfb.01`，要真的探测 server
#
# rm2fb 只有 **server**（`librm2fb_server.so.1`）能写 EPDC，其它进程往那块
# 共享内存写帧、由 server 刷到面板。Toltec 的 `rm2fb-preload.env` 用「文件
# 在不在」判断 server 在不在，可那文件是持久的——server 死了它还在，于是新进程
# 全都只加载 client 库，写的帧永远没人刷，**屏幕冻结而进程一切正常**。
# （完整说明见 `rm2fb-guard.sh`。）
#
# # 二、server 没了要把它拉回来，不能自己当 server
#
# 试过 `LD_PRELOAD=librm2fb_server.so.1 ./rmdev` 想自己接管，**不行**：
# 那个库是 Qt 的 QPA 平台插件（日志里打 `EPFramebuffer::WaveformMode`），
# 非 Qt 程序加载它必崩在段错误（实测退出码 139）。所以唯一的办法是让
# `rm2fb.service`（= 官方 xochitl + server 库）重新起来当 server，
# 我们的程序继续当 client。
#
# # 三、等 EPDC 队列排空
#
# 如果上一个客户端刚被杀掉、EPDC 还有没完成的更新，第一次刷屏的 ioctl
# 会阻塞很久，所以先等 2 秒。
export PATH=/opt/bin:$PATH
[ -f /home/root/dev/timing ] && export RM_TIMING=1
# 留一份日志（每次启动截断）。刷新成功/失败那两行始终会打，
# 卡死时日志会停在 "ioctl 前" 而没有配对的后一行。
LOG=/home/root/dev/rmdev.log
: > "$LOG"
echo "=== $(date) 启动 rmdev ===" >> "$LOG"

# 上个客户端退出的瞬间 EPDC 可能还有 pending update，等它排空
sleep 2

GUARD=/home/root/dev/rm2fb-guard.sh

ensure_server() {
    "$GUARD" 2>/dev/null && return 0
    echo "=== $(date) 没有 framebuffer server，拉起 rm2fb.service ===" >> "$LOG"
    systemctl reset-failed rm2fb 2>/dev/null || true
    systemctl start rm2fb 2>/dev/null || systemctl restart rm2fb 2>/dev/null || true
    # 最多等 12 秒让它把 EPDC 抓回来
    n=0
    while [ "$n" -lt 12 ]; do
        sleep 1
        "$GUARD" 2>/dev/null && return 0
        n=$((n + 1))
    done
    echo "=== $(date) 等了 12 秒还是没有 server ===" >> "$LOG"
    return 1
}

# 首帧看门狗（程序里以 exit(3) 退出）重试：第一次失败先把 server 弄健康，
# 第二次再失败说明不是 server 的问题，放弃让 remux 重启。
try=1
while [ "$try" -le 3 ]; do
    ensure_server || echo "=== $(date) server 恢复失败，硬着头皮启动 (第 $try 次) ===" >> "$LOG"
    /opt/bin/rm2fb-client /home/root/rmdev >> "$LOG" 2>&1
    rc=$?
    echo "=== $(date) rmdev 退出 rc=$rc (第 $try 次) ===" >> "$LOG"
    if [ "$rc" = "3" ] && [ "$try" -lt 3 ]; then
        try=$((try + 1))
        sleep 3
        continue
    fi
    break
done

# 用独立单元重启 remux，避免把本脚本（同一 cgroup）一起带走。
# 注意：只重启 remux（app launcher），**不要**碰 rm2fb.service —— 它可能
# 正充当 framebuffer server。
systemd-run --no-block --unit="remux-restore-$(date +%s)" /bin/systemctl restart remux >/dev/null 2>&1
