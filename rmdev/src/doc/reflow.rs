//! 重排引擎：把「章节 → 文本块」排成页，再渲染成灰度位图。
//!
//! EPUB / TXT / Markdown / HTML / FB2 / MOBI / DOCX 这些**流式**格式都共用这一份：
//! 各自只负责把文件解析成 [`Book`]（`Vec<Chapter>` + `Vec<TocEntry>`），
//! 断行、分页、字号行距、页面尺寸适配全在这里。
//!
//! # 坐标系
//!
//! 排版在**排版单位**里做：页面固定 `1000 × 1400`（[`PAGE_W`]/[`PAGE_H`]），
//! 字号、页边距都是这个单位。页数与 UI 传进来的像素尺寸无关，只有 [`draw_page`]
//! 才乘缩放系数（`w / PAGE_W`，必要时再被 `h / PAGE_H` 夹住）。
//! 好处：同一份分页结果可以渲染成缩略图和整页，翻页时不用重排。
//!
//! [`draw_page`] 是纯函数：同样的 (行, w, h) 一定逐字节相同。
//!
//! # 图片
//!
//! 图片是**整块原子**：块上挂 [`Img`]（已解码的灰度像素），分页时不拆分，
//! 放不下就整块换页；[`draw_page`] 按最近邻缩放到目标像素，再直接四舍五入到
//! 16 级灰阶（不做抖动——照片本来就看不清，抖动只是多花时间）。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;
use zune_png::PngDecoder;

use crate::fb::{Fb, WHITE};
use crate::ui::text::Text;

/// 565 的 6 bit 绿通道 → 8 bit 灰。面板只留 4 bit，这样量化无损于观感。
const GRAY64: [u8; 64] = {
    let mut t = [0u8; 64];
    let mut i = 0;
    while i < 64 {
        t[i] = ((i * 255 + 31) / 63) as u8;
        i += 1;
    }
    t
};

/// 排版页宽（排版单位）。
pub const PAGE_W: f32 = 1000.0;
/// 排版页高（排版单位）。
pub const PAGE_H: f32 = 1400.0;

/// 官方阅读设置里字号是 5 档，这里是排版单位下的取值（默认取中间档）。
pub const FONT_SIZES: [f32; 5] = [18.0, 21.0, 25.0, 30.0, 36.0];
pub const DEFAULT_FONT_SIZE: f32 = 25.0;
/// 行距 3 档（× 字号），对应官方那三个图标。
pub const LINE_SPACINGS: [f32; 3] = [1.30, 1.55, 1.85];
pub const DEFAULT_LINE_SPACING: f32 = 1.55;
/// 页边距 3 档（排版单位）。
pub const MARGINS: [i32; 3] = [50, 80, 120];
pub const DEFAULT_MARGIN: i32 = 80;

// ---------------------------------------------------------------------------
// 结构
// ---------------------------------------------------------------------------

/// 正文块的类型。只影响字号/加粗/段距，不做样式表。
///
/// `Image` 是唯一不带正文的类型：`Block::img` 才有像素，`Block::text` 是
/// 图片的来源 key（EPUB 是 zip 内路径，Markdown 是本地路径），搜索看不到它。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Body,
    H1,
    H2,
    H3,
    Image,
}

/// 一张解码好的图：灰度像素（255 = 白），行优先，长度 = `w * h`。
///
/// 不存彩色/alpha：面板本来就是 16 级灰阶，彩色在解码时就按感知亮度压掉了。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Img {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
}

/// 单张图解码后的像素上限（8 MB = 约 2896×2896）。超过就不显示这张图：
/// 设备只有 994 MB 内存，一张 2000 万像素的扫描图能顶掉整个页面缓存。
pub const MAX_IMG_PIXELS: usize = 8 << 20;

/// 一本书解码出来的图片**总字节**上限（灰度像素）。超了后面的图不再解码、
/// 整块丢掉——图册型 EPUB 全量解码能吃掉几百 MB，有界优先（和 `MAX_CHARS`
/// 截断正文同一套思路）。一本正常带插图的书（30 张 1024×1600）只有约 47 MB。
pub const MAX_IMG_TOTAL: usize = 48 << 20;

/// 一个正文块：类型 + 文本 + 标题锚点（`<h1 id>` 用来对齐目录页号）。
#[derive(Clone, Debug)]
pub struct Block {
    pub kind: Kind,
    pub text: String,
    pub id: String,
    /// `Kind::Image` 时的像素。`Arc` 是因为同一张图可能被多处引用，
    /// 各格式的缓存和分页结果都只克隆指针，不复制几十 MB 的像素。
    pub img: Option<Arc<Img>>,
}

impl Block {
    pub fn body(text: impl Into<String>) -> Block {
        Block {
            kind: Kind::Body,
            text: text.into(),
            id: String::new(),
            img: None,
        }
    }

    /// `level` 会夹到 1..=3。
    pub fn heading(level: u8, text: impl Into<String>) -> Block {
        let kind = match level {
            0 | 1 => Kind::H1,
            2 => Kind::H2,
            _ => Kind::H3,
        };
        Block {
            kind,
            text: text.into(),
            id: String::new(),
            img: None,
        }
    }

    /// 一张整块图片。`key` 是来源路径（调试用，也参与缓存键）。
    pub fn image(key: impl Into<String>, img: Arc<Img>) -> Block {
        Block {
            kind: Kind::Image,
            text: key.into(),
            id: String::new(),
            img: Some(img),
        }
    }

    pub fn is_heading(&self) -> bool {
        matches!(self.kind, Kind::H1 | Kind::H2 | Kind::H3)
    }
}

#[derive(Clone, Debug, Default)]
pub struct Chapter {
    pub blocks: Vec<Block>,
}

impl Chapter {
    pub fn is_empty(&self) -> bool {
        self.blocks.iter().all(|b| b.text.trim().is_empty())
    }
}

/// 目录项：`path` 指向 spine 里的路径（只有 EPUB 用），`anchor` 是块 id。
#[derive(Clone, Debug)]
pub struct TocEntry {
    pub level: u8,
    pub title: String,
    pub path: String,
    pub anchor: String,
}

/// 解析好的书：章节里是块，不是排版结果。
#[derive(Default)]
pub struct Book {
    pub chapters: Vec<Chapter>,
    pub toc: Vec<TocEntry>,
    /// 资源路径 → 章节下标（EPUB 的 href → spine index）。
    pub path_index: HashMap<String, usize>,
}

impl Book {
    /// 所有块里的字符数。用来做「读了多少」估算和溢出保护。
    pub fn char_count(&self) -> usize {
        self.chapters
            .iter()
            .flat_map(|c| c.blocks.iter())
            .map(|b| b.text.chars().count())
            .sum()
    }

