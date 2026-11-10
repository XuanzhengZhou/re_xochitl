//! 电源键手势 + 睡眠（挂起）/ 唤醒。
//!
//! 语义全部从官方 QML 里读出来（QML 树未随仓库分发），不是猜的：
//!
//! * `res_06b4fb3.qml`（PowerOffDialog）里挂着一个 `poweroffConfirmationTimer`，
//!   `interval: 3000`，`onTriggered: poweroffDialog.visible = true`；
//! * `res_06548820.qml` 的 `Keys.onPressed / onReleased`（`Qt.Key_PowerOff`）：
//!   按下启动这个 3 s 计时器；**抬起时计时器还在跑**（= 短按）就 `goToSleep(true)`；
//!   计时器已经响过（= 按住 3 s）则什么都不做——关机框已经弹出来了；
//! * `goToSleep(true)` → `BatteryManager.deepSleep()`，`false` → `lightSleep()`；
//! * 官方 `BatteryManager` 的字符串表里带 `/sys/power/state`、`mem`、`standby`、
//!   `/sys/class/rtc/rtc0/wakealarm`，还有 "Error while writing to /sys/power/state"
//!   和 "Suspend when display not showing sleep? Should not happen" ——
//!   也就是：**先把睡眠图刷上屏，再 `echo mem > /sys/power/state`**。
//!
//! 所以这台机器上的映射是：
//!
//! | 动作 | 结果 |
//! |---|---|
//! | 短按（< 3 s） | 睡着→唤醒；醒着→睡（挂起到内存） |
//! | 按住 ≥ 3 s | 弹关机框（"Do you want to turn off your reMarkable?"） |
//!
//! 唤醒源是电源键本身（`snvs-powerkey` 的 `power/wakeup` = enabled）。
//! 挂起期间进程被内核冻住，`suspend()` 返回的那一刻就是"已经醒了"。

use std::path::{Path, PathBuf};
use std::time::Instant;

/// 官方 `poweroffConfirmationTimer.interval`。
pub const LONG_MS: u64 = 3000;

/// 睡下去的方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 真挂起：`echo mem > /sys/power/state`。电池只剩余量自放电。
    Deep,
    /// 只停画面（面板是 bistable 的，不刷新就不耗电，但 CPU 还在跑）。
    /// 挂起不可用时的退路，也用来在驱动没验证过的时候试水。
    Light,
}

impl Mode {
    pub fn name(self) -> &'static str {
        match self {
            Mode::Deep => "深睡（挂起到内存）",
            Mode::Light => "浅睡（只停画面）",
        }
    }
}

/// 一次按下-抬起的结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    /// 短按：醒着就睡，睡着就醒
    Short,
    /// 按住跨过 3 s（关机框已经弹出来了），抬起不算短按
    Long,
    /// 没按下就抬起了 / 已经长按过
    None,
}

/// 电源键手势。纯状态机，时间由调用方给，方便单测。
#[derive(Clone, Debug, Default)]
pub struct Gesture {
    down: Option<Instant>,
    /// 这一次按有没有跨过 3 s（跨过以后抬起就**不再**触发睡眠）
    long_fired: bool,
}

impl Gesture {
    pub fn new() -> Gesture {
        Gesture::default()
    }

    /// 按下。
    pub fn press(&mut self, now: Instant) {
        self.down = Some(now);
        self.long_fired = false;
    }

    /// 抬起。短按返回 `Act::Short`，长按过返回 `Act::Long`。
    pub fn release(&mut self, now: Instant) -> Act {
        let Some(t0) = self.down.take() else {
            return Act::None;
        };
        if self.long_fired {
            self.long_fired = false;
            return Act::Long;
        }
        let held = now.saturating_duration_since(t0).as_millis() as u64;
        if held >= LONG_MS {
            Act::Long
        } else {
            Act::Short
        }
    }

    /// 每圈调一次：刚跨过 3 s 就返回 true（只返回一次）。
    ///
    /// 长按不是等抬起才发现——官方那个计时器 `onTriggered` 就弹框，
    /// 手还按着框已经出来了，抬起时才轮到"松手"。
    pub fn poll_long(&mut self, now: Instant) -> bool {
        let Some(t0) = self.down else { return false };
        if self.long_fired {
            return false;
        }
        if now.saturating_duration_since(t0).as_millis() as u64 >= LONG_MS {
            self.long_fired = true;
            return true;
        }
        false
    }

