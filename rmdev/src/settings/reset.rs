//! Factory reset 确认框 + 删除范围。
//!
//! 弹框文案照官方 `ResetDeviceDialog`（`res_06b546c.qml` 的 `logoutStart` 那一屏）；
//! `res_06852b1.qml` 里那个 `Factory reset` 面板按钮的说明文字也一并抄在这里
//! （`set_page_account` 用它）。
//!
//! **和官方的关键差异**：
//! 官方点确认之后是 同步 → 解除云账号 → 清空整个设备库 → 重启；我们**只删
//! `~/.config/rmdev`**（偏好 / 密码 / 实验开关 / 排版配置 / 服务器配对），
//! `/home/root/.local/share/remarkable/xochitl` 里的用户文档一个字节都不碰，
//! 也**不重启设备**。

use std::path::Path;

use super::config_dir;
use crate::panels::poweroff::{SECOND_MARGIN, SIDE_MARGIN};

/// 弹框标题（官方 `text:`）。
pub const TITLE: &str = "Confirm factory reset";
/// 正文两行（官方原文里就带 `\n`，这里照它拆成两行画）。
pub const DESCRIPTION: [&str; 2] = [
    "Are you sure you want to reset your",
    "reMarkable to its factory settings?",
];
/// 正文下面的小字（官方 `subDescription:`）。
pub const SUB: &str = "All of your data will be erased from the device.";
/// 两个按钮（官方 `highlight: false` = 黑底，`true` = 白底黑框）。
pub const YES: &str = "Yes, factory reset";
pub const NO: &str = "No, go back";

/// 清完以后设置页顶部那条提示。
///
/// 官方没有这串：官方清完直接重启，根本没有"完成"这一屏。这句故意只说设置 ——
/// 我们确实只动了设置，不能让用户以为文档没了。
pub const DONE: &str = "Settings were reset to their defaults.";

/// 账户页 `Factory reset` 那行的说明（官方 `res_06852b1.qml` 的 `text:`）。
///
/// 官方是**清整个库**，我们只清配置；官方原文照抄，差异靠
/// 完成提示说明（改文案属于自创字符串，铁律不允许）。
pub const PANEL_TEXT_1: &str = "This will delete all files on device, and restore";
pub const PANEL_TEXT_2: &str = "your device settings to their original state.";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Confirm,
    Cancel,
}

/// 两个按钮的矩形 `(x, y, w, h)`：左 = Yes，右 = No。
///
/// 官方 `DialogScreen` 的按钮：宽 = 屏宽/4、高 = 宽/4，整行底边 = 屏高/2 - 200，
/// 左右各离中线 22 —— 和关机框（`poweroff`）完全同一套锚点，所以直接复用它的常量。
pub fn buttons(w: i32, h: i32) -> ((i32, i32, i32, i32), (i32, i32, i32, i32)) {
    let bw = w / 4;
    let bh = bw / 4;
    let y = h / 2 - SECOND_MARGIN - bh;
    let yes = (w / 2 - SIDE_MARGIN - bw, y, bw, bh);
    let no = (w / 2 + SIDE_MARGIN, y, bw, bh);
    (yes, no)
}

/// 命中测试。
pub fn hit(x: f32, y: f32, w: i32, h: i32) -> Option<Act> {
    let (yes, no) = buttons(w, h);
    for (r, act) in [(yes, Act::Confirm), (no, Act::Cancel)] {
        if x >= r.0 as f32 && x < (r.0 + r.2) as f32 && y >= r.1 as f32 && y < (r.1 + r.3) as f32 {
            return Some(act);
        }
    }
    None
}

/// 目录名必须正好是 `rmdev` —— `clear_config` 的第二道保险。
///
/// 这个函数存在的唯一目的：保证"删配置"永远不会变成"删 HOME"。
pub fn is_config_dir(dir: &Path) -> bool {
    dir.file_name().map(|n| n == "rmdev").unwrap_or(false)
}

