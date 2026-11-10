//! 笔输入：数字化仪线程 → `PenEvent` 流。
//!
//! 官方在这里是 `Digitizer`（0x170 字节，`operator_new(0x170)` @0x16be50）+
//! `PenInputThread`，我们只保留"读 → 解码 → 派发"这三步。

pub mod decode;
pub mod evdev;
pub mod guard;
pub mod power;
pub mod touch;

use std::io;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::pen::InputPoint;
use decode::{Axes, Frame};
use guard::{GuardAct, LoopEnd, ReleaseGuard};

pub use decode::Axes as AxisRange;
pub use guard::{should_restart_after, PEN_TIMEOUT_MS, TOUCH_TIMEOUT_MS};

/// 设备静默时线程的醒来周期：`ReleaseGuard` 的超时判定靠它驱动。
const POLL_MS: i32 = 250;

/// 笔的离散事件。坐标已经归一化到屏幕像素。
#[derive(Clone, Copy, Debug)]
pub enum PenEvent {
    /// 落笔（`BTN_TOUCH` 0→1）
    Down(InputPoint),
    /// 落笔后每个 `SYN_REPORT`
    Move(InputPoint),
    /// 抬笔
    Up,
    /// 离开感应范围
    Out,
}

/// 键盘以外的所有输入：笔 + 手指。
#[derive(Clone, Copy, Debug)]
pub enum InputEvent {
    Pen(PenEvent),
    Touch(touch::TouchEvent),
    /// 电源键（`snvs-powerkey`）。短按睡/醒，按住 3 s 弹关机框——
    /// 语义在 `rmdev::power`，这里只负责把事件送到主循环。
    Power(power::PowerEvent),
}

/// 两个设备线程合流到一个 channel，主循环只认一种事件。
pub struct InputThread {
    rx: Receiver<InputEvent>,
    /// 触摸线程维护的全部手指；主循环收完事件后取快照喂给手势识别
    fingers: Arc<Mutex<touch::Fingers>>,
}

impl InputThread {
    pub fn spawn(pen_dev: &str, touch_dev: &str, w: f32, h: f32) -> std::io::Result<InputThread> {
        let (tx, rx) = channel();
        let pen_tx = tx.clone();
        PenThread::spawn_with(pen_dev, w, h, move |e| pen_tx.send(InputEvent::Pen(e)).is_ok())?;
        // 电源键：按能力自动找设备（谁能发 KEY_POWER 就是谁）。
        // 找不到只是"按电源键没反应"，不能连累笔和手指。
        let ptx = tx.clone();
        match power::find_device() {
            Some(dev) => match power::spawn(&dev, move |e| {
                let _ = ptx.send(InputEvent::Power(e));
            }) {
                Ok(()) => eprintln!("[input] 电源键 {dev}"),
                Err(e) => eprintln!("[input] 电源键不可用（按了没反应）：{e}"),
            },
            None => eprintln!("[input] 没找到电源键设备（没有 KEY_POWER）"),
        }
        let fingers = Arc::new(Mutex::new(touch::Fingers::new(w, h)));
        let ttx = tx.clone();
        let (touch_tx, touch_rx) = channel::<touch::TouchEvent>();
        match touch::spawn(touch_dev, touch_tx, &fingers) {
            Ok(()) => {
                std::thread::Builder::new()
                    .name("touch-fwd".into())
                    .spawn(move || {
                        while let Ok(e) = touch_rx.recv() {
                            if ttx.send(InputEvent::Touch(e)).is_err() {
                                break;
                            }
                        }
                    })
                    .ok();
            }
            Err(e) => eprintln!("[input] 触摸不可用（只能用笔）：{e}"),
        }
        Ok(InputThread { rx, fingers })
    }

    #[inline]
    pub fn try_recv(&self) -> Option<InputEvent> {
        self.rx.try_recv().ok()
    }

    /// 当前所有按着的手指 `(tracking_id, x, y)`，屏幕坐标。
    ///
    /// `TouchEvent` 报的是单指的 Down/Move/Up 生命周期，双指要的"全部手指"
    /// 从这里取；每次收到 `InputEvent::Touch` 后调一次就够。
    pub fn fingers(&self) -> Vec<(i64, f32, f32)> {
        self.fingers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .fingers()
    }

    pub fn finger_count(&self) -> usize {
        self.fingers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .finger_count()
    }
}

pub struct PenThread {
    rx: Receiver<PenEvent>,
}