    /// 粗排：按平均每页字数估个页数，供还没排版时（卡片角标）用。
    pub fn approx_pages(&self) -> usize {
        let n = self.char_count();
        (n / 900).max(1)
    }
}

// ---------------------------------------------------------------------------
// 排版结果
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct Line {
    /// 左边界（排版单位）
    pub x: f32,
    /// 基线 y（排版单位）
    pub baseline: f32,
    /// 字号（排版单位）
    pub size: f32,
    pub bold: bool,
    pub text: String,
}

/// 页面上的一张图：左上角 + 显示尺寸（都是排版单位）。
///
/// 分页时已经算好尺寸（等比缩到内容宽、最多半页高），渲染只负责乘缩放系数，
/// 所以同一份分页结果既能画整页也能画缩略图。
#[derive(Clone)]
pub struct ImgBox {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
    pub img: Arc<Img>,
}

#[derive(Clone, Default)]
pub struct Page {
    pub lines: Vec<Line>,
    /// 这一页上的图片。整块放进来，从不跨页。
    pub imgs: Vec<ImgBox>,
}

impl Page {
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && self.imgs.is_empty()
    }

    /// 这一页的文字（搜索用）。
    pub fn text(&self) -> String {
        let mut s = String::new();
        for l in &self.lines {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(&l.text);
        }
        s
    }
}

/// 排版参数。`PartialEq` 用来判断「设置变了要不要重排」。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Cfg {
    pub font_size: f32,
    pub line_spacing: f32,
    pub margin: i32,
}

impl Default for Cfg {
    fn default() -> Cfg {
        Cfg {
            font_size: DEFAULT_FONT_SIZE,
            line_spacing: DEFAULT_LINE_SPACING,
            margin: DEFAULT_MARGIN,
        }
    }
}

impl Cfg {
    /// 字号夹到 [8, 72]，行距夹到 [1.0, 3.0]，边距夹到页面能容下的范围。
    pub fn sanitized(self) -> Cfg {
        Cfg {
            font_size: self.font_size.clamp(8.0, 72.0),
            line_spacing: self.line_spacing.clamp(1.0, 3.0),
            margin: self.margin.clamp(0, (PAGE_W as i32 - 200) / 2),
        }
    }
}

pub struct Layout {
    pub cfg: Cfg,
    pub pages: Vec<Page>,
    /// 每个章节从第几页开始
    pub chapter_page: Vec<usize>,
    /// (章节下标, 标题 id) → 页码
    pub anchors: HashMap<(usize, String), usize>,
}

impl Layout {
    pub fn len(&self) -> usize {
        self.pages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pages.is_empty()
    }

    /// 章节 → 起始页。
    pub fn page_of_chapter(&self, ci: usize) -> usize {
        self.chapter_page.get(ci).copied().unwrap_or(0)
    }
}

// ---------------------------------------------------------------------------
// 字体
// ---------------------------------------------------------------------------

thread_local! {
    /// 全线程共用的一份 `Text`：16 MB 中文字体重载一次要几十毫秒，
    /// 每种格式各存一份既费内存也没必要（字形缓存不影响度量）。
    static FONTS: RefCell<Option<Rc<RefCell<Option<Text>>>>> = const { RefCell::new(None) };
    /// 渲染用的整屏临时画布：`Fb::open_mem()` 是固定 1404×1872 且会故意
    /// 泄掉那块内存（`mem::forget`），所以只能全线程建一次、每次重画前清白。
    static SCRATCH: RefCell<Fb> = RefCell::new(Fb::open_mem());
}

pub fn fonts() -> Rc<RefCell<Option<Text>>> {
    FONTS.with(|cell| {
        let mut slot = cell.borrow_mut();
        slot.get_or_insert_with(|| Rc::new(RefCell::new(Text::load())))
            .clone()
    })
}

// ---------------------------------------------------------------------------
// 断词 / 断行
// ---------------------------------------------------------------------------

pub fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x11ff      // 韩文字母
        | 0x2e80..=0x303f    // 部首、标点
        | 0x3040..=0x30ff    // 假名
        | 0x3100..=0x31ff
        | 0x3400..=0x4dbf    // 扩展 A
        | 0x4e00..=0x9fff    // 基本汉字
        | 0xa000..=0xa4cf
        | 0xac00..=0xd7af    // 谚文
        | 0xf900..=0xfaff    // 兼容汉字
        | 0xfe30..=0xfe4f
        | 0xff00..=0xffef    // 全角
        | 0x20000..=0x2fa1f)
}

/// 不能出现在行首的标点：跟上一个原子粘在一起，避免"。"单独一行。
pub fn is_cjk_tail(c: char) -> bool {
    matches!(
        c,
        '。' | '，'
            | '、'
            | '；'
            | '：'
            | '？'
            | '！'
            | '）'
            | '》'
            | '」'
            | '』'
            | '】'
            | '〕'
            | '”'
            | '’'
            | '…'
            | '—'
            | '·'
    )
}

fn fallback_adv(ch: char, size: f32) -> f32 {
    if ch == ' ' {
        size * 0.28
    } else if is_cjk(ch) {
        size
    } else {
        size * 0.52
    }
}

/// 字宽缓存：`Text::width` 是逐字符前进量累加（没有 kerning），
/// 所以"整串宽 = 每字宽之和"，按 (字符, 粗体, 字号) 缓存一次就能便宜地量整本书。
pub struct Adv<'a> {
    text: Option<&'a mut Text>,
    cache: HashMap<(char, bool, u32), f32>,
}

impl Adv<'_> {
    pub fn new(text: Option<&mut Text>) -> Adv<'_> {
        Adv {
            text,
            cache: HashMap::new(),
        }
    }

    fn of(&mut self, ch: char, size: f32, bold: bool) -> f32 {
        let key = (ch, bold, (size * 4.0) as u32);
        if let Some(&w) = self.cache.get(&key) {
            return w;
        }
        let w = match self.text.as_mut() {
            Some(t) => {
                t.set_bold(bold);
                t.width(size, &ch.to_string())
            }
            None => fallback_adv(ch, size),
        };
        self.cache.insert(key, w);
        w
    }

    /// 量一段文本的宽度（断行用不到，给居中/缩进之类的场合）。
    pub fn measure(&mut self, s: &str, size: f32, bold: bool) -> f32 {
        s.chars().map(|c| self.of(c, size, bold)).sum()
    }
}

/// 一个换行原子：一串不可再拆的拉丁词，或一个汉字/标点。
struct Atom {
    text: String,
    w: f32,
    /// 前面是否该补一个空格（原文这里确实有空白）
    sep: bool,
    /// 强制换行（`<br>`）
    brk: bool,
}

