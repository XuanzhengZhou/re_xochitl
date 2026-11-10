//! 面板自检：不启动 UI，只走 `Fb::open → 画图 → 整屏 GC16`。
//!
//! 用来把「Rust 移植正确与否」和「rmdev 界面逻辑」分开：这个工具出图正确，
//! 就说明几何/镜像/灰度/编码/翻页全对。
//!
//! 期望画面（竖屏 1404×1872）：
//!
//! * 白底；
//! * 左上角 220×220 黑方块 ← 检查原点在左上、没有转置/镜像；
//! * 顶部一条黑横条，右侧留白；
//! * 中段 16 条竖条，从左到右由黑到白 ← 检查灰度量化；
//! * 右下角 200×200 黑方块 ← 检查右下角坐标；
//! * 底部一条细黑尺（每 100 px 一格）← 检查横向比例。

use rmdev::fb::{gray565, Fb, UPDATE_MODE_FULL, W, H, WAVEFORM_MODE_GC16};

fn main() {
    let mut fb = Fb::open().expect("打不开面板（见上面的 [panel] 日志）");
    fb.clear(gray565(255));

    // 左上角方块：原点校验
    fb.fill_rect(0, 0, 220, 220, gray565(0));

    // 顶部横条（从 x=300 到右边 100px 处）
    fb.fill_rect(300, 0, W as i32 - 400, 160, gray565(0));

    // 16 级竖条
    for g in 0..16 {
        let x = 60 + g * 80;
        let v = (g * 17) as u8;
        fb.fill_rect(x as i32, 400, 64, 400, gray565(v));
    }

    // 右下角方块：右下角坐标校验
    fb.fill_rect(W as i32 - 200, H as i32 - 200, 200, 200, gray565(0));

    // 底部标尺：每 100px 一格，最后 20 格
    for i in 0..14 {
        fb.fill_rect(i * 100, H as i32 - 60, 4, 40, gray565(0));
    }

    eprintln!("[paneltest] 开始整屏 GC16");
    let t = std::time::Instant::now();
    fb.refresh(0, 0, W as i32, H as i32, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
    eprintln!("[paneltest] 完成，{} ms", t.elapsed().as_millis());
    let _ = fb.dump_pgm("/tmp/paneltest.pgm");
}
