//! Wi-Fi 数据：当前连接、可用网络、开关。
//!
//! rM2 上没有 `iwgetid` / `iwlist`，只有 `/usr/sbin/iw`、`/usr/sbin/wpa_cli`
//! 和 `/usr/sbin/rfkill`。所以：
//! - 当前 SSID：`iwgetid -r`（别的机器）→ `iw dev wlan0 link`（rM2）
//! - 扫描：`iw dev wlan0 scan`（rM2）→ `iwlist wlan0 scan`（老系统）
//! - 开关：`rfkill block/unblock wlan` + `ip link set wlan0 up/down`
//!
//! 读不到任何东西时返回空列表，UI 降级成「只有已连接那一条」。

#[derive(Clone, Debug, PartialEq)]
pub struct Network {
    pub ssid: String,
    /// dBm，负数；读不到就是 None。
    pub signal: Option<i32>,
    /// 有 RSN/WPA/Privacy 就算加密。
    pub secure: bool,
}

/// 官方分档：> -50 三格，> -60 两格，> -70 一格，否则零格。
pub fn signal_bars(dbm: i32) -> u8 {
    if dbm > -50 {
        3
    } else if dbm > -60 {
        2
    } else if dbm > -70 {
        1
    } else {
        0
    }
}

fn push_network(out: &mut Vec<Network>, cur: Network) {
    if cur.ssid.trim().is_empty() {
        return;
    }
    // 同一 SSID（多个 AP）只留信号最强的一条。
    if let Some(old) = out.iter_mut().find(|n| n.ssid == cur.ssid) {
        if cur.signal.unwrap_or(i32::MIN) > old.signal.unwrap_or(i32::MIN) {
            old.signal = cur.signal;
        }
        old.secure |= cur.secure;
        return;
    }
    out.push(cur);
}

/// 解析 `iw dev wlan0 scan`。
pub fn parse_iw_scan(text: &str) -> Vec<Network> {
    let mut out: Vec<Network> = Vec::new();
    let mut cur: Option<Network> = None;
    for line in text.lines() {
        let t = line.trim_start();
        if t.starts_with("BSS ") {
            if let Some(n) = cur.take() {
                push_network(&mut out, n);
            }
            cur = Some(Network {
                ssid: String::new(),
                signal: None,
                secure: false,
            });
            continue;
        }
        let Some(n) = cur.as_mut() else { continue };
        if let Some(v) = t.strip_prefix("SSID:") {
            n.ssid = v.trim().to_string();
        } else if let Some(v) = t.strip_prefix("signal:") {
            n.signal = v
                .split_whitespace()
                .next()
                .and_then(|x| x.parse::<f32>().ok())
                .map(|x| x.round() as i32);
        } else if t.starts_with("RSN:")
            || t.starts_with("WPA:")
            || (t.starts_with("capability:") && t.contains("Privacy"))
        {
            n.secure = true;
        }
    }
    if let Some(n) = cur {
        push_network(&mut out, n);
    }
    sort_networks(&mut out);
    out
}

/// 解析 `iwlist wlan0 scan`（老系统后备）。
pub fn parse_iwlist(text: &str) -> Vec<Network> {
    let mut out: Vec<Network> = Vec::new();
    let mut cur: Option<Network> = None;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("Cell ") {
            if let Some(n) = cur.take() {
                push_network(&mut out, n);
            }
            cur = Some(Network {
                ssid: String::new(),
                signal: None,
                secure: false,
            });
            continue;
        }
        let Some(n) = cur.as_mut() else { continue };
        if let Some(i) = t.find("ESSID:") {
            n.ssid = t[i + 6..].trim().trim_matches('"').to_string();
        } else if let Some(i) = t.find("Signal level=") {
            let rest = &t[i + 13..];
            // " -58 dBm" 或 "-58/100"
            let v = rest.split_whitespace().next().unwrap_or("");
            n.signal = v
                .split('/')
                .next()
                .and_then(|x| x.parse::<f32>().ok())
                .map(|x| x.round() as i32);
        } else if t.starts_with("Encryption key:on") {
            n.secure = true;
        }
    }
    if let Some(n) = cur {
        push_network(&mut out, n);
    }
    sort_networks(&mut out);
    out
}

fn sort_networks(v: &mut [Network]) {
    v.sort_by(|a, b| b.signal.unwrap_or(i32::MIN).cmp(&a.signal.unwrap_or(i32::MIN)));
}

/// 解析 `iw dev wlan0 link`：`Connected to ..` + `SSID: X` + `signal: -58 dBm`。
pub fn parse_iw_link(text: &str) -> Option<(String, Option<i32>)> {
    if !text.contains("Connected to") {
        return None;
    }
    let mut ssid = String::new();
    let mut signal = None;
    for line in text.lines() {
        let t = line.trim();
        if let Some(v) = t.strip_prefix("SSID:") {
            ssid = v.trim().to_string();
        } else if let Some(v) = t.strip_prefix("signal:") {
            signal = v
                .split_whitespace()
                .next()
                .and_then(|x| x.parse::<f32>().ok())
                .map(|x| x.round() as i32);
        }
    }
    if ssid.is_empty() {
        None
    } else {
        Some((ssid, signal))
    }
}

/// `rfkill list` 里 wlan 那条是否 soft blocked。
pub fn parse_rfkill_blocked(text: &str) -> Option<bool> {
    let mut in_wlan = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with(char::is_numeric) {
            in_wlan = t.to_lowercase().contains("wlan") || t.to_lowercase().contains("phy");
        } else if in_wlan && t.starts_with("Soft blocked:") {
            return Some(t.ends_with("yes"));
        }
    }
    None
}

/// 从 `iw ... link` 的 SSID 行读当前网络（给别的模块/测试用）。
pub fn parse_ssid_line(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.trim().strip_prefix("SSID:").map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty())
}

struct Tool;

impl Tool {
    /// 依次试几个路径，第一个成功（exit 0）的输出返回。
    fn run(paths: &[&str], args: &[&str]) -> Option<String> {
        for p in paths {
            if let Ok(o) = std::process::Command::new(p).args(args).output() {
                if o.status.success() {
                    return Some(String::from_utf8_lossy(&o.stdout).into_owned());
                }
            }
        }
        None
    }
}

const IWGETID: &[&str] = &["/usr/bin/iwgetid", "/sbin/iwgetid", "iwgetid"];
const IW: &[&str] = &["/usr/sbin/iw", "/sbin/iw", "iw"];
const IWLIST: &[&str] = &["/sbin/iwlist", "/usr/sbin/iwlist", "iwlist"];
const RFKILL: &[&str] = &["/usr/sbin/rfkill", "/sbin/rfkill", "/usr/bin/rfkill", "rfkill"];
const IP: &[&str] = &["/sbin/ip", "/usr/sbin/ip", "/usr/bin/ip", "ip"];

