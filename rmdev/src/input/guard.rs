//! 输入线程的两条兜底：**按下超时强制复位** + **事件循环重建**。
//!
//! # 为什么要这个
//!
//! `/dev/input/eventN` 上的 `BTN_TOUCH` 和 `MT_TRACKING_ID` 是**状态**不是动作。
//! 丢一个 UP（注入脚本被 kill、`SYN_DROPPED`、设备在半路被解绑、启动脚本写了一半
//! 就退出……）之后，两个设备线程各自的状态机会永远停在"按着"：
//!
//! * 笔（[`crate::input::PenEvent`]）：`was_contact` 一直是 `true`，之后每一帧都算
//!   `Move`，**再也产不出 `Down`** —— 主循环收不到落笔，屏幕上什么都不发生；
//! * 手指（[`crate::input::touch::Fingers`]）：某个 slot 的 `tid` 不复位，`sent`
//!   永远非空，之后每一次点击都只报 `Move`（`Down` 只在 `sent` 由空变非空时报）。
//!
//! 两条症状一模一样："画面正常、电源键有效、kill -USR1 抓图正常，但笔和手指都没反应"，
//! 因为坏掉的只是这两个线程里的状态机，主循环和事件通道一点事没有。
//! 而且内核会**抑制重复的按键事件**（`input_handle_event`：`!!test_bit(key) != !!value`
//! 才发），所以卡在 `BTN_TOUCH=1` 时，下一次真的按下不会再产生一条 `1` —— 状态机
//! 连自愈的机会都没有。
//!
//! [`ReleaseGuard`] 就是那个"超时兜底"：纯状态机，时间由调用方注入，单测不用等真时间。
//!
//! # 判据
//!
//! 从按下起算超过 `timeout`，**并且**最近一小段时间（[`QUIET_MS`]）一个事件都没有，
//! 就判定这一按丢了。加后半个条件是为了不切真笔画：连续画 12 秒的线每一帧都有事件，
//! 不会被当成卡死；而"按下之后再也没动静"（真正卡死的形态）10 秒内就会复位。

/// 笔按下的合理上限：正常写一笔不会有 10 秒的静默。
pub const PEN_TIMEOUT_MS: f64 = 10_000.0;
/// 手指按住的合理上限（点一下 UI 用不了 5 秒）。
pub const TOUCH_TIMEOUT_MS: f64 = 5_000.0;
/// "静默"的门槛：超过 `timeout` 之后还要再安静这么久才动手，
/// 免得把一条长时间连续落墨的笔画切断。
pub const QUIET_MS: f64 = 500.0;

/// 喂一帧之后的结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GuardAct {
    /// 没有变化
    None,
    /// 设备从"松开"变成"按着"（这一帧要发 Down）
    Down,
    /// 设备从"按着"变成"松开"（这一帧要发 Up）
    Up,
    /// 超时兜底：`timeout` 没等到松开，强制当成松开（要发一个 Up 再复位）
    Forced,
}

/// 一次"按下 → 抬起"的看门狗。
///
/// 调用方做两件事：
/// * 每收到该设备的一帧就 `frame(设备宣称按着吗, now)`；
/// * 没事件的时候也要周期醒来 `poll(now)`（线程用 `poll(2)` 超时，见
///   [`super::evdev::poll_readable`]），否则设备彻底静默时线程还堵在 `read()` 里，
///   看门狗根本没机会跑。
#[derive(Clone, Debug)]
pub struct ReleaseGuard {
    timeout_ms: f64,
    /// `Some(按下时刻)` = 认为按着
    down_at: Option<f64>,
    /// 最近一次见到事件 / 判定的时刻（`poll` 的静默判据用它）
    last_ms: f64,
    /// 这一按已经强制复位过了：设备还在说"按着"时不再重复报警，
    /// 一直等到设备真的松手（`frame(false)`）才重新武装。
    forced: bool,
}

impl ReleaseGuard {
    pub fn new(timeout_ms: f64) -> Self {
        ReleaseGuard {
            timeout_ms,
            down_at: None,
            last_ms: 0.0,
            forced: false,
        }
    }

    pub fn timeout_ms(&self) -> f64 {
        self.timeout_ms
    }

    /// 现在是否认为"按着"（强制复位后是 `false`）。
    pub fn pressed(&self) -> bool {
        self.down_at.is_some()
    }

    /// 喂一帧。`raw` = 设备此刻宣称按着吗（笔看 `BTN_TOUCH`，手指看"表里还有没有手指"）。
    pub fn frame(&mut self, raw: bool, now_ms: f64) -> GuardAct {
        self.last_ms = now_ms;
        match (raw, self.down_at) {
            // 强制复位过、设备还没松手：什么都不报，等它松
            (true, None) if self.forced => GuardAct::None,
            (true, None) => {
                self.down_at = Some(now_ms);
                GuardAct::Down
            }
            (true, Some(_)) => GuardAct::None,
            (false, Some(_)) => {
                self.down_at = None;
                self.forced = false;
                GuardAct::Up
            }
            (false, None) => {
                self.forced = false;
                GuardAct::None
            }
        }
    }

