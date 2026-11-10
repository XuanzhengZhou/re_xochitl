//! 把 AI 的回答**写成纸上的墨迹**（Live Note 正文）。
//!
//! 用户要的不是"底下弹一条带子"——那是把回答当成聊天气泡。要的是回答就长在
//! 手写旁边，能跟着页面翻、能被人用橡皮擦掉、导出的时候也在。所以整条链路是：
//!
//! ```text
//! 回答文本 ──排版──▶ 内存帧缓冲（文字 + 公式图 + 插图）──栅格──▶ 墨迹 op
//!                                                              │
//!                     annotate::append（写进独立的 `AI` 图层）◀─┘
//! ```
//!
//! # 为什么先渲染成位图再变墨迹
//!
//! 文字（含中文/CJK 折行）、LaTeX 公式（服务器 `pdflatex` 出来的灰度 PNG）、
//! 插图，这三样在屏幕上的画法 [`crate::ui::text`] 和 [`crate::panels::ai`] 里
//! 已经写好了。再实现一套"把 TeX 直接变笔画"的排版器是重复劳动，而且和屏幕上
//! 看到的永远对不齐。所以走位图这条最笨但最准的路：**屏幕上怎么画，纸上就怎么
//! 长**（16 级灰 → 阈值 → 每行连续暗像素一根 1 px 墨线，和
//! `annotate::text_lines` 里把字形轮廓变线是同一个做法）。
//!
//! # 代价
//!
//! 一页墨迹的条数就是"暗像素游程"的条数：一个字几十条，一段话几千条。这是
//! 墨迹格式本身的成本（用户自己手写一整页也是几百上千条），所以这里只做两件
//! 事控制体积：[`MAX_H`] 截断太长、[`MAX_OPS`] 兜底。
//!
//! # 线程
//!
//! [`render`] 会**同步取公式图/插图**，只能在后台线程调（主线程调会卡住界面）。

use std::collections::HashMap;
use std::sync::Mutex;

use crate::fb::{gray565, Fb};
use crate::net;
use crate::panels::ai;
use crate::panels::annotate::Op;
use crate::panels::teximg;
use crate::ui::text::Text;

/// 正文（和 C8 的 AI 面板同一档：32 px / 行高 46）
pub const FONT: f32 = 32.0;
/// 行高
pub const LINE_H: i32 = 46;
/// 行内基线下沉（在行框里的位置）
const BASELINE: i32 = 34;
/// 行内公式最高多少（再高就压行了）
const MATH_H: i32 = 48;
/// 插图最高多少
const IMG_H: i32 = 200;
/// 一块回答最多占多高（再长就截断加 `…`）
pub const MAX_H: i32 = 1100;
/// 墨迹条数上限。到顶就停（宁可少写几行，也不要把页面文件撑爆）
pub const MAX_OPS: usize = 14000;
/// 页面左侧留白。**必须让开工具条那一列**（展开时 120 px）：canvas 的
/// `clip_l` 就是 120，画在它左边的墨迹一个像素都贴不出去，表现是"回答左边被
/// 切掉半句"（真机踩过：`落点 (105,567)` → 左边 3 个字没了）。
pub const PAGE_LEFT: i32 = 132;
/// 右/上/下留白
pub const PAGE_MARGIN: i32 = 48;
/// 回答块和手写之间空多少
pub const GAP: i32 = 28;
/// 公式图请求的倍率（服务器那边 2 倍宽，字体最清楚）
const TEX_SCALE: u32 = 2;

/// 渲染好的一块回答：`ops` 的原点在 (0,0)，由 [`shifted`] 平移到落点。
#[derive(Debug, Clone, Default)]
pub struct Rendered {
    pub ops: Vec<Op>,
    /// 这块回答实际占的宽（≤ 请求宽度）
    pub w: i32,
    /// 实际占的高
    pub h: i32,
    /// 太长被截断了（末尾画了 `…`）
    pub truncated: bool,
}

// ---------------------------------------------------------------- 内联解析

/// 一行里的一个片段。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Piece {
    /// 一段可以折行的文字（英文按词、CJK 按字）
    Text(String),
    /// 行内公式原文（不含 `$`）
    Math(String),
    /// 插图源：`tex:<公式>` 或 `http(s)://…`
    Img(String),
}

/// 把一行拆成 `![…](…)` / `$…$` / 普通文字三种片段。
fn scan(line: &str) -> Vec<Piece> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut i = 0usize;
    while i < b.len() {
        // `![alt](src)`
        if b[i] == b'!' && b.get(i + 1) == Some(&b'[') {
            if let Some((alt_end, src_end, src)) = parse_md_img(line, i) {
                push_text(&mut out, &mut buf);
                let _ = alt_end;
                out.push(Piece::Img(src));
                i = src_end;
                continue;
            }
        }
        // `$…$`
        if b[i] == b'$' {
            if let Some(j) = line[i + 1..].find('$') {
                let f = line[i + 1..i + 1 + j].trim().to_string();
                if !f.is_empty() {
                    push_text(&mut out, &mut buf);
                    out.push(Piece::Math(f));
                    i = i + 1 + j + 1;
                    continue;
                }
            }
        }
        // 普通字符（按 UTF-8 边界整体搬运）
        let ch_len = utf8_len(b[i]);
        buf.push_str(&line[i..(i + ch_len).min(line.len())]);
        i += ch_len;
    }
    push_text(&mut out, &mut buf);
    out
}

