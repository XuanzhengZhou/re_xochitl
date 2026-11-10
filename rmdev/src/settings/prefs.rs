//! 设置页的持久化偏好：`~/.config/rmdev/settings`，`key=value` 纯文本。
//!
//! 屏幕锁密码**不在这里**（见 `security`）。Wi-Fi 开关是真实硬件状态，
//! 也不持久化（每次打开设置重新读 rfkill）。

use super::config_dir;

/// 笔尾橡皮（`BTN_TOOL_RUBBER`）该当哪种橡皮用。
///
/// 官方没有这个开关：笔尾永远是"蹭掉像素"的那块橡皮。但机器上还有
/// 一个"碰到就删整条笔迹"的模式（我们加的，见 `chrome::ERASE_ITEMS`），
/// 有人希望笔尾直接就是它 —— 于是给三档：固定刷白、固定删整笔、跟着
/// 橡皮菜单上一次的选择走。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TailEraser {
    /// 笔尾 = 刷白橡皮（官方行为，默认）
    Eraser,
    /// 笔尾 = 擦整笔（碰到哪条删哪条）
    Stroke,
    /// 笔尾 = 上一次在橡皮菜单里选的那一档
    Last,
}

impl TailEraser {
    pub fn key(self) -> &'static str {
        match self {
            TailEraser::Eraser => "eraser",
            TailEraser::Stroke => "stroke",
            TailEraser::Last => "last",
        }
    }

    pub fn parse(s: &str, def: TailEraser) -> TailEraser {
        // 也接受设置里写得更像人话的几种写法
        match s.trim().to_ascii_lowercase().as_str() {
            "eraser" | "brush" | "white" | "0" => TailEraser::Eraser,
            "stroke" | "erase-stroke" | "stroke_erase" | "1" => TailEraser::Stroke,
            "last" | "remember" | "2" => TailEraser::Last,
            _ => def,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Prefs {
    pub handwriting_lang: String,
    pub keyboard_lang: String,
    pub font_large: bool,
    pub flight_mode: bool,
    pub battery_percentage: bool,
    pub auto_sleep: bool,
    pub auto_power_off: bool,
    pub usb_web: bool,
    pub sync_before_sleep: bool,
    /// 笔尾橡皮映射到哪一种橡皮（设置页 Accessibility 里能改）
    pub tail_eraser: TailEraser,
    /// 左右利手。官方 `Handedness` 那两项；右手 = 工具栏在左（官方默认）。
    /// 以前这一项只有界面、不生效（改了没反应）；现在它真的把工具条换边。
    pub right_handed: bool,
    /// 宽页（PPT/讲义那种横比竖大的 PDF）自动进横屏。设置页可关。
    pub auto_landscape: bool,
    /// 横屏往哪边转：`true` = 逆时针（**充电口在左**，默认），`false` = 顺时针。
    pub land_dir_left: bool,
}

impl Default for Prefs {
    /// 默认值 = 官方截图里的状态（电池百分比、Auto sleep、USB/Sync 开，其余关）。
    ///
    /// Auto sleep 官方默认就是开的（Battery settings 原文
    /// "after 20 minutes of inactivity."），关掉它是设置页的显式选择。
    fn default() -> Self {
        Prefs {
            handwriting_lang: "en-US".into(),
            keyboard_lang: "en-US".into(),
            font_large: false,
            flight_mode: false,
            battery_percentage: true,
            auto_sleep: true,
            auto_power_off: false,
            usb_web: true,
            sync_before_sleep: true,
            // 默认跟官方一致：笔尾就是一块普通的刷白橡皮
            tail_eraser: TailEraser::Eraser,
            right_handed: true,
            auto_landscape: true,
            land_dir_left: true,
        }
    }
}

fn parse_bool(s: &str, def: bool) -> bool {
    match s.trim() {
        "1" | "true" | "yes" | "on" => true,
        "0" | "false" | "no" | "off" => false,
        _ => def,
    }
}

/// 解析配置文件。未知 key / 坏值都忽略。
pub fn parse(text: &str) -> Prefs {
    let mut p = Prefs::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "handwriting_lang" => p.handwriting_lang = v.to_string(),
            "keyboard_lang" => p.keyboard_lang = v.to_string(),
            "font_large" => p.font_large = parse_bool(v, p.font_large),
            "flight_mode" => p.flight_mode = parse_bool(v, p.flight_mode),
            "battery_percentage" => p.battery_percentage = parse_bool(v, p.battery_percentage),
            "auto_sleep" => p.auto_sleep = parse_bool(v, p.auto_sleep),
            "auto_power_off" => p.auto_power_off = parse_bool(v, p.auto_power_off),
            "usb_web" => p.usb_web = parse_bool(v, p.usb_web),
            "sync_before_sleep" => p.sync_before_sleep = parse_bool(v, p.sync_before_sleep),
            "tail_eraser" => p.tail_eraser = TailEraser::parse(v, p.tail_eraser),
            "right_handed" => p.right_handed = parse_bool(v, p.right_handed),
            "auto_landscape" => p.auto_landscape = parse_bool(v, p.auto_landscape),
            "land_dir_left" => p.land_dir_left = parse_bool(v, p.land_dir_left),
            _ => {}
        }
    }
    p
}

pub fn encode(p: &Prefs) -> String {
    let mut s = String::new();
    s.push_str("# rmdev settings (delete this file to restore defaults)\n");
    s.push_str(&format!("handwriting_lang={}\n", p.handwriting_lang));
    s.push_str(&format!("keyboard_lang={}\n", p.keyboard_lang));
    for (k, v) in [
        ("font_large", p.font_large),
        ("flight_mode", p.flight_mode),
        ("battery_percentage", p.battery_percentage),
        ("auto_sleep", p.auto_sleep),
        ("auto_power_off", p.auto_power_off),
        ("usb_web", p.usb_web),
        ("sync_before_sleep", p.sync_before_sleep),
        ("right_handed", p.right_handed),
        ("auto_landscape", p.auto_landscape),
        ("land_dir_left", p.land_dir_left),
    ] {
        s.push_str(&format!("{k}={}\n", u8::from(v)));
    }
    s.push_str(&format!("tail_eraser={}\n", p.tail_eraser.key()));
    s
}

