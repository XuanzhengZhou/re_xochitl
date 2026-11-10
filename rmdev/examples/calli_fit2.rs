//! 网格搜 (tilt_x, tilt_y)，看官方 .rm 存的 width 能不能用我们的公式整条复现。
use rmdev::doc::rm::RmPage;
use rmdev::pen::line::{Color, InputPoint, Tool};
use rmdev::pen::stroke::StrokeBuilder;

fn err_at(line: &rmdev::pen::line::Line, tx: f32, ty: f32) -> f32 {
    let thick = line.thickness as f32;
    let mut b = StrokeBuilder::new(Tool::Calligraphy, Color::Black, thick, 1.0);
    let f0 = &line.points[0];
    b.begin(InputPoint { x: f0.x, y: f0.y, pressure: f0.pressure, tilt_x: tx, tilt_y: ty, rubber: false });
    let mut e = 0.0f32;
    for p in line.points.iter().skip(1) {
        let q = b.add(InputPoint { x: p.x, y: p.y, pressure: p.pressure, tilt_x: tx, tilt_y: ty, rubber: false });
        e += (q.width - p.width).abs();
    }
    e / (line.points.len() - 1) as f32
}

fn main() {
    let mut a = std::env::args().skip(1);
    let path = a.next().unwrap();
    let idx: usize = a.next().unwrap().parse().unwrap();
    let page = RmPage::parse(&std::fs::read(&path).unwrap()).unwrap();
    let mut k = 0;
    for l in page.layers.iter() {
        for line in l.lines.iter() {
            if k == idx {
                let mut best = (f32::MAX, 0.0, 0.0);
                for i in -50..=50 {
                    for j in -50..=50 {
                        let (tx, ty) = (i as f32 * 0.04, j as f32 * 0.04);
                        let e = err_at(line, tx, ty);
                        if e < best.0 {
                            best = (e, tx, ty);
                        }
                    }
                }
                println!("L#{idx} 最优 MAE={:.3} tilt_x={:.2} tilt_y={:.2}", best.0, best.1, best.2);
                return;
            }
            k += 1;
        }
    }
}