#[allow(clippy::too_many_arguments)]
fn push_word(
    out: &mut Vec<Atom>,
    word: &mut String,
    word_w: &mut f32,
    sep: bool,
    size: f32,
    bold: bool,
    max_w: f32,
    adv: &mut Adv,
) {
    if word.is_empty() {
        *word_w = 0.0;
        return;
    }
    let w = std::mem::take(word);
    let width = std::mem::replace(word_w, 0.0);
    if width <= max_w {
        out.push(Atom {
            text: w,
            w: width,
            sep,
            brk: false,
        });
        return;
    }
    // 超长单词（URL、无空格的长串）：按字符硬切，否则会溢出页面
    let mut chunk = String::new();
    let mut cw = 0.0f32;
    let mut first = true;
    for ch in w.chars() {
        let cwid = adv.of(ch, size, bold);
        if !chunk.is_empty() && cw + cwid > max_w {
            out.push(Atom {
                text: std::mem::take(&mut chunk),
                w: cw,
                sep: first && sep,
                brk: false,
            });
            first = false;
            cw = 0.0;
        }
        chunk.push(ch);
        cw += cwid;
    }
    if !chunk.is_empty() {
        out.push(Atom {
            text: chunk,
            w: cw,
            sep: first && sep,
            brk: false,
        });
    }
}

fn atoms(text: &str, size: f32, bold: bool, max_w: f32, adv: &mut Adv) -> Vec<Atom> {
    let mut out: Vec<Atom> = Vec::new();
    let mut word = String::new();
    let mut word_w = 0.0f32;
    let mut word_sep = false;
    let mut pending_sep = false;
    for ch in text.chars() {
        if ch == '\n' {
            push_word(
                &mut out,
                &mut word,
                &mut word_w,
                word_sep,
                size,
                bold,
                max_w,
                adv,
            );
            word_sep = false;
            out.push(Atom {
                text: String::new(),
                w: 0.0,
                sep: false,
                brk: true,
            });
            pending_sep = false;
        } else if ch.is_whitespace() {
            push_word(
                &mut out,
                &mut word,
                &mut word_w,
                word_sep,
                size,
                bold,
                max_w,
                adv,
            );
            word_sep = false;
            pending_sep = true;
        } else if is_cjk(ch) {
            push_word(
                &mut out,
                &mut word,
                &mut word_w,
                word_sep,
                size,
                bold,
                max_w,
                adv,
            );
            word_sep = false;
            let cw = adv.of(ch, size, bold);
            if is_cjk_tail(ch) && out.last().map_or(false, |a| !a.brk && !a.text.is_empty()) {
                let last = out.last_mut().unwrap();
                last.text.push(ch);
                last.w += cw;
            } else {
                out.push(Atom {
                    text: ch.to_string(),
                    w: cw,
                    sep: pending_sep && !out.is_empty(),
                    brk: false,
                });
                pending_sep = false;
            }
        } else {
            if word.is_empty() {
                word_sep = pending_sep;
                pending_sep = false;
            }
            let cw = adv.of(ch, size, bold);
            word.push(ch);
            word_w += cw;
        }
    }
    push_word(
        &mut out,
        &mut word,
        &mut word_w,
        word_sep,
        size,
        bold,
        max_w,
        adv,
    );
    out
}