/// `![alt](src)` 的解析：返回 `(alt 结束位置, 整体结束位置, src)`。
fn parse_md_img(s: &str, at: usize) -> Option<(usize, usize, String)> {
    let b = s.as_bytes();
    let alt_open = at + 1; // `[`
    if b.get(alt_open) != Some(&b'[') {
        return None;
    }
    let alt_close = s[alt_open..].find(']')? + alt_open;
    if b.get(alt_close + 1) != Some(&b'(') {
        return None;
    }
    // `src` 里**括号是配对的**：`tex:\draw (0,0)--(2,1);` 这种，按第一个 `)`
    // 截断会把整段 TikZ 切碎，剩下的变成正文印在纸上。按层数配对找右括号。
    let src_start = alt_close + 2;
    let mut depth = 1i32;
    let mut src_close = None;
    for (k, ch) in s[src_start..].char_indices() {
        match ch {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    src_close = Some(src_start + k);
                    break;
                }
            }
            _ => {}
        }
    }
    let src_close = src_close?;
    let src = s[src_start..src_close].trim().to_string();
    if src.is_empty() {
        return None;
    }
    Some((alt_close, src_close + 1, src))
}

fn push_text(out: &mut Vec<Piece>, buf: &mut String) {
    let t = std::mem::take(buf);
    for tok in tokens(&t) {
        out.push(Piece::Text(tok));
    }
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    }
}

/// 折行用的最小单位：ASCII 词整体、CJK 一字一个（中文没有空格可断）。
fn tokens(s: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut word = String::new();
    let flush = |out: &mut Vec<String>, word: &mut String| {
        if !word.is_empty() {
            out.push(std::mem::take(word));
        }
    };
    for ch in s.chars() {
        if is_cjk(ch) {
            flush(&mut out, &mut word);
            out.push(ch.to_string());
        } else if ch == ' ' || ch == '\t' {
            flush(&mut out, &mut word);
            out.push(" ".to_string());
        } else {
            word.push(ch);
        }
    }
    flush(&mut out, &mut word);
    out
}

/// 和 `panels::live::is_cjk` 同一档判定（CJK 统一表意 + 全角标点 + 假名/谚文）。
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x2E80..=0x303F | 0x3040..=0x30FF | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF | 0xA000..=0xA4CF | 0xAC00..=0xD7AF
        | 0xF900..=0xFAFF | 0xFE30..=0xFE4F | 0xFF00..=0xFFEF
        | 0x20000..=0x2FA1F)
}

// ---------------------------------------------------------------- 资产

/// 取插图：`tex:<公式>` 走服务器的 TeX 端点，`http(s)://` 走 `/api/v1/img`。
///
/// 失败一律 `None`（调用方画 `[image]` 占位），绝不 panic、绝不重试打爆网络。
pub fn fetch_img(src: &str, scale: u32) -> Option<teximg::Img> {
    if let Some(f) = src.strip_prefix("tex:") {
        return teximg::get_or_fetch(f.trim(), scale).map(|a| teximg::Img {
            w: a.w,
            h: a.h,
            gray: a.gray.clone(),
        });
    }
    if !(src.starts_with("http://") || src.starts_with("https://")) {
        return None;
    }
    let path = format!("/api/v1/img?u={}", teximg::urlencode(src));
    let rd = net::open_stream("GET", &path, &[], std::time::Duration::from_secs(20)).ok()?;
    use std::io::Read;
    let mut png = Vec::new();
    rd.take(8 * 1024 * 1024).read_to_end(&mut png).ok()?;
    let (w, h, gray) = ai::png_to_gray(&png)?;
    if w == 0 || h == 0 || gray.len() < w * h {
        return None;
    }
    Some(teximg::Img { w, h, gray })
}

/// 把回答里所有公式/插图先取回来（**同步**，只在后台线程调）。
///
/// 先取再排版：公式有多宽决定了这一行还剩多少字能塞。
fn prefetch(blocks: &[Vec<Piece>]) -> HashMap<String, Option<teximg::Img>> {
    let mut got: HashMap<String, Option<teximg::Img>> = HashMap::new();
    for blk in blocks {
        for p in blk {
            let key = match p {
                Piece::Math(f) => f.clone(),
                Piece::Img(u) => u.clone(),
                Piece::Text(_) => continue,
            };
            if got.contains_key(&key) {
                continue;
            }
            let img = match p {
                Piece::Math(f) => teximg::get_or_fetch(f, TEX_SCALE).map(|a| teximg::Img {
                    w: a.w,
                    h: a.h,
                    gray: a.gray.clone(),
                }),
                Piece::Img(u) => fetch_img(u, TEX_SCALE),
                Piece::Text(_) => None,
            };
            got.insert(key, img);
        }
    }
    got
}

// ---------------------------------------------------------------- 渲染

