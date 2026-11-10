//! 离线看荧光笔的细网点、书法笔的扁笔尖：
//! `cargo run --release --example ink_kinds`
//!
//! 生成 `shots/ink-kinds.pgm`。面板上到底挡不挡字只能人看，这里先把"像素长什么样"
//! 摆出来——尤其荧光笔：官方是浅灰平涂，我们要用 1px 网点去凑同样的观感。

use rmdev::canvas::Canvas;
use rmdev::pen::line::Color;
use rmdev::pen::Tool;

fn main() {
    let (w, h) = (900usize, 620usize);
    let mut c = Canvas::new(w, h);

    // ---- 假装是正文：几行小字（黑色 2px 线段 + 词间距）----
    for row in 0..3 {
        let y0 = 60.0 + row as f32 * 46.0;
        for i in 0..40 {
            let x = 40.0 + i as f32 * 20.0;
            if i % 7 == 3 {
                continue; // 词间距
            }
            c.set_ink(Tool::BallpointV2, Color::Black, 0, 1.0);
            c.ink_segment(x, y0, 1.0, x + 13.0, y0, 1.0);
            // 再竖一下，像字母
            if i % 3 == 0 {
                c.ink_segment(x + 4.0, y0 - 7.0, 0.9, x + 4.0, y0 + 7.0, 0.9);
            }
        }
    }

    // ---- 荧光笔四档：横着各涂一条，压在"字"上 ----
    let hl = [
        Color::HighlighterYellow,
        Color::HighlighterGreen,
        Color::HighlighterPink,
        Color::HighlighterGray,
    ];
    for (i, col) in hl.iter().enumerate() {
        let y = 60.0 + i as f32 * 46.0;
        let mut x = 40.0;
        while x < 800.0 {
            c.set_ink(Tool::HighlighterV2, *col, 0, 1.0);
            c.ink_segment(x, y, 15.0, x + 2.0, y, 15.0);
            x += 2.0;
        }
    }

    // ---- 书法笔：八个方向各划一道，宽度按公式给的范围取 ----
    for (i, ang) in [0.0f32, 0.4, 0.87, 1.2, 1.57, 2.0, 2.6, 3.14].iter().enumerate() {
        let (cx, cy) = (120.0 + i as f32 * 100.0, 420.0);
        let (dx, dy) = (ang.cos(), ang.sin());
        // 从轻到重划一道：宽度 1.5 → 11（官方中档 + 满压的范围）
        let mut prev: Option<(f32, f32, f32)> = None;
        for k in 0..=40 {
            let t = k as f32 / 40.0;
            let p = 0.25 + 0.75 * t;
            let r = (1.5 + 9.5 * t) * 0.5;
            let (px, py) = (cx + dx * (t - 0.5) * 70.0, cy + dy * (t - 0.5) * 70.0);
            c.set_ink(Tool::Calligraphy, Color::Black, 0, p);
            if let Some((qx, qy, qr)) = prev {
                c.ink_segment(qx, qy, qr, px, py, r);
            }
            prev = Some((px, py, r));
        }
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
    std::fs::write("shots/ink-kinds.pgm", s).unwrap();
    eprintln!("写好了 shots/ink-kinds.pgm");
}