/// 断行：贪心填满一行（中文按字、英文按词），返回折好的行。
pub fn wrap(text: &str, max_w: f32, size: f32, bold: bool, adv: &mut Adv) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut line_w = 0.0f32;
    let space_w = adv.of(' ', size, bold);
    for atom in atoms(text, size, bold, max_w, adv) {
        if atom.brk {
            out.push(std::mem::take(&mut line));
            line_w = 0.0;
            continue;
        }
        let sep = atom.sep && !line.is_empty();
        let extra = if sep { space_w } else { 0.0 };
        if !line.is_empty() && line_w + extra + atom.w > max_w {
            out.push(std::mem::take(&mut line));
            line.push_str(&atom.text);
            line_w = atom.w;
        } else {
            if sep && !line.is_empty() {
                line.push(' ');
                line_w += space_w;
            }
            line.push_str(&atom.text);
            line_w += atom.w;
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

// ---------------------------------------------------------------------------
// 图片解码
// ---------------------------------------------------------------------------

/// 单边像素上限。`zune` 自己默认 16384，这里跟着 8 MB 的总量一起收紧。
const MAX_IMG_DIM: usize = 4096;

/// 解码一张 JPEG / PNG 成灰度（255 = 白）。
///
/// **GIF 不解**（也不支持 WebP/BMP/SVG）：电子书里的 GIF 多是动图或分块拼接，
/// 纯 Rust 没有成熟解码器，为几张表情包再引一个 crate 不值得——调用方遇到
/// `Err` 直接跳过这张图，正文照排。
///
/// 先用解码器读文件头的宽高做上限检查，再真解码：伪造尺寸的压缩炸弹连
/// 内存都不会申请。JPEG 的 EXIF 方向标记不处理（设备上没有旋转的阅读场景）。
pub fn decode_image(data: &[u8]) -> Result<Img, String> {
    let opts = DecoderOptions::default()
        .set_max_width(MAX_IMG_DIM)
        .set_max_height(MAX_IMG_DIM)
        .png_set_strip_to_8bit(true);

    if data.starts_with(&[0xFF, 0xD8]) {
        // 先探一次头（知道几个分量再决定要不要直接出灰度），再解一遍。
        // 头部只有几百字节，两次解析的开销远小于灰度图被展开成 RGB。
        let mut probe = JpegDecoder::new_with_options(ZCursor::new(data), opts);
        probe.decode_headers().map_err(|e| format!("jpeg: {e}"))?;
        let info = probe.info().ok_or("jpeg: no header")?;
        let (w, h, comps) = (info.width as usize, info.height as usize, info.components);
        let (w, h) = checked_dims(w, h)?;
        if comps == 4 {
            // CMYK/YCCK：zune 给的是 4 通道原始数据，转灰要先做色彩变换，不做
            return Err("unsupported jpeg color mode (CMYK)".into());
        }
        let opts = if comps == 1 {
            opts.jpeg_set_out_colorspace(ColorSpace::Luma)
        } else {
            opts
        };
        let mut dec = JpegDecoder::new_with_options(ZCursor::new(data), opts);
        let px = dec.decode().map_err(|e| format!("jpeg: {e}"))?;
        let ch = channel_count(px.len(), w, h, "jpeg")?;
        return Ok(to_gray(px, w, h, ch));
    }

    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        let mut dec = PngDecoder::new_with_options(ZCursor::new(data), opts);
        dec.decode_headers().map_err(|e| format!("png: {e}"))?;
        let (pw, ph) = dec.dimensions().ok_or("png: no dimensions")?;
        let (w, h) = checked_dims(pw as usize, ph as usize)?;
        let px = dec.decode_raw().map_err(|e| format!("png: {e}"))?;
        let ch = channel_count(px.len(), w, h, "png")?;
        return Ok(to_gray(px, w, h, ch));
    }

    Err("unsupported image format".into())
}

fn checked_dims(w: usize, h: usize) -> Result<(usize, usize), String> {
    if w == 0 || h == 0 {
        return Err("image has zero size".into());
    }
    if w * h > MAX_IMG_PIXELS {
        return Err(format!("image {w}x{h} exceeds {MAX_IMG_PIXELS} pixels"));
    }
    Ok((w, h))
}

/// 由解码输出长度反推通道数（JPEG 灰度只给 1 通道）。
fn channel_count(len: usize, w: usize, h: usize, what: &str) -> Result<usize, String> {
    let px = w * h;
    if px == 0 || !len.is_multiple_of(px) || !(1..=4).contains(&(len / px)) {
        return Err(format!("{what}: unexpected pixel buffer length {len}"));
    }
    Ok(len / px)
}

/// 任意通道数 → 灰度（带 alpha 的按白底合成）。
fn to_gray(px: Vec<u8>, w: usize, h: usize, ch: usize) -> Img {
    if ch == 1 {
        return Img { w, h, px };
    }
    let mut out = vec![255u8; w * h];
    for (o, p) in out.iter_mut().zip(px.chunks_exact(ch)) {
        *o = match ch {
            3 => luma(p[0], p[1], p[2]),
            4 => over_white(luma(p[0], p[1], p[2]), p[3]),
            _ => over_white(p[0], p[1]),
        };
    }
    Img { w, h, px: out }
}

/// 感知加权灰度：`(299r + 587g + 114b) / 1000`。
#[inline]
fn luma(r: u8, g: u8, b: u8) -> u8 {
    ((299 * r as u32 + 587 * g as u32 + 114 * b as u32) / 1000) as u8
}

/// 灰度 `g` 在透明度 `a` 下与白底合成。
#[inline]
fn over_white(g: u8, a: u8) -> u8 {
    if a == 255 {
        return g;
    }
    ((g as u32 * a as u32 + 255 * (255 - a as u32) + 127) / 255) as u8
}

/// 解码结果的 LRU 缓存（`HashMap` + 访问顺序表）。
///
/// 一本 EPUB 里同一张图常被多处引用（章首装饰、署名图），解一次就够；
/// 额度按「一屏能看到的图」定 8 张，超了淘汰最久未用的。
/// 只在解析线程里用，所以不加锁。
pub struct ImgCache {
    cap: usize,
    map: HashMap<String, Arc<Img>>,
    /// 最近使用的在末尾，淘汰从头部开始。
    order: Vec<String>,
}

impl ImgCache {
    pub fn new(cap: usize) -> ImgCache {
        ImgCache {
            cap: cap.max(1),
            map: HashMap::new(),
            order: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 取一张（命中就把它挪到最近使用）。
    pub fn get(&mut self, key: &str) -> Option<Arc<Img>> {
        let img = self.map.get(key)?.clone();
        self.touch(key);
        Some(img)
    }

    pub fn insert(&mut self, key: String, img: Arc<Img>) {
        self.map.insert(key.clone(), img);
        self.touch(&key);
        while self.map.len() > self.cap {
            let Some(old) = self.order.first().cloned() else {
                break;
            };
            self.order.remove(0);
            self.map.remove(&old);
        }
    }

    fn touch(&mut self, key: &str) {
        if let Some(i) = self.order.iter().position(|k| k == key) {
            let k = self.order.remove(i);
            self.order.push(k);
        } else {
            self.order.push(key.to_string());
        }
    }
}

// ---------------------------------------------------------------------------
// 分页
// ---------------------------------------------------------------------------

/// 把整本书排成页。
///
/// 竖排位置：行高 = 字号 × 行距（标题固定 1.25 倍），基线放在行盒中间偏下
/// ——常数 0.31 是拿 Maison Neue + NotoSansSC 实测量出来的：
/// 汉字墨迹在基线上方 0.83×字号、下方 0.06，拉丁降部到 0.25。
pub fn paginate(book: &Book, mut text: Option<&mut Text>, cfg: Cfg) -> Layout {
    let cfg = cfg.sanitized();
    let body = cfg.font_size;
    let margin = cfg.margin as f32;
    let content_w = (PAGE_W - 2.0 * margin).max(120.0);
    let content_h = (PAGE_H - 2.0 * margin).max(120.0);
    let bottom = margin + content_h;
    let mut adv = Adv::new(text.as_deref_mut());
    let mut pages: Vec<Page> = Vec::new();
    let mut cur: Vec<Line> = Vec::new();
    let mut cur_imgs: Vec<ImgBox> = Vec::new();
    let mut y = margin;
    let mut chapter_page: Vec<usize> = Vec::with_capacity(book.chapters.len());
    let mut anchors: HashMap<(usize, String), usize> = HashMap::new();

    for (ci, chapter) in book.chapters.iter().enumerate() {
        // 章首另起一页：官方阅读器也这样，目录页号才对得上
        if !cur.is_empty() || !cur_imgs.is_empty() {
            pages.push(Page {
                lines: std::mem::take(&mut cur),
                imgs: std::mem::take(&mut cur_imgs),
            });
            y = margin;
        }
        chapter_page.push(pages.len());
        for block in &chapter.blocks {
            // 图片是整块原子：先按内容宽等比缩放（最长半页高），放不下就整块换页。
            // `img` 为空的图片块（解析时没解出来）直接跳过，不排一行路径文字。
            if block.kind == Kind::Image {
                let Some(img) = block.img.as_ref() else {
                    continue;
                };
                let (iw, ih) = img_box_size(img, content_w, content_h);
                if y + ih > bottom && !(cur.is_empty() && cur_imgs.is_empty()) {
                    pages.push(Page {
                        lines: std::mem::take(&mut cur),
                        imgs: std::mem::take(&mut cur_imgs),
                    });
                    y = margin;
                }
                cur_imgs.push(ImgBox {
                    x: margin,
                    y,
                    w: iw,
                    h: ih,
                    img: img.clone(),
                });
                y += ih + body * 0.5;
                continue;
            }
            let heading = block.is_heading();
            let size = if heading { body * 1.2 } else { body };
            let bold = heading;
            let line_h = if heading {
                size * 1.25
            } else {
                size * cfg.line_spacing
            };
            if heading && y > margin + 1.0 {
                y += size * 0.6;
            }
            let lines = wrap(&block.text, content_w, size, bold, &mut adv);
            let mut first = true;
            for t in lines {
                if y + line_h > bottom && !(cur.is_empty() && cur_imgs.is_empty()) {
                    pages.push(Page {
                        lines: std::mem::take(&mut cur),
                        imgs: std::mem::take(&mut cur_imgs),
                    });
                    y = margin;
                }
                if first {
                    if heading && !block.id.is_empty() {
                        // 标题真的落在哪页，目录锚点就指哪页
                        anchors.insert((ci, block.id.clone()), pages.len());
                    }
                    first = false;
                }
                let k = line_h / size;
                let baseline = (y + size * (k / 2.0 + 0.31)).min(y + line_h - 0.08 * size);
                cur.push(Line {
                    x: margin,
                    baseline,
                    size,
                    bold,
                    text: t,
                });
                y += line_h;
            }
            y += if heading { size * 0.35 } else { body * 0.5 };
        }
    }
    if !cur.is_empty() || !cur_imgs.is_empty() {
        pages.push(Page {
            lines: cur,
            imgs: cur_imgs,
        });
    }
    Layout {
        cfg,
        pages,
        chapter_page,
        anchors,
    }
}

/// 图片在页面上的显示尺寸（排版单位）：先缩到内容宽，高度超过半页再按高夹。
///
/// 半页高是硬上限：再高的图一页也只能放一张，读者会以为翻页卡住了。
/// 页边距调到极限时内容高会小于半页，所以再跟可用高度取小，保证图不出血。
fn img_box_size(img: &Img, content_w: f32, content_h: f32) -> (f32, f32) {
    if img.w == 0 || img.h == 0 {
        return (0.0, 0.0);
    }
    let max_h = (PAGE_H / 2.0).min(content_h);
    let mut w = content_w;
    let mut h = content_w * img.h as f32 / img.w as f32;
    if h > max_h {
        h = max_h;
        w = max_h * img.w as f32 / img.h as f32;
    }
    (w, h)
}

// ---------------------------------------------------------------------------
// 渲染
// ---------------------------------------------------------------------------

/// 把一页排好的行画成 `w×h` 灰度（255 = 白），长度正好 `w*h`。
///
/// 宽度优先填满，页高不够时按高夹住；传进来的 (w,h) 比例若和
/// `PAGE_W/PAGE_H` 不一致，剩下的空白居中（UI 算错时也不难看）。
pub fn draw_page(page: &Page, w: usize, h: usize) -> Vec<u8> {
    if w == 0 || h == 0 {
        return Vec::new();
    }
    let scale = (w as f32 / PAGE_W).min(h as f32 / PAGE_H);
    let ox = ((w as f32 - PAGE_W * scale) / 2.0).round().max(0.0) as i32;
    let oy = ((h as f32 - PAGE_H * scale) / 2.0).round().max(0.0) as i32;
    SCRATCH.with(|cell| {
        let mut fb = cell.borrow_mut();
        let (fw, fh) = (fb.width, fb.height);
        fb.fill_rect(0, 0, fw, fh, WHITE);
        {
            let fonts = fonts();
            let mut guard = fonts.borrow_mut();
            if let Some(t) = guard.as_mut() {
                for line in &page.lines {
                    if line.text.is_empty() {
                        continue;
                    }
                    let x = (line.x * scale).round() as i32 + ox;
                    let y = (line.baseline * scale).round() as i32 + oy;
                    let size = line.size * scale;
                    if line.bold {
                        t.draw_bold(&mut fb, x, y, size, &line.text, 0);
                    } else {
                        t.draw(&mut fb, x, y, size, &line.text, 0);
                    }
                }
            }
        }
        for ib in &page.imgs {
            draw_img(&mut fb, ib, scale, ox, oy);
        }
        // 从 565 帧缓冲抠出 8 bit 灰。逐像素 `fb.get()`（带坐标运算和
        // 边界检查）在 1404×1872 上是十几万次调用，实测占满整页 render 的
        // 九成；按行取切片 + 64 项查表就只剩一次数据搬移。
        let mut out = vec![255u8; w * h];
        let rows = h.min(fb.height.max(0) as usize);
        for row in 0..rows {
            let Some(dst) = fb.row_mut(row as i32, 0, w as i32) else {
                continue;
            };
            let base = row * w;
            let n = dst.len();
            for (o, &v) in out[base..base + n].iter_mut().zip(dst.iter()) {
                *o = GRAY64[((v >> 5) & 0x3f) as usize];
            }
        }
        out
    })
}

/// 把一张图贴进帧缓冲：最近邻缩放到目标像素矩形，再量化到 16 级灰阶。
///
/// 不做抖动：面板只有 16 级，抖成网点在电子纸上反而更脏，而且每次重绘都要
/// 重算噪声。缩小时最近邻会丢细节，但这里的目标是「看得出是什么图」，
/// 不是还原照片——真要缩得好得做面积平均，一页多花几十毫秒不值。
fn draw_img(fb: &mut Fb, ib: &ImgBox, scale: f32, ox: i32, oy: i32) {
    let img = &ib.img;
    if img.w == 0 || img.h == 0 {
        return;
    }
    let x = (ib.x * scale).round() as i32 + ox;
    let y = (ib.y * scale).round() as i32 + oy;
    let w = ((ib.w * scale).round() as i32).max(1) as usize;
    let h = ((ib.h * scale).round() as i32).max(1) as usize;
    let mut buf = vec![255u8; w * h];
    for ry in 0..h {
        let sy = (ry * img.h / h).min(img.h - 1);
        let row = &img.px[sy * img.w..sy * img.w + img.w];
        let dst = &mut buf[ry * w..(ry + 1) * w];
        for (rx, d) in dst.iter_mut().enumerate() {
            let sx = (rx * img.w / w).min(img.w - 1);
            *d = gray16(row[sx]);
        }
    }
    fb.blit_gray(x, y, w as i32, h as i32, &buf);
}

/// 8 bit 灰 → 16 级灰阶（0,17,...,255 里最近的一档）。
#[inline]
fn gray16(g: u8) -> u8 {
    (((g as u32 * 15 + 127) / 255) * 17) as u8
}

// ---------------------------------------------------------------------------
// 纯文本 → 块（TXT/Markdown/HTML/FB2 之类共用的收尾工具）
// ---------------------------------------------------------------------------

/// 把一整段纯文本切成块：空行分段，行首 `#`/`##`/`###` 当标题（Markdown 风格），
/// 连续的非空行并成一个段落（PDF 抽文字、TXT 换行都这样，硬换行不是段）。
pub fn blocks_from_plain(text: &str, max_blocks: usize) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut para = String::new();
    let mut saw_blank = true;
    for raw in text.lines() {
        let line = raw.trim_end_matches(['\r']).trim();
        if let Some(rest) = line.strip_prefix('#') {
            let level = 1 + rest.chars().take_while(|c| *c == '#').count();
            if level <= 3 {
                flush_para(&mut out, &mut para);
                let t = rest.trim_start_matches('#').trim();
                if !t.is_empty() {
                    out.push(Block::heading(level as u8, t));
                }
                saw_blank = true;
                continue;
            }
        }
        if line.is_empty() {
            flush_para(&mut out, &mut para);
            saw_blank = true;
        } else {
            if !para.is_empty() {
                // 中文直接接，拉丁文字补空格
                let prev_cjk = para.chars().last().map_or(false, is_cjk);
                let next_cjk = line.chars().next().map_or(false, is_cjk);
                if !prev_cjk && !next_cjk {
                    para.push(' ');
                }
            } else if !saw_blank && !out.is_empty() {
                // 上一块是标题紧跟正文，不加额外东西
            }
            para.push_str(line);
            saw_blank = false;
        }
        if out.len() >= max_blocks {
            break;
        }
    }
    flush_para(&mut out, &mut para);
    out
}

fn flush_para(out: &mut Vec<Block>, para: &mut String) {
    let t = para.trim();
    if !t.is_empty() {
        out.push(Block::body(t));
    }
    para.clear();
}

/// 在分页结果里找一段文字，返回页码。
///
/// 用「只留字母数字 + 小写」再比：中文书里标点/换行不一致很常见，
/// 精确匹配反而找不到。
pub fn find_page(layout: &Layout, needle: &str) -> Option<usize> {
    let (nchars, _) = normalize_map(needle);
    if nchars.is_empty() {
        return None;
    }
    for (i, page) in layout.pages.iter().enumerate() {
        let (hchars, _) = normalize_map(&page.text());
        if contains(&hchars, &nchars).is_some() {
            return Some(i);
        }
    }
    None
}

/// 全文搜索：返回 (页号, 该页里截出来的片段)。
pub fn search(layout: &Layout, needle: &str, limit: usize) -> Vec<(usize, String)> {
    let (nchars, _) = normalize_map(needle);
    if nchars.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, page) in layout.pages.iter().enumerate() {
        let text = page.text();
        let (hchars, hoffs) = normalize_map(&text);
        if let Some(p) = contains(&hchars, &nchars) {
            // 命中的首尾**原文字节**位置（不是规范化后的下标）
            let start = hoffs[p];
            let last = p + nchars.len() - 1;
            let end = hoffs[last] + text[hoffs[last]..].chars().next().map_or(0, char::len_utf8);
            out.push((i, snippet_at(&text, start, end)));
            if out.len() >= limit {
                break;
            }
        }
    }
    out
}

/// 规范化 + 记住每个字符在原串里的字节偏移。
///
/// 搜索要忽略标点和大小写，但取片段必须回到原文的真字节位置——直接把
/// 规范化后的偏移当原文字节偏移用，遇到中文就会在非 char boundary 上
/// 切片 panic。
fn normalize_map(s: &str) -> (Vec<char>, Vec<usize>) {
    let mut chars = Vec::with_capacity(s.len());
    let mut offs = Vec::with_capacity(s.len());
    for (i, c) in s.char_indices() {
        if c.is_alphanumeric() {
            for lc in c.to_lowercase() {
                chars.push(lc);
                offs.push(i);
            }
        }
    }
    (chars, offs)
}

/// `hay` 里找 `needle`，返回起始下标。
fn contains(hay: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| hay[i..i + needle.len()] == *needle)
}

