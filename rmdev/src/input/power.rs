//! 电源键（`snvs-powerkey`，KEY_POWER = 116）。
//!
//! 这台机器上它挂在 `/dev/input/event0`（`30370000.snvs:snvs-powerkey`，
//! `H: Handlers=kbd event0`，`B: KEY=100000` 就是 KEY_POWER 那一位）。
//! 但 event 编号会随固件/设备树变，所以**按能力找**：谁有 KEY_POWER 就是谁。
//!
//! 注意：内核里这个键没有别人接管（没有 logind），xochitl 停掉之后
//! 按下去是真的"毫无反应"——不是坏的，是没人读。

use std::io;
use std::time::Instant;

use super::evdev::{self, Evdev};

pub const KEY_POWER: u16 = 116;

/// 一次按下 / 抬起。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerEvent {
    Down,
    /// 抬起，带按下时长（官方是 3 s 分界，见 `crate::power`）
    Up { held_ms: u32 },
}

/// 事件解码（纯逻辑，方便单测）。
#[derive(Default)]
pub struct Decoder {
    down_at: Option<Instant>,
}

impl Decoder {
    pub fn new() -> Decoder {
        Decoder::default()
    }

    /// 喂一个 evdev 事件；只认 KEY_POWER 的按下/抬起。
    pub fn feed(&mut self, typ: u16, code: u16, value: i32, now: Instant) -> Option<PowerEvent> {
        if typ != evdev::EV_KEY || code != KEY_POWER {
            return None;
        }
        match value {
            // 1 = 按下，0 = 抬起，2 = 自动重复（按住不放时内核会发，忽略）
            1 => {
                self.down_at = Some(now);
                Some(PowerEvent::Down)
            }
            0 => {
                let held = self
                    .down_at
                    .take()
                    .map(|t| now.saturating_duration_since(t).as_millis() as u32)
                    .unwrap_or(0);
                Some(PowerEvent::Up { held_ms: held })
            }
            _ => None,
        }
    }
}

/// 找一个带 KEY_POWER 的输入设备。
pub fn find_device() -> Option<String> {
    use std::os::unix::io::AsRawFd;
    for i in 0..8 {
        let p = format!("/dev/input/event{i}");
        let Ok(f) = std::fs::File::open(&p) else {
            continue;
        };
        if evdev::has_key(f.as_raw_fd(), KEY_POWER) {
            return Some(p);
        }
    }
    None
}

/// 起一个专用线程阻塞读电源键，事件交给 `sink`。
///
/// 没有独立的唤醒机制：主循环每圈 `try_recv`，电源键事件和笔/手指走同一个
/// channel，所以按下到响应最坏差一个主循环轮次（< 1 ms）。
pub fn spawn<F>(path: &str, sink: F) -> io::Result<()>
where
    F: Fn(PowerEvent) + Send + 'static,
{
    let mut dev = Evdev::open(path)?;
    std::thread::Builder::new()
        .name("powerkey".into())
        .spawn(move || run(&mut dev, sink))
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("powerkey: {e}")))?;
    Ok(())
}

fn run<F>(dev: &mut Evdev, sink: F)
where
    F: Fn(PowerEvent),
{
    let mut dec = Decoder::new();
    loop {
        let events = match dev.read() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[power] read: {e}");
                return;
            }
        };
        // 事件切片借用了 dev 的内部缓冲，要当场消费完
        for ev in events {
            if let Some(e) = dec.feed(ev.typ, ev.code, ev.value, Instant::now()) {
                sink(e);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn t() -> Instant {
        Instant::now()
    }

    #[test]
    fn press_and_release_carry_the_held_time() {
        let t0 = t();
        let mut d = Decoder::new();
        assert_eq!(
            d.feed(evdev::EV_KEY, KEY_POWER, 1, t0),
            Some(PowerEvent::Down)
        );
        assert_eq!(
            d.feed(evdev::EV_KEY, KEY_POWER, 0, t0 + Duration::from_millis(140)),
            Some(PowerEvent::Up { held_ms: 140 })
        );
    }

    /// 按住不放时内核会连发 value=2（自动重复），那不能当成一次抬起。
    #[test]
    fn autorepeat_is_ignored() {
        let t0 = t();
        let mut d = Decoder::new();
        d.feed(evdev::EV_KEY, KEY_POWER, 1, t0);
        for i in 1..=5 {
            assert_eq!(d.feed(evdev::EV_KEY, KEY_POWER, 2, t0), None, "第 {i} 次重复");
        }
        assert_eq!(
            d.feed(evdev::EV_KEY, KEY_POWER, 0, t0 + Duration::from_millis(4_200)),
            Some(PowerEvent::Up { held_ms: 4_200 })
        );
    }

    /// 同一个设备上的其它键（音量/亮屏之类）不能触发睡眠。
    #[test]
    fn other_keys_and_types_are_ignored() {
        let t0 = t();
        let mut d = Decoder::new();
        assert_eq!(d.feed(evdev::EV_KEY, 114, 1, t0), None, "KEY_VOLUMEDOWN");
        assert_eq!(d.feed(evdev::EV_SYN, 0, 0, t0), None);
        assert_eq!(d.feed(evdev::EV_ABS, 0, 100, t0), None);
        // 没有按下就收到抬起：给 0 ms，不 panic
        assert_eq!(
            d.feed(evdev::EV_KEY, KEY_POWER, 0, t0),
            Some(PowerEvent::Up { held_ms: 0 })
        );
    }
}
