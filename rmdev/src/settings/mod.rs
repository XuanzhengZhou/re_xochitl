//! 设置页的数据层。
//!
//! UI 只负责画，真机数据/持久化都在这里。所有读取都**不 panic**：
//! 设备上没有对应文件（或本机开发机没有）时返回 `None` / 默认值，
//! 调用方自己决定怎么降级。
//!
//! 配置目录：`~/.config/rmdev/`（可用 `RMDEV_CONFIG_DIR` 覆盖）。
//! - `settings`：普通偏好（语言、字号、各开关），`key=value` 文本
//! - `passcode`：屏幕锁密码的哈希，**删掉这个文件即可解锁**（见 `security`）
//! - `experimental`：Experimental 页的 7 个开关（见 `experimental`）

pub mod battery;
pub mod developer;
pub mod disk;
pub mod experimental;
pub mod help;
pub mod language;
pub mod prefs;
pub mod reset;
pub mod security;
pub mod wifi;

use std::path::PathBuf;

/// `~/.config/rmdev`。
pub fn config_dir() -> PathBuf {
    if let Ok(d) = std::env::var("RMDEV_CONFIG_DIR") {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    // 真机上 xochitl 的 HOME 是 /home/root；本机开发时用当前用户 HOME。
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/root".into());
    PathBuf::from(home).join(".config/rmdev")
}