/// 在原文里截 `[start, end)` 前后各留些上下文，两端加省略号。
fn snippet_at(text: &str, start: usize, end: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let start_ch = text[..start.min(text.len())].chars().count();
    let end_ch = text[..end.min(text.len())].chars().count();
    let a = start_ch.saturating_sub(20);
    let b = (end_ch + 30).min(chars.len());
    let mut s: String = chars[a.min(b)..b].iter().collect();
    if a > 0 {
        s.insert(0, '…');
    }
    if b < chars.len() {
        s.push('…');
    }
    s
}

// ---------------------------------------------------------------------------
// 通用壳
// ---------------------------------------------------------------------------

/// 「解析好的书 + 分页缓存 + 最近一页渲染缓存」的合体。
///
/// TXT / Markdown / HTML / FB2 / MOBI / DOCX 这些格式**只有解析那一步不一样**，
/// 拿到 [`Book`] 之后全走这里：分页懒算、改设置才重排、同一页同尺寸重复要
/// 直接返回缓存副本。
///
/// ```ignore
/// let book = parse_whatever(path)?;          // ← 各格式自己写
/// let doc = ReflowDoc::new(book, Cfg::default());
/// ```
pub struct ReflowDoc {
    book: Book,
    cfg: Cfg,
    pag: RefCell<Option<Layout>>,
    /// 最近一次渲染：(页号, w, h, 设置, 灰度)。
    last: RefCell<Option<(usize, usize, usize, Cfg, Vec<u8>)>>,
}

