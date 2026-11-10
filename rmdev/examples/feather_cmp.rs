//! 羽化开/关的边缘对比：`cargo run --release --example feather_cmp`
use rmdev::canvas::Canvas;
use rmdev::pen::line::Color;
use rmdev::pen::Tool;

fn line(c: &mut Canvas, x0: f32, y0: f32, x1: f32, y1: f32, w: f32) {
    c.set_ink(Tool::Calligraphy, Color::Black, 0, 1.0);
    let n = 90;
    let mut prev: Option<(f32, f32)> = None;
    for k in 0..=n {
        let t = k as f32 / n as f32;
        let (x, y) = (x0 + (x1 - x0) * t, y0 + (y1 - y0) * t);
        if let Some((px, py)) = prev {
            c.ink_segment(px, py, w * 0.5, x, y, w * 0.5);
        }
        prev = Some((x, y));
    }
}

fn main() {
    let (w, h) = (420usize, 260usize);
    let mut c = Canvas::new(w, h);
    c.feather = true;
    line(&mut c, 30.0, 30.0, 380.0, 110.0, 9.0);
    c.feather = false;
    line(&mut c, 30.0, 160.0, 380.0, 240.0, 9.0);
    let mut out = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            out.push(255 - c.dark_at(x, y));
        }
    }
    std::fs::create_dir_all("shots").ok();
    let mut s = format!("P5\n{w} {h}\n255\n").into_bytes();
    s.extend_from_slice(&out);
    std::fs::write("shots/feather_cmp.pgm", s).unwrap();
    eprintln!("写好了 shots/feather_cmp.pgm（上=羽化 下=硬边）");
}