    /// 没事件时的周期检查。返回 [`GuardAct::Forced`] 表示"这一按丢了，强制复位"。
    pub fn poll(&mut self, now_ms: f64) -> GuardAct {
        let Some(t0) = self.down_at else {
            return GuardAct::None;
        };
        if now_ms - t0 < self.timeout_ms {
            return GuardAct::None;
        }
        if now_ms - self.last_ms < QUIET_MS {
            return GuardAct::None;
        }
        self.down_at = None;
        self.forced = true;
        GuardAct::Forced
    }
}

/// 一个输入线程的结束原因。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoopEnd {
    /// 主循环没了（事件通道关了）：线程该退休，**不要**重建。
    ConsumerGone,
    /// 设备读/轮询失败（拔了、被解绑、EBADF、EOF……）：重建。
    Device(String),
    /// 线程里 panic 被兜住了：重建。
    Panic,
}

impl LoopEnd {
    /// 设备错误的便捷构造。
    pub fn device(e: impl std::fmt::Display) -> LoopEnd {
        LoopEnd::Device(e.to_string())
    }
}

/// 事件循环挂了要不要重建。纯函数，单测直接喂三种结局。
///
/// 只有"主循环没了"不该重建——那是有意的退出；设备消失/线程 panic 都必须
/// **重新打开设备再跑**，否则就是"线程静默死掉，用户在设备上对着没反应的屏"。
pub fn should_restart_after(end: &LoopEnd) -> bool {
    !matches!(end, LoopEnd::ConsumerGone)
}

/// 重建前的等待：设备刚被拔掉时立刻重开会连打一串失败。
pub const RESTART_DELAY: std::time::Duration = std::time::Duration::from_millis(1000);

/// 输入线程外面那层壳：跑 `body`，挂了就记录 + 重建，直到主循环不要我们了。
///
/// `body` 每次被调用都要自己重新打开设备（重开失败就返回
/// [`LoopEnd::Device`]，外面会隔 [`RESTART_DELAY`] 再来一次）。
///
/// 注意 `[profile.release] panic = "abort"`：真机上 panic 会**整个进程退出**
/// （比"线程静默死掉"好，至少 remux 会接回去）。`catch_unwind` 兜的是
/// debug/测试构建、以及以后把 `panic` 改回 unwind 的情况。
pub fn supervise<F>(name: &str, mut body: F)
where
    F: FnMut() -> LoopEnd,
{
    supervise_with(name, RESTART_DELAY, &mut body)
}

