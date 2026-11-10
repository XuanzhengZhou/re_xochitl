//! 屏幕键盘。搜索、重命名都用它。
//!
//! 三档布局（小写 / 大写 / 符号），底部一条：`123` `,` 空格 `.` 回车。

use crate::fb::{gray565, Fb};
use crate::ui::text::Text;
use crate::ui::widgets;

pub const KEY_H: i32 = 110;
const GAP: i32 = 10;
const PAD: i32 = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    Ch(char),
    Backspace,
    Shift,
    Layer,
    Space,
    Enter,
    Hide,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    Lower,
    Upper,
    Symbols,
}

#[derive(Clone)]
pub struct Keyboard {
    pub layer: Layer,
    pub text: String,
    pub done: bool,
}

impl Keyboard {
    pub fn new() -> Keyboard {
        Keyboard {
            layer: Layer::Lower,
            text: String::new(),
            done: false,
        }
    }

    pub fn height(&self) -> i32 {
        // 官方键盘区 1270..1872 = 601 高
        601
    }

    /// 按下一次。`Enter` 表示用户确认了输入。
    pub fn press(&mut self, k: Key) {
        match k {
            Key::Ch(c) => {
                self.text.push(c);
                if self.layer == Layer::Upper {
                    self.layer = Layer::Lower;
                }
            }
            Key::Space => self.text.push(' '),
            Key::Backspace => {
                self.text.pop();
            }
            Key::Shift => {
                self.layer = match self.layer {
                    Layer::Lower => Layer::Upper,
                    _ => Layer::Lower,
                }
            }
            Key::Layer => {
                self.layer = match self.layer {
                    Layer::Symbols => Layer::Lower,
                    _ => Layer::Symbols,
                }
            }
            Key::Enter => self.done = true,
            Key::Hide => {}
        }
    }

    /// 画到 `(x, y, w)` 这块区域。官方是**黑底白字、没有键框**的平板式键盘。
    /// 返回 `(矩形, 键)` 供命中测试。
    pub fn draw(
        &self,
        fb: &mut Fb,
        text: &mut Option<Text>,
        x: i32,
        y: i32,
        w: i32,
    ) -> Vec<((i32, i32, i32, i32), Key)> {
        let mut hits = Vec::new();
        let h = self.height();
        fb.fill_rect(x, y, w, h, gray565(0));

        // 每行 120 高，键距 112
        let kw = 112;
        let row_h = 120;
        let rows: &[&str] = match self.layer {
            Layer::Symbols => &["1234567890", "-_=+()[]{}<>", "!@#$%&*.,;:/"],
            _ => &["qwertyuiop", "asdfghjkl", "zxcvbnm"],
        };
        for (ri, row) in rows.iter().enumerate() {
            let ry = y + 40 + ri as i32 * row_h;
            let n = row.chars().count() as i32;
            if ri == 2 {
                // 行首 shift，行尾退格
                let sh = (x + 40, ry - 10, 90, 100);
                let label = if self.layer == Layer::Upper { "⇧" } else { "⇧" };
                key_label(fb, text, sh, label, 250);
                hits.push((sh, Key::Shift));
                let mut kx = x + 150;
                for ch in row.chars() {
                    let r = (kx, ry - 10, kw, 100);
                    key_label(fb, text, r, &one(ch, self.layer), 250);
                    hits.push((r, Key::Ch(one_char(ch, self.layer))));
                    kx += kw + 4;
                }
                let bs = (kx + 8, ry - 10, 110, 100);
                backspace_glyph(fb, bs, 250);
                hits.push((bs, Key::Backspace));
                continue;
            }
            let total = n * kw + (n - 1) * 4;
            let mut kx = x + (w - total) / 2;
            for ch in row.chars() {
                let r = (kx, ry - 10, kw, 100);
                key_label(fb, text, r, &one(ch, self.layer), 250);
                hits.push((r, Key::Ch(one_char(ch, self.layer))));
                kx += kw + 4;
            }
        }

        // 底部一行
        let by = y + h - 118;
        let glob = (x + 40, by, 70, 90);
        crate::ui::widgets::icon_globe(fb, glob.0 + 15, glob.1 + 22, 250);
        hits.push((glob, Key::Layer));
        let num = (x + 170, by, 110, 90);
        let num_label = if self.layer == Layer::Symbols { "ABC" } else { "123" };
        key_label(fb, text, num, num_label, 250);
        hits.push((num, Key::Layer));
        let comma = (x + 290, by, 60, 90);
        key_label(fb, text, comma, ",", 250);
        hits.push((comma, Key::Ch(',')));
        // 空格：一个有细白框的长条
        let sp = (x + 400, by - 10, 600, 100);
        box_key(fb, sp);
        key_label(fb, text, sp, "", 250);
        hits.push((sp, Key::Space));
        let dot = (x + 1010, by, 60, 90);
        key_label(fb, text, dot, ".", 250);
        hits.push((dot, Key::Ch('.')));
        let ret = (x + 1120, by - 10, 180, 100);
        box_key(fb, ret);
        // 回车键画一个折线箭头（字体里没有 ↵ 这个字形）
        {
            let (rx, ry) = (ret.0 + 60, ret.1 + 30);
            for i in 0..30 {
                fb.fill_rect(rx + i, ry, 3, 3, gray565(250));
            }
            for i in 0..26 {
                fb.fill_rect(rx, ry + i, 3, 3, gray565(250));
            }
            for i in 0..9 {
                fb.fill_rect(rx - i, ry + 26 - i, 3, 3, gray565(250));
                fb.fill_rect(rx + i, ry + 26 + i - 18, 3, 3, gray565(250));
            }
        }
        hits.push((ret, Key::Enter));

        hits
    }
}

