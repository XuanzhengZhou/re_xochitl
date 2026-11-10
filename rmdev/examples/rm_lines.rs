//! 临时：把 .rm 每一笔的包围盒/点数打出来（排查"哪些笔是垃圾"用）。
use rmdev::doc::rm::RmPage;

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let page = RmPage::parse(&bytes).unwrap();
    println!("layers={} lines={}", page.layers.len(), page.total_lines());
    for (li, l) in page.layers.iter().enumerate() {
        for (i, line) in l.lines.iter().enumerate() {
            let xs: Vec<f32> = line.points.iter().map(|p| p.x).collect();
            let ys: Vec<f32> = line.points.iter().map(|p| p.y).collect();
            let (x0, x1) = (
                xs.iter().cloned().fold(f32::MAX, f32::min),
                xs.iter().cloned().fold(f32::MIN, f32::max),
            );
            let (y0, y1) = (
                ys.iter().cloned().fold(f32::MAX, f32::min),
                ys.iter().cloned().fold(f32::MIN, f32::max),
            );
            // 场景坐标 → 屏幕：+702
            println!(
                "L{li}#{i:<2} {:?} n={:<4} screen x {:.0}..{:.0} y {:.0}..{:.0}",
                line.tool,
                line.points.len(),
                x0 + 702.0,
                x1 + 702.0,
                y0,
                y1
            );
        }
    }
}