pub fn load() -> Prefs {
    load_opt().unwrap_or_default()
}

/// 状态栏电量文字：`Show battery percentage` 关掉时返回 `None`（只画电池图标）。
///
/// 官方 `res_068451c.qml` 的 Battery 页第一行就是这个开关；关掉后状态栏
/// 只留电池图标，`NN%` 那段位置空出来。
pub fn battery_label(cap: u8, show: bool) -> Option<String> {
    if show {
        Some(format!("{cap}%"))
    } else {
        None
    }
}

/// 读偏好；文件不存在返回 None（用来区分「从没设置过」和「设置成默认值」）。
///
/// 无障碍字号靠这个：官方截图里 Accessibility 页默认高亮 Large，但字号是
/// 正常大小（说明要重启才生效）。所以首次启动按截图显示 Large，真正放大
/// 要等用户点过（写出 settings 文件）之后的下一轮启动。
pub fn load_opt() -> Option<Prefs> {
    std::fs::read_to_string(config_dir().join("settings"))
        .ok()
        .map(|s| parse(&s))
}

pub fn save(p: &Prefs) -> std::io::Result<()> {
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("settings"), encode(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_official_screenshot() {
        let p = Prefs::default();
        assert!(p.battery_percentage);
        assert!(p.auto_sleep, "官方默认 20 分钟自动睡眠");
        assert!(p.usb_web);
        assert!(p.sync_before_sleep);
        assert!(!p.font_large);
        // 官方没有笔尾映射这回事：默认就该是"笔尾 = 刷白橡皮"
        assert_eq!(p.tail_eraser, TailEraser::Eraser);
    }

    /// Auto sleep 默认开，但设置页/配置文件里的显式 `false` 必须能关掉它；
    /// 坏值不能关（退回默认开）。
    #[test]
    fn auto_sleep_defaults_on_and_can_be_turned_off() {
        assert!(Prefs::default().auto_sleep);
        assert!(parse("").auto_sleep, "空配置也走默认开");
        assert!(!parse("auto_sleep=0").auto_sleep, "显式关必须生效");
        assert!(!parse("auto_sleep=false").auto_sleep);
        assert!(parse("auto_sleep=maybe").auto_sleep, "坏值退回默认开");
        assert!(parse(&encode(&Prefs::default())).auto_sleep);
    }

    #[test]
    fn eraser_prefs_roundtrip() {
        for tail in [TailEraser::Eraser, TailEraser::Stroke, TailEraser::Last] {
            let p = Prefs {
                tail_eraser: tail,
                ..Prefs::default()
            };
            assert_eq!(parse(&encode(&p)), p, "{tail:?}");
        }
    }

    #[test]
    fn tail_eraser_tolerates_aliases_and_junk() {
        // 前两行是设置页会写的规范值；后面几种是手写配置文件时容易出现的写法。
        for (s, want) in [
            ("eraser", TailEraser::Eraser),
            ("stroke", TailEraser::Stroke),
            ("last", TailEraser::Last),
            (" Erase-Stroke ", TailEraser::Stroke),
            ("BRUSH", TailEraser::Eraser),
            ("remember", TailEraser::Last),
            ("1", TailEraser::Stroke),
            ("2", TailEraser::Last),
        ] {
            assert_eq!(TailEraser::parse(s, TailEraser::Eraser), want, "{s}");
        }
        // 认不出来的值退回默认，不要因为一行坏配置把笔尾变成删整笔
        assert_eq!(TailEraser::parse("wat", TailEraser::Eraser), TailEraser::Eraser);
    }

    #[test]
    fn battery_percentage_defaults_on_and_roundtrips() {
        assert!(Prefs::default().battery_percentage, "官方默认开");
        let off = Prefs {
            battery_percentage: false,
            ..Prefs::default()
        };
        assert!(!parse(&encode(&off)).battery_percentage);
        // 手写配置文件里的 0/1 也认
        assert!(!parse("battery_percentage=0\n").battery_percentage);
        assert!(parse("battery_percentage=1\n").battery_percentage);
        // 状态栏：关掉后连文字都不该有
        assert_eq!(battery_label(87, true).as_deref(), Some("87%"));
        assert_eq!(battery_label(87, false), None);
    }

    #[test]
    fn roundtrip() {
        let p = Prefs {
            handwriting_lang: "ja-JP".into(),
            keyboard_lang: "de-DE".into(),
            font_large: true,
            flight_mode: true,
            battery_percentage: false,
            auto_sleep: true,
            auto_power_off: true,
            usb_web: false,
            sync_before_sleep: false,
            tail_eraser: TailEraser::Last,
            right_handed: false,
            auto_landscape: false,
            land_dir_left: false,
        };
        assert_eq!(parse(&encode(&p)), p);
    }

    #[test]
    fn tolerates_garbage_and_unknown_keys() {
        let p = parse(
            "# comment\nwat\nfoo=bar\nauto_sleep=maybe\nfont_large=yes\nhandwriting_lang=ko-KR\n",
        );
        assert!(p.font_large);
        assert!(p.auto_sleep, "auto_sleep=maybe 是坏值，退回默认开");
        assert_eq!(p.handwriting_lang, "ko-KR");
        assert_eq!(p.keyboard_lang, "en-US");
    }
}
