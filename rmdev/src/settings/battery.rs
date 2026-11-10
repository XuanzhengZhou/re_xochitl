//! 电池状态：`/sys/class/power_supply/max77818_battery/uevent`。
//!
//! rM2 上**没有**标准的 `battery/capacity` 文件，只有一个 `uevent`
//! （`POWER_SUPPLY_CAPACITY=100`），充电器是另一个目录
//! `/sys/class/power_supply/max77818-charger/`。所以这里自己扫目录 + 解析 uevent。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// 平均耗电的滑动窗口：过去 10 分钟。
pub const WINDOW: Duration = Duration::from_secs(10 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ChargeState {
    Charging,
    Discharging,
    Full,
    NotCharging,
    #[default]
    Unknown,
}

impl ChargeState {
    pub fn label(self) -> &'static str {
        match self {
            ChargeState::Charging => "Charging",
            ChargeState::Discharging => "Discharging",
            ChargeState::Full => "Full",
            ChargeState::NotCharging => "Not charging",
            ChargeState::Unknown => "Unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Battery {
    pub capacity: Option<u8>,
    pub state: ChargeState,
    /// 充电器在线（USB 插着）。
    pub usb_online: bool,
    /// 剩余可用时间（分钟）。只在放电且数值合理时才有。
    pub minutes_left: Option<u32>,
    /// 满电容量（µAh）：`charge_full` / `POWER_SUPPLY_CHARGE_FULL`。
    pub charge_full_uah: Option<u32>,
    /// 当前电量（µAh）：`charge_now` / `POWER_SUPPLY_CHARGE_NOW`。
    pub charge_now_uah: Option<u32>,
    /// 电压（µV）：`voltage_now` / `POWER_SUPPLY_VOLTAGE_NOW`。
    pub voltage_uv: Option<u32>,
    /// 瞬时电流（µA，可能带符号）：`current_now` / `POWER_SUPPLY_CURRENT_NOW`。
    pub current_ua: Option<i32>,
}

impl Battery {
    /// 当前电量（mAh）。`charge_now` 是 µAh；读不到返回 None。
    pub fn capacity_mah(&self) -> Option<f32> {
        self.charge_now_uah.map(|uah| uah as f32 / 1000.0)
    }

    /// 瞬时电流（mA）。
    pub fn current_ma(&self) -> Option<f32> {
        self.current_ua.map(|ua| ua as f32 / 1000.0)
    }

    /// 瞬时功率（mW）：µV × µA / 1e9。
    pub fn power_mw(&self) -> Option<f32> {
        let (uv, ua) = (self.voltage_uv? as f32, self.current_ua? as f32);
        Some(uv * ua / 1_000_000_000.0)
    }

    /// 插着电（Charging / Full）：这时不给"还能用多久"。
    pub fn is_charging(&self) -> bool {
        matches!(self.state, ChargeState::Charging | ChargeState::Full)
    }
}

/// 解析一个 power_supply uevent 文本块。
pub fn parse_uevent(text: &str) -> Battery {
    let mut b = Battery::default();
    let mut time_to_empty: Option<u32> = None;
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        match k.trim() {
            "POWER_SUPPLY_CAPACITY" => b.capacity = v.trim().parse::<u8>().ok(),
            "POWER_SUPPLY_STATUS" => b.state = parse_status(v),
            "POWER_SUPPLY_ONLINE" => b.usb_online = v.trim() == "1",
            "POWER_SUPPLY_TIME_TO_EMPTY_NOW" => time_to_empty = parse_minutes(v),
            // 电量/电压/电流的原始单位：µAh / µV / µA。
            "POWER_SUPPLY_CHARGE_FULL" => b.charge_full_uah = parse_u32(v),
            "POWER_SUPPLY_CHARGE_NOW" => b.charge_now_uah = parse_u32(v),
            "POWER_SUPPLY_VOLTAGE_NOW" => b.voltage_uv = parse_u32(v),
            "POWER_SUPPLY_CURRENT_NOW" => b.current_ua = v.trim().parse::<i32>().ok(),
            _ => {}
        }
    }
    // 行序不保证：等 status 读完再决定这个时间可不可信。
    if b.state == ChargeState::Discharging {
        b.minutes_left = time_to_empty;
    }
    // 有些内核只把在线状态放在 charger 的 uevent 里；`read()` 会补。
    if matches!(b.state, ChargeState::Charging) {
        b.usb_online = true;
    }
    b
}

pub fn parse_status(s: &str) -> ChargeState {
    match s.trim() {
        "Charging" => ChargeState::Charging,
        "Discharging" => ChargeState::Discharging,
        "Full" => ChargeState::Full,
        "Not charging" => ChargeState::NotCharging,
        _ => ChargeState::Unknown,
    }
}

/// `time_to_empty_now` 是秒。0 / 非数字 / 超过 24 小时都当读不到
/// （满电时这个值经常是几十万秒的垃圾数）。
pub fn parse_minutes(secs: &str) -> Option<u32> {
    let s = secs.trim().parse::<u64>().ok()?;
    if s == 0 || s > 24 * 3600 {
        return None;
    }
    Some(((s + 59) / 60) as u32)
}

/// µAh / µV 一类的非负整数属性。
fn parse_u32(s: &str) -> Option<u32> {
    s.trim().parse::<u32>().ok()
}

/// 读一个单独属性文件（`charge_full` / `charge_now` / …）。没有/非数字都返回 None。
fn attr_u32(dir: &Path, name: &str) -> Option<u32> {
    parse_u32(&std::fs::read_to_string(dir.join(name)).ok()?)
}

/// "5 h 20 min" / "45 min"
pub fn fmt_hm(minutes: u32) -> String {
    if minutes >= 60 {
        format!("{} h {} min", minutes / 60, minutes % 60)
    } else {
        format!("{minutes} min")
    }
}

/// 设置页那一行：`100% - Full` / `72% - Charging` / `72% - Discharging - about 5 h 20 min remaining`。
pub fn summary(b: &Battery) -> Option<String> {
    let cap = b.capacity?;
    let mut s = format!("{cap}% - {}", b.state.label());
    if b.state == ChargeState::Discharging {
        if let Some(m) = b.minutes_left {
            s.push_str(&format!(" - about {} remaining", fmt_hm(m)));
        }
    }
    Some(s)
}

/// 找一个电池目录：名字里带 battery 的 power_supply。
fn battery_dir(root: &Path) -> Option<PathBuf> {
    let mut best: Option<PathBuf> = None;
    for e in std::fs::read_dir(root).ok()?.flatten() {
        let name = e.file_name().to_string_lossy().to_lowercase();
        if !name.contains("battery") {
            continue;
        }
        let p = e.path();
        if p.join("uevent").is_file() {
            // max77818_battery 优先，其余（如 bq27xxx）作为后备。
            if name.contains("max77818") {
                return Some(p);
            }
            best = Some(p);
        }
    }
    best
}

pub fn read() -> Option<Battery> {
    read_from(Path::new("/sys/class/power_supply"))
}

/// 便于测试/别的板子：从指定 power_supply 根目录读。
pub fn read_from(root: &Path) -> Option<Battery> {
    let dir = battery_dir(root)?;
    let text = std::fs::read_to_string(dir.join("uevent")).ok()?;
    let mut b = parse_uevent(&text);
    // rM2 的 uevent 里有 VOLTAGE_NOW / CURRENT_NOW，但没有 CHARGE_*：
    // 有些内核把 charge_full / charge_now 放在单独属性文件里，这里补读一次。
    // 还是没有就保持 None —— 页面那条线不画，不影响其它内容。
    if b.charge_full_uah.is_none() {
        b.charge_full_uah = attr_u32(&dir, "charge_full");
    }
    if b.charge_now_uah.is_none() {
        b.charge_now_uah = attr_u32(&dir, "charge_now");
    }
    if b.voltage_uv.is_none() {
        b.voltage_uv = attr_u32(&dir, "voltage_now");
    }
    if b.current_ua.is_none() {
        b.current_ua = std::fs::read_to_string(dir.join("current_now"))
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok());
    }
    if !b.usb_online {
        // 充电器是独立目录（max77818-charger / ...），扫一遍 ONLINE。
        if let Ok(rd) = std::fs::read_dir(root) {
            for e in rd.flatten() {
                let p = e.path();
                let name = e.file_name().to_string_lossy().to_string();
                if name.contains("battery") {
                    continue;
                }
                if let Ok(u) = std::fs::read_to_string(p.join("uevent")) {
                    if u.lines()
                        .any(|l| l.starts_with("POWER_SUPPLY_ONLINE=1"))
                    {
                        b.usb_online = true;
                        break;
                    }
                }
            }
        }
    }
    if b.state == ChargeState::Unknown && b.usb_online {
        b.state = ChargeState::Charging;
    }
    // time_to_empty_now 只有在放电时可信；解析时状态可能还没读到（行序不定），补一次。
    if b.state == ChargeState::Discharging && b.minutes_left.is_none() {
        b.minutes_left = std::fs::read_to_string(dir.join("time_to_empty_now"))
            .ok()
            .and_then(|s| parse_minutes(&s));
    }
    Some(b)
}