/// 把一段回答渲染成墨迹 op（原点在 (0,0)，宽 ≤ `width`）。
///
/// **只给后台线程用**：里面会同步取公式图/插图。
pub fn render(reply: &str, width: i32) -> Rendered {
    let width = width.max(120);
    let mut blocks: Vec<Vec<Piece>> = Vec::new();
    for line in reply.lines() {
        let l = line.trim_end();
        if l.trim().is_empty() {
            blocks.push(Vec::new());
            continue;
        }
        blocks.push(scan(l));
    }
    // 去掉开头的空段（模型爱在前面留一行）
    while blocks.first().map(|b| b.is_empty()).unwrap_or(false) {
        blocks.remove(0);
    }
    let assets = prefetch(&blocks);

    let mut fb = Fb::open_mem();
    // `open_mem()` 的缓冲初值是全 0 = 全黑，必须先铺白纸，否则整块都被当成墨
    fb.clear(gray565(255));
    // 没有字体也不能崩：公式/插图照样能画，文字部分退化成一堆占位
    let mut text = Text::load();
    let mut out = Rendered::default();
    let (mut pen, mut y, mut row_h) = (0i32, 0i32, LINE_H);
    let mut used_w = 0i32;

    let newline = |pen: &mut i32, y: &mut i32, row_h: &mut i32| {
        *y += *row_h;
        *row_h = LINE_H;
        *pen = 0;
    };

    'blocks: for blk in &blocks {
        if blk.is_empty() {
            // 空行 = 半个行高的呼吸
            y += LINE_H / 2;
            continue;
        }
        for p in blk {
            if y + row_h > MAX_H {
                out.truncated = true;
                break 'blocks;
            }
            match p {
                Piece::Text(t) => {
                    if t == " " && pen == 0 {
                        continue;
                    }
                    let tw = tw_of(&mut text, t);
                    if pen + tw > width && pen > 0 {
                        newline(&mut pen, &mut y, &mut row_h);
                        if t == " " {
                            continue;
                        }
                        if y + row_h > MAX_H {
                            out.truncated = true;
                            break 'blocks;
                        }
                    }
                    draw_text(&mut text, &mut fb, pen, y + BASELINE, t);
                    pen += tw;
                    used_w = used_w.max(pen);
                }
                Piece::Math(f) => {
                    let img = assets.get(f.as_str()).and_then(|x| x.as_ref());
                    match img {
                        Some(im) => {
                            let c = im.cropped();
                            if c.w == 0 {
                                continue;
                            }
                            let h = fitted_h(&c, MATH_H, width);
                            let s = c.scaled_to_h(h as usize);
                            if pen + s.w as i32 > width && pen > 0 {
                                newline(&mut pen, &mut y, &mut row_h);
                            }
                            let top = y + (row_h - s.h as i32) / 2;
                            ai::blit_gray_scaled(
                                &mut fb,
                                pen,
                                top,
                                s.w as i32,
                                s.h as i32,
                                s.w,
                                s.h,
                                &s.gray,
                                i32::MIN,
                                i32::MAX,
                            );
                            row_h = row_h.max(s.h as i32 + 8);
                            pen += s.w as i32 + 10;
                            used_w = used_w.max(pen);
                        }
                        None => {
                            // 取不到图：画占位（英文，e-ink 无彩色）
                            let ph = "[formula]";
                            let tw = tw_of(&mut text, ph);
                            if pen + tw > width && pen > 0 {
                                newline(&mut pen, &mut y, &mut row_h);
                            }
                            draw_text(&mut text, &mut fb, pen, y + BASELINE, ph);
                            pen += tw + 10;
                            used_w = used_w.max(pen);
                        }
                    }
                }
                Piece::Img(u) => {
                    let img = assets.get(u.as_str()).and_then(|x| x.as_ref());
                    if pen > 0 {
                        newline(&mut pen, &mut y, &mut row_h);
                    }
                    match img {
                        Some(im) => {
                            let c = im.cropped();
                            if c.w == 0 {
                                continue;
                            }
                            let h = fitted_h(&c, IMG_H, width);
                            let s = c.scaled_to_h(h as usize);
                            if y + s.h as i32 > MAX_H {
                                out.truncated = true;
                                break 'blocks;
                            }
                            ai::blit_gray_scaled(
                                &mut fb,
                                pen,
                                y,
                                s.w as i32,
                                s.h as i32,
                                s.w,
                                s.h,
                                &s.gray,
                                i32::MIN,
                                i32::MAX,
                            );
                            y += s.h as i32 + 10;
                            used_w = used_w.max(s.w as i32);
                            pen = 0;
                            row_h = LINE_H;
                        }
                        None => {
                            draw_text(&mut text, &mut fb, pen, y + BASELINE, "[image]");
                            newline(&mut pen, &mut y, &mut row_h);
                        }
                    }
                }
            }
        }
        newline(&mut pen, &mut y, &mut row_h);
    }

    let h = (y + row_h).min(MAX_H);
    let w = used_w.min(width).max(1);
    if out.truncated {
        let dots = "…";
        let tw = tw_of(&mut text, dots);
        let by = (h - 8).max(BASELINE);
        draw_text(&mut text, &mut fb, (w - tw).max(0), by, dots);
    }
    out.ops = raster_ops(&fb, w, h);
    out.w = w;
    out.h = h;
    out
}

