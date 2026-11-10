//! 睡眠屏（官方 `res_06b6225.qml` SleepScreen）。
//!
//! 官方就两张全屏 `Image`（`DeepSleep` / `Setup` 两个模式），源都是
//! `file:///usr/share/remarkable/suspended.png`，`anchors.fill: parent`
//! 也就是**拉伸铺满**，不保比例。图片加载失败时才画白底 + 居中的
//! `Text { font.pixelSize: 40; text: "reMarkable is sleeping" }`。
//!
//! 这张图在设备上是 1404×1872 RGB，和我们屏一模一样 —— 正常路径是 1:1 直贴。
//! 缩放分支留着，是因为固件升级/别的机型上尺寸可能不同，别为此 panic。

use std::path::Path;

use crate::doc::reflow::{decode_image, Img};
use crate::fb::{gray565, Fb};
use crate::ui::text::Text;

/// 官方路径。
pub const IMG: &str = "/usr/share/remarkable/suspended.png";

/// 图片加载不出来时的兜底文案（官方原串）。
pub const FALLBACK: &str = "reMarkable is sleeping";
/// 官方 `font.pixelSize: 40`。
pub const FALLBACK_SIZE: f32 = 40.0;

/// 读 + 解码睡眠图。读不到/格式不认都返回 `None`（调用方走兜底文案）。
///
/// `RM_SLEEP_IMG` 可以换图（本机渲染预览、或以后想换成自己的图）。
pub fn load() -> Option<Img> {
    match std::env::var("RM_SLEEP_IMG") {
        Ok(p) if !p.is_empty() => load_at(Path::new(&p)),
        _ => load_at(Path::new(IMG)),
    }
}

pub fn load_at(p: &Path) -> Option<Img> {
    let data = std::fs::read(p).ok()?;
    match decode_image(&data) {
        Ok(img) if img.w > 0 && img.h > 0 => Some(img),
        Ok(_) => None,
        Err(e) => {
            eprintln!("[sleep] {} 解码失败：{e}", p.display());
            None
        }
    }
}

/// 画一整屏睡眠图。`img` 为 `None` 时走官方兜底文案。
pub fn draw(fb: &mut Fb, text: &mut Option<Text>, img: Option<&Img>) {
    let (w, h) = (fb.width, fb.height);
    fb.fill_rect(0, 0, w, h, gray565(255));
    match img {
        Some(img) => blit_fill(fb, img),
        None => {
            if let Some(t) = text.as_mut() {
                let tw = t.width(FALLBACK_SIZE, FALLBACK);
                t.draw(
                    fb,
                    (w as f32 / 2.0 - tw / 2.0) as i32,
                    // 官方是垂直居中（`anchors.centerIn`），这里按基线落位
                    h / 2 + (FALLBACK_SIZE * 0.36) as i32,
                    FALLBACK_SIZE,
                    FALLBACK,
                    0,
                );
            }
        }
    }
}

/// 拉伸铺满（面积平均缩放）。
///
/// 用面积平均而不是最近邻：sleep 图是细衬线字，最近邻缩小会把笔画整根丢掉
/// （屏上就是"reMarkable"缺一个字母）。等比时它退化成逐像素拷贝。
fn blit_fill(fb: &mut Fb, img: &Img) {
    let (w, h) = (fb.width as usize, fb.height as usize);
    if w == 0 || h == 0 {
        return;
    }
    if img.w == w && img.h == h {
        fb.blit_gray(0, 0, fb.width, fb.height, &img.px);
        return;
    }
    let mut buf = vec![0u8; w * h];
    for y in 0..h {
        let sy0 = y * img.h / h;
        let sy1 = (((y + 1) * img.h / h).max(sy0 + 1)).min(img.h);
        for x in 0..w {
            let sx0 = x * img.w / w;
            let sx1 = (((x + 1) * img.w / w).max(sx0 + 1)).min(img.w);
            let mut sum = 0u32;
            let mut n = 0u32;
            for sy in sy0..sy1 {
                let base = sy * img.w;
                for &p in &img.px[base + sx0..base + sx1] {
                    sum += p as u32;
                    n += 1;
                }
            }
            buf[y * w + x] = if n == 0 { 255 } else { (sum / n) as u8 };
        }
    }
    fb.blit_gray(0, 0, fb.width, fb.height, &buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: usize, h: usize, px: Vec<u8>) -> Img {
        Img { w, h, px }
    }

    /// 拉伸铺满：2×2 的黑/白棋盘贴到整屏，四个象限各自整块同色。
    /// 顺带验证不保比例（官方 `anchors.fill` 也是不保比例）。
    #[test]
    fn fill_stretches_to_the_whole_screen() {
        let mut fb = Fb::open_mem();
        let (w, h) = (fb.width, fb.height);
        // 左黑右白
        let im = img(2, 1, vec![0, 255]);
        blit_fill(&mut fb, &im);
        assert_eq!(fb.get(10, 10), 0, "左半边应该是黑的");
        assert_eq!(fb.get(w - 10, 10), 255, "右半边应该是白的");
        assert_eq!(fb.get(w / 2 - 2, h / 2), 0);
        assert_eq!(fb.get(w / 2 + 2, h / 2), 255);
    }

    /// 尺寸正好一样时走逐像素直贴，灰度一个不差。
    #[test]
    fn same_size_is_a_plain_copy() {
        let mut fb = Fb::open_mem();
        let (w, h) = (fb.width as usize, fb.height as usize);
        let mut px = vec![255u8; w * h];
        // 左上角画一小块黑、右下角一块中灰，其余留白
        for y in 0..50 {
            for x in 0..50 {
                px[y * w + x] = 0;
            }
        }
        for y in (h - 50)..h {
            for x in (w - 50)..w {
                px[y * w + x] = 128;
            }
        }
        blit_fill(&mut fb, &img(w, h, px));
        assert_eq!(fb.get(10, 10), 0);
        assert_eq!(fb.get(5, 5), 0);
        // 面板只有 16 级灰：128 会就近落到 130 那一级（±8 以内都算对）
        let g = fb.get(fb.width - 5, fb.height - 5);
        assert!((g as i32 - 128).abs() <= 8, "中灰变成了 {g}");
        assert_eq!(fb.get(fb.width / 2, fb.height / 2), 255);
    }

    /// 读不到图片不能崩：走官方兜底文案（居中有墨）。
    #[test]
    fn missing_image_falls_back_to_the_official_text() {
        let mut fb = Fb::open_mem();
        let mut text = Text::load();
        draw(&mut fb, &mut text, None);
        // 整屏铺白
        assert_eq!(fb.get(5, 5), 255);
        assert_eq!(fb.get(fb.width - 5, fb.height - 5), 255);
        // 中间那条带里要有墨（没有字体时 Text::load() 返回 None，跳过这项）
        if text.is_some() {
            let (y0, y1) = (fb.height / 2 - 60, fb.height / 2 + 20);
            let dark = (y0..y1)
                .flat_map(|y| (0..fb.width).map(move |x| (x, y)))
                .filter(|&(x, y)| fb.get(x, y) < 128)
                .count();
            assert!(dark > 500, "兜底文案没画出来（暗像素 {dark}）");
        }
        assert!(load_at(Path::new("/nonexistent/suspended.png")).is_none());
    }
}