/// 当前 SSID。
pub fn current() -> Option<String> {
    if let Some(o) = Tool::run(IWGETID, &["-r"]) {
        let s = o.trim();
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    let o = Tool::run(IW, &["dev", "wlan0", "link"])?;
    parse_iw_link(&o).map(|(ssid, _)| ssid)
}

/// 当前 SSID + 信号（设置页只显示 SSID，信号图标用 dBm 分档）。
pub fn current_link() -> Option<(String, Option<i32>)> {
    let o = Tool::run(IW, &["dev", "wlan0", "link"])?;
    parse_iw_link(&o)
}

/// 扫一遍可用网络。会阻塞（`iw scan` 在设备上约 1~3 秒），调用方自己缓存结果。
pub fn scan() -> Vec<Network> {
    // 真机实测（2026-11-09）：已关联 + wpa_supplicant 正在后台扫描时，
    // `iw dev wlan0 scan` 会**立刻失败**（20 ms、0 个网络），于是设置页显示空列表，
    // 用户以为"Wi-Fi 坏了/连不上热点"。重试几次，还不行就改用真正管扫描的 `wpa_cli`。
    for _ in 0..3 {
        if let Some(o) = Tool::run(IW, &["dev", "wlan0", "scan"]) {
            let v = fix_ssids(parse_iw_scan(&o));
            if !v.is_empty() {
                return v;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    if run(&["scan"]).is_ok() {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        if let Ok(t) = run(&["scan_results"]) {
            let v = parse_scan_results(&t);
            if !v.is_empty() {
                return v;
            }
        }
    }
    if let Some(o) = Tool::run(IWLIST, &["wlan0", "scan"]) {
        return fix_ssids(parse_iwlist(&o));
    }
    Vec::new()
}

/// 解析 `wpa_cli scan_results`：
///
/// ```text
/// bssid / frequency / signal level / flags / ssid
/// 00:11:22:33:44:55	5180	-45	[WPA2-PSK-CCMP][ESS]	Campus-WLAN
/// ```
/// `iw` 会把 SSID 里的不可打印字节打成 `\xNN`（iPhone 热点结尾有个空格 →
/// `iPhone\x20`）。不反转义就会拿这个 9 个字符的串去连接，永远连不上，
/// 界面上还会把 `\x20` 原样显示出来。
pub fn unescape_iw_ssid(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1] == b'x' {
            let hex = &s[i + 2..i + 4];
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 4;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `iw` / `iwlist` 的输出里 SSID 可能带 `\xNN`，统一反转义一遍。
fn fix_ssids(mut v: Vec<Network>) -> Vec<Network> {
    for n in &mut v {
        n.ssid = unescape_iw_ssid(&n.ssid);
    }
    v
}

pub fn parse_scan_results(text: &str) -> Vec<Network> {
    let mut out: Vec<Network> = Vec::new();
    for line in text.lines().skip(1) {
        let mut it = line.splitn(5, '\t');
        let (Some(_bssid), Some(_freq), Some(sig), Some(flags), Some(ssid)) =
            (it.next(), it.next(), it.next(), it.next(), it.next())
        else {
            continue;
        };
        let ssid = ssid.trim();
        if ssid.is_empty() || out.iter().any(|n| n.ssid == ssid) {
            continue;
        }
        out.push(Network {
            ssid: ssid.to_string(),
            signal: sig.trim().parse().ok(),
            secure: flags.contains("WPA") || flags.contains("WEP") || flags.contains("RSN"),
        });
    }
    out
}

/// Wi-Fi 是否开着。读不到（本机开发）返回 None。
pub fn is_enabled() -> Option<bool> {
    if let Some(o) = Tool::run(RFKILL, &["list"]) {
        if let Some(blocked) = parse_rfkill_blocked(&o) {
            return Some(!blocked);
        }
    }
    let state = std::fs::read_to_string("/sys/class/net/wlan0/operstate").ok()?;
    match state.trim() {
        "up" => Some(true),
        "down" | "dormant" => Some(false),
        _ => None,
    }
}

/// 真正开关 Wi-Fi。两个工具都不可用才报错。
pub fn set_enabled(on: bool) -> Result<(), String> {
    let rf = if on { "unblock" } else { "block" };
    let mut did = false;
    if Tool::run(RFKILL, &[rf, "wlan"]).is_some() {
        did = true;
    }
    let link = if on { "up" } else { "down" };
    if Tool::run(IP, &["link", "set", "wlan0", link]).is_some() {
        did = true;
    }
    if did {
        Ok(())
    } else {
        Err("rfkill and ip are not available".into())
    }
}

// === C0W begin: 连接 / 忘记（只走 `wpa_cli`，绝不起/杀 `wpa_supplicant`）===
//
// `wpa_supplicant` 由系统服务 `rmdev-wifi.service`（`/home/root/dev/rmdev-wifi.sh`）
// 管着：开机自起、断线自动重连、Campus-WLAN 自动认证。这里只通过 `wpa_cli` 的
// ctrl_iface 下发/删除配置，进程本身不归我们管 —— 自己再起一份会两个实例抢
// 同一个 `ctrl_interface`，症状是状态时有时无。

/// 设备上的 `wpa_cli`。绝对路径优先：部署后的 `PATH` 很短。
const WPA_CLI: &[&str] = &["/usr/sbin/wpa_cli", "/sbin/wpa_cli", "wpa_cli"];

/// DHCP 客户端。`rmdev-wifi.sh` 也是用它，见 [`acquire_ipv4`]。
const UDHCPC: &[&str] = &["/sbin/udhcpc", "/usr/sbin/udhcpc", "udhcpc"];

/// 轮询 `wpa_cli status` 的间隔。400 ms 足够快，又不会把 ctrl_iface 问爆。
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(400);

/// 跑一条 `wpa_cli` 命令：成功返回 stdout（trim 过），失败返回**英文原文**。
///
/// 两个坑：
/// - `wpa_cli` 很多命令失败时**退出码仍是 0**，只在 stdout 打一行 `FAIL`
///   （`remove_network` 删当前连接就是这么回），所以不能只看 `status.success()`。
/// - `wpa_supplicant` 没起时它把 "Failed to connect to non-global ctrl_iface..."
///   打在 stdout/stderr 上，退出码 255 —— 原文原样往上传，UI 不翻译。
fn run(args: &[&str]) -> Result<String, String> {
    let mut last = "wpa_cli is not available".to_string();
    for p in WPA_CLI {
        match std::process::Command::new(p)
            .args(args)
            .stderr(std::process::Stdio::piped())
            .output()
        {
            Ok(o) => {
                let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
                let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
                if o.status.success() && !out.starts_with("FAIL") {
                    return Ok(out);
                }
                // 光一个 `FAIL` 对用户没信息量：补上是哪条命令失败。
                if out == "FAIL" || out.is_empty() {
                    let cmd = args.first().copied().unwrap_or("");
                    return Err(if err.is_empty() {
                        format!("wpa_cli {cmd} failed")
                    } else {
                        err
                    });
                }
                return Err(out);
            }
            Err(e) => last = format!("{p}: {e}"),
        }
    }
    Err(last)
}

/// 把 PSK 包成 `wpa_cli set_network` 要的那个带引号参数。
///
/// `wpa_cli` 走的是串口式文本协议（空格分隔、`"` 包字符串），所以：
/// - 整个值要包一层 `"`（`wpa_supplicant` 靠"带不带引号"区分"口令"和"64 位
///   十六进制 PSK"）；
/// - 值里的 `"` 和 `\` 各自用 `\` 转义。
///
/// 不转义的后果很实在：口令里带一个 `"` 就会把参数截成两段，`wpa_cli` 回
/// `FAIL`，现象就是"点了没反应"。
///
/// SSID 不走这里 —— 它一律发十六进制字节（见 [`hex_ssid`]），空格、引号、
/// emoji、非 ASCII 都不会碰到转义这一层。
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// 参数校验：SSID 1..=32 **字节**（802.11 上限），PSK 8..=63 字节（WPA2 passphrase）。
///
/// 错误串是英文的：UI 里直接显示，不翻译。
fn check(ssid: &str, open: bool, psk: &str) -> Result<(), String> {
    if ssid.is_empty() || ssid.len() > 32 {
        return Err("The network name must be 1 to 32 characters".into());
    }
    if !open && !(8..=63).contains(&psk.len()) {
        return Err("The passphrase must be 8 to 63 characters".into());
    }
    Ok(())
}

/// 解析 `wpa_cli list_networks`：
///
/// ```text
/// network id / ssid / bssid / flags
/// 0	Campus-WLAN	any	[CURRENT]
/// 1	Home	any
/// ```
pub fn parse_list_networks(text: &str) -> Vec<(u32, String)> {
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with("network id") {
            continue;
        }
        let mut it = line.split('\t');
        let Some(id) = it.next().and_then(|s| s.trim().parse::<u32>().ok()) else {
            continue;
        };
        let Some(ssid) = it.next().map(|s| s.trim()) else {
            continue;
        };
        if ssid.is_empty() {
            continue;
        }
        out.push((id, ssid.to_string()));
    }
    out
}

/// `list_networks` 里 ssid 对应的网络 id。
pub fn network_id(nets: &[(u32, String)], ssid: &str) -> Option<u32> {
    nets.iter().find(|(_, n)| n == ssid).map(|(id, _)| *id)
}

/// 已有的网络块里找 ssid。找不到就返回 Err —— `forget` 的"没这个网络"分支。
pub fn forget_id(nets: &[(u32, String)], ssid: &str) -> Result<u32, String> {
    network_id(nets, ssid).ok_or_else(|| format!("{ssid} is not a saved network"))
}

fn list_networks() -> Result<Vec<(u32, String)>, String> {
    Ok(parse_list_networks(&run(&["list_networks"])?))
}

/// 已知（配置里保存过）的网络名。`wpa_supplicant` 没起时返回空列表。
pub fn known() -> Vec<String> {
    list_networks()
        .map(|v| v.into_iter().map(|(_, s)| s).collect())
        .unwrap_or_default()
}

/// 解析 `wpa_cli status` 的 key=value 文本，抽 `wpa_state` / `ssid`。
///
/// 未关联时 `ssid=` 是空的，两行都可能缺（`status` 在接口没起来时只有
/// `wpa_state=INTERFACE_DISABLED`），所以两个返回值都是 Option。
pub fn parse_status(text: &str) -> (Option<String>, Option<String>) {
    let mut state = None;
    let mut ssid = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("wpa_state=") {
            if !v.trim().is_empty() {
                state = Some(v.trim().to_string());
            }
        } else if let Some(v) = line.strip_prefix("ssid=") {
            if !v.trim().is_empty() {
                ssid = Some(v.trim().to_string());
            }
        }
    }
    (state, ssid)
}

// ---------------------------------------------------------------------------
// 连不上的排查（C20）：多档配置依次试 + 把 supplicant 的失败输出留下来
//
// 实测：校园网 `Campus-WLAN`（PEAP / 开放 + 门户）能连，手机热点连不上。差别在
// 「这套配置只会写一种：`psk` + `key_mgmt=WPA-PSK`」，而手机热点（iPhone
// 个人热点 / Android 便携热点）基本都开了 WPA3 或 WPA2/WPA3 过渡 + PMF：
//
// | 可能原因 | 怎么证实 | 这里怎么处理 |
// |---|---|---|
// | `key_mgmt` 只写 `WPA-PSK`，SAE / 过渡模式不匹配 | `wpa_state` 停在 `SCANNING`/`ASSOCIATING`；`scan_results` 的 flags 是 `[WPA2-PSK+SAE-CCMP]` | 逐档下发 `WPA-PSK` / `SAE` / `WPA-PSK SAE` |
// | 缺 `ieee80211w`（PMF），关联/握手被拒 | `CTRL-EVENT-ASSOC-REJECT ... status_code=`、`4-Way Handshake failed`、`SSID-TEMP-DISABLED` | 逐档补 `ieee80211w=1` / `=2` |
// | 隐藏 SSID | `scan_results` 里没有这个 SSID | 全程 `scan_ssid=1` |
// | SSID 里的引号/反斜杠/空格/emoji/非 ASCII | `set_network ssid` 回 `FAIL`，或 SSID 被截断 | SSID 一律发十六进制字节 |
// | 5 GHz-only 热点 | `scan_results` 里 frequency ≥ 5000 | 记进日志（射频/信道我们改不了） |
// | `country`/regdom 没设，信道不可用 | `iw reg get` 首行是 `country 00` | 记进日志 |
// | 关联成功但 udhcpc 没拿到地址 | `wpa_state=COMPLETED` 且 `ip addr` 里没有 `inet` | **单独**报「没 IP」，不和关联失败混在一起 |
//
// 关键：**失败输出不丢**。每一档都留 `wpa_cli status` 原文，加上 `wpa_cli -a`
// 收的 `CTRL-EVENT-*`、`wpa_cli scan_results` 里目标 AP 的 flags、regdom，
// 全部追加进 `/tmp/rmdev-wifi.log`；界面上只给一行英文原因（`notice`）。
// 真机上失败一次，`cat /tmp/rmdev-wifi.log` 就能拿到证据。

/// 每一档最多等多久。四档 × 5 s ≈ 20 s，比 `poll_wifi_conn` 的 45 s 看门狗
/// （`main.rs`）短一截 —— 超了它会把原因吞成 `timed out`，证据就白留了。
const ATTEMPT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 取证日志。设备上 `cat` 这个文件就是一次完整的失败现场。
pub const WIFI_LOG: &str = "/tmp/rmdev-wifi.log";

/// 一档 `wpa_supplicant` 配置。失败就换下一档。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Attempt {
    /// `key_mgmt` 原文。空格分隔 = 双写（WPA2/WPA3 过渡模式）。
    pub key_mgmt: &'static str,
    /// `ieee80211w`：`None` = 不下发（沿用 supplicant 默认，等于改动前的行为）。
    pub ieee80211w: Option<&'static str>,
    /// 日志里的名字。
    pub label: &'static str,
}

/// 尝试阶梯，按「最常见、副作用最小」排序：
///
/// 1. `WPA-PSK`：WPA2-PSK，改动前唯一的行为（校园网 / 老路由器走这条）；
/// 2. `SAE` + `ieee80211w=1`：WPA3-SAE，PMF capable；
/// 3. `WPA-PSK` + `ieee80211w=1`：WPA2 + PMF capable —— iPhone 个人热点、
///    Android「WPA2/WPA3 过渡」最常见的组合；
/// 4. `WPA-PSK SAE` + `ieee80211w=2`：过渡模式双写 + PMF 强制（最严格）。
///
/// `scan_ssid=1` 每一档都带（隐藏 SSID 必需），第 4 档再显式写一次，让
/// 「隐藏热点 + 强制 PMF」这种最刁的组合也有一次完整机会。
pub fn plan(open: bool) -> Vec<Attempt> {
    if open {
        return vec![Attempt {
            key_mgmt: "NONE",
            ieee80211w: None,
            label: "open",
        }];
    }
    vec![
        Attempt {
            key_mgmt: "WPA-PSK",
            ieee80211w: None,
            label: "WPA2-PSK",
        },
        Attempt {
            key_mgmt: "SAE",
            ieee80211w: Some("1"),
            label: "WPA3-SAE + PMF capable",
        },
        Attempt {
            key_mgmt: "WPA-PSK",
            ieee80211w: Some("1"),
            label: "WPA2-PSK + PMF capable",
        },
        Attempt {
            key_mgmt: "WPA-PSK SAE",
            ieee80211w: Some("2"),
            label: "WPA2/WPA3 mixed + PMF required + scan_ssid",
        },
    ]
}

/// SSID → `wpa_cli set_network` 能吃的十六进制串。
///
/// `wpa_cli` 走的是串口式文本协议（空格分隔、`"` 包字符串、`\` 转义），
/// 引号 / 反斜杠 / 空格 / emoji / 非 ASCII 全靠那一层转义很容易出错（SSID 叫
/// `a"b` 时参数会被截成两段，回 `FAIL`，现象是"点了没反应"）。而
/// `wpa_supplicant` 的 `set_network ssid` 本来就接受十六进制形式
/// （`wpa_config_parse_string`），直接发字节就没有任何歧义 —— 顺带把
/// "SSID 恰好是一串十六进制字符"这种歧义（`dead` vs `\xde\xad`）也解决了。
pub fn hex_ssid(ssid: &str) -> String {
    let mut out = String::with_capacity(ssid.len() * 2);
    for b in ssid.as_bytes() {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap_or('0'));
    }
    out
}

/// `ip -4 addr show dev wlan0` 里的 IPv4 地址（拿不到就是 `None`）。
///
/// 关联成功 ≠ 能用：`wpa_state=COMPLETED` 只说明四次握手过了，地址是
/// `udhcpc` 另一条路的事。把它分出来，"连不上热点"才不会和"热点不给 IP"
/// 混成一句话。
pub fn parse_ipv4(text: &str) -> Option<String> {
    text.split_whitespace()
        .skip_while(|w| *w != "inet")
        .nth(1)
        .map(|s| s.split('/').next().unwrap_or(s).to_string())
        .filter(|s| !s.is_empty())
}

/// `wpa_cli scan_results` 里目标 SSID 那一行原文（含 flags）。
///
/// 表头是 `bssid / frequency / signal level / flags / ssid`，SSID 里可能有
/// 空格，所以按 tab 切 5 段。flags 直接带 AKM 和 PMF 位，例如
/// `[WPA2-PSK+SAE-CCMP][MFPC][MFPR][ESS]` —— 判断"这个热点到底要什么"的
/// 最短路径，比解析 `iw scan` 的 RSN 段省事得多。
pub fn scan_line(text: &str, ssid: &str) -> Option<String> {
    for line in text.lines() {
        let mut it = line.splitn(5, '\t');
        let (Some(_b), Some(_f), Some(_s), Some(_flags), Some(name)) =
            (it.next(), it.next(), it.next(), it.next(), it.next())
        else {
            continue;
        };
        // SSID 末尾的空格在扫描结果里常被吞掉，两种都比一遍。
        if name == ssid || name.trim_end() == ssid.trim_end() && !ssid.is_empty() {
            return Some(line.to_string());
        }
    }
    None
}

/// `iw reg get` 首行的国家码（`country 00: DFS-UNSET` → `00`）。
///
/// `00` = 世界域，5 GHz 一大半信道和某些 2.4 G 信道不可用。我们不改
/// regdom（要 CRDA、还可能把 5 G 全关掉），只把它记进取证 —— 如果热点在
/// 12/13 信道或 5 GHz，这一行就是"扫不到"的直接解释。
pub fn parse_regdom(text: &str) -> Option<String> {
    text.lines()
        .next()?
        .trim()
        .strip_prefix("country ")
        .map(|s| s.split(':').next().unwrap_or(s).trim().to_string())
}

/// 从取证里挑一条最可能的原因（英文，直接进界面那一行）。
///
/// 证据优先级：四次握手失败（八成是密码）> 关联被拒 > 连扫描都没出结果 >
/// 走到了关联阶段又掉线（也是密码）> 其它（安全模式不支持）。
pub fn failure_reason(
    events: &[String],
    last_state: &str,
    saw_handshake: bool,
    reached_assoc: bool,
) -> &'static str {
    let has = |needle: &str| events.iter().any(|e| e.contains(needle));
    if has("4-Way Handshake failed") || has("pre-shared key") || saw_handshake {
        return "the password may be incorrect";
    }
    if has("ASSOC-REJECT") || has("authentication with") || has("SSID-TEMP-DISABLED") {
        return "the network rejected the association";
    }
    if last_state == "SCANNING" || last_state.is_empty() {
        return "the network was not found";
    }
    // rM2 上 `wpa_cli -a` 一条事件都不吐（2026-11 实测，连 SCAN-STARTED 都没有），
    // 所以 `events` 永远是空的：只能靠 `status` 轮询留下的痕迹。
    // "走到过关联阶段（ASSOCIATING 及以后）"说明 AP 在、关联也发出去了，
    // 那剩下的最常见原因就是口令 —— 报"安全模式不支持"会把人带偏。
    if reached_assoc {
        return "the password may be incorrect";
    }
    "the security mode is not supported"
}

