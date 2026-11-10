//! 把某一笔的 (x,y,dir,width,pressure) 打成 CSV：`cargo run --release --example rm_csv -- f.rm 45`
use rmdev::doc::rm::RmPage;

fn main() {
    let mut a = std::env::args().skip(1);
    let path = a.next().unwrap();
    let idx: usize = a.next().map(|s| s.parse().unwrap()).unwrap_or(0);
    let page = RmPage::parse(&std::fs::read(&path).unwrap()).unwrap();
    let mut k = 0;
    for l in page.layers.iter() {
        for line in l.lines.iter() {
            if k == idx {
                eprintln!("tool={:?} n={} thick={}", line.tool, line.points.len(), line.thickness);
                println!("x,y,dir,width,pressure");
                for p in &line.points {
                    println!("{:.3},{:.3},{:.4},{:.3},{:.4}", p.x, p.y, p.dir, p.width, p.pressure);
                }
                return;
            }
            k += 1;
        }
    }
}