impl ReflowDoc {
    pub fn new(book: Book, cfg: Cfg) -> ReflowDoc {
        ReflowDoc {
            book,
            cfg: cfg.sanitized(),
            pag: RefCell::new(None),
            last: RefCell::new(None),
        }
    }

    pub fn book(&self) -> &Book {
        &self.book
    }

    /// 分页结果不在缓存里（或设置变了）就重排一次。
    fn ensure(&self) {
        let stale = self
            .pag
            .borrow()
            .as_ref()
            .map_or(true, |l| l.cfg != self.cfg);
        if !stale {
            return;
        }
        let fonts = fonts();
        let mut guard = fonts.borrow_mut();
        let layout = paginate(&self.book, guard.as_mut(), self.cfg);
        drop(guard);
        *self.pag.borrow_mut() = Some(layout);
    }

    pub fn page_count(&self) -> usize {
        self.ensure();
        self.pag.borrow().as_ref().map_or(0, |l| l.pages.len())
    }

    /// 重排格式的页面比例固定是 `PAGE_W / PAGE_H`。
    pub fn aspect(&self, _idx: usize) -> f32 {
        PAGE_W / PAGE_H
    }

    pub fn render(&self, idx: usize, w: usize, h: usize) -> Result<Vec<u8>, String> {
        if w == 0 || h == 0 {
            return Err("render size must be positive".into());
        }
        self.ensure();
        let n = self.pag.borrow().as_ref().map_or(0, |l| l.pages.len());
        if idx >= n {
            return Err(format!("page {idx} out of range (0..{n})"));
        }
        if let Some((i, lw, lh, lc, gray)) = self.last.borrow().as_ref() {
            if *i == idx && *lw == w && *lh == h && *lc == self.cfg {
                return Ok(gray.clone());
            }
        }
        let page = {
            let lay = self.pag.borrow();
            lay.as_ref().map(|l| l.pages[idx].clone()).unwrap_or_default()
        };
        let gray = draw_page(&page, w, h);
        *self.last.borrow_mut() = Some((idx, w, h, self.cfg, gray.clone()));
        Ok(gray)
    }