/// 删掉 `dir` 整个目录（不存在算成功）。只删传进来的这一个目录，不猜路径。
pub fn clear_in(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        // 本来就是空的（或第一次启动从没写过配置）→ 就是"已经清干净了"
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// 只删 `dir`：**目录名必须是 `rmdev`**，否则拒绝 —— 这是 `clear_config` 的安全闸。
///
/// 单独拆出来是为了能用手边的临时目录测（不去改进程级环境变量，测试才不飘）。
pub fn clear_config_in(dir: &Path) -> Result<(), String> {
    if !is_config_dir(dir) {
        return Err(format!("refusing to delete {}", dir.display()));
    }
    clear_in(dir).map_err(|e| format!("{}: {e}", dir.display()))
}

/// Factory reset 的全部副作用：清 `~/.config/rmdev`。
///
/// 用户文档在 `/home/root/.local/share/remarkable/xochitl`，**不在这个路径下面**，
/// 这里也没有任何代码会走到那儿。
pub fn clear_config() -> Result<(), String> {
    let dir = config_dir();
    // `RMDEV_CONFIG_DIR` 是测试/调试用的显式覆盖，其它情况必须叫 rmdev
    if std::env::var("RMDEV_CONFIG_DIR")
        .map(|v| !v.is_empty())
        .unwrap_or(false)
    {
        return clear_in(&dir).map_err(|e| format!("{}: {e}", dir.display()));
    }
    clear_config_in(&dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("rmdev-reset-{}-{n}-{name}", std::process::id()))
    }

    #[test]
    fn strings_match_official_qml() {
        assert_eq!(TITLE, "Confirm factory reset");
        assert_eq!(
            DESCRIPTION,
            [
                "Are you sure you want to reset your",
                "reMarkable to its factory settings?"
            ]
        );
        assert_eq!(SUB, "All of your data will be erased from the device.");
        assert_eq!(YES, "Yes, factory reset");
        assert_eq!(NO, "No, go back");
    }

    #[test]
    fn buttons_follow_dialog_anchors() {
        let (yes, no) = buttons(1404, 1872);
        // 351×87，底边 = 936 - 200 = 736 → 顶边 649
        assert_eq!(yes, (329, 649, 351, 87));
        assert_eq!(no, (724, 649, 351, 87));
        // 两个按钮之间留 44 px（左右各 22），不重叠
        assert_eq!(yes.0 + yes.2 + 2 * SIDE_MARGIN, no.0);
    }

    #[test]
    fn hit_test_both_buttons_and_gap() {
        let (w, h) = (1404, 1872);
        assert_eq!(hit(400.0, 700.0, w, h), Some(Act::Confirm));
        assert_eq!(hit(800.0, 700.0, w, h), Some(Act::Cancel));
        assert_eq!(hit(702.0, 700.0, w, h), None, "中间的空隙不算点中");
        assert_eq!(hit(400.0, 100.0, w, h), None, "标题区域不算");
        assert_eq!(hit(1403.0, 1871.0, w, h), None);
    }

    #[test]
    fn only_rmdev_directories_are_deletable() {
        assert!(is_config_dir(Path::new("/home/root/.config/rmdev")));
        assert!(!is_config_dir(Path::new("/home/root/.config")));
        assert!(!is_config_dir(Path::new("/home/root")));
        assert!(!is_config_dir(Path::new("/")));
        assert!(!is_config_dir(Path::new("/home/root/.local/share/remarkable/xochitl")));
    }

    #[test]
    fn clear_in_removes_the_whole_config_dir() {
        let dir = tmp("clear");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("settings"), "font_large=1\n").unwrap();
        std::fs::write(dir.join("passcode"), "rmdev-passcode-v1 0 0\n").unwrap();
        std::fs::write(dir.join("sub/readcfg.json"), "{}").unwrap();
        clear_in(&dir).unwrap();
        assert!(!dir.exists(), "整个目录都要没了");
    }

    #[test]
    fn clear_in_tolerates_missing_dir() {
        let dir = tmp("absent");
        assert!(!dir.exists());
        clear_in(&dir).unwrap();
    }

    /// 端到端确认删除范围：只删 `<…>/rmdev`，旁边的"用户文档"必须活着。
    ///
    /// 不用 `RMDEV_CONFIG_DIR`（改进程级环境变量会让别的测试飘），
    /// 直接给 `clear_config_in` 一个临时目录，走的是同一条安全闸。
    #[test]
    fn clear_config_leaves_user_documents_alone() {
        let root = tmp("cfgroot");
        let cfg = root.join("rmdev");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("settings"), "font_large=1\n").unwrap();
        // 模拟 xochitl 的库目录（真机上在 /home/root/.local/share/remarkable/xochitl）
        let docs = root.join("xochitl");
        std::fs::create_dir_all(&docs).unwrap();
        std::fs::write(docs.join("abc.metadata"), "{}\n").unwrap();

        // 库目录绝不能被当成配置目录删掉
        let err = clear_config_in(&docs).unwrap_err();
        assert!(err.contains("refusing"), "{err}");
        assert!(
            docs.join("abc.metadata").exists(),
            "用户文档一字节都不能动"
        );

        // 真正的配置目录：整个删掉，旁边的库目录还在
        clear_config_in(&cfg).unwrap();
        assert!(!cfg.exists(), "配置目录要整个没掉");
        assert!(
            docs.join("abc.metadata").exists(),
            "用户文档一字节都不能动"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
