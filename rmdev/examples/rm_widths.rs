//! 临时：把 .rm 每一笔的工具/宽度范围打出来（判断官方用了哪些笔、多宽）。
use rmdev::doc::rm::RmPage;

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let page = RmPage::parse(&std::fs::read(&path).unwrap()).unwrap();
    for (li, l) in page.layers.iter().enumerate() {
        for (i, line) in l.lines.iter().enumerate() {
            if line.points.is_empty() {
                continue;
            }
            let mut wmin = f32::MAX;
            let mut wmax = f32::MIN;
            for p in &line.points {
                wmin = wmin.min(p.width);
                wmax = wmax.max(p.width);
            }
            let xs: Vec<f32> = line.points.iter().map(|p| p.x + 702.0).collect();
            let ys: Vec<f32> = line.points.iter().map(|p| p.y).collect();
            println!(
                "L{li}#{i:<3} {:?} color={:?} n={:<4} w {wmin:5.1}..{wmax:5.1}  x {:4.0}..{:4.0} y {:4.0}..{:4.0}",
                line.tool,
                line.color,
                line.points.len(),
                xs.iter().cloned().fold(f32::MAX, f32::min),
                xs.iter().cloned().fold(f32::MIN, f32::max),
                ys.iter().cloned().fold(f32::MAX, f32::min),
                ys.iter().cloned().fold(f32::MIN, f32::max)
            );
        }
    }
}