/// 一次连接的取证：每一档一行 + 状态原文 + 事件原文 + 现场探针。
///
/// 失败时整段追加到 [`WIFI_LOG`]，界面上只留最后那一行原因。
#[derive(Default)]
struct Trace {
    lines: Vec<String>,
    events: Vec<String>,
    /// 最近一次看到的 `wpa_state`。
    last_state: String,
    /// 上一次记进 `lines` 的状态，用来去重（400 ms 轮一次，别刷屏）。
    seen: String,
    /// 这一次是不是走到过四次握手（走没走到是区分"没关联上"和"密码不对"的关键）。
    saw_handshake: bool,
    /// 这一次有没有离开过扫描阶段（`ASSOCIATING` 及以后）。事件通道在 rM2 上
    /// 是死的，这条是"AP 确实在、关联确实发出去了"的唯一轮询证据。
    reached_assoc: bool,
}

impl Trace {
    /// 记一行（`attempt`/`status`/探针都走这里）。
    fn note(&mut self, line: impl Into<String>) {
        self.lines.push(line.into());
    }

    /// 记一次 `wpa_cli status` 的原文。同一个状态只记一次。
    fn status(&mut self, raw: &str) {
        let (state, _) = parse_status(raw);
        let st = state.unwrap_or_default();
        if st == "4WAY_HANDSHAKE" {
            self.saw_handshake = true;
        }
        if matches!(st.as_str(), "ASSOCIATING" | "ASSOCIATED" | "4WAY_HANDSHAKE") {
            self.reached_assoc = true;
        }
        if st.is_empty() {
            return;
        }
        self.last_state = st.clone();
        if self.seen == st {
            return;
        }
        self.seen = st;
        // 多行原文压成一行：日志是给人 grep 的，不是给机器解析的。
        let one: Vec<&str> = raw.lines().filter(|l| !l.trim().is_empty()).collect();
        self.note(format!("  status: {}", one.join(" ")));
    }