/// 没有字体时 `Text::load()` 是 `None`：宽度按 0 算、画的时候就跳过。
fn tw_of(text: &mut Option<Text>, s: &str) -> i32 {
    text.as_mut()
        .map(|t| t.width(FONT, s) as i32)
        .unwrap_or(0)
}

fn draw_text(text: &mut Option<Text>, fb: &mut Fb, x: i32, base: i32, s: &str) {
    if let Some(t) = text.as_mut() {
        t.draw(fb, x, base, FONT, s, 0);
    }
}

/// 图按 `max_h` 摆，但也不能宽过 `width`：取两个约束里更小的那个高度。
fn fitted_h(c: &teximg::Img, max_h: i32, width: i32) -> i32 {
    if c.w == 0 {
        return 1;
    }
    let by_w = ((width as f32) * (c.h as f32) / (c.w as f32)).floor() as i32;
    by_w.min(max_h).max(1)
}

/// 位图 → 墨迹：把暗像素按"竖直同形的游程"并成**一个矩形**，一个矩形一根线。
///
/// 以前的写法是"每一行每一段游程一根 1 px 横线"：一段 200 px 高的中文写下来就是
/// 几千条墨迹，一页 `.rm` 直接涨到 1.4 MB（正常手写页 300 KB），于是打开/保存/
/// 撤销/指纹全都慢十倍，真机表现就是"一操作就卡死"。
///
/// 现在：连续若干行的游程完全一样（`[x0,x1)` 相同）就合成一个矩形，用**一根带宽度
/// 的线段**画出来——`w >= h` 时是横向胶囊，`h > w` 时是竖向胶囊。胶囊的圆头会少画
/// 两个角，所以端点按半径内缩；长宽相等时退化成一根点。这样笔画数掉一个数量级，
/// 而墨迹覆盖的像素几乎不变。
fn raster_ops(fb: &Fb, w: i32, h: i32) -> Vec<Op> {
    let mut ops: Vec<Op> = Vec::new();
    // 还活着的矩形
    let mut active: Vec<(i32, i32, i32, i32)> = Vec::new(); // x0, x1, y0, y1
    for y in 0..h {
        let mut runs: Vec<(i32, i32)> = Vec::new();
        let mut x = 0;
        while x < w {
            if fb.get(x, y) >= 128 {
                x += 1;
                continue;
            }
            let start = x;
            while x < w && fb.get(x, y) < 128 {
                x += 1;
            }
            runs.push((start, x));
        }
        let mut kept = vec![false; active.len()];
        for (rx0, rx1) in runs {
            // **容差合并**：相邻两行的游程端点常常差 1 px（抗锯齿 + 128 阈值），
            // 要求完全相同的话一个字会被切成十几块。差 ≤1 px 就当同一段，取并集。
            let hit = active.iter().enumerate().position(|(i, a)| {
                !kept[i]
                    && (a.0 - rx0).abs() <= SPAN_TOL
                    && (a.1 - rx1).abs() <= SPAN_TOL
            });
            match hit {
                Some(i) => {
                    kept[i] = true;
                    let a = &mut active[i];
                    a.0 = a.0.min(rx0);
                    a.1 = a.1.max(rx1);
                    a.3 = y;
                }
                None => {
                    // 新开的这个在当前行是"活的"：kept 必须记 true，
                    // 否则同一行末尾的结算逻辑会立刻把它结掉（每行一根线）。
                    active.push((rx0, rx1, y, y));
                    kept.push(true);
                }
            }
        }
        // 这一行没续上的 → 结算
        let mut i = 0;
        while i < active.len() {
            if !kept[i] {
                let (x0, x1, y0, y1) = active.remove(i);
                ops.push(rect_stroke(x0, x1, y0, y1));
                if ops.len() >= MAX_OPS {
                    return ops;
                }
            } else {
                i += 1;
            }
        }
    }
    for (x0, x1, y0, y1) in active {
        ops.push(rect_stroke(x0, x1, y0, y1));
        if ops.len() >= MAX_OPS {
            break;
        }
    }
    ops
}

/// 相邻行游程端点的容差（像素）。
///
/// **必须是 0**：试过 1 px 容差（抗锯齿会让相邻行端点差一个像素），条数只再降
/// 18%，但合并时取并集会把细笔画变粗、公式糊成一坨——得不偿失。0 时矩形是
/// 逐像素精确的并集，画出来和原图一模一样。
const SPAN_TOL: i32 = 0;