/// 过去 10 分钟的电流采样（mA）。`tick()` 每 30 s 塞一个点。
///
/// 窗口外的样本在 `push` 时就丢掉，所以长度最多 21 个（10 分钟 / 30 秒）。
#[derive(Clone, Debug, Default)]
pub struct Usage {
    samples: VecDeque<(Instant, f32)>,
}

impl Usage {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::new(),
        }
    }

    /// 记一次电流，并丢掉 10 分钟以前的样本。
    pub fn push(&mut self, now: Instant, current_ma: f32) {
        if !current_ma.is_finite() {
            return;
        }
        self.samples.push_back((now, current_ma));
        while let Some(&(t, _)) = self.samples.front() {
            if now.saturating_duration_since(t) > WINDOW {
                self.samples.pop_front();
            } else {
                break;
            }
        }
    }

    /// `window` 内样本的平均电流。样本 < 2 返回 None（一个点没有"平均"可言）。
    ///
    /// 窗口以**最新一个样本**为右端，而不是墙钟：tick 被睡眠/后台任务打断时，
    /// 用墙钟会把整段历史都算成"过期"，页面就永远只剩一个点。
    pub fn avg_ma(&self, window: Duration) -> Option<f32> {
        let last = self.samples.back()?.0;
        let mut sum = 0.0;
        let mut n = 0u32;
        for &(t, ma) in &self.samples {
            if last.saturating_duration_since(t) <= window {
                sum += ma;
                n += 1;
            }
        }
        if n < 2 {
            None
        } else {
            Some(sum / n as f32)
        }
    }

    /// 实际样本跨度（分钟）：不足 10 分钟就是实际几分钟。
    pub fn span_minutes(&self) -> f32 {
        let (Some((a, _)), Some((b, _))) = (self.samples.front(), self.samples.back()) else {
            return 0.0;
        };
        b.saturating_duration_since(*a).as_secs_f32() / 60.0
    }
}

