#!/bin/sh
# rmdev WiFi 守护：xochitl 停了以后 WiFi 没人管，这个脚本替它管。
#   - 开机把 wlan0 拉起来、起 wpa_supplicant、udhcpc 要地址
#   - Campus-WLAN 自动跑 /home/root/campus_wlan_login.sh --auto
#   - 休眠后 brcmfmac 固件会挂死（dmesg: failed backplane access），自检重载
#   - 用户在 app 里关掉 WiFi（rfkill soft block）就安静待着，不抢
PATH=/usr/sbin:/sbin:/usr/bin:/bin
export PATH
IF=wlan0
CONF=/etc/wpa_supplicant.conf
LOG=/tmp/rmdev-wifi.log
mkdir -p /var/run/wpa_supplicant

log() { echo "$(date +%H:%M:%S) $*" >>"$LOG"; }

blocked() { rfkill list 2>/dev/null | grep -q "Soft blocked: yes"; }
have_ip() { ip -4 addr show $IF 2>/dev/null | grep -q "inet "; }
# 169.254.x = DHCP 失败的链路本地地址，等于没网
linklocal() { ip -4 addr show $IF 2>/dev/null | grep -q "inet 169[.]254[.]"; }
# resolv.conf 里一个 nameserver 都没有 = 域名解析全废（tailscale 也活不了）
no_dns() { grep -q "^nameserver" /etc/resolv.conf 2>/dev/null && return 1 || return 0; }
associated() { wpa_cli -i $IF status 2>/dev/null | grep -q "^wpa_state=COMPLETED"; }
fw_hung() { dmesg 2>/dev/null | tail -n 60 | grep -q "failed backplane access"; }

log "start (pid $$)"
fails=0
while :; do
    if blocked; then
        sleep 15
        continue
    fi
    ip link set $IF up 2>/dev/null

    if ! pidof wpa_supplicant >/dev/null 2>&1; then
        log "起 wpa_supplicant"
        wpa_supplicant -B -i $IF -c $CONF -D nl80211 >>"$LOG" 2>&1
        sleep 3
    fi

    if associated; then
        fails=0
        # 睡眠唤醒后租约会丢：IP 变 169.254 或者 resolv.conf 被清空，
        # 这时候"有 IP"也等于没网（tailscale 卡在 NoState、sync 全失败）。
        # 2026-11-10 真机实测：唤醒后只剩链路本地地址。
        need_dhcp=0
        have_ip || need_dhcp=1
        linklocal && need_dhcp=1
        no_dns && need_dhcp=1
        if [ "$need_dhcp" = 1 ]; then
            log "dhcp（重来）"
            ip addr flush dev $IF scope global 2>/dev/null
            udhcpc -i $IF -b -q -t 8 -T 3 >>"$LOG" 2>&1
        fi
        if iw dev $IF link 2>/dev/null | grep -q "SSID: Campus-WLAN"; then
            /home/root/campus_wlan_login.sh --auto >>"$LOG" 2>&1
        fi
        sleep 25
    else
        fails=$((fails + 1))
        log "未关联（第 $fails 次）"
        if [ $fails -ge 3 ] && fw_hung; then
            log "固件挂了，重载 brcmfmac"
            rmmod brcmfmac 2>/dev/null
            rmmod brcmutil 2>/dev/null
            sleep 1
            modprobe brcmfmac 2>/dev/null
            sleep 3
            ip link set $IF up 2>/dev/null
            fails=0
        fi
        sleep 10
    fi
done
