//! 临时（验证用）：把左工具条画进内存缓冲，导出灰度原始数据。
//!
//! 用法：`cargo run --quiet --example tb_dump -- <state> <out.raw> [x y w h]`
//! state: pen | sel | eraser | erase_sel | hidden | pens | stroke | erase
//!
//! 真机 `Fb::open()` 只有面板可用；这里用实测的 `Fb` 字段偏移在内存里搭一个同构对象。
//! 文字：本机没有 `/home/root/.fonts/NotoSansSC.otf`，`ui::text` 会返回 None，
//! 所以这里用 ab_glyph + 系统 Arial **按同样的落笔位置**补字，只为跟官方截图并排看版式；
//! 位置全部取自 `chrome` 的公开常量，和 `Chrome::draw_menu` 是同一条线。
use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use rmdev::chrome::{
    Chrome, Menu, COLORS, MENU_FONT, MENU_HEAD, MENU_HEAD_BASE, MENU_HEAD_DX, MENU_ROW, MENU_SEP,
    MENU_TEXT_BASE, MENU_TEXT_BASE_THICK, MENU_TEXT_DX, THICKNESSES, TOOLS,
};
use rmdev::fb::Fb;
use std::io::Write;
use std::mem::{size_of, transmute_copy, ManuallyDrop};

const W: i32 = 1404;
const H: i32 = 1872;
const CAP_RATIO: f32 = 0.716;

struct Fonts {
    font: FontVec,
}

impl Fonts {
    fn load() -> Option<Fonts> {
        for p in [
            "/System/Library/Fonts/Supplemental/Arial.ttf",
            "/System/Library/Fonts/Supplemental/Verdana.ttf",
        ] {
            if let Ok(data) = std::fs::read(p) {
                if let Ok(font) = FontVec::try_from_vec(data) {
                    eprintln!("[tbdump] 字体 {p}");
                    return Some(Fonts { font });
                }
            }
        }
        None
    }

    fn draw(&self, fb: &mut Fb, x: i32, baseline: i32, size: f32, s: &str, tone: u8) {
        let cap = 0.7 * size;
        let px = cap / CAP_RATIO;
        let scaled = self.font.as_scaled(PxScale::from(px));
        let mut pen = x as f32;
        for ch in s.chars() {
            let id = self.font.glyph_id(ch);
            let g = id.with_scale(PxScale::from(px));
            if let Some(outline) = self.font.outline_glyph(g) {
                let b = outline.px_bounds();
                outline.draw(|sx, sy, c| {
                    let px_x = pen.round() as i32 + b.min.x as i32 + sx as i32;
                    let px_y = baseline + b.min.y as i32 + sy as i32;
                    if c > 0.0 {
                        let v = 255.0 - (255.0 - tone as f32) * c;
                        fb.put(px_x, px_y, rmdev::fb::gray565(v.round() as u8));
                    }
                });
            }
            pen += scaled.h_advance(id);
        }
    }
}

