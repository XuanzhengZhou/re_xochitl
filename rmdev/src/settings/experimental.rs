//! Experimental 页的 7 个开关：`~/.config/rmdev/experimental`，`key=value` 纯文本。
//!
//! 键名照官方 `res_069e1aa.qml`（QSettings 的 `Experimental` 组 + key），
//! 行文案照官方 `res_0686587.qml`（顺序也照抄）。
//!
//! 文件坏了（不是 UTF-8 / 全是乱码）一律**降级成"全关"**并 `eprintln!`，绝不 panic：
//! 这一页只有开关，读不出来就当没开过，不能因为一个坏文件让整个设置页打不开。
//!
//! 注意：这 7 个开关这一版**只存不做** —— 对应功能（扫动出工具条、文件系统
//! 浏览器、WebRtc 投屏…）还没有实现。页面照官方 QML 把行都列出来，等以后
//! 补功能时不用再改版面。

use std::path::{Path, PathBuf};

use super::config_dir;

/// 官方 `res_0686587.qml` 的一个 PanelToggle。
pub struct Row {
    /// 官方 QSettings key（`res_069e1aa.qml`）
    pub key: &'static str,
    /// `header:`（粗体行）
    pub header: &'static str,
    /// `text:`（副标题；空串 = 官方这行没有副标题）
    pub text: &'static str,
}

/// 7 行，顺序 = 官方页面从上到下。
pub const ROWS: [Row; 7] = [
    Row {
        key: "SwipeToolbarGesture",
        header: "Enable swipe to show toolbar",
        text: "",
    },
    Row {
        key: "FilesystemExplorer",
        header: "Enable Filesystem Explorer",
        text: "Requires reboot",
    },
    Row {
        key: "LibraryExplorer",
        header: "Enable Library Explorer",
        text: "Requires reboot",
    },
    Row {
        key: "OverrideScreenCast",
        header: "Enable ScreenShare outside docs",
        text: "",
    },
    Row {
        key: "Antialiasing",
        header: "Enable antialiasing",
        text: "",
    },
    Row {
        key: "Orientation",
        header: "Screen orientation",
        text: "Enable screen orientation toggle",
    },
    Row {
        key: "ScreenShareWebRtc",
        header: "ScreenShare WebRtc",
        text: "Enable WebRtc for ScreenShare",
    },
];

/// 页面标题和说明（官方 `res_0686587.qml`）。
pub const TITLE: &str = "Experimental features";
pub const DESCRIPTION: &str = "These features are only intended for internal testing. There are no guarantees that they will end up in a future product from reMarkable.";

/// 7 个开关。用 `on[i]` 而不是 7 个具名字段：版面/解析/落盘都按 `ROWS` 顺序
/// 循环，加一行只改 `ROWS`。
#[derive(Clone, Debug, PartialEq)]
pub struct Experimental {
    pub on: [bool; 7],
}

impl Default for Experimental {
    /// 官方 `ExperimentalState` 默认就是关。
    fn default() -> Self {
        Experimental { on: [false; 7] }
    }
}

impl Experimental {
    pub fn get(&self, i: usize) -> bool {
        self.on.get(i).copied().unwrap_or(false)
    }

    pub fn set(&mut self, i: usize, v: bool) {
        if let Some(s) = self.on.get_mut(i) {
            *s = v;
        }
    }
}

pub fn path() -> PathBuf {
    config_dir().join("experimental")
}

/// `1/0`、`true/false`、`on/off`、`yes/no` 都认；其它值 = 关（默认值就是关）。
fn parse_bool(s: &str) -> bool {
    matches!(s, "1" | "true" | "on" | "yes")
}

/// 解析配置文件。未知 key / 没有 `=` 的行 / 坏值都忽略。
pub fn parse(text: &str) -> Experimental {
    let mut e = Experimental::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let Some(i) = ROWS.iter().position(|r| r.key == k.trim()) else {
            continue;
        };
        e.on[i] = parse_bool(v.trim());
    }
    e
}

pub fn encode(e: &Experimental) -> String {
    let mut s = String::from("# rmdev experimental features (delete this file to reset)\n");
    for (i, r) in ROWS.iter().enumerate() {
        s.push_str(&format!("{}={}\n", r.key, u8::from(e.get(i))));
    }
    s
}

/// 读开关；文件不存在 / 读不动 / 不是 UTF-8 都降级成全关，只打一行日志。
pub fn load() -> Experimental {
    load_from(&path())
}