    /// 失败收尾：整段写进日志 + stderr，返回给界面的一行英文原因。
    fn finish(&self, ssid: &str, why: &str) -> String {
        let mut buf = format!("=== {} {} ({}) ===\n", ts(), ssid, self.lines.len());
        for l in &self.lines {
            buf.push_str(l);
            buf.push('\n');
        }
        if !self.events.is_empty() {
            buf.push_str("--- events ---\n");
            for e in &self.events {
                buf.push_str(e);
                buf.push('\n');
            }
        }
        eprintln!("[wifi] 连接失败取证：\n{buf}");
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(WIFI_LOG)
        {
            let _ = f.write_all(buf.as_bytes());
        }
        format!("Could not connect to {ssid} ({why})")
    }
}

/// 尽力而为的 `CTRL-EVENT-*` 收集器。
///
/// `wpa_cli -a <脚本>` 在每次事件时执行脚本，同时把事件原文打到 stdout：我们
/// 不需要脚本做任何事（`/bin/true`），只收它的 stdout。supplicant 没起、
/// `wpa_cli` 不在、`/bin/true` 不存在……全都只意味着"少一份证据"，连接流程
/// 照常走。
struct EventTap {
    child: Option<std::process::Child>,
    lines: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl EventTap {
    fn start() -> EventTap {
        let lines: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        for p in WPA_CLI {
            let Ok(mut child) = std::process::Command::new(p)
                .args(["-a", "/bin/true"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
            else {
                continue;
            };
            if let Some(out) = child.stdout.take() {
                let store = lines.clone();
                std::thread::Builder::new()
                    .name("wpa-events".into())
                    .spawn(move || {
                        use std::io::BufRead;
                        for line in std::io::BufReader::new(out).lines() {
                            let Ok(line) = line else { break };
                            let l = line.trim().to_string();
                            if l.is_empty() {
                                continue;
                            }
                            let mut v = store.lock().unwrap_or_else(|e| e.into_inner());
                            if v.len() >= 40 {
                                v.remove(0);
                            }
                            v.push(l);
                        }
                    })
                    .ok();
            }
            return EventTap {
                child: Some(child),
                lines,
            };
        }
        EventTap { child: None, lines }
    }

    /// 收工：杀掉监听进程，把收集到的事件取出来。
    fn stop(&mut self) -> Vec<String> {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
        self.lines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

impl Drop for EventTap {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `wpa_cli add_network` 的 stdout → 网络 id。
///
/// 真机上是**两行**：`wpa_cli` 只有一个网卡又没给 `-i` 时，会先打一行
/// `Selected interface 'wlan0'`，id 才是第二行。整段 `.parse::<u32>()` 必然
/// 失败，于是每一档都在下发配置之前就 `continue` 掉了 —— 一次
/// `select_network` 都没发出去，`wpa_state` 停在 `SCANNING`，界面上就变成
/// "the network was not found"（2024-11 真机 iPhone 热点就是这么挂的）。
///
/// 只认最后一行非空内容：id 永远在最后，前面有几行噪音都不影响。
pub fn parse_add_network(out: &str) -> Option<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .next_back()
        .filter(|l| l.parse::<u32>().is_ok())
        .map(str::to_string)
}

/// `add_network` 拿一个空闲的网络 id。
fn add_network() -> Result<String, String> {
    let out = run(&["add_network"])?;
    parse_add_network(&out).ok_or_else(|| format!("wpa_cli add_network: {out}"))
}

/// 丢掉一个网络块。`select_network` 之后必须走这条：直接 `remove_network`
/// 正在用的那块会回 `FAIL`。
fn drop_network(id: &str) {
    let _ = run(&["disable_network", id]);
    let _ = run(&["remove_network", id]);
}

/// 一档配置要下发的 `set_network` 键值对。
///
/// 纯函数，单测直接看它 —— 真机上生成的 `wpa_supplicant` 配置就是这几行。
/// `ssid` 走十六进制（见 [`hex_ssid`]），`psk` 走引号（`wpa_supplicant` 要靠
/// 8..=63 的长度判断这是口令、不是 64 位 PSK）。
fn settings_for(a: &Attempt, open: bool, ssid: &str, psk: &str) -> Vec<(&'static str, String)> {
    let mut v = vec![
        ("ssid", hex_ssid(ssid)),
        // 隐藏 SSID 也要能连：不打开它 supplicant 只被动等广播。
        ("scan_ssid", "1".to_string()),
    ];
    if open {
        v.push(("key_mgmt", "NONE".to_string()));
    } else {
        v.push(("psk", quote(psk)));
        v.push(("key_mgmt", a.key_mgmt.to_string()));
    }
    if let Some(pmf) = a.ieee80211w {
        v.push(("ieee80211w", pmf.to_string()));
    }
    v.push(("priority", "1".to_string()));
    v
}

/// 把一档配置写进 `wpa_cli`（逐条 `set_network`，顺序无所谓：`enable_network`
/// 那一刻才读整块配置）。
fn apply(id: &str, a: &Attempt, ssid: &str, open: bool, psk: &str) -> Result<(), String> {
    for (name, value) in settings_for(a, open, ssid, psk) {
        run(&["set_network", id, name, &value])?;
    }
    Ok(())
}

/// 一档配置的结果。
enum Outcome {
    /// `wpa_state=COMPLETED`（关联 + 四次握手都过了；IP 是下一步的事）。
    Connected,
    /// 走到过四次握手又掉回 `DISCONNECTED`：这一档已经确定失败。
    Handshake,
    /// 等满了还没结论。
    TimedOut,
    /// `wpa_state=INTERFACE_DISABLED` / `UNINITIALIZED`：Wi-Fi 关了 / 网卡没起，
    /// 再试下一档也是白试。
    Hard(String),
}

/// 等这一档的结果。
fn wait(trace: &mut Trace, ssid: &str) -> Outcome {
    let t0 = std::time::Instant::now();
    loop {
        std::thread::sleep(POLL_INTERVAL);
        // 读状态失败（supplicant 刚好在重启）不算失败，继续等到超时。
        let raw = run(&["status"]).unwrap_or_default();
        let (state, cur) = parse_status(&raw);
        let Some(state) = state else {
            if t0.elapsed() >= ATTEMPT_TIMEOUT {
                return Outcome::TimedOut;
            }
            continue;
        };
        if state == "COMPLETED" && (cur.is_none() || cur.as_deref() == Some(ssid)) {
            trace.status(&raw);
            return Outcome::Connected;
        }
        if matches!(state.as_str(), "INTERFACE_DISABLED" | "UNINITIALIZED") {
            trace.status(&raw);
            return Outcome::Hard(state);
        }
        trace.status(&raw);
        // 到最后一次四次握手又断了：这一档确定失败，别把 5 s 等满。
        if trace.saw_handshake && state == "DISCONNECTED" {
            return Outcome::Handshake;
        }
        if t0.elapsed() >= ATTEMPT_TIMEOUT {
            return Outcome::TimedOut;
        }
    }
}

/// IPv4 地址（`ip addr` 读不到就是 `None`；本机开发环境也是 `None`）。
fn ipv4() -> Option<String> {
    let o = Tool::run(IP, &["-4", "addr", "show", "dev", "wlan0"])?;
    parse_ipv4(&o)
}

/// 关联成功之后把地址拿到手。
///
/// `rmdev-wifi.sh` 那轮循环在 `associated` 分支末尾要睡 25 s，所以我们刚
/// `COMPLETED` 时它多半还在睡 —— 这时候直接读 `ip addr` 必然没有 `inet`，
/// 界面就会报「连上了但没 IP」的**假失败**（用户看到的是"连不上"）。这里自己
/// 催一次 `udhcpc`：和 keeper 跑的是同一条命令，重复跑不会坏，只是多一次
/// DHCP 往返。催完再给 3 s 让它落地。
fn acquire_ipv4() -> Option<String> {
    if let Some(ip) = ipv4() {
        return Some(ip);
    }
    // `-n`：拿不到就退出（别挂后台）；`-t 3 -T 2`：三次、每次隔 2 s。
    let _ = Tool::run(UDHCPC, &["-i", "wlan0", "-q", "-n", "-t", "3", "-T", "2"]);
    for _ in 0..6 {
        if let Some(ip) = ipv4() {
            return Some(ip);
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    None
}

/// `select_network` 会顺手把其它网络置成 disabled，失败后必须放回来 ——
/// 否则连热点失败一次，原来能用的校园网也回不来了。
fn restore_others() {
    if let Ok(nets) = list_networks() {
        for (id, _) in nets {
            let _ = run(&["enable_network", &id.to_string()]);
        }
    }
    let _ = run(&["reconnect"]);
}

/// 连一个网络：多档配置依次试，失败把 supplicant 的输出留在 [`WIFI_LOG`]。
///
/// 步骤（见 `docs/CLOUD.md`：真正的连接归系统服务，app 只下发配置）：
/// `add_network → set_network ... → enable_network → select_network`，然后轮询
/// `status` 到 `COMPLETED`（每档 ≤ [`ATTEMPT_TIMEOUT`]，最多 [`plan`] 那么多档）。
/// 只有成功那一档才 `save_config`，失败的临时网络块全部删掉 —— 一次点按不会
/// 在 `/etc/wpa_supplicant.conf` 里留下四五个垃圾块。
///
/// 返回的 `Err` 是**一行英文原因**（`notice` 直接显示）；完整取证在日志里。
pub fn connect(ssid: &str, open: bool, psk: &str) -> Result<(), String> {
    // 校验放最前面：参数不合法时一个命令都不发（单测因此不用真跑 wpa_cli）。
    check(ssid, open, psk)?;
    // 已经连着的就是这个网络：别再下发一遍（重复 `set_network psk` 会白白掉线重连）。
    // 但要顺手确认拿到地址了没有 —— "连上了但没 IP"得报出来。
    if let Ok(text) = run(&["status"]) {
        let (state, cur) = parse_status(&text);
        if state.as_deref() == Some("COMPLETED") && cur.as_deref() == Some(ssid) {
            return match acquire_ipv4() {
                Some(_) => Ok(()),
                None => Err(format!("Connected to {ssid} but no IP address was assigned")),
            };
        }
    }
    // 同一个 SSID 的旧块先删掉：不删的话每点一次就多一个块，重连时还会
    // 挑到旧密码那块。`select_network` 之后直接 remove 会 FAIL，所以先 disable。
    for (id, _) in list_networks()?.iter().filter(|(_, n)| n == ssid) {
        drop_network(&id.to_string());
    }

    let steps = plan(open);
    let mut trace = Trace::default();
    trace.note(format!(
        "open={open} ssid={} bytes={}",
        ssid,
        ssid.len()
    ));
    // 现场探针：目标 AP 的 flags（AKM / PMF）+ regdom。
    if let Ok(t) = run(&["scan_results"]) {
        trace.note(match scan_line(&t, ssid) {
            Some(l) => format!("ap: {l}"),
            None => "ap: 不在 scan_results 里（没扫到 / 隐藏 SSID）".to_string(),
        });
    }
    if let Some(r) = Tool::run(IW, &["reg", "get"]).as_deref().and_then(parse_regdom) {
        trace.note(format!("regdom: {r}"));
    }
    let mut tap = EventTap::start();

    for (i, a) in steps.iter().enumerate() {
        trace.note(format!("[{}/{}] {}", i + 1, steps.len(), a.label));
        let id = match add_network() {
            Ok(id) => id,
            Err(e) => {
                trace.note(format!("  add_network: {e}"));
                continue;
            }
        };
        if let Err(e) = apply(&id, a, ssid, open, psk) {
            trace.note(format!("  set_network: {e}"));
            drop_network(&id);
            continue;
        }
        if let Err(e) = run(&["enable_network", &id]).and_then(|_| run(&["select_network", &id])) {
            trace.note(format!("  select_network: {e}"));
            drop_network(&id);
            continue;
        }
        match wait(&mut trace, ssid) {
            Outcome::Connected => {
                let _ = run(&["save_config"]);
                trace.events = tap.stop();
                return match acquire_ipv4() {
                    Some(ip) => {
                        trace.note(format!("  ip_address={ip}"));
                        eprintln!("[wifi] {ssid} 已连上（{ip}，{}）", a.label);
                        Ok(())
                    }
                    // 关联 + 握手都过了，但 udhcpc 没拿到地址：和"连不上"分开报。
                    None => {
                        trace.note("  ip_address=(none)");
                        Err(trace.finish(ssid, "no IP address was assigned"))
                    }
                };
            }
            Outcome::Hard(state) => {
                trace.note(format!("  wpa_state={state}"));
                drop_network(&id);
                trace.events = tap.stop();
                restore_others();
                return Err(format!("Could not connect to {ssid}: wpa_state={state}"));
            }
            Outcome::Handshake | Outcome::TimedOut => drop_network(&id),
        }
    }
    trace.events = tap.stop();
    restore_others();
    let why = failure_reason(
        &trace.events,
        &trace.last_state,
        trace.saw_handshake,
        trace.reached_assoc,
    );
    Err(trace.finish(ssid, why))
}

/// 从配置里删掉某网络。
///
/// 先 `disable_network` 再 `remove_network`：`wpa_supplicant` 不允许直接删
/// 正在用的那个网络块（回 `FAIL`），禁用之后就删得掉了。最后 `save_config`
/// 落盘，否则重启又回来了。
pub fn forget(ssid: &str) -> Result<(), String> {
    let id = forget_id(&list_networks()?, ssid)?;
    let id = id.to_string();
    let _ = run(&["disable_network", &id]);
    let _ = run(&["remove_network", &id]);
    // 删没删掉以 `list_networks` 为准：`remove_network` 的 FAIL 也可能是
    // "网络不存在"，只有列表里确实没了才算成功。
    if list_networks()?.iter().any(|(_, n)| n == ssid) {
        return Err(format!("Could not forget {ssid}"));
    }
    run(&["save_config"])?;
    Ok(())
}
// === C0W end ===

#[cfg(test)]
mod scan_results_tests {
    use super::*;

    #[test]
    fn unescapes_iw_hex_ssids() {
        assert_eq!(unescape_iw_ssid("iPhone\\x20"), "iPhone ");
        assert_eq!(unescape_iw_ssid("Campus-WLAN"), "Campus-WLAN");
        assert_eq!(unescape_iw_ssid("a\\x2db"), "a-b");
    }

    #[test]
    fn parses_wpa_scan_results_and_dedupes() {
        let t = "bssid / frequency / signal level / flags / ssid\n\
                 aa:bb:cc:dd:ee:ff\t5180\t-45\t[WPA2-PSK-CCMP][ESS]\tCampus-WLAN\n\
                 aa:bb:cc:dd:ee:00\t2412\t-70\t[WPA2-PSK-CCMP][ESS]\tCampus-WLAN\n\
                 aa:bb:cc:dd:ee:11\t2412\t-60\t[ESS]\tMy Hotspot\n";
        let v = parse_scan_results(t);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].ssid, "Campus-WLAN");
        assert!(v[0].secure);
        assert_eq!(v[0].signal, Some(-45));
        assert_eq!(v[1].ssid, "My Hotspot");
        assert!(!v[1].secure);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IW_SCAN: &str = "BSS 00:11:22:33:44:55(on wlan0) -- associated
\tlast seen: 12294.660s [boottime]
\tfreq: 5180
\tsignal: -60.00 dBm
\tcapability: ESS Privacy SpectrumMgmt (0x1511)
\tSSID: Campus-WLAN
\tRSN:\t * Version: 1
\t\t * Group cipher: CCMP

BSS aa:bb:cc:dd:ee:ff(on wlan0)
\tfreq: 2412
\tsignal: -78.50 dBm
\tcapability: ESS (0x0411)
\tSSID: Cafe-Guest

BSS 11:22:33:44:55:66(on wlan0)
\tfreq: 2437
\tsignal: -45.00 dBm
\tcapability: ESS Privacy (0x0411)
\tSSID: Home
\tWPA:\t * Version: 1
";

    #[test]
    fn parses_iw_scan_with_security_and_signal() {
        let v = parse_iw_scan(IW_SCAN);
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].ssid, "Home", "按信号从强到弱");
        assert_eq!(v[0].signal, Some(-45));
        assert!(v[0].secure);
        let cafe = v.iter().find(|n| n.ssid == "Cafe-Guest").unwrap();
        assert!(!cafe.secure);
        assert_eq!(cafe.signal, Some(-79));
        let campus = v.iter().find(|n| n.ssid == "Campus-WLAN").unwrap();
        assert!(campus.secure);
    }

    #[test]
    fn skips_hidden_and_dedups_strongest() {
        let s = "BSS aa
\tsignal: -80.00 dBm
\tSSID: 
BSS bb
\tsignal: -70.00 dBm
\tSSID: Same
BSS cc
\tsignal: -40.00 dBm
\tSSID: Same
";
        let v = parse_iw_scan(s);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].signal, Some(-40));
    }

    #[test]
    fn parses_iwlist() {
        let s = "wlan0     Scan completed :
          Cell 01 - Address: 00:11:22:33:44:55
                    Channel:6
                    Quality=70/70  Signal level=-40 dBm
                    Encryption key:on
                    ESSID:\"Office\"
          Cell 02 - Address: 00:11:22:33:44:66
                    Signal level=-75 dBm
                    Encryption key:off
                    ESSID:\"Open\"
";
        let v = parse_iwlist(s);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].ssid, "Office");
        assert!(v[0].secure);
        assert_eq!(v[1].ssid, "Open");
        assert!(!v[1].secure);
    }