impl PenThread {
    /// 起一个专用线程独占 `/dev/input/eventN`，事件交给外部闭包（合流用）。
    ///
    /// 为什么必须是线程：`read()` 会阻塞，而主线程要负责渲染和刷屏；
    /// 官方同样用了独立线程 + 条件变量，这是"不丢笔"的前提。
    ///
    /// `sink` 返回 `false` = 收端没了（主循环退出），线程退休、不重建。
    ///
    /// 线程外面套了 [`guard::supervise`]：读设备失败（ENODEV / EOF / poll 挂断）
    /// 或者线程 panic 都**不是静默死掉**，而是隔一秒重新打开设备再来一遍。
    pub fn spawn_with<F>(path: &str, w: f32, h: f32, sink: F) -> std::io::Result<()>
    where
        F: Fn(PenEvent) -> bool + Send + 'static,
    {
        // 先开一次：设备不在 / 量程读不到就照旧让调用方（主程序）知道，
        // 那是启动期的硬错误，不该进重建循环。
        {
            let dev = evdev::Evdev::open(path)?;
            let _ = Axes::from_fd(dev.fd())?;
        }
        let path = path.to_string();
        std::thread::Builder::new()
            .name("digitizer".into())
            .spawn(move || {
                guard::supervise("pen", || match open_pen(&path) {
                    Ok((mut dev, axes)) => run_with(&mut dev, &axes, w, h, &sink),
                    Err(e) => LoopEnd::device(e),
                })
            })
            .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("digitizer: {e}")))?;
        Ok(())
    }

    pub fn spawn(path: &str, w: f32, h: f32) -> std::io::Result<PenThread> {
        let (tx, rx): (Sender<PenEvent>, Receiver<PenEvent>) = channel();
        PenThread::spawn_with(path, w, h, move |e| tx.send(e).is_ok())?;
        Ok(PenThread { rx })
    }

    #[inline]
    pub fn try_recv(&self) -> Option<PenEvent> {
        self.rx.try_recv().ok()
    }
}

/// 打开笔设备 + 量程。重建时每次都要重新走一遍（fd 可能已经换了）。
fn open_pen(path: &str) -> io::Result<(evdev::Evdev, Axes)> {
    let dev = evdev::Evdev::open(path)?;
    let axes = Axes::from_fd(dev.fd())?;
    eprintln!(
        "[pen] {} axes x=[{:.0},{:.0}] y=[{:.0},{:.0}] p_max={:.0}",
        path, axes.x_min, axes.x_max, axes.y_min, axes.y_max, axes.p_max
    );
    Ok((dev, axes))
}

/// 笔的事件循环。返回 [`LoopEnd`]：`ConsumerGone` = 正常收工。
///
/// 按下 → 抬起由 [`ReleaseGuard`] 说话（不再自己维护 `was_contact`），
/// 这样"丢了 UP"时它能 10 s 后强制复位，而不是永远等下去。
fn run_with<F>(
    dev: &mut evdev::Evdev,
    axes: &Axes,
    w: f32,
    h: f32,
    sink: &F,
) -> LoopEnd
where
    F: Fn(PenEvent) -> bool,
{
    let mut frame = Frame::default();
    let mut guard = ReleaseGuard::new(PEN_TIMEOUT_MS);
    let t0 = Instant::now();
    let now_ms = |t0: &Instant| t0.elapsed().as_secs_f64() * 1000.0;

    loop {
        // 静默 250 ms 醒一次：设备再也不会来事件时，超时兜底才有机会跑。
        match dev.poll(POLL_MS) {
            Ok(true) => {}
            Ok(false) => {
                if guard.poll(now_ms(&t0)) == GuardAct::Forced {
                    eprintln!(
                        "[pen] DOWN 后 {} s 没有 UP，强制复位",
                        (PEN_TIMEOUT_MS / 1000.0) as u64
                    );
                    if !sink(PenEvent::Up) {
                        return LoopEnd::ConsumerGone;
                    }
                }
                continue;
            }
            Err(e) => return LoopEnd::device(e),
        }

        let events = match dev.read() {
            Ok(e) => e,
            Err(e) => return LoopEnd::device(e),
        };
        // 事件切片借用了 dev 的内部缓冲，这里要立刻消费完。
        let mut synced = false;
        for ev in events {
            if frame.feed(ev.typ, ev.code, ev.value) {
                synced = true;
            }
        }
        if !synced {
            continue;
        }

        let now = now_ms(&t0);
        if frame.dropped {
            // 这一帧的坐标已经不可信（官方也是整帧作废）。但"松开"必须传下去：
            // 丢一个 UP 正是我们要防的死锁。
            let act = guard.frame(frame.contact, now);
            frame.clear_dirty();
            if act == GuardAct::Up && !sink(PenEvent::Up) {
                return LoopEnd::ConsumerGone;
            }
            continue;
        }

        let act = guard.frame(frame.contact, now);
        let dirty = frame.dirty || act != GuardAct::None;
        if !dirty {
            frame.clear_dirty();
            continue;
        }

        let p = frame.point(axes, w, h);
        frame.clear_dirty();

        if let Some(ev) = pen_event(act, guard.pressed(), frame.in_range, p) {
            // 诊断：笔事件和触摸事件打在同一个 stderr 上，`RM_TOUCH_LOG=1` 时
            // 手掌进来那一段能按时间戳和 `[tlog] T` 对起来。
            if touch::tlog() {
                match ev {
                    PenEvent::Down(_) => eprintln!(
                        "[tlog] P down {:.0},{:.0} p={:.3} press={}",
                        p.x, p.y, p.pressure, frame.contact
                    ),
                    PenEvent::Move(_) => eprintln!(
                        "[tlog] P move {:.0},{:.0} p={:.3} press={}",
                        p.x, p.y, p.pressure, frame.contact
                    ),
                    PenEvent::Up => eprintln!("[tlog] P up press={}", frame.contact),
                    PenEvent::Out => eprintln!("[tlog] P out"),
                }
            }
            if !sink(ev) {
                return LoopEnd::ConsumerGone;
            }
        }
    }
}