/// 按 `draw_ma` 的耗电速度，`charge_mah` 还能用多久。
///
/// `charge / draw` 小时；电流 <= 0（读不到/充电中）或电量缺失都返回 None。
pub fn estimate(draw_ma: f32, charge_mah: f32) -> Option<Duration> {
    if !draw_ma.is_finite() || !charge_mah.is_finite() || draw_ma <= 0.0 || charge_mah <= 0.0 {
        return None;
    }
    // `try_from_secs_f32` 对 NaN/溢出返回 Err，不会 panic。
    Duration::try_from_secs_f32(charge_mah / draw_ma * 3600.0).ok()
}

/// `about 9 h 12 min` / `about 47 min`。不足 1 分钟按 0 或 1 分钟算（四舍五入）。
pub fn human_duration(d: Duration) -> String {
    let mins = (d.as_secs_f64() / 60.0).round() as u64;
    if mins < 60 {
        return format!("about {mins} min");
    }
    let (h, m) = (mins / 60, mins % 60);
    if m == 0 {
        format!("about {h} h")
    } else {
        format!("about {h} h {m} min")
    }
}

/// 千位分隔：`3672` → `3,672`。
pub fn group_thousands(n: u32) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.char_indices() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// 设置页那一行容量：`3,672 mAh`。
pub fn fmt_mah(mah: f32) -> String {
    format!("{} mAh", group_thousands(mah.max(0.0).round() as u32))
}