/// 同 [`supervise`]，等待时长由调用方给（单测用 0，别真睡）。
pub fn supervise_with<F>(name: &str, delay: std::time::Duration, mut body: F)
where
    F: FnMut() -> LoopEnd,
{
    let mut restarts = 0u32;
    loop {
        let end = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(&mut body)) {
            Ok(end) => end,
            Err(_) => LoopEnd::Panic,
        };
        if !should_restart_after(&end) {
            eprintln!("[{name}] 事件循环结束（{end:?}），线程退出");
            return;
        }
        restarts += 1;
        eprintln!("[{name}] 事件循环结束（{end:?}），重建（第 {restarts} 次）");
        std::thread::sleep(delay);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 正常一笔：down → up，不多报不漏报。
    #[test]
    fn down_then_up_is_reported_once() {
        let mut g = ReleaseGuard::new(PEN_TIMEOUT_MS);
        assert_eq!(g.frame(true, 0.0), GuardAct::Down);
        assert!(g.pressed());
        // 中间每帧都是"还按着"，不重复报 down
        assert_eq!(g.frame(true, 8.0), GuardAct::None);
        assert_eq!(g.frame(true, 16.0), GuardAct::None);
        assert_eq!(g.frame(false, 24.0), GuardAct::Up);
        assert!(!g.pressed());
        // 再松一次不重复报
        assert_eq!(g.frame(false, 32.0), GuardAct::None);
    }

    /// 按下之后设备彻底静默：10 s 判丢，强制复位一次。
    #[test]
    fn pen_down_without_up_is_forced_after_ten_seconds() {
        let mut g = ReleaseGuard::new(PEN_TIMEOUT_MS);
        assert_eq!(g.frame(true, 0.0), GuardAct::Down);
        // 9.9 s：还不到点
        assert_eq!(g.poll(9_900.0), GuardAct::None);
        assert!(g.pressed());
        // 10 s：强制复位
        assert_eq!(g.poll(10_000.0), GuardAct::Forced);
        assert!(!g.pressed());
        // 不重复报：后面每 250 ms 醒一次也只有一个 Forced
        for t in [10_250.0, 11_000.0, 30_000.0] {
            assert_eq!(g.poll(t), GuardAct::None, "{t}");
        }
        // 设备还在说"按着"也不能再报 Down（等它真的松手）
        assert_eq!(g.frame(true, 30_100.0), GuardAct::None);
        // 设备松手：不补一个 Up（已经报过了），只是重新武装
        assert_eq!(g.frame(false, 30_200.0), GuardAct::None);
        // 重新武装之后新的一笔正常
        assert_eq!(g.frame(true, 30_300.0), GuardAct::Down);
    }

    /// 手指和笔各有自己的计时器，互不影响（5 s < 10 s）。
    #[test]
    fn pen_and_touch_timers_are_independent() {
        let mut pen = ReleaseGuard::new(PEN_TIMEOUT_MS);
        let mut touch = ReleaseGuard::new(TOUCH_TIMEOUT_MS);
        assert_eq!(pen.frame(true, 0.0), GuardAct::Down);
        assert_eq!(touch.frame(true, 0.0), GuardAct::Down);

        assert_eq!(touch.poll(5_000.0), GuardAct::Forced, "手指 5 s 就该复位");
        assert_eq!(pen.poll(5_000.0), GuardAct::None, "笔 5 s 还没到点");
        assert!(pen.pressed());

        assert_eq!(touch.poll(5_250.0), GuardAct::None, "手指复位过就别再报");
        assert_eq!(pen.poll(10_000.0), GuardAct::Forced, "笔 10 s 到点");
        assert!(!pen.pressed());
    }

    /// 反复按下：每次 down 都重新起算，不能被前一次的残留时间误判成超时。
    #[test]
    fn repeated_presses_restart_the_clock() {
        let mut g = ReleaseGuard::new(PEN_TIMEOUT_MS);
        assert_eq!(g.frame(true, 0.0), GuardAct::Down);
        assert_eq!(g.frame(false, 2_000.0), GuardAct::Up);
        assert_eq!(g.frame(true, 3_000.0), GuardAct::Down);
        // 距离上一次按下才 5 s：不误复位
        assert_eq!(g.poll(8_000.0), GuardAct::None);
        assert!(g.pressed());
        // 从第二次按下起算满 10 s 才复位
        assert_eq!(g.poll(13_000.0), GuardAct::Forced);
    }

    /// 连续落墨（每帧都有事件）不能被当成卡死切断：静默不够久就不动手。
    #[test]
    fn continuous_ink_keeps_the_guard_armed() {
        let mut g = ReleaseGuard::new(PEN_TIMEOUT_MS);
        assert_eq!(g.frame(true, 0.0), GuardAct::Down);
        // 画到 9.7 s 还在动
        assert_eq!(g.frame(true, 9_700.0), GuardAct::None);
        // 总时长过了 10 s，但 0.3 s 前才有事件 → 不动手
        assert_eq!(g.poll(10_000.0), GuardAct::None);
        assert!(g.pressed());
        // 又画到 12 s（事件一直在），照样不动
        assert_eq!(g.frame(true, 12_000.0), GuardAct::None);
        assert_eq!(g.poll(12_400.0), GuardAct::None, "静默才 400 ms");
        // 笔停住不动了：静默 500 ms 之后才复位（总时长早过 10 s）
        assert_eq!(g.poll(12_500.0), GuardAct::Forced);
    }

    /// 静默门槛本身：只有时长够、但刚刚还有事件 → 不判丢。
    #[test]
    fn quiet_window_is_required_even_after_the_timeout() {
        let mut g = ReleaseGuard::new(TOUCH_TIMEOUT_MS);
        g.frame(true, 0.0);
        g.frame(true, 5_000.0); // 这一帧还在动
        assert_eq!(g.poll(5_300.0), GuardAct::None, "静默才 300 ms");
        assert_eq!(g.poll(5_500.0), GuardAct::Forced, "静默满 500 ms");
    }

    /// 重建判定：只有"主循环没了"才不重建。
    #[test]
    fn restart_only_when_the_thread_did_not_finish_on_purpose() {
        assert!(!should_restart_after(&LoopEnd::ConsumerGone));
        assert!(should_restart_after(&LoopEnd::Device("ENODEV".into())));
        assert!(should_restart_after(&LoopEnd::Panic));
        // 设备错误的便捷构造（文案随平台走，别写死 Linux 的 strerror）
        assert_eq!(
            LoopEnd::device(std::io::Error::from_raw_os_error(19)),
            LoopEnd::Device(
                std::io::Error::from_raw_os_error(19).to_string()
            )
        );
    }

    /// 那层壳真的会重建：失败两次、第三次主循环没了 → 正好跑三次。
    #[test]
    fn supervise_rebuilds_until_the_consumer_is_gone() {
        let mut runs = 0;
        supervise_with("test", std::time::Duration::ZERO, || {
            runs += 1;
            match runs {
                1 | 2 => LoopEnd::Device("gone".into()),
                _ => LoopEnd::ConsumerGone,
            }
        });
        assert_eq!(runs, 3, "两次失败各重建一次");
    }
}