    #[test]
    fn signal_bars_match_official_thresholds() {
        assert_eq!(signal_bars(-40), 3);
        assert_eq!(signal_bars(-50), 2, "官方是 > -50 才三格");
        assert_eq!(signal_bars(-59), 2);
        assert_eq!(signal_bars(-60), 1);
        assert_eq!(signal_bars(-69), 1);
        assert_eq!(signal_bars(-70), 0);
    }

    #[test]
    fn parses_iw_link() {
        let s = "Connected to 00:11:22:33:44:55 (on wlan0)
\tSSID: Campus-WLAN
\tfreq: 5180
\tsignal: -58 dBm
";
        assert_eq!(
            parse_iw_link(s),
            Some(("Campus-WLAN".to_string(), Some(-58)))
        );
        assert_eq!(parse_iw_link("Not connected."), None);
    }

    #[test]
    fn parses_rfkill_list() {
        let s = "0: phy0: wlan
\tSoft blocked: no
\tHard blocked: no
";
        assert_eq!(parse_rfkill_blocked(s), Some(false));
        let s = "0: phy0: wlan\n\tSoft blocked: yes\n\tHard blocked: no\n";
        assert_eq!(parse_rfkill_blocked(s), Some(true));
        assert_eq!(parse_rfkill_blocked(""), None);
    }