    pub fn is_down(&self) -> bool {
        self.down.is_some()
    }

    /// 丢掉当前这次按（唤醒那一刻要清，不然抬起会被算成"又要睡"）。
    pub fn forget(&mut self) {
        self.down = None;
        self.long_fired = false;
    }
}

/// `/sys/power/state`。
pub fn state_path() -> PathBuf {
    match std::env::var("RM_SUSPEND_STATE") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from("/sys/power/state"),
    }
}

/// 这个内核支不支持挂起到内存（`state` 里要有 `mem`）。
pub fn can_suspend_at(p: &Path) -> bool {
    let Ok(txt) = std::fs::read_to_string(p) else {
        return false;
    };
    txt.split_whitespace().any(|w| w == "mem")
}

pub fn can_suspend() -> bool {
    can_suspend_at(&state_path())
}

/// 挂起到内存：**阻塞到被唤醒**，返回 Ok 表示"醒了"。
pub fn suspend_at(p: &Path) -> std::io::Result<()> {
    std::fs::write(p, "mem")
}

pub fn suspend() -> std::io::Result<()> {
    suspend_at(&state_path())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 官方 3 s 的边界：2999 ms 是短按，3000 ms 就是长按。
    #[test]
    fn short_and_long_press_boundary() {
        let t0 = Instant::now();
        let mut g = Gesture::new();

        g.press(t0);
        assert_eq!(g.release(t0 + Duration::from_millis(120)), Act::Short);
        // 抬完再抬一次不能又算一次
        assert_eq!(g.release(t0 + Duration::from_millis(200)), Act::None);

        g.press(t0);
        assert_eq!(g.release(t0 + Duration::from_millis(2_999)), Act::Short);

        g.press(t0);
        assert_eq!(g.release(t0 + Duration::from_millis(3_000)), Act::Long);
    }

    /// 按住跨过 3 s：`tick` 里先弹框（`poll_long` 只响一次），
    /// 抬起时不能再触发睡眠——否则松手就睡了，关机框白弹。
    #[test]
    fn long_press_fires_once_and_swallows_release() {
        let t0 = Instant::now();
        let mut g = Gesture::new();
        g.press(t0);
        assert!(!g.poll_long(t0 + Duration::from_millis(2_900)));
        assert!(g.poll_long(t0 + Duration::from_millis(3_050)));
        assert!(!g.poll_long(t0 + Duration::from_millis(4_000)), "只能响一次");
        // 按了 5 s 才松手：抬起不算短按
        assert_eq!(g.release(t0 + Duration::from_millis(5_000)), Act::Long);
        // 下一次短按照常
        g.press(t0);
        assert_eq!(g.release(t0 + Duration::from_millis(80)), Act::Short);
    }

    /// 唤醒那一刻把这一次按丢掉：不然"按一下唤醒"会被同一个抬起事件
    /// 再判成"按一下就睡"，表现为按了电源键屏幕闪一下又黑。
    #[test]
    fn forgetting_the_wake_press_ignores_its_release() {
        let t0 = Instant::now();
        let mut g = Gesture::new();
        g.press(t0);
        g.forget();
        assert!(!g.is_down());
        assert_eq!(g.release(t0 + Duration::from_millis(90)), Act::None);
        assert!(!g.poll_long(t0 + Duration::from_millis(4_000)));
    }

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("rmdev-suspend-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn suspend_support_is_read_from_state_file() {
        let p = tmp("state");
        std::fs::write(&p, "freeze standby mem\n").unwrap();
        assert!(can_suspend_at(&p));
        std::fs::write(&p, "freeze\n").unwrap();
        assert!(!can_suspend_at(&p), "没有 mem 就不该挂起");
        assert!(!can_suspend_at(&tmp("missing")), "文件不存在不能 panic");
        let _ = std::fs::remove_file(&p);
    }

    /// 挂起就是把 `mem` 写进去。这里用真文件代替 `/sys/power/state`：
    /// 写进去看得到内容，等于验证了参数和路径拼装。
    #[test]
    fn suspend_writes_mem() {
        let p = tmp("write");
        std::fs::write(&p, "freeze standby mem\n").unwrap();
        suspend_at(&p).unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "mem");
        let _ = std::fs::remove_file(&p);
        // 路径不可写时要回错误，而不是 panic
        assert!(suspend_at(&tmp("nodir/state")).is_err());
    }
}
