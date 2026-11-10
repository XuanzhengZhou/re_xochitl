//! 笔设备探针：只打开 evdev、打印解码后的帧，不跑 UI。
//!
//! 用来把「内核有没有事件」和「app 的事件通路有没有问题」分开。

use rmdev::input::evdev::{self, Evdev};
use rmdev::input::decode::{Axes, Frame};

fn main() {
    let path = std::env::args().nth(1).unwrap_or_else(|| "/dev/input/event1".into());
    let mut dev = Evdev::open(&path).expect("open evdev");
    let axes = Axes::from_fd(dev.fd()).expect("EVIOCGABS");
    println!("探针已开 {path}，x≤{:.0} y≤{:.0} p≤{:.0}", axes.x_max, axes.y_max, axes.p_max);
    let mut frame = Frame::default();
    let mut n = 0u64;
    let mut last = std::time::Instant::now();
    loop {
        let evs = match dev.read() {
            Ok(e) => e,
            Err(e) => {
                println!("read: {e}");
                return;
            }
        };
        for ev in evs {
            n += 1;
            if frame.feed(ev.typ, ev.code, ev.value) {
                if frame.dirty || frame.contact {
                    let p = frame.point(&axes, 1404.0, 1872.0);
                    println!(
                        "t={:.1}s 接触={} 范围={} {:?}",
                        last.elapsed().as_secs_f32(),
                        frame.contact,
                        frame.in_range,
                        p
                    );
                }
                frame.clear_dirty();
            }
        }
        if n % 200 == 0 {
            println!("累计原始事件 {n}");
        }
        let _ = evdev::EV_SYN;
    }
}