    #[test]
    fn empty_scan_is_empty() {
        assert!(parse_iw_scan("").is_empty());
        assert!(parse_iwlist("").is_empty());
    }

    // === C0W begin: 连接 / 忘记 ===

    const STATUS_OK: &str = "bssid=00:11:22:33:44:55
freq=5180
ssid=Campus-WLAN
id=0
mode=station
pairwise_cipher=CCMP
group_cipher=CCMP
key_mgmt=WPA2-PSK
wpa_state=COMPLETED
ip_address=10.0.0.10
p2p_device_address=00:11:22:33:44:55
address=00:11:22:33:44:55
";

    #[test]
    fn parse_status_reads_state_and_ssid() {
        assert_eq!(
            parse_status(STATUS_OK),
            (Some("COMPLETED".to_string()), Some("Campus-WLAN".to_string()))
        );
    }

    #[test]
    fn parse_status_without_ssid() {
        // 未关联：`ssid=` 那行是空的
        let s = "wpa_state=DISCONNECTED\nssid=\naddress=00:11:22:33:44:55\n";
        assert_eq!(
            parse_status(s),
            (Some("DISCONNECTED".to_string()), None)
        );
        // 连 wpa_state 都没有（刚起来的 supplicant 只回一行）
        assert_eq!(parse_status("address=00:11:22:33:44:55\n"), (None, None));
    }

