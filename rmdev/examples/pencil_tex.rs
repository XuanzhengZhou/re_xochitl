//! 离线看铅笔网点长什么样：`cargo run --release --example pencil_tex`
//!
//! 生成 `shots/pencil-tex.pgm`（灰度），再转 PNG 放大看。
//! 这不是给设备的程序，只是开发时眼睛的替代品——面板上的浓淡只能靠人看。

use rmdev::canvas::Canvas;
use rmdev::pen::line::Color;
use rmdev::pen::pencil::GRADES;
use rmdev::pen::Tool;

fn main() {
    let (w, h) = (900usize, GRADES.len() * 140 + 40);
    let mut c = Canvas::new(w, h);
    let mut stroke = |c: &mut Canvas, y0: f32, r: f32, gi: usize| {
        let mut prev: Option<(f32, f32)> = None;
        for i in 0..=70 {
            let t = i as f32 / 70.0;
            let x = 40.0 + t * 820.0;
            let y = y0 + (t * 18.0).sin() * 14.0;
            // 压力沿线变化：左轻右重
            c.set_ink(Tool::PencilV2, Color::Gray, gi, 0.15 + 0.85 * t);
            if let Some((px, py)) = prev {
                c.ink_segment(px, py, r, x, y, r);
            }
            prev = Some((x, y));
        }
    };
    for gi in 0..GRADES.len() {
        let y0 = 70.0 + gi as f32 * 140.0;
        // 细线（r=1.5，等于最细的铅笔）在上，粗线（r=5，倾斜时的实际宽度）在下
        stroke(&mut c, y0, 1.5, gi);
        stroke(&mut c, y0 + 58.0, 5.0, gi);
    }
    let mut out = vec![0u8; w * h];
    for y in 0..h {
        for x in 0..w {
            out[y * w + x] = 255 - c.dark_at(x, y);
        }
    }
    let mut s = format!("P5\n{w} {h}\n255\n").into_bytes();
    s.extend_from_slice(&out);
    std::fs::create_dir_all("shots").ok();
    std::fs::write("shots/pencil-tex.pgm", s).unwrap();
    eprintln!("写好了 shots/pencil-tex.pgm（7 档 × 细/粗两种线宽）");
}
