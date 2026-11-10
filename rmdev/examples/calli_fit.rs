//! 官方 .rm 里的书法笔实测点 (x,y,pressure) 回放进我们的公式，和官方存的 width 对比。
//! `cargo run --release --example calli_fit -- f.rm 69 [tilt_x tilt_y]`
use rmdev::doc::rm::RmPage;
use rmdev::pen::line::{Color, InputPoint, Tool};
use rmdev::pen::stroke::StrokeBuilder;

fn main() {
    let mut a = std::env::args().skip(1);
    let path = a.next().unwrap();
    let idx: usize = a.next().unwrap().parse().unwrap();
    let tx: f32 = a.next().map(|s| s.parse().unwrap()).unwrap_or(0.0);
    let ty: f32 = a.next().map(|s| s.parse().unwrap()).unwrap_or(0.0);
    let page = RmPage::parse(&std::fs::read(&path).unwrap()).unwrap();
    let mut k = 0;
    for l in page.layers.iter() {
        for line in l.lines.iter() {
            if k == idx {
                let thick = line.thickness as f32;
                let mut b = StrokeBuilder::new(Tool::Calligraphy, Color::Black, thick, 1.0);
                let first = &line.points[0];
                b.begin(InputPoint { x: first.x, y: first.y, pressure: first.pressure, tilt_x: tx, tilt_y: ty, rubber: false });
                let mut errs: Vec<f32> = Vec::new();
                let mut worst: Vec<(f32, f32, f32, f32)> = Vec::new();
                for p in line.points.iter().skip(1) {
                    let q = b.add(InputPoint { x: p.x, y: p.y, pressure: p.pressure, tilt_x: tx, tilt_y: ty, rubber: false });
                    let e = q.width - p.width;
                    errs.push(e);
                    worst.push((p.width, q.width, p.dir, p.pressure));
                }
                let mae: f32 = errs.iter().map(|e| e.abs()).sum::<f32>() / errs.len() as f32;
                let bias: f32 = errs.iter().sum::<f32>() / errs.len() as f32;
                println!("L#{idx} thick={thick} n={} MAE={mae:.2} bias={bias:+.2}", errs.len());
                worst.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
                for (ow, mw, dir, p) in worst.iter().take(5) {
                    println!("  官方 {ow:5.2}  我们 {mw:5.2}  dir={dir:.2} p={p:.3}");
                }
                worst.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
                for (ow, mw, dir, p) in worst.iter().take(3) {
                    println!("  官方 {ow:5.2}  我们 {mw:5.2}  dir={dir:.2} p={p:.3}");
                }
                return;
            }
            k += 1;
        }
    }
}