/// 一帧的结论 → 要发给主循环的事件。纯函数，单测直接喂 `GuardAct`。
///
/// 这里就是那个"丢了 UP 就再也收不到落笔"的地方：`guard` 一直认为按着时，
/// 每一帧只能产出 `Move`，`Down` 要等 `guard` 复位之后才可能再出现。
fn pen_event(act: GuardAct, pressed: bool, in_range: bool, p: InputPoint) -> Option<PenEvent> {
    match act {
        GuardAct::Down => Some(PenEvent::Down(p)),
        GuardAct::Up | GuardAct::Forced => Some(PenEvent::Up),
        GuardAct::None => {
            if pressed {
                Some(PenEvent::Move(p))
            } else if in_range {
                // 悬停：官方有 PenClosenessMonitor + 光标，M1 先丢弃
                None
            } else {
                Some(PenEvent::Out)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt() -> InputPoint {
        InputPoint {
            x: 300.0,
            y: 400.0,
            pressure: 0.5,
            tilt_x: 0.0,
            tilt_y: 0.0,
            rubber: false,
        }
    }

    /// 正常一笔：down → move → up。
    #[test]
    fn a_normal_stroke_maps_down_move_up() {
        let mut g = ReleaseGuard::new(PEN_TIMEOUT_MS);
        let act = g.frame(true, 0.0);
        assert!(matches!(
            pen_event(act, g.pressed(), false, pt()),
            Some(PenEvent::Down(_))
        ));
        let act = g.frame(true, 8.0);
        assert!(matches!(
            pen_event(act, g.pressed(), false, pt()),
            Some(PenEvent::Move(_))
        ));
        let act = g.frame(false, 16.0);
        assert!(matches!(
            pen_event(act, g.pressed(), false, pt()),
            Some(PenEvent::Up)
        ));
    }

    /// 根因证据：UP 丢了之后每一帧都只能是 `Move`，永远等不到 `Down`
    /// （主循环收不到落笔 = 用户在屏幕上"笔没反应"）；超时兜底之后才恢复。
    #[test]
    fn a_lost_up_swallows_every_later_down_until_the_guard_resets() {
        let mut g = ReleaseGuard::new(PEN_TIMEOUT_MS);
        g.frame(true, 0.0);
        for t in [16.0, 100.0, 1_000.0, 9_000.0] {
            let act = g.frame(true, t);
            let ev = pen_event(act, g.pressed(), false, pt());
            assert!(
                matches!(ev, Some(PenEvent::Move(_))),
                "丢 UP 之后 t={t} 只能是 Move，实际 {ev:?}"
            );
        }
        // 静默到 10 s：强制复位，主循环收到 Up（把半截笔收拾干净）
        assert_eq!(g.poll(10_000.0), GuardAct::Forced);
        assert!(matches!(
            pen_event(GuardAct::Forced, g.pressed(), false, pt()),
            Some(PenEvent::Up)
        ));
        // 设备还宣称按着：不重复报；真的松开再按下就恢复
        assert_eq!(g.frame(true, 10_100.0), GuardAct::None);
        assert_eq!(g.frame(false, 10_200.0), GuardAct::None);
        let act = g.frame(true, 10_300.0);
        assert!(matches!(
            pen_event(act, g.pressed(), false, pt()),
            Some(PenEvent::Down(_))
        ));
    }

    /// 悬停（在感应范围内、没接触）什么都不发；离开感应范围发 `Out`。
    #[test]
    fn hover_is_silent_and_leaving_range_is_out() {
        let g = ReleaseGuard::new(PEN_TIMEOUT_MS);
        assert!(pen_event(GuardAct::None, g.pressed(), true, pt()).is_none());
        assert!(matches!(
            pen_event(GuardAct::None, g.pressed(), false, pt()),
            Some(PenEvent::Out)
        ));
    }
}
