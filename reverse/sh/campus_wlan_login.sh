#!/bin/sh
# Campus-WLAN 自动认证（Dr.COM / eportal 通用模板）
#
# 不同学校的门户域名 / 端口 / 参数名不一样，用之前先把 HOST/PORT
# 和 login() 里的查询参数改成本校的（抓一次浏览器登录请求就够）。
#
# 凭据放在 /home/root/.campus_cred（chmod 600），格式：
#   USER_ID=202300000000
#   PASSWORD=你的密码
#
# 用法：
#   ./campus_wlan_login.sh          手动：未认证则登录
#   ./campus_wlan_login.sh --auto   自动：供 systemd timer 调用，静默
#   ./campus_wlan_login.sh status   查看当前会话
#   ./campus_wlan_login.sh perceive 只试无感知认证（免密）

CRED="/home/root/.campus_cred"
[ -f "$CRED" ] && . "$CRED"

HOST="w.portal.example.edu.cn"
PORT="801"
JSVER="3.3.3"
EP="https://${HOST}:${PORT}/eportal/"
QUIET=0
[ "$1" = "--auto" ] && QUIET=1

log() { [ "$QUIET" = "1" ] || echo "$@"; }

get_ip()  { ip -4 addr show wlan0 2>/dev/null | awk '/inet /{print $2}' | cut -d/ -f1; }
get_mac() { cat /sys/class/net/wlan0/address 2>/dev/null; }
fetch()   { wget -q -T 10 --no-check-certificate -O - "$1" 2>/dev/null; }

ensure_host() {
    grep -q "w.portal.example.edu.cn" /etc/hosts 2>/dev/null || \
        echo "10.0.0.1 w.portal.example.edu.cn" >> /etc/hosts
}

status() {
    ip_addr=$(get_ip); mac=$(get_mac)
    fetch "${EP}?c=Portal&a=online_list&user_account=drcom&user_password=123&wlan_user_ip=${ip_addr}&wlan_user_mac=${mac}&curr_user_ip=${ip_addr}&jsVersion=${JSVER}"
}

is_online() {
    ip_addr=$(get_ip)
    [ -n "$ip_addr" ] && status | grep -q "\"online_ip\":\"${ip_addr}\""
}

login() {
    ip_addr=$(get_ip); mac=$(get_mac)
    [ -z "$ip_addr" ] && return 1
    [ -z "$mac" ] && mac="000000000000"
    url="${EP}?c=Portal&a=login&callback=dr1003&login_method=1\
&user_account=%2C0%2C${USER_ID}&user_password=${PASSWORD}\
&wlan_user_ip=${ip_addr}&wlan_user_ipv6=&wlan_user_mac=${mac}\
&wlan_ac_ip=&wlan_ac_name=&jsVersion=${JSVER}&v=3391"
    log "login ip=$ip_addr mac=$mac user=$USER_ID"
    fetch "$(echo "$url" | tr -d '\n')"
}

perceive() {
    ip_addr=$(get_ip); mac=$(get_mac)
    url="${EP}?c=Portal&a=perceive&callback=dr1003&login_method=1\
&wlan_user_ip=${ip_addr}&wlan_user_ipv6=&wlan_user_mac=${mac}\
&wlan_ac_ip=&wlan_ac_name=&jsVersion=${JSVER}&data_format=2"
    log "perceive ip=$ip_addr mac=$mac"
    fetch "$(echo "$url" | tr -d '\n')"
}

ensure_host

case "$1" in
    status)   ip_addr=$(get_ip); [ -z "$ip_addr" ] && { echo "wlan0 无 IP"; exit 0; }; status; echo ;;
    perceive) perceive; echo; sleep 2; is_online && echo "perceive OK" || echo "perceive 未生效" ;;
    *)
        ip_addr=$(get_ip)
        [ -z "$ip_addr" ] && { log "wlan0 无 IP，跳过"; exit 0; }
        if is_online; then log "已认证"; exit 0; fi
        # 1) 先试无感知（不需要密码）
        perceive >/dev/null 2>&1; sleep 2
        if is_online; then log "无感知认证成功"; exit 0; fi
        # 2) 回退到账号密码
        if [ -n "$PASSWORD" ]; then
            login >/dev/null 2>&1; sleep 2
            is_online && { log "账号登录成功"; exit 0; } || log "登录失败"
        else
            log "未配置密码，跳过账号登录"
        fi
        exit 0 ;;
esac