/// 平均耗电那一行：`412 mA · last 8 min`。
pub fn fmt_usage(avg_ma: f32, minutes: u32) -> String {
    format!(
        "{} mA · last {} min",
        group_thousands(avg_ma.abs().round() as u32),
        minutes
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // 真机 `ssh rm2 cat /sys/class/power_supply/max77818_battery/uevent` 的原样输出。
    const UEVENT: &str = "POWER_SUPPLY_NAME=max77818_battery
POWER_SUPPLY_STATUS=Full
POWER_SUPPLY_STATUS_EX=USB-C connected
POWER_SUPPLY_PRESENT=1
POWER_SUPPLY_VOLTAGE_NOW=4377500
POWER_SUPPLY_CAPACITY=100
POWER_SUPPLY_TEMP=234
POWER_SUPPLY_CURRENT_NOW=0
POWER_SUPPLY_TIME_TO_EMPTY_NOW=368634
POWER_SUPPLY_CHARGER_MODE=Charger
";

    #[test]
    fn parses_real_device_uevent() {
        let b = parse_uevent(UEVENT);
        assert_eq!(b.capacity, Some(100));
        assert_eq!(b.state, ChargeState::Full);
        assert_eq!(b.minutes_left, None, "Full 时 time_to_empty 是垃圾数");
    }

    #[test]
    fn charging_sets_usb_online() {
        let b = parse_uevent("POWER_SUPPLY_STATUS=Charging\nPOWER_SUPPLY_CAPACITY=42\n");
        assert_eq!(b.capacity, Some(42));
        assert!(b.usb_online);
        assert_eq!(b.state, ChargeState::Charging);
    }

    #[test]
    fn discharging_minutes() {
        let b = parse_uevent("POWER_SUPPLY_STATUS=Discharging\nPOWER_SUPPLY_TIME_TO_EMPTY_NOW=19200\n");
        assert_eq!(b.minutes_left, Some(320));
        assert_eq!(fmt_hm(320), "5 h 20 min");
    }

    #[test]
    fn rejects_bogus_time() {
        assert_eq!(parse_minutes("0"), None);
        assert_eq!(parse_minutes("999999"), None);
        assert_eq!(parse_minutes("x"), None);
        assert_eq!(parse_minutes("45"), Some(1));
    }

    #[test]
    fn summary_texts() {
        let full = Battery {
            capacity: Some(100),
            state: ChargeState::Full,
            ..Default::default()
        };
        assert_eq!(summary(&full).as_deref(), Some("100% - Full"));
        let dis = Battery {
            capacity: Some(72),
            state: ChargeState::Discharging,
            minutes_left: Some(75),
            ..Default::default()
        };
        assert_eq!(
            summary(&dis).as_deref(),
            Some("72% - Discharging - about 1 h 15 min remaining")
        );
        assert_eq!(summary(&Battery::default()), None);
    }

    #[test]
    fn read_from_empty_dir_is_none() {
        let d = std::env::temp_dir().join("rmdev-battery-none");
        let _ = std::fs::create_dir_all(&d);
        assert!(read_from(&d).is_none());
    }

    #[test]
    fn reads_real_sysfs_layout() {
        // 造一个和 rM2 一样的两目录布局：max77818_battery + max77818-charger。
        let root = std::env::temp_dir().join(format!("rmdev-ps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("max77818_battery")).unwrap();
        std::fs::create_dir_all(root.join("max77818-charger")).unwrap();
        std::fs::write(
            root.join("max77818_battery/uevent"),
            "POWER_SUPPLY_STATUS=Discharging\nPOWER_SUPPLY_CAPACITY=55\n",
        )
        .unwrap();
        std::fs::write(
            root.join("max77818_battery/time_to_empty_now"),
            "7200\n",
        )
        .unwrap();
        std::fs::write(
            root.join("max77818-charger/uevent"),
            "POWER_SUPPLY_ONLINE=0\n",
        )
        .unwrap();
        let b = read_from(&root).unwrap();
        assert_eq!(b.capacity, Some(55));
        assert_eq!(b.state, ChargeState::Discharging);
        assert_eq!(b.minutes_left, Some(120));
        assert!(!b.usb_online);
        let _ = std::fs::remove_dir_all(&root);
    }

    // === BATT begin: 容量 / 平均耗电 / 剩余时间估算 ===
    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn avg_ma_needs_two_samples() {
        let base = Instant::now();
        let mut u = Usage::new();
        assert_eq!(u.avg_ma(WINDOW), None, "空窗口没有平均");
        u.push(base, 400.0);
        assert_eq!(u.avg_ma(WINDOW), None, "单点不算平均");
        u.push(at(base, 30), 500.0);
        assert_eq!(u.avg_ma(WINDOW), Some(450.0));
    }

    #[test]
    fn avg_ma_full_window_and_clipping() {
        let base = Instant::now();
        let mut u = Usage::new();
        for (i, ma) in [100.0, 200.0, 300.0].iter().enumerate() {
            u.push(at(base, 300 * i as u64), *ma);
        }
        // 满窗：三个点都算。
        assert_eq!(u.avg_ma(WINDOW), Some(200.0));
        // 只看最近 5 分钟：0 s 那个点被裁掉。
        assert_eq!(u.avg_ma(Duration::from_secs(300)), Some(250.0));
        // 裁到只剩一个点 → 不够算平均。
        assert_eq!(u.avg_ma(Duration::from_secs(100)), None);
    }

    #[test]
    fn push_drops_samples_outside_window() {
        let base = Instant::now();
        let mut u = Usage::new();
        u.push(base, 100.0);
        u.push(at(base, 600), 200.0);
        // 600 s 正好在窗口边上，还留着。
        assert_eq!(u.span_minutes(), 10.0);
        assert_eq!(u.avg_ma(WINDOW), Some(150.0));
        u.push(at(base, 601), 300.0);
        // 0 s 的点出窗，只剩 600 / 601 两个点。
        assert_eq!(u.avg_ma(WINDOW), Some(250.0));
        assert!(u.span_minutes() < 0.1);
    }

    #[test]
    fn span_minutes_is_the_real_span() {
        let base = Instant::now();
        let mut u = Usage::new();
        assert_eq!(u.span_minutes(), 0.0);
        u.push(base, 1.0);
        assert_eq!(u.span_minutes(), 0.0, "单点跨度为 0");
        u.push(at(base, 480), 1.0);
        assert_eq!(u.span_minutes(), 8.0);
    }

    #[test]
    fn estimate_edges_and_hours() {
        assert_eq!(estimate(0.0, 3600.0), None, "0 电流不估算");
        assert_eq!(estimate(-10.0, 3600.0), None, "负电流不估算");
        assert_eq!(estimate(400.0, 0.0), None, "没有电量不估算");
        assert_eq!(estimate(400.0, -1.0), None);
        assert_eq!(estimate(f32::NAN, 3600.0), None);
        assert_eq!(
            estimate(400.0, 3600.0),
            Some(Duration::from_secs(9 * 3600))
        );
        let d = estimate(400.0, 3680.0).unwrap();
        assert_eq!(human_duration(d), "about 9 h 12 min");
    }

    #[test]
    fn human_duration_forms() {
        assert_eq!(human_duration(Duration::from_secs(47 * 60)), "about 47 min");
        assert_eq!(human_duration(Duration::from_secs(9 * 3600)), "about 9 h");
        assert_eq!(
            human_duration(Duration::from_secs(9 * 3600 + 12 * 60)),
            "about 9 h 12 min"
        );
        // 59 min 40 s 四舍五入成 1 小时，不写 "0 h 60 min"。
        assert_eq!(
            human_duration(Duration::from_secs(59 * 60 + 40)),
            "about 1 h"
        );
        assert_eq!(human_duration(Duration::from_secs(0)), "about 0 min");
    }

    #[test]
    fn capacity_mah_converts_micro_ah() {
        let b = Battery {
            charge_now_uah: Some(3_672_000),
            ..Default::default()
        };
        assert_eq!(b.capacity_mah(), Some(3672.0));
        assert_eq!(Battery::default().capacity_mah(), None);
    }

    #[test]
    fn thousands_separator() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1000), "1,000");
        assert_eq!(group_thousands(3672), "3,672");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn formats_capacity_and_usage_lines() {
        assert_eq!(fmt_mah(3671.6), "3,672 mAh");
        assert_eq!(fmt_usage(412.4, 8), "412 mA · last 8 min");
        assert_eq!(fmt_usage(411.6, 10), "412 mA · last 10 min");
        assert_eq!(fmt_usage(-412.0, 3), "412 mA · last 3 min");
    }

    #[test]
    fn parses_micro_units() {
        let b = parse_uevent(
            "POWER_SUPPLY_CHARGE_NOW=3672000\nPOWER_SUPPLY_CHARGE_FULL=3720000\n\
             POWER_SUPPLY_VOLTAGE_NOW=4377500\nPOWER_SUPPLY_CURRENT_NOW=-412000\n",
        );
        assert_eq!(b.charge_now_uah, Some(3_672_000));
        assert_eq!(b.charge_full_uah, Some(3_720_000));
        assert_eq!(b.capacity_mah(), Some(3672.0));
        assert_eq!(b.current_ma(), Some(-412.0));
        let mw = b.power_mw().unwrap();
        assert!((mw + 1803.53).abs() < 0.01, "{mw}");
    }

    #[test]
    fn missing_charge_attrs_degrade_to_none() {
        // 真机 uevent 原样（没有 CHARGE_*）：电压/电流有，电量没有 → 不 panic。
        let b = parse_uevent(UEVENT);
        assert_eq!(b.charge_now_uah, None);
        assert_eq!(b.capacity_mah(), None);
        assert_eq!(b.voltage_uv, Some(4_377_500));
        assert_eq!(b.current_ma(), Some(0.0));
    }

    #[test]
    fn charging_states() {
        let mut b = Battery::default();
        assert!(!b.is_charging(), "Unknown 不算充电");
        b.state = ChargeState::Discharging;
        assert!(!b.is_charging());
        b.state = ChargeState::Charging;
        assert!(b.is_charging());
        b.state = ChargeState::Full;
        assert!(b.is_charging());
    }

    #[test]
    fn reads_charge_attrs_from_files() {
        let root = std::env::temp_dir().join(format!("rmdev-batt-charge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("max77818_battery")).unwrap();
        std::fs::write(
            root.join("max77818_battery/uevent"),
            "POWER_SUPPLY_STATUS=Full\nPOWER_SUPPLY_CAPACITY=100\n\
             POWER_SUPPLY_VOLTAGE_NOW=4377500\nPOWER_SUPPLY_CURRENT_NOW=0\n",
        )
        .unwrap();
        std::fs::write(root.join("max77818_battery/charge_full"), "3720000\n").unwrap();
        std::fs::write(root.join("max77818_battery/charge_now"), "3600000\n").unwrap();
        let b = read_from(&root).unwrap();
        assert_eq!(b.charge_full_uah, Some(3_720_000));
        assert_eq!(b.capacity_mah(), Some(3600.0));
        assert!(b.is_charging());
        let _ = std::fs::remove_dir_all(&root);
    }
    // === BATT end ===
}
