#!/bin/sh
# rm2fb 的 framebuffer server 活着吗？活着 exit 0，不在 exit 1。
#
# # 为什么不能只看 `/dev/shm/swtfb.01` 在不在
#
# `rm2fb` 的架构是：**server**（`librm2fb_server.so.1`）独占 EPDC，其它进程
# 往 `/dev/shm/swtfb.01` 这块共享内存写帧，server 负责刷到面板。
#
# Toltec 的 `/opt/etc/xochitl.env.d/rm2fb-preload.env` 用**文件是否存在**来
# 判断 server 在不在：
#
#     if [[ -f /dev/shm/swtfb.01 ]]; then
#         export LD_PRELOAD=...librm2fb_client.so.1
#     fi
#
# 可这个文件是持久的——server 死了它照样在。于是 server 一死，之后所有
# 新起的进程都只加载 **client** 库：它们写共享内存永远"成功"，日志一切正常，
# CPU 也不忙，但**没有任何东西把帧刷到 EPDC**。表现就是：
#
#   * 屏幕永远停在 server 死前的最后一帧；
#   * 程序照样收输入、照样跑（`ps` 状态 S、负载 0.0x）；
#   * `/dev/shm/swtfb.01` 的 mtime 停在某一刻再也不动。
#
# 2026-11-07 那次「卡死」就是这个：`deploy.sh` 里 `systemctl stop remux`
# 把当时正充当 server 的 remux 子进程（日志里 `EPD platform plugin loaded!`）
# 一起杀了，`rm2fb.service` 的 server 早在 05:29:42 就 `Succeeded` 退出，
# 于是系统里彻底没有 server。
#
# server 库会把自己的 `comm` 改成 `rm2fb-server`（而不是 argv[0] 的 xochitl），
# 所以扫描 /proc/*/comm 是可靠的判据。
for p in /proc/[0-9]*; do
    [ -r "$p/comm" ] || continue
    case "$(cat "$p/comm" 2>/dev/null)" in
        rm2fb-server*) exit 0 ;;
    esac
done
exit 1
