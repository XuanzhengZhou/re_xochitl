//! 文字渲染。
//!
//! 官方用 Qt 的字体栈；我们用 `ab_glyph` + 设备上已有的字体文件。
//! `/home/root/.fonts/NotoSansSC.otf`（16 MB）是中文字形的来源，
//! 系统自带的 Noto Sans 只有拉丁字母。
//!
//! 面板只有 16 级灰，所以直接把覆盖度乘到目标灰度上，不做次像素渲染。

use std::collections::HashMap;
use std::fs;

use ab_glyph::{Font, FontVec, GlyphId, PxScale, ScaleFont};

use crate::fb::{gray565, Fb};

/// 官方 QML 里的字号 → 我们这里的字号系数。
///
/// 实测：`NotoSansSC.otf` 用 `PxScale::from(size)` 渲染时，大写字母高度只有
/// `0.5 × size`；而官方（Qt 的 Noto Sans）是 `0.7 × size`。
/// 所以把调用方传来的"官方字号"乘 1.4，字形高度才对得上官方截图。
fn official_to_font() -> f32 {
    std::env::var("RMDEV_FONT_SCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(OFFICIAL_TO_FONT)
}

/// 官方 QML 字号 → ab_glyph 的 PxScale 系数。
/// 换成 Maison Neue 之后重新标定：字号 36 → 大写高 25px（和官方截图一致）。
const OFFICIAL_TO_FONT: f32 = 1.2;

/// 汉字（NotoSansSC）在同一字号下比 Maison 的拉丁字矮一截，
/// 按官方截图量：卡片标题拉丁 cap=22 时汉字高 30 → ×1.25 才对齐。
const CJK_SCALE: f32 = 1.25;

const FONT_CANDIDATES: [&str; 3] = [
    "/home/root/.fonts/NotoSansSC.otf",
    "/usr/share/fonts/ttf/noto/NotoSans-Regular.ttf",
    "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
];

#[derive(Clone)]
struct Cached {
    w: u32,
    h: u32,
    left: i32,
    top: i32,
    cov: Vec<u8>,
}

pub struct Text {
    /// 正文（官方是 Maison Neue Book，设备 webui 目录里有 woff，部署时转成 ttf）
    font: FontVec,
    /// 粗体（Maison Neue Bold；官方标题/按钮/侧栏都是粗的）
    bold: Option<FontVec>,
    /// 中日韩兜底（Maison 没有汉字，NotoSansSC 有）
    cjk: Option<FontVec>,
    /// 官方图标字体（设备 webui 里的 icomoon.woff，部署时转成 ttf）
    icons: Option<FontVec>,
    /// 接下来这几笔要不要用粗体（调用方 `set_bold` 切换）
    bold_on: bool,
    /// (字符, 字号 × 4, 字面) → 位图
    cache: HashMap<(char, u32, u8), Cached>,
    pub path: String,
}

impl Text {
    pub fn load() -> Option<Text> {
        // 本机渲染（`--render`）时用一个显式指定的字体，真机上不设这个变量。
        let over = std::env::var("RMDEV_FONT").ok();
        let local = std::env::var("HOME")
            .ok()
            .map(|h| format!("{h}/rm2_re/fonts/MaisonNeue-Book.ttf"));
        let cands: Vec<String> = over
            .into_iter()
            .chain(local)
            .chain(
                [
                    "/home/root/.fonts/MaisonNeue-Book.ttf",
                    "/usr/share/remarkable/webui/fonts/MaisonNeue-Book.ttf",
                ]
                .iter()
                .map(|s| s.to_string()),
            )
            .chain(FONT_CANDIDATES.iter().map(|s| s.to_string()))
            .collect();
        for p in &cands {
            let Ok(data) = fs::read(p) else { continue };
            let Ok(font) = FontVec::try_from_vec(data) else {
                continue;
            };
            let bold = load_bold();
            let cjk = load_cjk();
            let icons = load_icons();
            eprintln!("[text] 字体 {p}{}{}", if bold.is_some() { " + 粗体" } else { "" },
                if cjk.is_some() { " + 中文" } else { "" });
            return Some(Text {
                font,
                bold,
                cjk,
                icons,
                bold_on: false,
                cache: HashMap::new(),
                path: p.to_string(),
            });
        }
        eprintln!("[text] 找不到可用字体，文字将不显示");
        None
    }

    /// 开关粗体。官方 UI 的标题/按钮/侧栏条目都是粗的，正文是常规。
    pub fn set_bold(&mut self, on: bool) {
        self.bold_on = on;
    }

    /// 这个字符该用哪套字形：粗体字面里没有的字（例如中文）退回常规字面，
    /// 常规字面里没有的（汉字）再退回 NotoSansSC。
    /// 返回值第二个是位图缓存用的字面编号：0 常规 / 1 粗体 / 2 中文。
    fn face(&self, ch: char) -> (&FontVec, u8) {
        let primary = if self.bold_on {
            self.bold.as_ref().unwrap_or(&self.font)
        } else {
            &self.font
        };
        if primary.glyph_id(ch).0 != 0 {
            return (primary, u8::from(self.bold_on));
        }
        if let Some(c) = &self.cjk {
            if c.glyph_id(ch).0 != 0 {
                return (c, 2);
            }
        }
        (primary, u8::from(self.bold_on))
    }

    pub fn width(&mut self, size: f32, s: &str) -> f32 {
        let size = size * official_to_font();
        let mut x = 0.0;
        for ch in s.chars() {
            let (font, which) = self.face(ch);
            let gs = if which == 2 { size * CJK_SCALE } else { size };
            let scaled = font.as_scaled(PxScale::from(gs));
            x += scaled.h_advance(font.glyph_id(ch));
        }
        x
    }

    /// `size` 必须是**已经乘过 OFFICIAL_TO_FONT** 的像素字号。
    fn build(&self, ch: char, size: f32, which: u8) -> Cached {
        let font = match which {
            1 => self.bold.as_ref().unwrap_or(&self.font),
            2 => self.cjk.as_ref().unwrap_or(&self.font),
            3 => self.icons.as_ref().unwrap_or(&self.font),
            _ => &self.font,
        };
        let g = font.glyph_id(ch).with_scale(PxScale::from(size));
        let mut entry = Cached {
            w: 0,
            h: 0,
            left: 0,
            top: 0,
            cov: Vec::new(),
        };
        if let Some(outline) = font.outline_glyph(g) {
            let b = outline.px_bounds();
            let w = b.width().ceil().max(0.0) as u32;
            let h = b.height().ceil().max(0.0) as u32;
            if w > 0 && h > 0 {
                let mut cov = vec![0u8; (w * h) as usize];
                outline.draw(|x, y, c| {
                    let (x, y) = (x as usize, y as usize);
                    if x < w as usize && y < h as usize {
                        let i = y * w as usize + x;
                        let v = (c * 255.0).round().clamp(0.0, 255.0) as u8;
                        if v > cov[i] {
                            cov[i] = v;
                        }
                    }
                });
                entry = Cached {
                    w,
                    h,
                    left: b.min.x.floor() as i32,
                    top: b.min.y.floor() as i32,
                    cov,
                };
            }
        }
        entry
    }

    /// 在 (x, y) 画一行字；`y` 是**基线**。返回实际行宽。
    ///
    /// `fb` 开着二值模式（`Fb::set_mono`）时走 1-bit 路径：覆盖率 ≥ 50% 的
    /// 像素画成纯 `gray`，其余留空，**不做抗锯齿**。A2/DU 这类二值波形对中间
    /// 灰一个驱动码都没有，抗锯齿出来的边会留在屏上变成残影（改前输密码时
    /// 每次退格都能看见上一帧的字没擦干净）。
    pub fn draw(&mut self, fb: &mut Fb, x: i32, y: i32, size: f32, s: &str, gray: u8) -> i32 {
        let size = size * official_to_font();
        let mono = fb.mono();
        // 1-bit 下目标色只有黑/白两种，先定下来（`gray` 是 UI 那边的 0..255 灰度）。
        let mono_fg = if gray >= 128 { crate::fb::WHITE } else { crate::fb::BLACK };
        let mut pen = x as f32;
        for ch in s.chars() {
            let (face, which) = self.face(ch);
            let gs = if which == 2 { size * CJK_SCALE } else { size };
            let adv = {
                face.as_scaled(PxScale::from(gs))
                    .h_advance(face.glyph_id(ch))
            };
            let key = (ch, (size * 4.0) as u32, which);
            if !self.cache.contains_key(&key) {
                let entry = self.build(ch, gs, which);
                self.cache.insert(key, entry);
            }
            if let Some(g) = self.cache.get(&key) {
                let ox = pen.round() as i32 + g.left;
                let oy = y + g.top;
                for row in 0..g.h {
                    for col in 0..g.w {
                        let a = g.cov[(row * g.w + col) as usize];
                        if a == 0 {
                            continue;
                        }
                        if mono {
                            // 二值：半覆盖以上算落墨，覆盖不到一半整个丢掉。
                            if a >= 128 {
                                fb.put(ox + col as i32, oy + row as i32, mono_fg);
                            }
                            continue;
                        }
                        // 覆盖度 → 往目标色插值。目标色可能比底色深（黑字压白底）
                        // 也可能比底色浅（菜单反白行里的白字压黑底），所以必须
                        // 按已有像素做双向 lerp：
                        //   旧写法 `255 - (255-gray)*a/255` 假定底色是白，
                        //   gray=255 时任何覆盖率都算成纯白 → 反白行的字会发胖。
                        let bg = fb.get(ox + col as i32, oy + row as i32) as u32;
                        let fg = gray as u32;
                        let a = a as u32;
                        let v = if fg >= bg {
                            bg + (fg - bg) * a / 255
                        } else {
                            bg - (bg - fg) * a / 255
                        };
                        fb.put(ox + col as i32, oy + row as i32, gray565(v as u8));
                    }
                }
            }
            pen += adv;
        }
        (pen - x as f32) as i32
    }

    /// 粗体版 `draw`（自动开关 `bold_on`）。
    pub fn draw_bold(&mut self, fb: &mut Fb, x: i32, y: i32, size: f32, s: &str, gray: u8) -> i32 {
        self.bold_on = true;
        let w = self.draw(fb, x, y, size, s, gray);
        self.bold_on = false;
        w
    }

    /// 画一个图标字体里的字形，缩放到 `size` 的方框里并居中。
    /// `code` 是 icomoon 的码位（例如 my files = U+E9C4）。
    pub fn icon(&mut self, fb: &mut Fb, cx: i32, cy: i32, size: i32, code: char, tone: u8) {
        let Some(font) = self.icons.as_ref() else { return };
        // 先按 100px 量出墨迹包围盒，再反算能填满 `size` 的字号
        let Some((_w, h, _left, _top)) = icon_bounds(font, code) else { return };
        if h <= 0 { return; }
        let px = 100.0 * size as f32 / h as f32;
        let key = (code, (px * 4.0) as u32, 3u8);
        if !self.cache.contains_key(&key) {
            let e = self.build(code, px, 3);
            self.cache.insert(key, e);
        }
        let Some(g) = self.cache.get(&key) else { return };
        // 墨迹包围盒居中放进 size×size 的格子
        let ox = cx - g.w as i32 / 2;
        let oy = cy - g.h as i32 / 2;
        let c = crate::fb::gray565(tone);
        for row in 0..g.h {
            for col in 0..g.w {
                if g.cov[(row * g.w + col) as usize] > 127 {
                    fb.put(ox + col as i32, oy + row as i32, c);
                }
            }
        }
    }

    /// 粗体版 `width`。
    pub fn width_bold(&mut self, size: f32, s: &str) -> f32 {
        self.bold_on = true;
        let w = self.width(size, s);
        self.bold_on = false;
        w
    }

    /// 粗体版 `ellipsize_mid`。
    pub fn ellipsize_mid_bold(&mut self, size: f32, s: &str, max_w: i32) -> String {
        self.bold_on = true;
        let r = self.ellipsize_mid(size, s, max_w);
        self.bold_on = false;
        r
    }

    /// 截断到给定宽度，超出加省略号。
    pub fn ellipsize(&mut self, size: f32, s: &str, max_w: i32) -> String {
        let full: String = s.to_string();
        if self.width(size, &full) <= max_w as f32 {
            return full;
        }
        let mut out = String::new();
        let mut w = 0.0;
        for ch in full.chars() {
            let cw = self.width(size, &ch.to_string());
            if w + cw + size > max_w as f32 {
                out.push('…');
                break;
            }
            out.push(ch);
            w += cw;
        }
        out
    }

    /// 中间截断：`Function-free Optimization via Comparison Oracles-中文译本`
    /// → `Function-f…acles-中文译本`（官方库卡片就是这么显示的）。
    ///
    /// 官方保留结尾是因为结尾常带语言/版本后缀，信息量比中间大。
    /// 左右交替往中间吃字符，哪边窄就先吃哪边，最后两边像素宽度大致相等。
    pub fn ellipsize_mid(&mut self, size: f32, s: &str, max_w: i32) -> String {
        if self.width(size, s) <= max_w as f32 {
            return s.to_string();
        }
        let chars: Vec<char> = s.chars().collect();
        let budget = max_w as f32 - self.width(size, "…");
        let (mut head, mut tail): (Vec<char>, Vec<char>) = (Vec::new(), Vec::new());
        let (mut i, mut j) = (0usize, chars.len());
        let (mut wh, mut wt) = (0.0f32, 0.0f32);
        while i < j {
            // 先给窄的那边补一个字符
            let from_head = wh <= wt;
            let ch = if from_head { chars[i] } else { chars[j - 1] };
            let cw = self.width(size, &ch.to_string());
            if wh + wt + cw > budget {
                break;
            }
            if from_head {
                head.push(ch);
                wh += cw;
                i += 1;
            } else {
                tail.push(ch);
                wt += cw;
                j -= 1;
            }
        }
        tail.reverse();
        let mut out: String = head.into_iter().collect();
        out.push('…');
        out.extend(tail);
        out
    }
}

fn font_from(cands: &[String]) -> Option<FontVec> {
    for p in cands {
        let Ok(data) = fs::read(p) else { continue };
        if let Ok(font) = FontVec::try_from_vec(data) {
            return Some(font);
        }
    }
    None
}

fn home_fonts(name: &str) -> Option<String> {
    std::env::var("HOME")
        .ok()
        .map(|h| format!("{h}/rm2_re/fonts/{name}"))
}

/// 粗体：Maison Neue Bold → NotoSans-Bold。
fn load_bold() -> Option<FontVec> {
    let mut cands: Vec<String> = std::env::var("RMDEV_FONT_BOLD").into_iter().collect();
    cands.extend(home_fonts("MaisonNeue-Bold.ttf"));
    cands.push("/home/root/.fonts/MaisonNeue-Bold.ttf".into());
    cands.push("/usr/share/remarkable/webui/fonts/MaisonNeue-Bold.ttf".into());
    cands.extend(home_fonts("NotoSans-Bold.ttf"));
    cands.push("/usr/share/fonts/ttf/noto/NotoSans-Bold.ttf".into());
    cands.push("/usr/share/fonts/truetype/noto/NotoSans-Bold.ttf".into());
    font_from(&cands)
}

/// 汉字兜底：NotoSansSC（16 MB，设备 /home/root/.fonts 里那份）。
fn load_cjk() -> Option<FontVec> {
    let mut cands: Vec<String> = std::env::var("RMDEV_FONT_CJK").into_iter().collect();
    cands.extend(home_fonts("NotoSansSC.otf"));
    cands.push("/home/root/.fonts/NotoSansSC.otf".into());
    font_from(&cands)
}

/// 官方图标字体（webui 里的 icomoon，部署时 woff 转 ttf）。
fn load_icons() -> Option<FontVec> {
    let mut cands: Vec<String> = std::env::var("RMDEV_FONT_ICONS").into_iter().collect();
    cands.extend(home_fonts("icomoon.ttf"));
    cands.push("/home/root/.fonts/icomoon.ttf".into());
    font_from(&cands)
}

/// 图标字形在 100px 字号下的墨迹包围盒：(w, h, left, top)。
fn icon_bounds(font: &FontVec, ch: char) -> Option<(u32, u32, i32, i32)> {
    let g = font.glyph_id(ch).with_scale(PxScale::from(100.0));
    let o = font.outline_glyph(g)?;
    let b = o.px_bounds();
    let w = b.width().ceil() as u32;
    let h = b.height().ceil() as u32;
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h, b.min.x.floor() as i32, b.min.y.floor() as i32))
}
