//! 用我们的渲染器画一份官方 .rm 页，输出 PGM，用来和官方截图逐像素比。
//! `cargo run --release --example rm_render -- f.rm out.pgm`
use rmdev::canvas::Canvas;
use rmdev::doc::rm::RmPage;

fn main() {
    let mut a = std::env::args().skip(1);
    let path = a.next().unwrap();
    let out = a.next().unwrap_or_else(|| "shots/rm_render.pgm".into());
    let page = RmPage::parse(&std::fs::read(&path).unwrap()).unwrap();
    let (w, h) = (1404usize, 1872usize);
    let mut c = Canvas::new(w, h);
    for l in page.layers.iter() {
        for line in l.lines.iter() {
            c.ink_line(line, 1.0, (702.0, 0.0), 2);
        }
    }
    c.solid_ink();
    let mut buf = Vec::with_capacity(w * h);
    for y in 0..h {
        for x in 0..w {
            buf.push(255 - c.dark_at(x, y));
        }
    }
    let mut s = format!("P5\n{w} {h}\n255\n").into_bytes();
    s.extend_from_slice(&buf);
    std::fs::write(&out, s).unwrap();
    eprintln!("写好 {out}");
}