    #[test]
    fn parse_status_empty_is_none() {
        assert_eq!(parse_status(""), (None, None));
    }

    #[test]
    fn connect_rejects_short_passphrase() {
        // 参数校验在发命令之前：单测不会真跑 wpa_cli
        let e = connect("Home", false, "1234567").unwrap_err();
        assert!(e.contains("passphrase"), "错误串要带 passphrase：{e}");
        let e = connect("Home", false, "").unwrap_err();
        assert!(e.contains("passphrase"));
        // 64 字节超过 WPA2 passphrase 上限
        let long = "a".repeat(64);
        assert!(connect("Home", false, &long).err().unwrap().contains("passphrase"));
    }

    #[test]
    fn connect_rejects_bad_ssid() {
        // 32 字节正好是上限（这里只看校验，不看 wpa_cli 是否存在）
        assert!(check(&"a".repeat(32), true, "").is_ok());
        assert!(check(&"a".repeat(33), true, "").is_err());
        assert!(check("", true, "").is_err());
        // 开放网络不看 psk
        assert!(check("Cafe", true, "").is_ok());
    }

    #[test]
    fn parse_add_network_skips_selected_interface_line() {
        // 真机 `wpa_cli add_network` 的原始输出（2024-11 rM2）：
        //   Selected interface 'wlan0'
        //   13
        // 整段解析会失败，四档配置全被跳过 → "the network was not found"。
        assert_eq!(
            parse_add_network("Selected interface 'wlan0'\n13\n").as_deref(),
            Some("13")
        );
        assert_eq!(parse_add_network("0").as_deref(), Some("0"));
        assert_eq!(parse_add_network("FAIL"), None);
        assert_eq!(parse_add_network(""), None);
    }

    #[test]
    fn quote_escapes_quotes_and_backslashes() {
        // `wpa_cli` 里 ssid `a"b` 要写成 "\"a\"b\""
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
        assert_eq!(quote("x\\y"), "\"x\\\\y\"");
        // 普通名字只加一层引号（空格也在这一层里被保住）
        assert_eq!(quote("Cafe Guest"), "\"Cafe Guest\"");
        assert_eq!(quote(""), "\"\"");
        // 反斜杠 + 引号连着：两个都要转义
        assert_eq!(quote("\\\""), "\"\\\\\\\"\"");
    }

    #[test]
    fn parse_list_networks_skips_header_and_empty_ssid() {
        let s = "network id / ssid / bssid / flags
0	Campus-WLAN	any	[CURRENT]
1	Home	any
2		any
";
        assert_eq!(
            parse_list_networks(s),
            vec![(0, "Campus-WLAN".to_string()), (1, "Home".to_string())]
        );
        assert!(parse_list_networks("").is_empty());
    }

    #[test]
    fn forget_missing_network_is_err() {
        let nets = vec![(0, "Campus-WLAN".to_string()), (1, "Home".to_string())];
        // `forget` 的"配置里没这个网络"分支就是 `forget_id`，不用真跑 wpa_cli
        assert_eq!(forget_id(&nets, "Home"), Ok(1));
        let e = forget_id(&nets, "Nope").unwrap_err();
        assert!(e.contains("not a saved network"), "错误串要说明原因：{e}");
        assert_eq!(network_id(&nets, "Nope"), None);
    }
    // === C0W end ===

    // ---- 热点连不上：配置阶梯 / 取证 ----