fn main() {
    assert_eq!(size_of::<Fb>(), 64, "Fb 大小变了，偏移要重测");
    let mut buf = vec![0xffffu16; (W * H) as usize];
    let len = buf.len() * 2;
    let mut raw = [0u8; 64];
    raw[0..8].copy_from_slice(&(buf.as_mut_ptr() as usize).to_ne_bytes());
    raw[8..16].copy_from_slice(&len.to_ne_bytes());
    raw[16..24].copy_from_slice(&((W * 2) as usize).to_ne_bytes());
    raw[24..32].copy_from_slice(&2usize.to_ne_bytes());
    raw[32..36].copy_from_slice(&(-1i32).to_ne_bytes());
    raw[36..40].copy_from_slice(&W.to_ne_bytes());
    raw[40..44].copy_from_slice(&H.to_ne_bytes());
    let mut fb: ManuallyDrop<Fb> = ManuallyDrop::new(unsafe { transmute_copy(&raw) });
    if fb.width != W || fb.height != H {
        panic!("布局不对 {}x{}", fb.width, fb.height);
    }
    fb.fill_rect(0, 0, 3, 3, 0x0000);
    if buf[0] != 0 || buf[(W + 2) as usize] != 0 {
        panic!("fill_rect 没写进缓冲");
    }
    buf.iter_mut().for_each(|p| *p = 0xffff);

    let args: Vec<String> = std::env::args().collect();
    let state = args.get(1).map(|s| s.as_str()).unwrap_or("pen");
    let mut chrome = Chrome::default();
    match state {
        "sel" => chrome.selecting = true,
        "eraser" => chrome.erasing = true,
        "erase_sel" => {
            chrome.erasing = true;
            chrome.erase_section = true;
        }
        "hidden" => chrome.visible = false,
        "pens" => chrome.menu = Menu::Pens,
        "pens_text" => {
            chrome.selected = 8;
            chrome.menu = Menu::Pens;
        }
        "stroke" => chrome.menu = Menu::Settings,
        "stroke_thin" => {
            chrome.thickness = 1.0;
            chrome.menu = Menu::Settings;
        }
        "erase" => {
            chrome.erasing = true;
            chrome.menu = Menu::Erase;
        }
        _ => {}
    }
    let mut text = None;
    chrome.draw_in(&mut fb, &mut text);

    if let Some(f) = Fonts::load() {
        let x = rmdev::chrome::MENU_X;
        match chrome.menu {
            Menu::Pens => {
                let top = chrome.menu_top();
                for (i, (_, name)) in TOOLS.iter().enumerate() {
                    let y = top + i as i32 * MENU_ROW;
                    let on = i == chrome.selected;
                    let tone = if on { 255 } else { 0 };
                    f.draw(&mut fb, x + MENU_TEXT_DX, y + MENU_TEXT_BASE, MENU_FONT, name, tone);
                }
            }
            Menu::Erase => {
                let top = chrome.menu_top();
                for (i, name) in ["Eraser", "Erase selection", "Erase all"].iter().enumerate() {
                    let y = top + i as i32 * MENU_ROW;
                    let on = i == 0;
                    let tone = if on { 255 } else { 0 };
                    f.draw(&mut fb, x + MENU_TEXT_DX, y + MENU_TEXT_BASE, MENU_FONT, name, tone);
                }
            }
            Menu::Settings => {
                let mut y = chrome.menu_top() + MENU_SEP;
                f.draw(&mut fb, x + MENU_HEAD_DX, y + MENU_HEAD_BASE, MENU_FONT, "Stroke", 0);
                y += MENU_HEAD + MENU_SEP;
                for (i, (w, name)) in THICKNESSES.iter().enumerate() {
                    let ry = y + i as i32 * MENU_ROW;
                    let on = (*w - chrome.thickness).abs() < 0.01;
                    let tone = if on { 255 } else { 0 };
                    f.draw(&mut fb, x + MENU_TEXT_DX, ry + MENU_TEXT_BASE_THICK, MENU_FONT, name, tone);
                }
                y += 3 * MENU_ROW + MENU_SEP;
                f.draw(&mut fb, x + MENU_HEAD_DX, y + MENU_HEAD_BASE, MENU_FONT, "Color", 0);
                y += MENU_HEAD + MENU_SEP;
                for (i, (c, name)) in COLORS.iter().enumerate() {
                    let ry = y + i as i32 * MENU_ROW;
                    let on = *c == chrome.color;
                    let tone = if on { 255 } else { 0 };
                    f.draw(&mut fb, x + MENU_TEXT_DX, ry + MENU_TEXT_BASE, MENU_FONT, name, tone);
                }
            }
            _ => {}
        }
    }

    let (x0, y0, w, h) = if args.len() >= 7 {
        let a: Vec<i32> = args[3..7].iter().map(|s| s.parse().unwrap()).collect();
        (a[0], a[1], a[2], a[3])
    } else {
        (0, 0, 120, H)
    };
    let mut out = Vec::with_capacity((w * h) as usize);
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            let c = buf[(y * W + x) as usize];
            let g = ((c >> 5) & 0x3f) as u32 * 255 / 63;
            out.push(g as u8);
        }
    }
    let path = args.get(2).map(|s| s.as_str()).unwrap_or("/tmp/tb.raw");
    std::fs::File::create(path).unwrap().write_all(&out).unwrap();
    println!("wrote {path} {w}x{h} from ({x0},{y0}) state={state}");
}