impl Default for Keyboard {
    fn default() -> Self {
        Keyboard::new()
    }
}

fn one(ch: char, layer: Layer) -> String {
    one_char(ch, layer).to_string()
}

fn one_char(ch: char, layer: Layer) -> char {
    if layer == Layer::Upper {
        ch.to_ascii_uppercase()
    } else {
        ch
    }
}

fn draw_key(fb: &mut Fb, text: &mut Option<Text>, r: (i32, i32, i32, i32), label: &str, on: bool) {
    let (x, y, w, h) = r;
    let bg = if on { 60 } else { 252 };
    fb.fill_rect(x, y, w, h, gray565(bg));
    // 一圈细边框
    let edge = gray565(if on { 60 } else { 205 });
    fb.fill_rect(x, y, w, 1, edge);
    fb.fill_rect(x, y + h - 1, w, 1, edge);
    fb.fill_rect(x, y, 1, h, edge);
    fb.fill_rect(x + w - 1, y, 1, h, edge);
    if label.is_empty() {
        return;
    }
    let tone = if on { 250 } else { 20 };
    if let Some(t) = text.as_mut() {
        let size = 40.0;
        let tw = t.width(size, label) as i32;
        t.draw(fb, x + (w - tw) / 2, y + h / 2 + 15, size, label, tone);
    }
}

/// 平板键盘上的字：没有框，只有白字居中。
fn key_label(fb: &mut Fb, text: &mut Option<Text>, r: (i32, i32, i32, i32), label: &str, tone: u8) {
    if label.is_empty() {
        return;
    }
    let (x, y, w, h) = r;
    if let Some(t) = text.as_mut() {
        let size = 34.0;
        let tw = t.width(size, label) as i32;
        t.draw(fb, x + (w - tw) / 2, y + h / 2 + 12, size, label, tone);
    }
}

/// 有白色细边框的键（空格 / 回车）。
fn box_key(fb: &mut Fb, r: (i32, i32, i32, i32)) {
    let (x, y, w, h) = r;
    let c = gray565(250);
    fb.fill_rect(x, y, w, 3, c);
    fb.fill_rect(x, y + h - 3, w, 3, c);
    fb.fill_rect(x, y, 3, h, c);
    fb.fill_rect(x + w - 3, y, 3, h, c);
}

/// 退格键：左指的箭头里嵌一个叉。
fn backspace_glyph(fb: &mut Fb, r: (i32, i32, i32, i32), tone: u8) {
    let (x, y, w, h) = r;
    let (cx, cy) = (x + w / 2, y + h / 2);
    let c = gray565(tone);
    let half = 20;
    // 五边形轮廓（尖朝左）
    widgets::line(fb, cx - half, cy, cx - half + 12, cy - 14, 3, tone);
    widgets::line(fb, cx - half, cy, cx - half + 12, cy + 14, 3, tone);
    widgets::line(fb, cx - half + 12, cy - 14, cx + half, cy - 14, 3, tone);
    widgets::line(fb, cx - half + 12, cy + 14, cx + half, cy + 14, 3, tone);
    widgets::line(fb, cx + half, cy - 14, cx + half, cy + 14, 3, tone);
    // 叉
    for i in 0..14 {
        fb.fill_rect(cx - 6 + i, cy - 7 + i, 3, 3, c);
        fb.fill_rect(cx + 7 - i, cy - 7 + i, 3, 3, c);
    }
}
