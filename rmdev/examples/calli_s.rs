//! 按用户的验收方法复现书法笔：清屏 → 画 S 型（两个半圆），压力随 x 变化。
//! `cargo run --release --example calli_s`
//!
//! 走的是**真实管线**：`StrokeBuilder` 算宽度 → `Canvas::ink_segment` 落墨，
//! 和 `main.rs` 里笔在屏上画的那条路径一模一样（之前的 calli3 是手搓半径，骗自己）。
use rmdev::canvas::Canvas;
use rmdev::pen::line::{Color, InputPoint, Tool};
use rmdev::pen::stroke::StrokeBuilder;

const W: usize = 1404;
const H: usize = 1400;

fn ip(x: f32, y: f32, p: f32) -> InputPoint {
    InputPoint { x, y, pressure: p, tilt_x: 0.0, tilt_y: 0.0, rubber: false }
}

/// 画一条 S（两个半圆），半径 r，从 (cx, cy-2r) 往下画到 (cx, cy+2r)。
fn draw_s(c: &mut Canvas, b: &mut StrokeBuilder, cx: f32, cy: f32, r: f32, p0: f32, p1: f32) {
    let n = 160;
    let mut prev: Option<(f32, f32, f32)> = None;
    let mut widths = Vec::new();
    // 经典 S：三次贝塞尔，控制点随手写一笔的走向
    let ctrl = [
        (cx + r, cy - 2.0 * r),
        (cx - 2.0 * r, cy - 2.0 * r),
        (cx + 2.0 * r, cy + 2.0 * r),
        (cx - r, cy + 2.0 * r),
    ];
    let s_point = |t: f32| -> (f32, f32) {
        let u = 1.0 - t;
        let b = [u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t];
        let mut x = 0.0;
        let mut y = 0.0;
        for i in 0..4 {
            x += b[i] * ctrl[i].0;
            y += b[i] * ctrl[i].1;
        }
        (x, y)
    };
    b.begin(ip(s_point(0.0).0, s_point(0.0).1, p0));
    for k in 0..=n {
        let t = k as f32 / n as f32;
        let (x, y) = s_point(t);
        let p = b.add(ip(x, y, p0 + (p1 - p0) * t));
        widths.push(p.width);
        let rad = p.width * 0.5;
        c.set_ink(b.tool(), b.color(), 0, p.pressure);
        if let Some((qx, qy, qr)) = prev {
            c.ink_segment(qx, qy, qr, x, y, rad);
        }
        prev = Some((x, y, rad));
    }
    let mx = widths.iter().cloned().fold(f32::MIN, f32::max);
    let mn = widths.iter().cloned().fold(f32::MAX, f32::min);
    eprintln!("S @x={cx}: 宽度 {mn:.2}..{mx:.2}px（直径）");
}

fn main() {
    let mut c = Canvas::new(W, H);
    // 三档粗细、三道 S，压力从 0.15 递增到 0.95（模拟"越画越重"）
    for (i, thick) in [1.0f32, 2.0, 3.0].into_iter().enumerate() {
        let mut b = StrokeBuilder::new(Tool::Calligraphy, Color::Black, thick, 1.0);
        let cx = 300.0 + i as f32 * 400.0;
        draw_s(&mut c, &mut b, cx, 400.0, 180.0, 0.15, 0.95);
        let mut b = StrokeBuilder::new(Tool::Calligraphy, Color::Black, thick, 1.0);
        draw_s(&mut c, &mut b, cx, 900.0, 180.0, 0.95, 0.15);
    }
    let mut out = Vec::with_capacity(W * H);
    for y in 0..H {
        for x in 0..W {
            out.push(255 - c.dark_at(x, y));
        }
    }
    std::fs::create_dir_all("shots").ok();
    let mut s = format!("P5\n{W} {H}\n255\n").into_bytes();
    s.extend_from_slice(&out);
    std::fs::write("shots/calli_s.pgm", s).unwrap();
    eprintln!("写好了 shots/calli_s.pgm");
}
