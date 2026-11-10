//! Developer 页的真实数据：IP 地址 / MAC 地址。
//!
//! 官方 `res_06887bf.qml` 在 Prod 构建里就三块（顺序照抄）：
//!
//! 1. `SSH root password`（`Settings.developerPassword`）
//! 2. `IP addresses` + `MAC address`（`WifiManager.ipAddresses` / `hardwareAddress`）
//! 3. `Design grid` 开关（`Show the design grid on top of the app UI`）
//!
//! 我们只能给第 2 块真值：root 密码是 xochitl 按序列号派生的，干净重写里
//! 没有这条链路；design grid 是官方 UI 的调试网格，也没实现。这两块在页面上
//! 按任务契约灰显 `Not available`（这串官方 QML 里没有，是本项目统一的占位词）。

/// 页面标题（官方 `TextHeaderLarge`）。
pub const TITLE: &str = "Developer settings";
/// 第 1 块的行标题。
pub const SSH_PASSWORD: &str = "SSH root password";
/// 第 2 块的两个行标题。
pub const IP_ADDRESSES: &str = "IP addresses";
pub const MAC_ADDRESS: &str = "MAC address";
/// 第 3 块：开关行 + 副标题。
pub const DESIGN_GRID: &str = "Design grid";
pub const DESIGN_GRID_TEXT: &str = "Show the design grid on top of the app UI";
/// 没实现 / 拿不到的行的值。
pub const NOT_AVAILABLE: &str = "Not available";

const IP: &[&str] = &["/sbin/ip", "/usr/sbin/ip", "/usr/bin/ip", "ip"];
const MAC_PATH: &str = "/sys/class/net/wlan0/address";

/// 解析 `ip -o -4 addr show` 的输出，按出现顺序返回点分 IPv4，去重。
///
/// 每行长这样（`-o` = 一行一条）：
/// `3: wlan0    inet 192.168.1.42/24 brd 192.168.1.255 scope global wlan0\ ...`
pub fn parse_ip_output(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        // 找 `inet` 后面那一段（`inet6` 不算）
        let mut it = line.split_whitespace();
        while let Some(tok) = it.next() {
            if tok != "inet" {
                continue;
            }
            let Some(cidr) = it.next() else { continue };
            let addr = cidr.split('/').next().unwrap_or("");
            if is_ipv4(addr) && !out.iter().any(|a| a == addr) {
                out.push(addr.to_string());
            }
        }
    }
    out
}

/// 点分四段、每段数字且 0..=255。
pub fn is_ipv4(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    parts.len() == 4
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.len() <= 3
                && p.chars().all(|c| c.is_ascii_digit())
                && p.parse::<u8>().is_ok()
        })
}

/// `aa:bb:cc:dd:ee:ff` → Some；别的（`00:00:00:00:00:00`、空、乱码）→ None。
pub fn parse_mac(s: &str) -> Option<String> {
    let s = s.trim().to_ascii_lowercase();
    let groups: Vec<&str> = s.split(':').collect();
    let ok = groups.len() == 6
        && groups
            .iter()
            .all(|g| g.len() == 2 && g.chars().all(|c| c.is_ascii_hexdigit()));
    if !ok || groups.iter().all(|g| *g == "00") {
        return None;
    }
    Some(s)
}

/// 真机上的 IPv4 地址（读不到返回空）。
pub fn ip_addresses() -> Vec<String> {
    for p in IP {
        if let Ok(o) = std::process::Command::new(p)
            .args(["-o", "-4", "addr", "show"])
            .output()
        {
            if o.status.success() {
                let v = parse_ip_output(&String::from_utf8_lossy(&o.stdout));
                if !v.is_empty() {
                    return v;
                }
            }
        }
    }
    Vec::new()
}

/// wlan0 的 MAC（本机开发机上没有 wlan0 → None）。
pub fn mac_address() -> Option<String> {
    parse_mac(&std::fs::read_to_string(MAC_PATH).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP_O: &str = "\
1: lo    inet 127.0.0.1/8 scope host lo\\       valid_lft forever preferred_lft forever
3: wlan0    inet 192.168.1.42/24 brd 192.168.1.255 scope global wlan0\\       valid_lft forever preferred_lft forever
4: usb0    inet 10.11.99.1/24 brd 10.11.99.255 scope global usb0\\       valid_lft forever preferred_lft forever
5: wlan0    inet6 fe80::1/64 scope link\\       valid_lft forever preferred_lft forever
";

    #[test]
    fn parses_ipv4_in_order_and_skips_ipv6() {
        assert_eq!(
            parse_ip_output(IP_O),
            ["127.0.0.1", "192.168.1.42", "10.11.99.1"]
        );
        assert!(parse_ip_output("").is_empty());
        assert!(parse_ip_output("garbage\nno inet here\n").is_empty());
    }

    #[test]
    fn ipv4_validation() {
        assert!(is_ipv4("10.11.99.1"));
        assert!(!is_ipv4("10.11.99"));
        assert!(!is_ipv4("10.11.99.256"));
        assert!(!is_ipv4("fe80::1"));
        assert!(!is_ipv4(""));
    }

    #[test]
    fn mac_validation() {
        assert_eq!(
            parse_mac("B8:27:EB:12:34:56\n").as_deref(),
            Some("b8:27:eb:12:34:56")
        );
        assert_eq!(parse_mac("00:00:00:00:00:00"), None, "全零 = 没有 MAC");
        assert_eq!(parse_mac("b8:27:eb:12:34"), None);
        assert_eq!(parse_mac("b8-27-eb-12-34-56"), None);
        assert_eq!(parse_mac(""), None);
    }

    #[test]
    fn strings_match_official_qml() {
        // 文案是 `res_06887bf.qml` 逐字抄的
        assert_eq!(TITLE, "Developer settings");
        assert_eq!(SSH_PASSWORD, "SSH root password");
        assert_eq!(IP_ADDRESSES, "IP addresses");
        assert_eq!(MAC_ADDRESS, "MAC address");
        assert_eq!(DESIGN_GRID, "Design grid");
        assert_eq!(DESIGN_GRID_TEXT, "Show the design grid on top of the app UI");
    }

    #[test]
    fn mac_reader_is_safe_off_device() {
        // 本机没有 wlan0：返回 None，不能 panic
        if let Some(m) = mac_address() {
            assert_eq!(parse_mac(&m).as_deref(), Some(m.as_str()));
        }
    }
}