pub fn load_from(p: &Path) -> Experimental {
    match std::fs::read(p) {
        Ok(bytes) => match String::from_utf8(bytes) {
            Ok(text) => parse(&text),
            Err(_) => {
                eprintln!(
                    "[experimental] {} 不是 UTF-8，按默认值（全关）处理",
                    p.display()
                );
                Experimental::default()
            }
        },
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!(
                    "[experimental] 读 {} 失败：{e}，按默认值（全关）处理",
                    p.display()
                );
            }
            Experimental::default()
        }
    }
}

pub fn save(e: &Experimental) -> std::io::Result<()> {
    save_to(&path(), e)
}

/// 写到指定路径（测试用；`save` 也走这里）。
pub fn save_to(p: &Path, e: &Experimental) -> std::io::Result<()> {
    if let Some(dir) = p.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir)?;
        }
    }
    std::fs::write(p, encode(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机临时文件（测试并行跑，名字里带 pid + 纳秒）。
    fn tmp(name: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("rmdev-exp-{}-{n}-{name}", std::process::id()))
    }

    #[test]
    fn defaults_are_all_off() {
        let e = Experimental::default();
        assert_eq!(e.on, [false; 7]);
        assert!(!e.get(0));
        assert!(!e.get(6));
        // 越界不 panic（`set_page_experimental` 用循环下标，理论上不会越界）
        assert!(!e.get(7));
    }

    #[test]
    fn rows_match_official_qml() {
        // 文案是 `res_0686587.qml` 逐字抄的，别"顺手改进"
        let heads: Vec<&str> = ROWS.iter().map(|r| r.header).collect();
        assert_eq!(
            heads,
            [
                "Enable swipe to show toolbar",
                "Enable Filesystem Explorer",
                "Enable Library Explorer",
                "Enable ScreenShare outside docs",
                "Enable antialiasing",
                "Screen orientation",
                "ScreenShare WebRtc",
            ]
        );
        let subs: Vec<&str> = ROWS.iter().map(|r| r.text).collect();
        assert_eq!(
            subs,
            [
                "",
                "Requires reboot",
                "Requires reboot",
                "",
                "",
                "Enable screen orientation toggle",
                "Enable WebRtc for ScreenShare",
            ]
        );
        assert_eq!(
            ROWS[1].key, "FilesystemExplorer",
            "key 要跟官方 QSettings 一致"
        );
        assert_eq!(ROWS[6].key, "ScreenShareWebRtc");
    }

    #[test]
    fn roundtrip() {
        let mut e = Experimental::default();
        for i in [0, 2, 5] {
            e.set(i, true);
        }
        assert_eq!(parse(&encode(&e)), e, "写出去再读回来要一模一样");
        // 全开也要能原样回来
        let all = Experimental { on: [true; 7] };
        assert_eq!(parse(&encode(&all)), all);
    }

    #[test]
    fn tolerates_garbage_lines() {
        let e = parse(
            "# comment\n\
             wat\n\
             NoEqualsHere\n\
             Antialiasing=maybe\n\
             FilesystemExplorer=1\n\
             UnknownKey=1\n\
             =1\n\
             LibraryExplorer = yes \n",
        );
        assert!(!e.get(4), "认不出来的值算关");
        assert!(e.get(1));
        assert!(e.get(2), "等号两边可以有空格");
        assert_eq!(e.on.iter().filter(|v| **v).count(), 2);
    }

    #[test]
    fn corrupt_file_falls_back_to_defaults() {
        let p = tmp("corrupt");
        // 坏字节（非 UTF-8）必须降级成"全关"，不能 panic，也不能只读半截
        std::fs::write(&p, b"\xff\xfe\x00\x01 not text").unwrap();
        assert_eq!(load_from(&p), Experimental::default());
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn missing_file_is_default() {
        let p = tmp("missing");
        assert_eq!(load_from(&p), Experimental::default());
        // 也顺便确认"文件不存在"不算错误（不会 panic / 不会卡住）
        assert!(!p.exists());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let p = tmp("roundtrip");
        let mut e = Experimental::default();
        e.set(3, true);
        e.set(4, true);
        save_to(&p, &e).unwrap();
        assert_eq!(load_from(&p), e);
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("OverrideScreenCast=1\n"));
        assert!(text.contains("SwipeToolbarGesture=0\n"));
        let _ = std::fs::remove_file(&p);
    }
}