    /// 阶梯必须从"改动前的行为"开始，然后才敢往 SAE / PMF 上加 —— 已经有
    /// 能用的网（校园网）的用户，第一条不能变。
    #[test]
    fn plan_starts_with_plain_wpa2_and_covers_sae_pmf() {
        let p = plan(false);
        assert_eq!(p.len(), 4, "四档：{p:?}");
        assert_eq!(p[0].key_mgmt, "WPA-PSK");
        assert_eq!(p[0].ieee80211w, None, "第一档不写 PMF，等于改动前的行为");
        // SAE 至少出现一档，而且必须带 PMF（WPA3 规范里 PMF 是强制的）
        let sae: Vec<&Attempt> = p.iter().filter(|a| a.key_mgmt.contains("SAE")).collect();
        assert!(!sae.is_empty(), "要有 WPA3-SAE 档");
        assert!(sae.iter().any(|a| a.ieee80211w.is_some()), "SAE 档要带 PMF");
        // WPA2 + PMF capable（热点最常见）必须在
        assert!(
            p.iter().any(|a| a.key_mgmt == "WPA-PSK" && a.ieee80211w == Some("1")),
            "要有 WPA2-PSK + ieee80211w=1 档"
        );
        // 过渡模式双写 + PMF 强制收尾
        assert!(p[3].key_mgmt.contains("SAE") && p[3].ieee80211w == Some("2"));
        // 开放网络只有一档，且不写 psk
        let o = plan(true);
        assert_eq!(o.len(), 1);
        assert_eq!(o[0].key_mgmt, "NONE");
    }

    /// `ssid` 一律发十六进制：空格/引号/反斜杠/emoji/非 ASCII 都不会走到
    /// wpa_cli 的转义那一层。顺手钉住"全是十六进制字符的 SSID 也不会被
    /// 当成字节串解错"（`dead` → `64656164`，不是 `\xde\xad`）。
    #[test]
    fn hex_ssid_survives_quotes_and_non_ascii() {
        assert_eq!(hex_ssid("Home"), "486f6d65");
        assert_eq!(hex_ssid("My Phone"), "4d792050686f6e65");
        assert_eq!(hex_ssid("a\"b"), "612262");
        assert_eq!(hex_ssid("x\\y"), "785c79");
        assert_eq!(hex_ssid("dead"), "64656164");
        // iPhone 默认名里的弯曲撇号（3 字节）+ 中文
        assert_eq!(hex_ssid("’"), "e28099");
        assert_eq!(hex_ssid("热点"), "e783ade782b9");
        // 32 字节上限 → 64 个十六进制字符
        assert_eq!(hex_ssid(&"a".repeat(32)).len(), 64);
    }

    /// 每一档真的生成的 `set_network` 行（这就是设备上会看到的配置样例）。
    #[test]
    fn settings_for_emits_wpa2_then_sae_then_pmf() {
        let wpa2 = settings_for(&plan(false)[0], false, "My Phone", "hunter2!!");
        let get = |k: &str| {
            wpa2.iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("ssid").as_deref(), Some("4d792050686f6e65"));
        assert_eq!(get("psk").as_deref(), Some("\"hunter2!!\""));
        assert_eq!(get("key_mgmt").as_deref(), Some("WPA-PSK"));
        assert_eq!(get("scan_ssid").as_deref(), Some("1"), "隐藏 SSID 必需");
        assert_eq!(get("ieee80211w"), None, "第一档不写 PMF");
        assert_eq!(get("priority").as_deref(), Some("1"));

        // PMF capable 档
        let pmf = settings_for(&plan(false)[2], false, "Hotspot", "12345678");
        assert_eq!(
            pmf.iter()
                .find(|(n, _)| *n == "ieee80211w")
                .map(|(_, v)| v.as_str()),
            Some("1")
        );
        // 过渡模式档：key_mgmt 双写 + PMF 强制
        let mixed = settings_for(&plan(false)[3], false, "Hotspot", "12345678");
        assert_eq!(
            mixed
                .iter()
                .find(|(n, _)| *n == "key_mgmt")
                .map(|(_, v)| v.as_str()),
            Some("WPA-PSK SAE")
        );
        assert_eq!(
            mixed
                .iter()
                .find(|(n, _)| *n == "ieee80211w")
                .map(|(_, v)| v.as_str()),
            Some("2")
        );
        // 开放网络不发 psk
        let open = settings_for(&plan(true)[0], true, "Cafe", "");
        assert!(!open.iter().any(|(n, _)| *n == "psk"));
        assert_eq!(
            open.iter()
                .find(|(n, _)| *n == "key_mgmt")
                .map(|(_, v)| v.as_str()),
            Some("NONE")
        );
    }

    /// "关联成功但没 IP" 必须和 "连不上" 分开：前者 `wpa_state=COMPLETED`，
    /// 但 `ip addr` 里没有 `inet`。
    #[test]
    fn parse_ipv4_reads_inet_line() {
        let s = "2: wlan0    inet 10.0.0.10/22 brd 10.0.3.255 scope global wlan0\n\
                 valid_lft forever preferred_lft forever\n";
        assert_eq!(parse_ipv4(s), Some("10.0.0.10".to_string()));
        // 只有 IPv6 / 什么都没拿到
        assert_eq!(parse_ipv4("2: wlan0    inet6 fe80::1/64 scope link\n"), None);
        assert_eq!(parse_ipv4(""), None);
    }

    /// `scan_results` 里目标 AP 的 flags（AKM + PMF）—— 判断热点要什么的最短路径。
    #[test]
    fn scan_line_finds_ap_flags_with_spaces_in_ssid() {
        let s = "bssid / frequency / signal level / flags / ssid\n\
                 00:11:22:33:44:55\t5180\t-58\t[WPA2-PSK+SAE-CCMP][MFPC][ESS]\tMy iPhone\n\
                 11:22:33:44:55:66\t2412\t-70\t[WPA2-PSK-CCMP][ESS]\tHome\n";
        let l = scan_line(s, "My iPhone").unwrap();
        assert!(l.contains("[WPA2-PSK+SAE-CCMP][MFPC][ESS]"), "flags 要带上：{l}");
        assert!(l.contains("5180"), "频段要带上（5 GHz-only 热点靠它认）");
        assert!(scan_line(s, "Nope").is_none());
        // 表头不能算命中
        assert!(scan_line("bssid / frequency / signal level / flags / ssid\n", "flags").is_none());
    }

    #[test]
    fn parse_regdom_reads_country_line() {
        assert_eq!(
            parse_regdom("country 00: DFS-UNSET\n\t(2402 - 2472 @ 40), (N/A, 20), (N/A)\n"),
            Some("00".to_string())
        );
        assert_eq!(parse_regdom("country CN: DFS-ETSI\n"), Some("CN".to_string()));
        assert_eq!(parse_regdom(""), None);
    }

    /// 界面那一行原因的挑选顺序：握手失败（八成密码）> 关联被拒 > 没扫到。
    #[test]
    fn failure_reason_prefers_the_strongest_evidence() {
        let ev = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            failure_reason(
                &ev(&["<3>CTRL-EVENT-ASSOC-REJECT bssid=00:11:22:33:44 status_code=1"]),
                "DISCONNECTED",
                false,
                true
            ),
            "the network rejected the association"
        );
        assert_eq!(
            failure_reason(
                &ev(&["<3>4-Way Handshake failed - pre-shared key may be incorrect"]),
                "DISCONNECTED",
                false,
                true
            ),
            "the password may be incorrect"
        );
        // 有握手痕迹（哪怕事件没抓到）也算密码问题
        assert_eq!(
            failure_reason(&[], "4WAY_HANDSHAKE", true, true),
            "the password may be incorrect"
        );
        // 一直停在扫描 = 根本没找到这个网络
        assert_eq!(
            failure_reason(&[], "SCANNING", false, false),
            "the network was not found"
        );
        // 事件通道是死的（rM2 实测）：走到过关联阶段又掉回 DISCONNECTED，
        // 唯一还剩的证据就是"关联确实发出去了"，按密码不对报。
        assert_eq!(
            failure_reason(&[], "DISCONNECTED", false, true),
            "the password may be incorrect"
        );
        // 既没事件也没状态：退到"安全模式不支持"
        assert_eq!(
            failure_reason(&[], "ASSOCIATING", false, false),
            "the security mode is not supported"
        );
    }
}