    pub fn outline(&self) -> Vec<crate::doc::Toc> {
        self.ensure();
        let lay = self.pag.borrow();
        let Some(lay) = lay.as_ref() else {
            return Vec::new();
        };
        self.book
            .toc
            .iter()
            .filter_map(|e| {
                let ci = *self.book.path_index.get(&e.path)?;
                let start = lay.chapter_page.get(ci).copied().unwrap_or(0);
                // 锚点指向标题时用标题所在页，否则退到章首页
                let page = if e.anchor.is_empty() {
                    start
                } else {
                    lay.anchors
                        .get(&(ci, e.anchor.clone()))
                        .copied()
                        .unwrap_or(start)
                };
                Some(crate::doc::Toc {
                    depth: e.level.clamp(1, 3),
                    title: e.title.clone(),
                    page,
                })
            })
            .collect()
    }

    pub fn find(&self, needle: &str, limit: usize) -> Vec<(usize, String)> {
        self.ensure();
        let lay = self.pag.borrow();
        lay.as_ref()
            .map_or_else(Vec::new, |l| search(l, needle, limit))
    }

    /// 第 `idx` 页属于哪一章。
    pub fn chapter_title(&self, idx: usize) -> Option<String> {
        let toc = self.outline();
        let mut best: Option<(usize, String)> = None;
        for t in toc {
            if t.depth == 1 && t.page <= idx && best.as_ref().map_or(true, |(p, _)| t.page >= *p) {
                best = Some((t.page, t.title));
            }
        }
        best.map(|(_, t)| t)
    }

    pub fn current_layout(&self) -> crate::doc::LayoutCfg {
        crate::doc::LayoutCfg::from_reflow(self.cfg)
    }

    /// 换排版参数；参数没变返回 `false`（不用重排）。
    pub fn apply_layout(&mut self, cfg: crate::doc::LayoutCfg) -> bool {
        if crate::doc::LayoutCfg::from_reflow(self.cfg) == cfg {
            return false;
        }
        self.cfg = cfg.to_reflow().sanitized();
        // 分页缓存跟着设置失效，下次 page_count 自然重排
        *self.pag.borrow_mut() = None;
        *self.last.borrow_mut() = None;
        true
    }

    /// 当前分页结果的可读文本（测试/调试用）。
    pub fn page_text(&self, idx: usize) -> Option<String> {
        self.ensure();
        let lay = self.pag.borrow();
        lay.as_ref().and_then(|l| l.pages.get(idx).map(Page::text))
    }
}

/// 测试用：1×1 灰度 PNG（像素 128）的原始字节。`PIL` 生成后抄的十六进制。
#[cfg(test)]
pub(crate) fn test_png_1x1() -> Vec<u8> {
    hex_bytes(PNG_1X1)
}

/// 测试用：1×1 灰度 JPEG（质量 75，像素约 128）的原始字节。
#[cfg(test)]
pub(crate) fn test_jpeg_1x1() -> Vec<u8> {
    hex_bytes(JPEG_1X1)
}

#[cfg(test)]
const PNG_1X1: &str = "89504e470d0a1a0a0000000d4948445200000001000000010800000000\
3a7e9b550000000a49444154789c636800000082008177cd72b60000000049454e44ae426082";

#[cfg(test)]
const JPEG_1X1: &str = concat!(
    "ffd8ffe000104a46494600010100000100010000ffdb004300080606070605080707070909080a0c140d0c0b0b0c",
    "1912130f141d1a1f1e1d1a1c1c20242e2720222c231c1c2837292c30313434341f27393d38323c2e333432ffc0000b",
    "080001000101011100ffc4001f0000010501010101010100000000000000000102030405060708090a0bffc400b5",
    "100002010303020403050504040000017d01020300041105122131410613516107227114328191a1082342b1c115",
    "52d1f02433627282090a161718191a25262728292a3435363738393a434445464748494a535455565758595a636465",
    "666768696a737475767778797a838485868788898a92939495969798999aa2a3a4a5a6a7a8a9aab2b3b4b5b6b7b8b9",
    "bac2c3c4c5c6c7c8c9cad2d3d4d5d6d7d8d9dae1e2e3e4e5e6e7e8e9eaf1f2f3f4f5f6f7f8f9faffda0008010100",
    "003f002bffd9"
);