/// 一个实心矩形 `[x0, x1) × [y0, y1+1)` → 一根带宽度（胶囊）的线段。
///
/// 横向胶囊：两点在矩形中线，左右各内缩半径；竖向胶囊：两点在中垂线，上下各内缩
/// 半径。长宽相等就画一根点（`pts` 只有一个点，`annotate` 会把单点笔迹当圆点）。
fn rect_stroke(x0: i32, x1: i32, y0: i32, y1: i32) -> Op {
    let rw = (x1 - x0) as f32;
    let rh = (y1 - y0 + 1) as f32;
    let xc = x0 as f32 + rw * 0.5;
    let yc = y0 as f32 + rh * 0.5;
    if rw >= rh {
        let d = rh;
        let half = d * 0.5;
        let (a, b) = (x0 as f32 + half, x1 as f32 - half);
        let pts = if b > a + 0.01 {
            vec![(a, yc), (b, yc)]
        } else {
            vec![(xc, yc)]
        };
        Op::Stroke { pts, w: d.max(1.0) }
    } else {
        let d = rw;
        let half = d * 0.5;
        let (a, b) = (y0 as f32 + half, (y1 + 1) as f32 - half);
        let pts = if b > a + 0.01 {
            vec![(xc, a), (xc, b)]
        } else {
            vec![(xc, yc)]
        };
        Op::Stroke { pts, w: d.max(1.0) }
    }
}

// ---------------------------------------------------------------- 落点

/// 选一块回答在页面上的落点（场景坐标，和 `.rm` 一致：x 以页面中线为 0）。
///
/// 优先顺序（能放下就用，放不下往下走）：
/// 1. **手写正下方**（左边缘和手写对齐）——最自然，像接着往下写；
/// 2. 手写正上方（底边贴着手写顶边）；
/// 3. 手写右边（手写靠左的时候）；
/// 4. 兜底：贴页面下边、允许压一点手写（宁可压，也不要画到纸外面）。
///
/// `anchor` 是这次手写的包围盒 `[x0, y0, x1, y1]`。
pub fn place(anchor: [i32; 4], size: (i32, i32), page: (i32, i32)) -> (i32, i32) {
    let (w, h) = size;
    let (pw, ph) = page;
    let [ax0, ay0, ax1, ay1] = anchor;
    // 左对齐手写左边，但夹在页边距里（手写写到右边缘时回答不能被推出纸外）
    let x = ax0.clamp(PAGE_LEFT, (pw - PAGE_MARGIN - w).max(PAGE_LEFT));

    let below = ay1 + GAP;
    if below + h <= ph - PAGE_MARGIN {
        return (x, below);
    }
    let above = ay0 - GAP - h;
    if above >= PAGE_MARGIN {
        return (x, above);
    }
    let right = ax1 + GAP;
    if right + w <= pw - PAGE_MARGIN && right >= PAGE_LEFT {
        return (right, ay0.max(PAGE_MARGIN));
    }
    (x, (ph - PAGE_MARGIN - h).max(PAGE_MARGIN))
}

/// 把渲染好的 ops 平移到落点。
pub fn shifted(r: &Rendered, at: (i32, i32), dx: i32) -> Vec<Op> {
    let (ox, oy) = at;
    r.ops
        .iter()
        .map(|op| match op {
            // 落点给的是**屏幕/页面像素**坐标，墨迹要的是场景坐标（x 以中线为 0）
            Op::Stroke { pts, w } => Op::Stroke {
                pts: pts
                    .iter()
                    .map(|&(x, y)| (x + ox as f32 - dx as f32, y + oy as f32))
                    .collect(),
                w: *w,
            },
            Op::Text { x, y, size, text } => Op::Text {
                x: *x + ox as f32 - dx as f32,
                y: *y + oy as f32,
                size: *size,
                text: text.clone(),
            },
            other => other.clone(),
        })
        .collect()
}

// ---------------------------------------------------------------- 后台渲染

/// 一条渲染任务的回执。
pub struct Done {
    /// 已经平移到页面上的墨迹 op（场景坐标）
    pub ops: Vec<Op>,
    /// 落点与尺寸（日志用）
    pub at: (i32, i32),
    /// `summary()` 的内容
    pub what: String,
}

type Rx = std::sync::mpsc::Receiver<Result<Done, String>>;

static JOB: Mutex<Option<Rx>> = Mutex::new(None);

/// 起一条后台渲染（同一时间只跑一条：新的会顶掉旧的接收端，旧线程自己跑完丢弃）。
///
/// `reply` 是模型的完整回答；`anchor` 是这次手写的包围盒（页面像素）；
/// `width` 是回答允许占的宽；`page` 是页面尺寸；`dx` 是页面中线（场景坐标原点）。
///
/// 主线程**立即返回**，用 [`poll`] 收结果。
pub fn spawn(reply: String, anchor: [i32; 4], width: i32, page: (i32, i32), dx: i32) {
    let (tx, rx) = std::sync::mpsc::channel();
    if let Ok(mut g) = JOB.lock() {
        *g = Some(rx);
    }
    let _ = std::thread::Builder::new()
        .name("inknote".into())
        .spawn(move || {
            let r = render(&reply, width);
            if r.ops.is_empty() {
                let _ = tx.send(Err("empty reply".to_string()));
                return;
            }
            let at = place(anchor, (r.w, r.h), page);
            let ops = shifted(&r, at, dx);
            let _ = tx.send(Ok(Done {
                ops,
                at,
                what: summary(&r),
            }));
        });
}

