//! Help 页的真实数据：版本、构建、序列号。
//!
//! 设备的版本号**不在** `/etc/version`（那里面是构建时间戳 `20221208102714`），
//! 真正给用户看的 release version 在 `/usr/share/remarkable/update.conf`
//! 的 `REMARKABLE_RELEASE_VERSION=3.0.4.1305`。两个都读出来给 Help 显示。
//!
//! 序列号：`/sys/devices/soc0/serial_number`（有的话），否则
//! `/proc/cpuinfo` 的 `Serial : ...`（rM2 实测只有后者）。

#[derive(Clone, Debug, PartialEq)]
pub struct DeviceInfo {
    /// 3.0.4.1305
    pub version: String,
    /// 20221208102714（`/etc/version` 原样）
    pub build: String,
    pub serial: String,
    pub model: String,
    pub kernel: String,
}

impl Default for DeviceInfo {
    fn default() -> Self {
        DeviceInfo {
            version: "3.0.4.1305".into(),
            build: "20221208102714".into(),
            serial: "unknown".into(),
            model: "reMarkable 2".into(),
            kernel: "unknown".into(),
        }
    }
}

/// `update.conf` 这类 ini 的 `KEY=VALUE`。忽略注释行。
pub fn parse_ini(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == key {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// `/proc/cpuinfo` 里的 `Serial : 0123456789012`。
pub fn parse_cpuinfo_serial(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("serial") {
                let v = v.trim();
                if !v.is_empty() {
                    return Some(v.to_string());
                }
            }
        }
    }
    None
}

/// `20221208102714` → `2022-12-08 10:27:14`。不是 14 位数字就原样返回。
pub fn format_build(s: &str) -> String {
    let s = s.trim();
    if s.len() != 14 || !s.chars().all(|c| c.is_ascii_digit()) {
        return s.to_string();
    }
    let num = |a: usize, b: usize| -> u32 { s[a..b].parse().unwrap_or(0) };
    let (y, mo, d) = (num(0, 4), num(4, 6), num(6, 8));
    let (h, mi, sec) = (num(8, 10), num(10, 12), num(12, 14));
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 59 {
        return s.to_string();
    }
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{sec:02}")
}

fn read_trim(path: &str) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

pub fn read() -> DeviceInfo {
    let mut d = DeviceInfo::default();
    let build = read_trim("/etc/version");
    d.version = read_trim("/usr/share/remarkable/update.conf")
        .and_then(|s| parse_ini(&s, "REMARKABLE_RELEASE_VERSION"))
        .or_else(|| build.clone())
        .unwrap_or_else(|| d.version.clone());
    if let Some(b) = build {
        d.build = b;
    }
    d.serial = read_trim("/sys/devices/soc0/serial_number")
        .or_else(|| read_trim("/proc/cpuinfo").and_then(|s| parse_cpuinfo_serial(&s)))
        .unwrap_or_else(|| "unknown".into());
    d.model = read_trim("/sys/devices/soc0/machine")
        .or_else(|| read_trim("/proc/device-tree/model"))
        .map(|s| s.replace('\0', "").trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| d.model.clone());
    d.kernel = read_trim("/proc/sys/kernel/osrelease").unwrap_or_else(|| "unknown".into());
    d
}

/// About 页的静态合规声明（官方是 DeviceInfo.certifications，这里静态英文）。
pub const COMPLIANCE_TEXT: &str = "This device complies with Part 15 of the FCC Rules. \
Operation is subject to the following two conditions: (1) this device may not cause \
harmful interference, and (2) this device must accept any interference received, \
including interference that may cause undesired operation.";

/// 许可页的静态英文文本。设备上真正的许可文本在 xochitl 的 deviceinfo 里，这里给要点。
pub const LICENSE_TEXT: &str = "This device contains free and open source software. \
Portions are licensed under the GNU General Public License version 3, the GNU Lesser \
General Public License version 3, the MIT License and the BSD 2-Clause License. \
The Linux kernel, BusyBox and Qt are used under their respective licenses. \
Source code for the GPL and LGPL components is available from reMarkable on request. \
See https://support.remarkable.com for more information.";

pub const TUTORIAL_TEXT: &str = "The tutorial introduces the toolbar, page overview, \
layers and document settings the first time you use your reMarkable. Restarting it \
will show those tooltips again the next time you open a notebook.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_update_conf() {
        let s = "[General]
#REMARKABLE_RELEASE_VERSION={98DA7DF2}
#SERVER=https://example
GROUP=Prod
REMARKABLE_RELEASE_VERSION=3.0.4.1305
";
        assert_eq!(
            parse_ini(s, "REMARKABLE_RELEASE_VERSION").as_deref(),
            Some("3.0.4.1305")
        );
        assert_eq!(parse_ini(s, "GROUP").as_deref(), Some("Prod"));
        assert_eq!(parse_ini(s, "NOPE"), None);
    }

    #[test]
    fn parses_cpuinfo_serial() {
        let s = "processor\t: 0
Hardware\t: Freescale i.MX7 Dual (Device Tree)
Revision\t: 0000
Serial\t\t: 0123456789012
";
        assert_eq!(parse_cpuinfo_serial(s).as_deref(), Some("0123456789012"));
        assert_eq!(parse_cpuinfo_serial("Serial\t\t: \n"), None);
        assert_eq!(parse_cpuinfo_serial(""), None);
    }

    #[test]
    fn formats_build_stamp() {
        assert_eq!(format_build("20221208102714"), "2022-12-08 10:27:14");
        assert_eq!(format_build("nightly"), "nightly");
        assert_eq!(format_build("20221308102714"), "20221308102714", "月份非法就原样");
    }

    #[test]
    fn defaults_are_official_values() {
        let d = DeviceInfo::default();
        assert_eq!(d.version, "3.0.4.1305");
        assert_eq!(d.model, "reMarkable 2");
    }
}