#[cfg(test)]
fn hex_bytes(s: &str) -> Vec<u8> {
    assert!(s.len() % 2 == 0, "hex 长度必须是偶数");
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn book_of(paras: &[&str]) -> Book {
        Book {
            chapters: vec![Chapter {
                blocks: paras.iter().map(|p| Block::body(*p)).collect(),
            }],
            ..Default::default()
        }
    }

    fn img_of(w: usize, h: usize, gray: u8) -> Arc<Img> {
        Arc::new(Img {
            w,
            h,
            px: vec![gray; w * h],
        })
    }

    #[test]
    fn empty_book_has_no_pages() {
        let lay = paginate(&Book::default(), None, Cfg::default());
        assert_eq!(lay.pages.len(), 0);
    }

    #[test]
    fn long_text_makes_more_than_one_page() {
        let big: String = std::iter::repeat("这是很长的一段中文正文。").take(400).collect();
        let lay = paginate(&book_of(&[&big]), None, Cfg::default());
        assert!(lay.pages.len() > 3, "got {}", lay.pages.len());
    }

    #[test]
    fn bigger_font_makes_more_pages() {
        let big: String = std::iter::repeat("The quick brown fox jumps over the lazy dog. ")
            .take(300)
            .collect();
        let b = book_of(&[&big]);
        let small = paginate(&b, None, Cfg { font_size: 18.0, ..Cfg::default() });
        let large = paginate(&b, None, Cfg { font_size: 36.0, ..Cfg::default() });
        assert!(large.pages.len() > small.pages.len());
    }

    #[test]
    fn draw_page_is_deterministic_and_right_size() {
        let lay = paginate(&book_of(&["hello world"]), None, Cfg::default());
        let a = draw_page(&lay.pages[0], 400, 560);
        let b = draw_page(&lay.pages[0], 400, 560);
        assert_eq!(a.len(), 400 * 560);
        assert_eq!(a, b);
        // 有墨
        assert!(a.iter().any(|&v| v < 200));
        // 四周是白（居中留白）
        assert_eq!(a[0], 255);
        assert_eq!(a[a.len() - 1], 255);
    }

    #[test]
    fn cfg_is_sanitized() {
        let c = Cfg { font_size: 500.0, line_spacing: 9.0, margin: 9999 }.sanitized();
        assert!(c.font_size <= 72.0);
        assert!(c.line_spacing <= 3.0);
        assert!(c.margin < 500);
    }

    #[test]
    fn chapter_starts_on_new_page() {
        let b = Book {
            chapters: vec![
                Chapter { blocks: vec![Block::body("one")] },
                Chapter { blocks: vec![Block::body("two")] },
            ],
            ..Default::default()
        };
        let lay = paginate(&b, None, Cfg::default());
        assert_eq!(lay.chapter_page, vec![0, 1]);
        assert_eq!(lay.pages.len(), 2);
    }

    #[test]
    fn plain_text_split_keeps_paragraphs_and_headings() {
        let blocks = blocks_from_plain("# Title\n\nfirst para\nstill first\n\nsecond\n", 100);
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].kind, Kind::H1);
        assert_eq!(blocks[0].text, "Title");
        assert_eq!(blocks[1].kind, Kind::Body);
        assert!(blocks[1].text.contains("first para"));
    }

    #[test]
    fn search_finds_page_and_normalizes() {
        let b = book_of(&["alpha beta gamma DELTA epsilon"]);
        let lay = paginate(&b, None, Cfg::default());
        let hits = search(&lay, "delta", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, 0);
        assert!(hits[0].1.contains("DELTA"));
        assert!(search(&lay, "zzz", 10).is_empty());
    }

    /// 回归：`search` 曾把「规范化后的偏移」当「原文字节偏移」用，
    /// 中文一搜就在非 char boundary 上切片 panic。
    #[test]
    fn search_snippet_on_chinese_does_not_panic() {
        let b = book_of(&["这是一段中文，用来验证搜索切片段不会在字节边界上出错。"]);
        let lay = paginate(&b, None, Cfg::default());
        let hits = search(&lay, "中文", 10);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].1.contains("中文"), "snippet = {:?}", hits[0].1);
        // 去掉标点也能命中（normalize 的语义）
        let hits2 = search(&lay, "这是一段中文用来验证", 10);
        assert_eq!(hits2.len(), 1);
        // 越界的 needle 不能 panic
        assert!(search(&lay, "", 10).is_empty());
        assert!(search(&lay, "不存在的词", 10).is_empty());
    }

    #[test]
    fn find_page_ignores_punctuation() {
        let b = book_of(&["你好，世界。这是测试。"]);
        let lay = paginate(&b, None, Cfg::default());
        assert_eq!(find_page(&lay, "你好世界"), Some(0));
    }

    #[test]
    fn approx_pages_grows_with_text() {
        let small = book_of(&["x"]);
        let big = book_of(&[&"y".repeat(9000)]);
        assert!(big.approx_pages() > small.approx_pages());
    }

    // ---------------- 图片 ----------------

    #[test]
    fn decode_one_by_one_png_and_jpeg() {
        let png = decode_image(&test_png_1x1()).expect("解 1×1 PNG");
        assert_eq!((png.w, png.h), (1, 1));
        assert_eq!(png.px.len(), 1);
        assert_eq!(png.px[0], 128, "PNG 无损，灰度必须原样");

        let jpg = decode_image(&test_jpeg_1x1()).expect("解 1×1 JPEG");
        assert_eq!((jpg.w, jpg.h), (1, 1), "JPEG 尺寸要从 SOF 读出来");
        assert_eq!(jpg.px.len(), 1);
        // JPEG 有损：1×1 的 DC 也可能有 ±几的误差
        assert!(
            (jpg.px[0] as i32 - 128).abs() <= 8,
            "jpg gray = {}",
            jpg.px[0]
        );

        // 不是图片 / 空 / GIF 都只是 Err，不能 panic
        assert!(decode_image(b"not an image").is_err());
        assert!(decode_image(&[]).is_err());
        assert!(decode_image(b"GIF89a\x01\x00\x01\x00").is_err());
    }

    #[test]
    fn image_block_is_atomic_and_moves_to_next_page() {
        // 文字先把第一页填到只剩几十个单位：图片（最高半页 = 700）只能整块换页
        let text: String = std::iter::repeat('中').take(33 * 30).collect();
        let b = Book {
            chapters: vec![Chapter {
                blocks: vec![
                    Block::body(text),
                    Block::image("p.png", img_of(800, 800, 64)),
                ],
            }],
            ..Default::default()
        };
        let lay = paginate(&b, None, Cfg::default());
        assert!(lay.pages.len() >= 2, "got {}", lay.pages.len());
        let pi = lay
            .pages
            .iter()
            .position(|p| !p.imgs.is_empty())
            .expect("应该有一页带图");
        assert!(pi > 0, "放不下必须换页，不能挤进第一页");
        let ib = &lay.pages[pi].imgs[0];
        // 整块在一页里：顶边不越过上边距、底边不越过下边距
        assert!(ib.y >= 80.0 - 0.01, "y = {}", ib.y);
        assert!(ib.y + ib.h <= 1320.0 + 0.01, "越过下边距：{}", ib.y + ib.h);
        // 方图按内容宽 840 会超过半页，所以高度被夹到 700，宽度跟着缩到 700
        assert!((ib.h - 700.0).abs() < 0.01, "h = {}", ib.h);
        assert!((ib.w - 700.0).abs() < 0.01, "w = {}", ib.w);
        // 只出现一次，没有任何一页放半张
        assert_eq!(lay.pages.iter().map(|p| p.imgs.len()).sum::<usize>(), 1);
    }

    #[test]
    fn draw_page_draws_image_and_quantizes_to_16_levels() {
        let b = Book {
            chapters: vec![Chapter {
                blocks: vec![Block::image("g.png", img_of(100, 100, 64))],
            }],
            ..Default::default()
        };
        let lay = paginate(&b, None, Cfg::default());
        let page = &lay.pages[0];
        let ib = &page.imgs[0];
        let (w, h) = (400usize, 560usize);
        let gray = draw_page(page, w, h);
        assert_eq!(gray.len(), w * h);
        // 图中央：源灰度 64 → 最近一档 = round(64*15/255)=4 → 4*17=68；
        // 经 565 面板 6 bit 绿通道往返会差 1，所以给 ±1 的容差。
        let scale = w as f32 / PAGE_W;
        let cx = ((ib.x + ib.w / 2.0) * scale) as usize;
        let cy = ((ib.y + ib.h / 2.0) * scale) as usize;
        let v = gray[cy * w + cx] as i32;
        assert!((v - 68).abs() <= 1, "图中央灰度 = {v}");
        // 图片以外还是白的
        assert_eq!(gray[0], 255);
    }

    #[test]
    fn img_cache_evicts_least_recently_used() {
        let mut cache = ImgCache::new(3);
        for i in 0..3 {
            cache.insert(format!("p{i}"), img_of(1, 1, 0));
        }
        assert_eq!(cache.len(), 3);
        // 碰一下 p0：它从「最久未用」变成最近用，被淘汰的应该是 p1
        assert!(cache.get("p0").is_some());
        cache.insert("p3".to_string(), img_of(1, 1, 0));
        assert_eq!(cache.len(), 3, "超过上限必须淘汰");
        assert!(cache.get("p1").is_none(), "p1 是最久未用的");
        for k in ["p0", "p2", "p3"] {
            assert!(cache.get(k).is_some(), "{k} 不该被淘汰");
        }
        // 没插过的 key 只是 None
        assert!(cache.get("nope").is_none());
        assert!(!cache.is_empty());
    }
}