/// 收渲染结果（不阻塞）。`Some(Err)` = 这一轮没法落墨。
pub fn poll() -> Option<Result<Done, String>> {
    let rx = {
        let mut g = match JOB.lock() {
            Ok(g) => g,
            Err(e) => e.into_inner(),
        };
        g.take()?
    };
    match rx.try_recv() {
        Ok(r) => Some(r),
        Err(std::sync::mpsc::TryRecvError::Empty) => {
            // 还没好：把接收端放回去
            if let Ok(mut g) = JOB.lock() {
                if g.is_none() {
                    *g = Some(rx);
                }
            }
            None
        }
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            Some(Err("render thread died".to_string()))
        }
    }
}

/// 有没有一条渲染在路上。
pub fn busy() -> bool {
    match JOB.lock() {
        Ok(g) => g.is_some(),
        Err(e) => e.into_inner().is_some(),
    }
}

/// 只留几何信封（日志/通知用）。
pub fn summary(r: &Rendered) -> String {
    format!(
        "{} 条墨迹 {}×{}{}",
        r.ops.len(),
        r.w,
        r.h,
        if r.truncated { "（截断）" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_splits_math_and_images() {
        let p = scan(r"看这个 $x^2$ 和 ![图](tex:\frac{1}{2}) 对不对");
        assert_eq!(p[0], Piece::Text("看".into()));
        assert_eq!(p[1], Piece::Text("这".into()));
        assert_eq!(p[2], Piece::Text("个".into()));
        assert_eq!(p[3], Piece::Text(" ".into()));
        assert_eq!(p[4], Piece::Math("x^2".into()));
        assert_eq!(p[5], Piece::Text(" ".into()));
        assert!(p.iter().any(|x| matches!(x, Piece::Img(s) if s.starts_with("tex:"))));
    }

    #[test]
    fn scan_handles_balanced_parens_in_the_source() {
        // TikZ 的坐标都是括号：按第一个 `)` 截断会把整段 TikZ 当正文印出来
        let p = scan(r"![o](tex:\draw (0,0)--(2,1);) 尾");
        assert_eq!(
            p[0],
            Piece::Img(r"tex:\draw (0,0)--(2,1);".to_string())
        );
        assert_eq!(p[1], Piece::Text(" ".into()));
        assert_eq!(p[2], Piece::Text("尾".into()));
    }

    #[test]
    fn scan_keeps_unpaired_dollar_as_text() {
        let p = scan("价格 5 元");
        assert!(p.iter().all(|x| matches!(x, Piece::Text(_))));
        let p = scan("只有一边 $x");
        assert!(p.iter().all(|x| matches!(x, Piece::Text(_))));
    }

    #[test]
    fn tokens_break_cjk_per_char_and_keep_ascii_words() {
        assert_eq!(tokens("定积分 dx"), ["定", "积", "分", " ", "dx"]);
        assert_eq!(tokens("hello world"), ["hello", " ", "world"]);
    }

    #[test]
    fn place_prefers_below_the_ink() {
        let (x, y) = place([200, 300, 700, 400], (400, 200), (1404, 1872));
        assert_eq!(x, 200, "左边缘和手写对齐");
        assert_eq!(y, 400 + GAP);
    }

    #[test]
    fn place_goes_above_when_no_room_below() {
        let (x, y) = place([200, 1400, 700, 1700], (400, 200), (1404, 1872));
        assert_eq!(x, 200);
        assert!(y + 200 <= 1400 - GAP, "回答在手写上方且不重叠：y={y}");
        assert!(y >= PAGE_MARGIN);
    }

    #[test]
    fn place_clamps_into_the_page_when_ink_is_at_the_right_edge() {
        let (x, _) = place([1300, 300, 1390, 400], (400, 200), (1404, 1872));
        assert!(x + 400 <= 1404 - PAGE_MARGIN, "不能画到纸外面：x={x}");
    }

    #[test]
    fn place_never_leaves_the_page_when_nothing_fits() {
        // 手写占满整页：还是得给一个页内落点
        let (x, y) = place([48, 48, 1356, 1824], (400, 300), (1404, 1872));
        assert!(y >= PAGE_MARGIN && y + 300 <= 1872, "y={y}");
        assert!(x >= PAGE_MARGIN && x + 400 <= 1404, "x={x}");
    }

    #[test]
    fn shift_moves_into_scene_coordinates() {
        let r = Rendered {
            ops: vec![Op::Stroke {
                pts: vec![(1.0, 2.0), (3.0, 4.0)],
                w: 1.0,
            }],
            w: 10,
            h: 10,
            truncated: false,
        };
        // 页面中线 702：屏幕 x=702 对应场景 x=0
        let ops = shifted(&r, (702, 100), 702);
        match &ops[0] {
            Op::Stroke { pts, .. } => {
                assert!((pts[0].0 - 1.0).abs() < 1e-6, "{:?}", pts[0]);
                assert!((pts[0].1 - 102.0).abs() < 1e-6);
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn raster_ops_turns_a_line_into_one_stroke() {
        let mut fb = Fb::open_mem();
        fb.fill_rect(0, 0, 40, 40, gray565(255));
        fb.fill_rect(5, 7, 20, 1, gray565(0));
        let ops = raster_ops(&fb, 40, 40);
        assert_eq!(ops.len(), 1, "一条横线就是一根墨迹");
        match &ops[0] {
            Op::Stroke { pts, w } => {
                assert_eq!(*w, 1.0);
                assert!((pts[0].0 - 5.5).abs() < 1e-6);
                assert!((pts[1].0 - 24.5).abs() < 1e-6);
                assert!((pts[0].1 - 7.5).abs() < 1e-6);
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn raster_ops_merges_rows_into_one_thick_stroke() {
        // 20×20 的实心方块：合并以后是一根宽度 20 的线段，不是 20 根横线
        let mut fb = Fb::open_mem();
        fb.clear(gray565(255));
        fb.fill_rect(10, 10, 20, 20, gray565(0));
        let ops = raster_ops(&fb, 60, 60);
        assert_eq!(ops.len(), 1, "一个方块就是一根墨迹：{ops:?}");
        match &ops[0] {
            Op::Stroke { w, .. } => assert!((*w - 20.0).abs() < 1e-6, "笔宽要盖满 20 行"),
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn raster_ops_uses_a_vertical_capsule_for_tall_narrow_stems() {
        // 4 宽 × 40 高的竖笔画：竖向胶囊，一根
        let mut fb = Fb::open_mem();
        fb.clear(gray565(255));
        fb.fill_rect(10, 5, 4, 40, gray565(0));
        let ops = raster_ops(&fb, 60, 60);
        assert_eq!(ops.len(), 1, "{ops:?}");
        match &ops[0] {
            Op::Stroke { pts, w } => {
                assert!((*w - 4.0).abs() < 1e-6);
                assert_eq!(pts.len(), 2);
                assert!((pts[0].0 - pts[1].0).abs() < 1e-6, "竖胶囊两点同 x");
                assert!(pts[0].1 < pts[1].1);
            }
            o => panic!("{o:?}"),
        }
    }

    #[test]
    fn raster_ops_is_far_cheaper_than_one_row_one_stroke() {
        // 一整块"文字"（横竖都有）：合并后条数必须是行数的几分之一
        let mut fb = Fb::open_mem();
        fb.clear(gray565(255));
        // 画 8 个 24×24 的方框当字
        for i in 0..8 {
            let x = 10 + i * 30;
            fb.fill_rect(x, 10, 24, 3, gray565(0));
            fb.fill_rect(x, 31, 24, 3, gray565(0));
            fb.fill_rect(x, 10, 3, 24, gray565(0));
            fb.fill_rect(x + 21, 10, 3, 24, gray565(0));
        }
        let rows = 60usize;
        let ops = raster_ops(&fb, 300, rows as i32);
        // 老写法：24 行 × 每行 ~16 段 = 380+ 条
        assert!(
            ops.len() < rows * 2,
            "合并后还是太多：{} 条（行数 {rows}）",
            ops.len()
        );
    }

    #[test]
    fn raster_ops_still_caps_runaway_pages() {
        let mut fb = Fb::open_mem();
        fb.clear(gray565(255));
        // 棋盘格：没有任何相邻行同形，最坏情况
        for y in 0..200 {
            for x in 0..1400 {
                if (x / 1 + y) % 2 == 0 {
                    fb.put(x, y, gray565(0));
                }
            }
        }
        let ops = raster_ops(&fb, 1400, 200);
        assert!(ops.len() <= MAX_OPS);
    }

    #[test]
    fn raster_ops_sees_white_paper_as_no_ink() {
        let mut fb = Fb::open_mem();
        fb.clear(gray565(255));
        assert!(raster_ops(&fb, 200, 50).is_empty());
    }

    #[test]
    fn render_of_plain_text_produces_ink_inside_the_width() {
        let r = render("先算括号里的，再乘除。", 600);
        assert!(!r.ops.is_empty(), "文字应该变成墨迹");
        assert!(r.w <= 600);
        assert!(r.h > 0 && r.h <= MAX_H);
    }

    #[test]
    fn render_without_assets_still_draws_placeholders() {
        // 公式取不到（没有配置服务器 → 立即失败）也不能崩，要画占位
        let r = render("所以 $\\frac{1}{2}$ 是这样的", 600);
        assert!(!r.ops.is_empty());
    }

    #[test]
    fn render_respects_max_height_and_reports_truncation() {
        let long = "这是一行很长的解释，用来把高度撑到上限。\n".repeat(200);
        let r = render(&long, 800);
        assert!(r.h <= MAX_H, "h={}", r.h);
        assert!(r.truncated);
    }

    #[test]
    fn render_of_empty_reply_is_empty() {
        let r = render("", 600);
        assert!(r.ops.is_empty());
        assert!(r.h <= LINE_H);
    }

    #[test]
    fn fitted_h_respects_both_constraints() {
        let wide = teximg::Img {
            w: 4000,
            h: 200,
            gray: vec![255; 4000 * 200],
        };
        assert_eq!(fitted_h(&wide, 200, 1000), 50, "宽度是硬约束");
        let tall = teximg::Img {
            w: 100,
            h: 5000,
            gray: vec![255; 100 * 5000],
        };
        assert_eq!(fitted_h(&tall, 200, 1000), 200, "高度是硬约束");
    }
}

// ---------------------------------------------------------------- 出图（验证用）

/// 把墨迹 op 画回帧缓冲（横线段 + 端点圆点），用来离屏看效果。
///
/// op 里是**场景坐标**（x 以页面中线为 0），所以要加 `dx` 才回到像素。
fn draw_ops(fb: &mut Fb, ops: &[Op], dx: i32) {
    for op in ops {
        if let Op::Stroke { pts, w } = op {
            let r = (*w * 0.5).max(0.5);
            // 单点笔迹（合并出来的圆点）要单独画：`windows(2)` 是空的，
            // 真渲染器会把它当圆点（`annotate` 有 `single_point_stroke_is_a_dot`），
            // 这里不补的话离屏出图会缺一块，看着像字被啃掉了。
            if pts.len() == 1 {
                dot(fb, pts[0].0 + dx as f32, pts[0].1, r);
                continue;
            }
            for pair in pts.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                let (ex, ey) = (b.0 - a.0, b.1 - a.1);
                let len = (ex * ex + ey * ey).sqrt();
                let steps = (len.max(r * 2.0) as i32).max(1);
                for i in 0..=steps {
                    let t = i as f32 / steps as f32;
                    dot(fb, a.0 + ex * t + dx as f32, a.1 + ey * t, r);
                }
            }
        }
    }
}

/// 实心圆（离屏出图专用）。
fn dot(fb: &mut Fb, cx: f32, cy: f32, r: f32) {
    let ri = r.ceil() as i32;
    for yy in -ri..=ri {
        for xx in -ri..=ri {
            if ((xx * xx + yy * yy) as f32).sqrt() <= r {
                fb.put(cx as i32 + xx, cy as i32 + yy, gray565(0));
            }
        }
    }
}

fn demo_ink(fb: &mut Fb, x0: i32, y0: i32) {
    let pts = [
        (0, 60), (8, 20), (18, 10), (26, 40), (30, 60), // 2
        (56, 30), (70, 30), // +
        (96, 60), (108, 10), (118, 40), (120, 60), // 2
        (150, 10), (150, 60), // =
        (176, 60), (190, 10), (200, 60), // 3
    ];
    for pair in pts.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        for i in 0..=40 {
            let t = i as f32 / 40.0;
            let cx = a.0 as f32 + (b.0 - a.0) as f32 * t;
            let cy = a.1 as f32 + (b.1 - a.1) as f32 * t;
            for yy in -2..=2 {
                for xx in -2..=2 {
                    fb.put(x0 + cx as i32 + xx, y0 + cy as i32 + yy, gray565(0));
                }
            }
        }
    }
}

/// 演示回答（覆盖：中文、行内公式、整块公式、图片/示意图、超长截断）。
fn demo_reply(which: &str) -> String {
    match which {
        "diagram" => "画一下这个函数的形状：\n![抛物线](tex:\\begin{tikzpicture}[scale=0.9]\\draw[->] (-1.6,0)--(1.8,0) node[right]{$x$};\\draw[->] (0,-0.6)--(0,2.4) node[above]{$y$};\\draw[thick,domain=-1.3:1.3,samples=60] plot (\\x,{1.6-\\x*\\x});\\end{tikzpicture})\n顶点在 $(0,1.6)$，开口向下。".to_string(),
        "long" => (0..40)
            .map(|i| format!("第 {} 条解释：把左边的式子先通分再合并。\n", i + 1))
            .collect(),
        _ => "「$2+2$」算错了，应该是 $2+2=4$。\n\
              你写成了 $3$，大概是进位的时候把 $1$ 丢了。\n\
              整块公式：\n\
              $\\displaystyle \\int_0^1 x^2\\,dx=\\frac{1}{3}$\n\
              再算一遍。"
            .to_string(),
    }
}

/// `--render inknote:demo|diagram|long[:<缩略>]`：把回答渲染成墨迹画出来（不连库）。
pub fn render_demo(spec: &str, out: &str) {
    let spec = spec.trim_start_matches(':');
    let which = spec.split(':').next().unwrap_or("demo");
    let reply = demo_reply(which);
    let t0 = std::time::Instant::now();
    let (w, h) = (1404, 1872);
    // 手写摆在上半部：回答应该落在它正下方
    let anchor = [120, 240, 420, 340];
    let width = (w - 2 * PAGE_MARGIN - 40).max(200);
    let r = render(&reply, width);
    let at = place(anchor, (r.w, r.h), (w, h));
    let ops = shifted(&r, at, (w / 2) as i32);
    let mut fb = Fb::open_mem();
    fb.clear(gray565(255));
    demo_ink(&mut fb, anchor[0], anchor[1]);
    draw_ops(&mut fb, &ops, w / 2);
    match fb.dump_pgm(out) {
        Ok(()) => eprintln!(
            "[render] inknote:{which} → {out}：{}，落点 ({}, {})，{} ms",
            summary(&r),
            at.0,
            at.1,
            t0.elapsed().as_millis()
        ),
        Err(e) => eprintln!("[render] 写 {out} 失败：{e}"),
    }
}
