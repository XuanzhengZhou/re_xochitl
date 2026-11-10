//! AI 流式回答面板（C8）：黑头 + 问题 + 增量回答 + 公式图 + 滚动 + 底部动作条。
//!
//! # 数据从哪来
//!
//! `POST /api/v1/agent/ask`（JSON body）回 SSE，三种事件（见 `docs/CLOUD.md`）：
//!
//! ```text
//! event: token
//! data: {"t":"增量文本"}
//!
//! event: done
//! data: {"session":"…","usage":{"in":1,"out":1}}
//!
//! event: error
//! data: {"message":"…"}
//! ```
//!
//! 走 [`crate::net::open_stream`]：它带好 token / SOCKS5 / 响应头解析，返回的
//! `BufReader` 停在响应头之后，这里按行读 SSE。服务器是流式的，主线程绝不能
//! 碰网络——[`Ai::open_with`] 只起线程，事件经 `mpsc` 回到主线程，主循环每帧
//! 调 [`Ai::tick`] 用 `try_recv` 收（**不阻塞**）。
//!
//! # 公式
//!
//! 回答里成对的 `$…$` 由 [`split_math`] 切成 [`Seg::Math`]，占一个"公式块"；
//! 后台线程 `GET /api/v1/tex?f=<urlencode>&scale=2` 拿 8 位灰度 PNG，
//! [`png_to_gray`] 解成灰度缓冲后缓存（同一道公式只取一次）。取图失败的位置
//! 画 `[formula]`（英文，e-ink 上没有红色，用最深的墨色）。
//!
//! # 几何
//!
//! 整屏 sheet（几何常量见文件头常量）：黑头 48、问题区 48..130、正文
//! 130..1700、底部动作条 1700..1872。正文超出一屏可拖动滚动，步长照
//! `panels::toc::DRAG_STEP`。字号 32 / 行高 46 / 左右边距 64。
//!
//! 绘制顺序是「正文 → 问题区抹白 → 黑头 → 底条」，这样滚动到边界时正文会被
//! 后画的两块盖住，不会串到标题或按钮上。
//!
//! # 内联模式（D1：框选问 AI 用）
//!
//! [`Ai::set_inline`] 给一个固定矩形以后，[`Ai::paint_inline`] 只在这个矩形里
//! 画**同一份** `body` / `imgs` / 滚动状态（裁切由"先画正文、再拿头/底/边框
//! 盖回去"实现，和整屏那份是同一套做法）。整屏的 [`Ai::paint`] 一个字节没改，
//! 17 个老单测照旧。
//!
//! 内联卡片固定在上半部，下半部留给笔：卡片显示/流式增长期间笔完全不受影响
//! （路由策略在 `panels::selectai::route`）。

use std::collections::{HashMap, HashSet};
use std::io::{BufRead, Read};
use std::net::{Shutdown, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use crate::fb::{gray565, Fb};
use crate::net;
use crate::ui::text::Text;
use crate::ui::widgets;

// ---------------------------------------------------------------- 几何

/// 面板宽（设备屏宽，整屏 sheet）。
pub const W: i32 = 1404;
/// 面板高（设备屏高）。
pub const H: i32 = 1872;
/// 黑头高。
pub const HDR_H: i32 = 48;
/// 左右边距。
pub const MARGIN: i32 = 64;
/// 正文区顶。
pub const BODY_TOP: i32 = 130;
/// 正文区底（= 底部动作条顶边）。
pub const BODY_BOT: i32 = 1700;
/// 正文行高。
pub const LINE_H: i32 = 46;
/// 正文字号。
pub const FONT: f32 = 32.0;
/// 手指/笔上下拖多少像素算滚一档（同 `panels::toc::DRAG_STEP`）。
///
/// `Act::Scroll(dy)` 的单位是**像素**，直接加到 `scroll` 上；想照 toc 的手感
/// 把"拖 80 px 走一行"搬过来，主循环里这样换算：
/// `ai.apply(Act::Scroll((drag_px as f32 / ai::DRAG_STEP * ai::LINE_H as f32) as i32))`
pub const DRAG_STEP: f32 = 80.0;

/// 黑头标题基线。
const HDR_BASE: i32 = 33;
/// 黑头标题字号。
const FONT_HDR: f32 = 28.0;
/// 问题区第一行基线。
const Q_TOP: i32 = 74;
/// 问题行间距。
const Q_LINE: i32 = 34;
/// 问题字号（灰字，最多两行）。
const FONT_Q: f32 = 26.0;
/// 底部按钮。
const BTN_W: i32 = 280;
const BTN_H: i32 = 92;
const BTN_TOP: i32 = 1736;
const BTN_BORDER: i32 = 2;
/// 按钮文字字号。
const FONT_BTN: f32 = 32.0;
/// 用量小字字号。
const FONT_USAGE: f32 = 20.0;

/// 正文可用宽。
const WRAP_W: i32 = W - 2 * MARGIN;
/// **复杂**公式图的高度上限 = 多少倍**正文行高**（`\frac` / `\int` 这类高结构）。
pub const MATH_H_FACTOR: f32 = 1.6;
/// **单符号/单数字**公式图的高度上限 = 多少倍**正文字号**。
///
/// 真机实测：模型爱把 `$0$`、`$[0,1]$` 单独换行，画到 1.6×（55 px）时比正文
/// （31 px）大一倍，看着像标题。单符号收到 32×1.15 ≈ 37 px 就和正文一个量级。
pub const MATH_H_FACTOR_SIMPLE: f32 = 1.15;
/// 公式图的最小显示高度（正文行高的一半）：太小的放大到看得清。
pub const MATH_H_FLOOR: i32 = LINE_H / 2;
/// 行内公式在行里占的宽度（图宽 + 左右各留一点空）。
const INLINE_MATH_GAP: f32 = 8.0;
/// 公式源码里那些"高结构"——有它们就不可能一行内放得下。
const MATH_TALL_CMDS: [&str; 12] = [
    "\\frac", "\\dfrac", "\\tfrac", "\\int", "\\sum", "\\prod", "\\sqrt", "\\lim",
    "\\begin", "\\matrix", "\\over", "\\binom",
];

/// 这个公式是不是"单符号/单数字"（决定高度上限 1.15× 还是 1.6×）。
///
/// 判据：**没有高结构**，且去掉括号/花括号/下标/标点后"可见字符" ≤ 2 个。
/// `$0$`、`$[0,1]$`、`$x_i$` 都算；`$e^{-1} \approx 0.368$` 不算。
pub fn math_is_simple(src: &str) -> bool {
    let s = src.trim();
    if MATH_TALL_CMDS.iter().any(|c| s.contains(c)) {
        return false;
    }
    let mut n = 0;
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        match c {
            // `\le`、`\approx` 这种命令算"一个符号"
            '\\' => {
                while let Some(&c2) = it.peek() {
                    if c2.is_ascii_alphabetic() {
                        it.next();
                    } else {
                        break;
                    }
                }
                n += 1;
            }
            // 结构/标点不算
            '{' | '}' | '[' | ']' | '(' | ')' | ',' | '.' | ' ' | '_' | '^' | '\\' => {}
            _ => n += 1,
        }
    }
    n <= 2
}

/// 公式图的显示高度上限（按源码判断简单/复杂）。
pub fn math_cap_h(src: &str) -> i32 {
    let cap = if math_is_simple(src) {
        // 单符号：按**字号**算（32 × 1.15 ≈ 37）
        (FONT * MATH_H_FACTOR_SIMPLE).round() as i32
    } else {
        // 高结构：按**行高**算（46 × 1.6 ≈ 74）
        (LINE_H as f32 * MATH_H_FACTOR).round() as i32
    };
    cap.max(MATH_H_FLOOR)
}
/// 公式块上下留白。
const MATH_PAD: i32 = 6;
/// 一屏正文区高（算滚动上限用）。
const VIEW_H: i32 = BODY_BOT - BODY_TOP;

/// 流式光标闪烁周期。
const BLINK: Duration = Duration::from_millis(500);
/// `agent/ask` 整轮上限（服务器侧也是 180 s）。
const ASK_TIMEOUT: Duration = Duration::from_secs(180);
/// 单张公式图的上限（TeX 要起 pdflatex，慢一点）。
const TEX_TIMEOUT: Duration = Duration::from_secs(30);
/// 同时最多取几张公式图。
const TEX_PARALLEL: usize = 8;
/// 公式图缓存条数上限（每张按显示尺寸算几百 KB，几十张就是几 MB）。
const IMG_CACHE_MAX: usize = 32;

/// 文字色调：正文、灰字、细分线。面板只有 16 级灰，没有"红字"。
const BG: u8 = 255;
const INK: u8 = 20;
const MUTED: u8 = 130;
const RULE: u8 = 205;
/// 滚动条轨道 / 滑块。
const TRACK: u8 = 238;
const THUMB: u8 = 120;
const HDR_BG: u8 = 0;
const HDR_INK: u8 = 255;

// ---- 内联模式（D1 框选问 AI）：卡片里的几何 ----

/// 卡片头（标题 + Close）。常量只有一份，命中区和绘制共用
/// `panels::selectai::CARD_HDR`，免得两边各写一个数又对不上。
pub const INLINE_HDR: i32 = crate::panels::selectai::CARD_HDR;
/// 卡片左右内边距。
pub const INLINE_PAD: i32 = 28;
/// 卡片底边那条明显的分隔线（画在卡片内部底边）。
pub const INLINE_SEP: i32 = 4;
/// 发送后这么久还没有第一个字，卡片上补一行英文提示（**不**中断请求）。
pub const SLOW_HINT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------- 类型

/// 提问方式。服务器按它拼提示词（`cloud/src/agent_api.rs::build_prompt`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// 实时模式：短答、口语。
    Live,
    /// 框选提问。
    Select,
    /// 整页批改。
    Review,
    /// 整页整理。
    Organize,
}

impl Mode {
    /// 黑头标题。
    pub fn title(self) -> &'static str {
        match self {
            Mode::Live => "Live Note",
            Mode::Select => "Ask AI",
            Mode::Review => "Review",
            Mode::Organize => "Organize",
        }
    }

    /// 请求体里的 mode 串。
    pub fn wire(self) -> &'static str {
        match self {
            Mode::Live => "live",
            Mode::Select => "select",
            Mode::Review => "review",
            Mode::Organize => "organize",
        }
    }

    /// 出图入口解析用：`live` / `select` / `review` / `organize` → 模式。
    pub fn from_wire(s: &str) -> Mode {
        match s {
            "select" => Mode::Select,
            "review" => Mode::Review,
            "organize" => Mode::Organize,
            _ => Mode::Live,
        }
    }
}

/// 面板上的动作。`Scroll` 不占命中区（滚动是拖动算出来的）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Close,
    Stop,
    Retry,
    /// 正数往后看（往下滚），负数往前。
    Scroll(i32),
}

/// 回答切出来的两段：普通文字 / 公式原文（不含 `$`）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Seg {
    Text(String),
    Math(String),
}

/// 解码后的一张灰度图（255 = 白）。
#[derive(Clone, Debug)]
pub struct Img {
    pub w: usize,
    pub h: usize,
    pub gray: Vec<u8>,
}

impl Img {
    fn new(w: usize, h: usize, gray: Vec<u8>) -> Img {
        Img { w, h, gray }
    }
}

/// 后台线程 → 主线程的事件。
enum Ev {
    /// 连接建好了：留一份 `TcpStream`，`Stop` 时 `shutdown` 能把阻塞的读唤醒。
    Opened(TcpStream),
    Chunk(String),
    Done { usage: Option<(u64, u64)>, published: bool },
    Fail(String),
    /// 公式图回来（`None` = 取图/解码失败）。
    Img(String, Option<Img>),
}

/// 一行里的一个片段：文字 / 行内公式。
///
/// 模型很爱把 `$0$`、`$[0,1]$` 单独换行（真机实测 10 行里 6 行是公式），
/// 所以公式分两种画法：
/// * **行内**：和文字排在同一行、基线对齐（行高跟着公式长高）；
/// * **块级**：只有"整行只有一个公式"（没并进任何文字）时才升级成
///   `Item::Math`，居中 + 上下留白（`\frac` 单独成行时就是这种）。
#[derive(Clone, Debug)]
enum Run {
    Text(String),
    Math { key: String, dw: i32, dh: i32 },
}

#[derive(Clone, Debug)]
enum Item {
    /// 一行（可含行内公式）
    Line(Vec<Run>),
    /// 独立成块的公式
    Math { key: String, dw: i32, dh: i32 },
}

impl Item {
    fn height(&self) -> i32 {
        match self {
            Item::Line(runs) => {
                // 行内公式比字高时，这一行跟着长高（不然后一行会压上去）
                let mut h = LINE_H;
                for r in runs {
                    if let Run::Math { dh, .. } = r {
                        h = h.max(*dh + 12);
                    }
                }
                h
            }
            Item::Math { dh, .. } => {
                if *dh > 0 {
                    dh + MATH_PAD * 2
                } else {
                    // 还没取到图：先占一行，等图回来再长高
                    LINE_H
                }
            }
        }
    }
}

/// 行内公式画在行里的纵向位置：**底边落在基线下 6 px**（和文字下伸部对齐）。
fn inline_math_top(base: i32, dh: i32) -> i32 {
    base - dh + 6
}

// ---------------------------------------------------------------- 面板

/// AI 回答面板。字段名照 C8 的公开 API 约定，别的任务直接读。
pub struct Ai {
    pub open: bool,
    pub mode: Mode,
    pub question: String,
    /// 累积的回答原文（含 `$…$`，流式追加）。
    pub answer: String,
    pub busy: bool,
    pub error: Option<String>,
    pub scroll: i32,

    /// 重新提问要用的请求参数（`Retry`）。
    doc: String,
    page: usize,
    image_blob: Option<String>,
    /// `done` 事件里的用量，底部动作条左下角显示。
    usage: Option<(u64, u64)>,
    /// 发布用：`(回答 PDF 的 uuid, 父回答 uuid, 第几轮)`。`None` = 不落 PDF。
    publish: Option<(String, String, u32)>,
    /// 这一轮服务器已经发布成 PDF（`done` 里带 `pdf`）。
    published: bool,
    /// 这一轮失败了，原因待主循环取一次（用来把 Job status 里那条标成 `failed`）。
    failed_msg: Option<String>,

    // ---- 内联模式（D1）：只加字段，不动整屏那条路 ----
    /// 内联卡片的矩形（`None` = 老的全屏 sheet）。
    inline: Option<(i32, i32, i32, i32)>,
    /// 内联模式的折行宽度（`None` 时就是 `WRAP_W`）。
    wrap_w: i32,
    /// 框选提问的附加字段：`(rect, crop_blob)`。`None` = 老请求体（不带这两个键）。
    sel: Option<([i32; 4], Option<String>)>,
    /// 这一轮开始的时刻（"Thinking…" / 30 s 慢提示）。
    started: Instant,

    /// 后台线程的收货端（`tick` 里 drain）。
    rx: Option<Receiver<Ev>>,
    tx: Option<Sender<Ev>>,
    /// 当前连接的克隆，`Stop` 时用来 `shutdown`。
    sock: Option<TcpStream>,

    /// 公式缓存 / 取图失败 / 正在取。
    imgs: HashMap<String, Img>,
    failed: HashSet<String>,
    pending: HashSet<String>,

    /// 折好的正文块（`rebuild` 后有效）。
    body: Vec<Item>,
    body_h: i32,
    layout_dirty: bool,
    /// 这一帧要不要重画（`paint` 清掉）。
    dirty: bool,
    /// 视口贴在底部（流式追加时跟着走）。
    follow: bool,
    /// 光标这一拍亮不亮。
    cursor_on: bool,
    blink: Instant,

    /// 面板自己持有一份字体（`paint` 只收 `fb`，见 API 约定）。懒加载：
    /// `Ai::new` 不碰字体，第一次 `paint` 才 `Text::load`。
    text: Option<Text>,
    text_tried: bool,
}

impl Ai {
    pub fn new() -> Ai {
        Ai {
            open: false,
            mode: Mode::Live,
            question: String::new(),
            answer: String::new(),
            busy: false,
            error: None,
            scroll: 0,
            doc: String::new(),
            page: 0,
            image_blob: None,
            usage: None,
            publish: None,
            published: false,
            failed_msg: None,
            inline: None,
            wrap_w: WRAP_W,
            sel: None,
            started: Instant::now(),
            rx: None,
            tx: None,
            sock: None,
            imgs: HashMap::new(),
            failed: HashSet::new(),
            pending: HashSet::new(),
            body: Vec::new(),
            body_h: 0,
            layout_dirty: true,
            dirty: true,
            follow: true,
            cursor_on: true,
            blink: Instant::now(),
            text: None,
            text_tried: false,
        }
    }

    /// 打开面板并开始一轮提问。**不阻塞**：网络在后台线程里。
    pub fn open_with(
        &mut self,
        mode: Mode,
        question: &str,
        doc: &str,
        page: usize,
        image_blob: Option<String>,
    ) {
        self.mode = mode;
        self.question = question.to_string();
        self.doc = doc.to_string();
        self.page = page;
        self.image_blob = image_blob;
        self.start();
    }

    /// 这一轮要不要落成 PDF：`ai_doc` 是服务器发布用的回答文档 uuid，
    /// `parent` 是追问的上一份回答（空 = 从当前文档起一条新链）。
    pub fn set_publish(&mut self, ai_doc: String, parent: String, n: u32) {
        self.publish = Some((ai_doc, parent, n));
        self.published = false;
    }

    /// 这一轮失败的原因（取一次就清零）。主循环用它把 Job status 标成 `failed`。
    pub fn take_failed(&mut self) -> Option<String> {
        self.failed_msg.take()
    }

    /// 还有一轮在跑（卡片关掉之后也要继续 `tick`）。
    pub fn running(&self) -> bool {
        self.busy || self.rx.is_some()
    }

    /// 这一轮服务器是不是真的发布了 PDF（取一次就清零）。
    pub fn take_published(&mut self) -> bool {
        std::mem::take(&mut self.published)
    }

    // ---- 内联模式（D1 框选问 AI）----------------------------------------

    /// 切到内联模式：`Some(rect)` = 只在这个矩形里画；`None` = 回到整屏 sheet。
    ///
    /// 折行宽度跟着卡片宽度走（`rect.w - 2*INLINE_PAD`），所以同一份 `answer`
    /// 在两个模式下都能正确折行——`rebuild` 读的就是这个值。
    pub fn set_inline(&mut self, rect: Option<(i32, i32, i32, i32)>) {
        self.inline = rect;
        self.wrap_w = match rect {
            Some((_, _, w, _)) => (w - 2 * INLINE_PAD).max(120),
            None => WRAP_W,
        };
        self.layout_dirty = true;
        self.dirty = true;
    }

    /// 当前内联卡片矩形（`None` = 整屏）。
    pub fn inline_rect(&self) -> Option<(i32, i32, i32, i32)> {
        self.inline
    }

    /// 框选提问要额外发的两个字段。**必须在 [`Ai::open_with`] 之前调**
    ///（请求体在 `start()` 里一次性拼好）。
    pub fn set_sel(&mut self, rect: [i32; 4], crop_blob: Option<String>) {
        self.sel = Some((rect, crop_blob));
    }

    /// 卡片先立起来、正文空着（发送后立刻给反馈，别让用户盯着没反应的屏幕）。
    pub fn show_pending(&mut self) {
        self.open = true;
        self.busy = true;
        self.answer.clear();
        self.error = None;
        self.usage = None;
        self.scroll = 0;
        self.follow = true;
        self.cursor_on = true;
        self.blink = Instant::now();
        self.started = Instant::now();
        self.layout_dirty = true;
        self.dirty = true;
    }

    /// 卡片里的滚动（手指拖动，正数往下）。
    pub fn scroll_by(&mut self, dy: i32) {
        self.rebuild();
        let max = self.max_scroll();
        self.scroll = self.scroll.saturating_add(dy).clamp(0, max);
        self.follow = false;
        self.dirty = true;
    }

    /// 正文可滚多远（先把折行算好，`seed` 之后立刻问也有值）。
    pub fn max_scroll_pub(&mut self) -> i32 {
        self.rebuild();
        self.max_scroll()
    }

    /// 卡片内容**实际需要**的高度（表头 + 上下内边距 + 正文 + 底边线）。
    ///
    /// 调用方用 `selectai::card_height()` 夹到 `[CARD_MIN_H, CARD_H]` 再
    /// `set_inline()`——卡片跟着内容长高，页面在卡片周围一直看得见（BUG 1）。
    pub fn inline_needed_height(&mut self) -> i32 {
        self.rebuild();
        let body = if self.busy && self.answer.is_empty() {
            // "Thinking…" 那一行（慢提示出来时两行）
            if self.started.elapsed() >= SLOW_HINT {
                LINE_H * 2
            } else {
                LINE_H
            }
        } else {
            self.body_h
        };
        INLINE_HDR + INLINE_SEP + 2 * INLINE_PAD + body
    }

    /// 内联卡片上的命中：只有 `Close`（滚动是拖动算出来的，不占命中区）。
    pub fn inline_hit(&self, x: f32, y: f32) -> Option<Act> {
        let (cx, cy, cw, ch) = inline_close_rect(self.inline?);
        let (xi, yi) = (x as i32, y as i32);
        if xi >= cx && yi >= cy && xi < cx + cw && yi < cy + ch {
            Some(Act::Close)
        } else {
            None
        }
    }

    /// 后台线程每收到一块就往这塞。`tick` 会把 channel 里的事件 drain 到它；
    /// 也可以在主线程直接调（离线注入/单测）。
    pub fn push_chunk(&mut self, t: &str) {
        if t.is_empty() {
            return;
        }
        self.answer.push_str(t);
        self.layout_dirty = true;
        self.dirty = true;
    }

    /// 回答正常结束（收到 `done`）。
    pub fn finish(&mut self, usage: Option<(u64, u64)>) {
        self.busy = false;
        self.usage = usage;
        self.cursor_on = true;
        self.sock = None;
        self.layout_dirty = true;
        self.dirty = true;
    }

    /// 回答失败（`error` 事件、连不上、流断了）。
    pub fn fail(&mut self, msg: &str) {
        self.busy = false;
        self.failed_msg = Some(msg.to_string());
        self.error = Some(msg.to_string());
        self.cursor_on = true;
        self.sock = None;
        // 失败信息排在正文最后，滚动跟着走
        self.follow = true;
        self.layout_dirty = true;
        self.dirty = true;
    }

    /// 主循环每帧调一次：收后台事件、起公式取图、收滚动、判定重绘。
    pub fn tick(&mut self) {
        // 1) 把 channel 里的事件一次收干净（不阻塞）
        let mut evs: Vec<Ev> = Vec::new();
        if let Some(rx) = self.rx.as_ref() {
            while let Ok(ev) = rx.try_recv() {
                evs.push(ev);
                if evs.len() >= 1024 {
                    break;
                }
            }
        }
        for ev in evs {
            self.on_ev(ev);
        }
        // 2) 公式取图（有缓存/在飞/失败过的都跳过）
        self.fetch_missing_math();
        // 3) 收滚动
        self.rebuild();
        let max = self.max_scroll();
        self.scroll = if self.follow {
            max
        } else {
            self.scroll.clamp(0, max)
        };
        // 4) 光标闪烁
        if self.busy {
            if self.blink.elapsed() >= BLINK {
                self.blink = Instant::now();
                self.cursor_on = !self.cursor_on;
                self.dirty = true;
            }
        } else if !self.cursor_on {
            self.cursor_on = true;
        }
    }

    /// 命中区，顺序 = 绘制顺序（后画的在前）。`Scroll` 不在这里。
    pub fn layout(&self) -> Vec<(Act, (i32, i32, i32, i32))> {
        let mut v = Vec::with_capacity(2);
        // Close 按黑头顶边对齐，宽 160 + 右边距，好点一点
        v.push((Act::Close, (W - MARGIN - 160, 0, 160 + MARGIN, HDR_H)));
        let act = if self.busy { Act::Stop } else { Act::Retry };
        v.push((act, button_rect()));
        v
    }

    /// 手指落点 → 动作。
    pub fn hit(&self, x: f32, y: f32) -> Option<Act> {
        let (x, y) = (x as i32, y as i32);
        self.layout()
            .into_iter()
            .rev()
            .find(|(_, r)| x >= r.0 && y >= r.1 && x < r.0 + r.2 && y < r.1 + r.3)
            .map(|(a, _)| a)
    }

    /// 执行一个动作。`Close` / `Stop` / `Retry` / `Scroll` 都在这里落状态，
    /// 调用方只管把命中/拖动换算成 `Act`。
    pub fn apply(&mut self, act: Act) {
        match act {
            Act::Close => {
                // 正在跑的时候关卡片 **不等于取消**：题目已经发出去了，服务器
                // 那边会跑完并把回答发布成 PDF（设备 sync 一次就有）。这里只把
                // 界面收掉，流继续在后台收 —— `tick()` 由主循环无条件调用。
                if !self.busy {
                    self.stop();
                }
                self.open = false;
                self.dirty = true;
            }
            Act::Stop => self.stop(),
            Act::Retry => {
                if !self.busy {
                    self.start();
                }
            }
            Act::Scroll(dy) => {
                self.rebuild();
                let max = self.max_scroll();
                self.scroll = self.scroll.saturating_add(dy).clamp(0, max);
                self.follow = self.scroll >= max;
                self.dirty = true;
            }
        }
    }

    /// 这一帧要不要重画（`paint` 之后自动清掉）。
    pub fn needs_redraw(&self) -> bool {
        self.dirty
    }

    /// 画整屏。调用方保证 `open`；`fb` 会被整块刷白。
    pub fn paint(&mut self, fb: &mut Fb) {
        self.ensure_text();
        self.rebuild();
        fb.fill_rect(0, 0, W, H, gray565(BG));

        // 字体借出去画正文，最后还回来（`paint` 的签名里没有 Text 参数）
        let mut text = self.text.take();

        // ---------- 正文（可越界画，随后被问题区/黑头/底条盖住）
        let mut y = BODY_TOP - self.scroll;
        let mut cursor: Option<(i32, i32)> = Some((MARGIN, BODY_TOP + 33));
        for it in &self.body {
            let ih = it.height();
            // 视口外的块整块跳过：长回答（几千行）滚动时不能每帧都去光栅化
            // 屏幕外的字，否则一次重绘要几百毫秒。
            if y + ih < BODY_TOP || y > BODY_BOT {
                y += ih;
                continue;
            }
            match it {
                Item::Line(runs) => {
                    let base = y + ih - 13;
                    let empty = runs
                        .iter()
                        .all(|r| matches!(r, Run::Text(s) if s.is_empty()));
                    if !empty {
                        let end = draw_runs(
                            fb,
                            &mut text,
                            MARGIN,
                            base,
                            runs,
                            &self.imgs,
                            &self.failed,
                            None,
                        );
                        if base >= BODY_TOP + 4 && base <= BODY_BOT {
                            cursor = Some((end, base));
                        }
                    }
                }
                Item::Math { key, dw, dh } => {
                    if let Some(img) = self.imgs.get(key) {
                        let top = y + MATH_PAD;
                        blit_scaled(fb, MARGIN, top, *dw, *dh, img);
                        if top >= BODY_TOP && top + dh <= BODY_BOT {
                            // 公式是独立块：光标落到它下面一行
                            cursor = Some((MARGIN, top + dh + 27));
                        }
                    } else if self.failed.contains(key) {
                        // 取图失败：原位画英文占位（e-ink 无彩色，拿最深墨色）
                        let base = y + 33;
                        if let Some(t) = text.as_mut() {
                            t.draw(fb, MARGIN, base, FONT, "[formula]", INK);
                            if base >= BODY_TOP + 4 && base <= BODY_BOT {
                                cursor = Some((MARGIN + t.width(FONT, "[formula]") as i32, base));
                            }
                        }
                    }
                    // 还在取图：留白，不闪字
                }
            }
            y += ih;
        }

        // ---------- 流式光标（▍画成 7×34 的方块，比找字形可靠）
        if self.busy && self.cursor_on {
            if let Some((bx, base)) = cursor {
                if base >= BODY_TOP + 4 && base <= BODY_BOT {
                    fb.fill_rect(bx + 2, base - 30, 7, 34, gray565(INK));
                }
            }
        }

        // ---------- 滚动条（只有内容超出一屏才画）
        let max = self.max_scroll();
        if max > 0 {
            let track_x = W - 30;
            let track_y = BODY_TOP + 8;
            let track_h = VIEW_H - 16;
            fb.fill_rect(track_x, track_y, 6, track_h, gray565(TRACK));
            let thumb_h = ((VIEW_H as f32 / self.body_h as f32) * track_h as f32)
                .max(48.0)
                .min(track_h as f32) as i32;
            let pos = self.scroll as f32 / max as f32;
            let thumb_y = track_y + ((track_h - thumb_h) as f32 * pos) as i32;
            fb.fill_rect(track_x, thumb_y, 6, thumb_h, gray565(THUMB));
        }

        // ---------- 问题区（先抹白：滚动时正文会画到 130 以上，得盖掉）
        fb.fill_rect(0, HDR_H, W, BODY_TOP - HDR_H, gray565(BG));
        let mut qlines = wrap(&self.question, WRAP_W, FONT_Q);
        if qlines.len() > 2 {
            qlines.truncate(2);
            let last = qlines.remove(1);
            let cut = match text.as_mut() {
                Some(t) => t.ellipsize(FONT_Q, &last, WRAP_W),
                None => last,
            };
            qlines.push(cut);
        }
        if let Some(t) = text.as_mut() {
            for (i, line) in qlines.iter().enumerate() {
                t.draw(fb, MARGIN, Q_TOP + i as i32 * Q_LINE, FONT_Q, line, MUTED);
            }
        }

        // ---------- 黑头：标题按 mode 变，右边 Close
        fb.fill_rect(0, 0, W, HDR_H, gray565(HDR_BG));
        if let Some(t) = text.as_mut() {
            t.set_bold(true);
            t.draw(fb, MARGIN, HDR_BASE, FONT_HDR, self.mode.title(), HDR_INK);
            let cw = t.width_bold(FONT_HDR, "Close") as i32;
            t.draw(fb, W - MARGIN - cw, HDR_BASE, FONT_HDR, "Close", HDR_INK);
            t.set_bold(false);
        }

        // ---------- 底部动作条：白底盖住正文，顶边一条细线，中间一个按钮
        fb.fill_rect(0, BODY_BOT, W, H - BODY_BOT, gray565(BG));
        fb.fill_rect(0, BODY_BOT, W, 1, gray565(RULE));
        let (bx, by, bw, bh) = button_rect();
        fb.fill_rect(bx, by, bw, BTN_BORDER, gray565(INK));
        fb.fill_rect(bx, by + bh - BTN_BORDER, bw, BTN_BORDER, gray565(INK));
        fb.fill_rect(bx, by, BTN_BORDER, bh, gray565(INK));
        fb.fill_rect(bx + bw - BTN_BORDER, by, BTN_BORDER, bh, gray565(INK));
        let label = if self.busy { "Stop" } else { "Retry" };
        if let Some(t) = text.as_mut() {
            let lw = t.width(FONT_BTN, label) as i32;
            t.draw(
                fb,
                bx + (bw - lw) / 2,
                by + bh / 2 + 11,
                FONT_BTN,
                label,
                INK,
            );
            if let Some((i, o)) = self.usage {
                if !self.busy {
                    t.draw(
                        fb,
                        MARGIN,
                        by + 52,
                        FONT_USAGE,
                        &format!("{i} in / {o} out"),
                        MUTED,
                    );
                }
            }
        }

        self.text = text;
        self.dirty = false;
    }

    /// 内联卡片：只画在 `set_inline` 给的矩形里（D1 框选问 AI）。
    ///
    /// 构图和整屏那份一模一样：**先画正文，再拿头部/底边/边框盖回去**，
    /// 所以越界的行不会串到标题或卡片外面。卡片外面一个像素都不动，
    /// 下半部（笔在写的地方）完全不受影响。
    ///
    /// 卡片内容：头（`Ask AI` + `Close` + 2px 分隔线）/ 正文 / 右侧滚动条 /
    /// 底边一条 4px 黑色分隔线。
    pub fn paint_inline(&mut self, fb: &mut Fb) {
        let Some((x, y, w, h)) = self.inline else { return };
        self.ensure_text();
        self.rebuild();
        let mut text = self.text.take();

        let body_top = y + INLINE_HDR;
        let body_bot = y + h - INLINE_SEP;
        let tx = x + INLINE_PAD;

        // 底
        fb.fill_rect(x, y, w, h, gray565(BG));

        // ---------- 正文（**严格裁在卡片里**，见 BUG 4）
        //
        // 三条裁法，缺一不可：
        // 1. 整块在视口外的直接跳过（原来的做法）；
        // 2. 公式图按行裁到 `body_top..body_bot`（图最高 212 px，跨边界时会
        //    画到卡片上方 y≈-66，把页面顶部和 `Live Note` 按钮盖掉）；
        // 3. 文字行只画**整行都在视口里**的（`Text::draw` 没法按像素裁，
        //    基线贴底边的那行会往下溢 30 多 px）。
        let mut yy = body_top + INLINE_PAD - self.scroll;
        let mut cursor: Option<(i32, i32)> = Some((tx, body_top + INLINE_PAD + 33));
        for it in &self.body {
            let ih = it.height();
            if yy + ih < body_top || yy > body_bot {
                yy += ih;
                continue;
            }
            match it {
                Item::Line(runs) => {
                    let base = yy + ih - 13;
                    // 整行（含行内公式）都得在正文区里，不然会画到卡片外
                    if yy >= body_top && yy + ih <= body_bot {
                        let empty = runs
                            .iter()
                            .all(|r| matches!(r, Run::Text(s) if s.is_empty()));
                        if !empty {
                            let end = draw_runs(
                                fb,
                                &mut text,
                                tx,
                                base,
                                runs,
                                &self.imgs,
                                &self.failed,
                                Some((body_top, body_bot)),
                            );
                            cursor = Some((end, base));
                        }
                    }
                }
                Item::Math { key, dw, dh } => {
                    if let Some(img) = self.imgs.get(key) {
                        let top = yy + MATH_PAD;
                        blit_scaled_clip(fb, tx, top, *dw, *dh, img, body_top, body_bot);
                        if top >= body_top && top + dh <= body_bot {
                            cursor = Some((tx, top + dh + 27));
                        }
                    } else if self.failed.contains(key) {
                        let base = yy + 33;
                        if base - 33 >= body_top && base + 11 <= body_bot {
                            if let Some(t) = text.as_mut() {
                                t.draw(fb, tx, base, FONT, "[formula]", INK);
                                cursor = Some((tx + t.width(FONT, "[formula]") as i32, base));
                            }
                        }
                    }
                }
            }
            yy += ih;
        }

        // ---------- 还没吐第一个字：立刻给反馈（"思考中" + 30 s 慢提示）
        if self.busy && self.answer.is_empty() {
            let base = body_top + INLINE_PAD + 33;
            let slow = self.started.elapsed() >= SLOW_HINT;
            if let Some(t) = text.as_mut() {
                t.draw(fb, tx, base, FONT, "Thinking…", MUTED);
                if slow {
                    t.draw(
                        fb,
                        tx,
                        base + LINE_H,
                        FONT_Q + 4.0,
                        "Large pages can take up to two minutes.",
                        MUTED,
                    );
                }
            }
            cursor = None;
        }

        // ---------- 流式光标
        if self.busy && self.cursor_on {
            if let Some((bx, base)) = cursor {
                if base >= body_top + 4 && base <= body_bot {
                    fb.fill_rect(bx + 2, base - 30, 7, 34, gray565(INK));
                }
            }
        }

        // ---------- 滚动条（贴着卡片右内边）
        let max = self.max_scroll();
        if max > 0 {
            let track_x = x + w - 22;
            let track_y = body_top + 6;
            let track_h = (body_bot - body_top - 12).max(1);
            fb.fill_rect(track_x, track_y, 6, track_h, gray565(TRACK));
            let thumb_h = ((track_h as f32 * (self.view_h() as f32 / self.body_h.max(1) as f32))
                .max(48.0)
                .min(track_h as f32)) as i32;
            let pos = self.scroll as f32 / max as f32;
            let thumb_y = track_y + ((track_h - thumb_h) as f32 * pos) as i32;
            fb.fill_rect(track_x, thumb_y, 6, thumb_h, gray565(THUMB));
        }

        // ---------- 头：**黑底白字**（和全屏 C8 面板、工具条菜单同一套语言）
        //
        // 原来画的是白底黑字：在一整页纯白的 PDF 上，一张大号白卡片看起来
        // 就像"整页被刷白了"（真机 BUG 1 就是这么被报上来的）。黑表头一加上，
        // 一眼就知道那是一个浮层。
        fb.fill_rect(x, y, w, INLINE_HDR, gray565(HDR_BG));
        if let Some(t) = text.as_mut() {
            t.set_bold(true);
            t.draw(fb, tx, y + 38, FONT_HDR, "Ask AI", HDR_INK);
            let cw = t.width_bold(FONT_HDR, "Close") as i32;
            t.draw(fb, x + w - INLINE_PAD - cw, y + 38, FONT_HDR, "Close", HDR_INK);
            t.set_bold(false);
        }

        // ---------- 底边：明显的分隔线（用户看得见"卡片到这儿结束"）
        fb.fill_rect(x, body_bot, w, INLINE_SEP, gray565(0));
        fb.fill_rect(x, body_bot - 1, w, 1, gray565(RULE));

        // ---------- 外框
        widgets::outline_rect(fb, x, y, w, h, 2, 0);

        self.text = text;
        self.dirty = false;
    }

    /// 内联卡片上一帧画完没有（main 用来判断要不要补画）。
    pub fn painted(&self) -> bool {
        !self.dirty
    }

    // ------------------------------------------------------------ 假数据

    /// 假数据：不连服务器，直接摆好问题与回答（`--render ai:…` 和单测用）。
    /// 回答里的 `$…$` 照样切公式块，图由 [`Ai::seed_png`] 预先放好。
    pub fn seed(&mut self, mode: Mode, question: &str, answer: &str) {
        self.open = true;
        self.mode = mode;
        self.question = question.to_string();
        self.answer = answer.to_string();
        self.busy = false;
        self.error = None;
        self.usage = None;
        self.scroll = 0;
        self.follow = false;
        self.cursor_on = true;
        self.rx = None;
        self.tx = None;
        self.layout_dirty = true;
        self.dirty = true;
    }

    /// 把一张 PNG 解码后放进公式缓存（出图时现造占位公式图用）。
    pub fn seed_png(&mut self, formula: &str, png: &[u8]) -> bool {
        match png_to_gray(png) {
            Some((w, h, gray)) => {
                self.imgs.insert(formula.trim().to_string(), Img::new(w, h, gray));
                self.layout_dirty = true;
                self.dirty = true;
                true
            }
            None => false,
        }
    }

    // ------------------------------------------------------------ 内部

    /// 开一轮新的提问：清状态、起线程。`Retry` 复用它。
    fn start(&mut self) {
        self.open = true;
        self.answer.clear();
        self.error = None;
        self.usage = None;
        self.busy = true;
        self.scroll = 0;
        self.follow = true;
        self.cursor_on = true;
        self.blink = Instant::now();
        self.failed.clear();
        self.pending.clear();
        self.layout_dirty = true;
        self.dirty = true;
        // 上一轮的连接/线程：关掉旧 socket（阻塞的 read 立刻返回），
        // 再换掉收货端——旧线程下一次 send 会失败并自己退出。
        if let Some(s) = self.sock.take() {
            let _ = s.shutdown(Shutdown::Both);
        }
        let (tx, rx) = mpsc::channel();
        self.tx = Some(tx.clone());
        self.rx = Some(rx);

        let body = self.request_body();
        self.started = Instant::now();
        thread::spawn(move || stream_thread(body, tx));
    }

    /// 这一轮要发的请求体（纯函数，起线程之前拼好；单测直接验它）。
    ///
    /// 框选提问（`set_sel` 调过）走 `panels::selectai::ask_body`：**请求体的
    /// 字段只有一个来源**，这里和真机发出去的是同一段代码。老模式
    /// （live/review/organize）保持原来的四五个字段，一个都不多。
    fn request_body(&self) -> String {
        if let (Some((rect, crop)), Some(img)) = (&self.sel, self.image_blob.as_deref()) {
            let r = crate::panels::selectai::Rect::new(rect[0], rect[1], rect[2], rect[3]);
            let body = crate::panels::selectai::ask_body(
                &self.doc,
                self.page as u32,
                r,
                img,
                crop.as_deref().unwrap_or(""),
                &self.question,
            );
            return self.with_publish(body);
        }
        let mut body = serde_json::json!({
            "mode": self.mode.wire(),
            "doc": self.doc,
            "page": self.page,
            "text": self.question,
        });
        if let Some(b) = &self.image_blob {
            body["image_blob"] = serde_json::Value::String(b.clone());
        }
        self.with_publish(body.to_string())
    }

    /// 把发布字段并进请求体（`ai_doc` / `parent` / `n`）。
    ///
    /// 放这里而不是改 `selectai::ask_body`：那个函数有 6 个参数的既有单测，
    /// 多两个可选字段不值得改一圈；字段名跟服务器 `AskReq` 对齐。
    fn with_publish(&self, body: String) -> String {
        let Some((ai, parent, n)) = &self.publish else {
            return body;
        };
        let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&body) else {
            return body;
        };
        v["ai_doc"] = serde_json::Value::String(ai.clone());
        v["n"] = serde_json::Value::from(*n);
        if !parent.trim().is_empty() {
            v["parent"] = serde_json::Value::String(parent.clone());
        }
        v.to_string()
    }

    /// 停：关连接、退出"回答中"。正文留着，可以继续看。
    fn stop(&mut self) {
        self.busy = false;
        self.cursor_on = true;
        self.follow = false;
        if let Some(s) = self.sock.take() {
            let _ = s.shutdown(Shutdown::Both);
        }
        self.dirty = true;
    }

    fn on_ev(&mut self, ev: Ev) {
        match ev {
            Ev::Opened(s) => {
                if self.busy {
                    self.sock = Some(s);
                } else {
                    // 已经 Stop 了才连上：直接关，别让服务器傻跑 180 s
                    let _ = s.shutdown(Shutdown::Both);
                }
            }
            Ev::Chunk(t) => {
                if self.busy {
                    self.push_chunk(&t);
                }
            }
            Ev::Done { usage, published } => {
                self.published = published;
                if self.busy {
                    self.finish(usage);
                }
            }
            Ev::Fail(m) => {
                if self.busy {
                    self.fail(&m);
                }
            }
            Ev::Img(key, img) => {
                self.pending.remove(&key);
                match img {
                    Some(i) => {
                        // 理论上一次回答不会出现几十个不同公式；真出现就丢掉旧缓存，
                        // 免得长时间用下来把设备内存吃光（留在正文里的会重新取）。
                        if self.imgs.len() >= IMG_CACHE_MAX && !self.imgs.contains_key(&key) {
                            self.imgs.clear();
                        }
                        self.imgs.insert(key, i);
                    }
                    None => {
                        self.failed.insert(key);
                    }
                }
                self.layout_dirty = true;
                self.dirty = true;
            }
        }
    }

    /// 正文里缺图的公式块 → 起后台线程取图。
    fn fetch_missing_math(&mut self) {
        self.rebuild();
        let Some(tx) = self.tx.clone() else { return };
        if self.pending.len() >= TEX_PARALLEL {
            return;
        }
        let mut want: Vec<String> = Vec::new();
        for it in &self.body {
            if let Item::Math { key, .. } = it {
                if !self.imgs.contains_key(key)
                    && !self.failed.contains(key)
                    && !self.pending.contains(key)
                {
                    want.push(key.clone());
                }
            }
        }
        for key in want {
            if self.pending.len() >= TEX_PARALLEL {
                break;
            }
            self.pending.insert(key.clone());
            let tx = tx.clone();
            thread::spawn(move || {
                let img = fetch_tex(&key);
                let _ = tx.send(Ev::Img(key, img));
            });
        }
    }

    /// 回答 → 正文块（文字折行 / 公式块）。只在 `layout_dirty` 时真的做。
    fn rebuild(&mut self) {
        if !self.layout_dirty {
            return;
        }
        self.layout_dirty = false;
        // 1) 先把"整行只有一个公式"并回相邻文字行（模型爱这么换行）
        let answer = self.answer.clone();
        let joined = join_lone_formulas(&answer, self.wrap_w, |s| self.measure_mixed(s));
        // 2) 再按段排成行（文字 + 行内公式混排，高的公式仍然独占一行）
        let mut body = self.layout_body(&joined);
        if let Some(err) = &self.error {
            for line in wrap(&format!("Error: {err}"), self.wrap_w, FONT) {
                body.push(Item::Line(vec![Run::Text(line)]));
            }
        }
        self.body_h = body.iter().map(|i| i.height()).sum();
        self.body = body;
    }

    /// 公式图的显示尺寸（没取到图时 (0,0)，占位后等图回来重排）。
    fn math_size(&self, key: &str) -> (i32, i32) {
        match self.imgs.get(key) {
            Some(img) => math_box_cap(self.wrap_w, math_cap_h(key), img.w, img.h),
            None => (0, 0),
        }
    }

    /// 估算"文字 + 行内公式"混排的宽度（回流时判断拼起来放不放得下）。
    fn measure_mixed(&self, s: &str) -> i32 {
        let mut w = 0.0f32;
        let mut rest = s;
        while let Some(i) = rest.find('$') {
            let (head, tail) = rest.split_at(i);
            for c in head.chars() {
                w += char_w(c, FONT);
            }
            let after = &tail[1..];
            match after.find('$') {
                Some(j) => {
                    let key = after[..j].trim();
                    if key.is_empty() {
                        w += char_w('$', FONT) * 2.0;
                    } else {
                        w += self.inline_math_w(key) as f32 + INLINE_MATH_GAP;
                    }
                    rest = &after[j + 1..];
                }
                None => {
                    w += char_w('$', FONT);
                    rest = "";
                }
            }
        }
        for c in rest.chars() {
            w += char_w(c, FONT);
        }
        w.ceil() as i32
    }

    /// 行内公式占的宽度：有图用真宽度，没图的按源码估（宁可估宽）。
    fn inline_math_w(&self, key: &str) -> i32 {
        match self.imgs.get(key) {
            Some(img) => math_box_cap(self.wrap_w, math_cap_h(key), img.w, img.h).0,
            None => {
                let n = key.chars().count().max(1) as f32;
                (FONT * 0.62 * n).min(self.wrap_w as f32).round() as i32 + 8
            }
        }
    }

    /// 分段 → 行：文字按剩余宽度折行，行的公式跟着文字走，高的公式独占一行。
    fn layout_body(&self, text: &str) -> Vec<Item> {
        let width = self.wrap_w as f32;
        let mut out: Vec<Item> = Vec::new();
        let mut line: Vec<Run> = Vec::new();
        let mut line_w = 0.0f32;
        for seg in split_math(text) {
            match seg {
                Seg::Text(t) => {
                    for (i, part) in t.split('\n').enumerate() {
                        if i > 0 {
                            flush_line(&mut out, &mut line, &mut line_w);
                        }
                        let mut rest = part.to_string();
                        let mut guard = 0;
                        while !rest.is_empty() && guard < 10_000 {
                            guard += 1;
                            let avail = (width - line_w).max(FONT * 2.0);
                            let lines = wrap(&rest, avail as i32, FONT);
                            let Some(first) = lines.first() else { break };
                            if first.is_empty() {
                                break;
                            }
                            line_w += est_w(first);
                            line.push(Run::Text(first.clone()));
                            // 把这一行从剩余文本里切掉（`wrap` 的行是原文前缀）
                            let n = first.len();
                            if rest.starts_with(first.as_str()) && rest.is_char_boundary(n) {
                                rest = rest[n..].trim_start().to_string();
                            } else {
                                break;
                            }
                            if !rest.is_empty() {
                                flush_line(&mut out, &mut line, &mut line_w);
                            }
                        }
                    }
                }
                Seg::Math(key) => {
                    let (dw, dh) = self.math_size(&key);
                    if line_w + dw as f32 + INLINE_MATH_GAP > width && !line.is_empty() {
                        flush_line(&mut out, &mut line, &mut line_w);
                    }
                    line_w += dw as f32 + INLINE_MATH_GAP;
                    line.push(Run::Math { key, dw, dh });
                }
            }
        }
        flush_line(&mut out, &mut line, &mut line_w);
        // 后处理：整行只有一个公式的（没并进任何文字的），升级成块级公式
        // （居中、上下留白）——`\frac` 那种高结构单独占一行时才这么画。
        for it in out.iter_mut() {
            if let Item::Line(runs) = it {
                let lone = runs.len() == 1 && matches!(runs[0], Run::Math { .. });
                if lone {
                    if let Run::Math { key, dw, dh } = runs[0].clone() {
                        *it = Item::Math { key, dw, dh };
                    }
                }
            }
        }
        out
    }

    fn max_scroll(&self) -> i32 {
        (self.body_h - self.view_h()).max(0)
    }

    /// 正文可视高度：整屏是 `VIEW_H`，内联卡片是"卡片高 - 头 - 底边线 - 上下内边距"。
    fn view_h(&self) -> i32 {
        match self.inline {
            Some((_, _, _, h)) => {
                (h - INLINE_HDR - INLINE_SEP - 2 * INLINE_PAD).max(LINE_H)
            }
            None => VIEW_H,
        }
    }

    fn ensure_text(&mut self) {
        if !self.text_tried {
            self.text_tried = true;
            self.text = Text::load();
        }
    }
}

impl Default for Ai {
    fn default() -> Self {
        Ai::new()
    }
}

/// 底部按钮矩形（`layout` 和 `paint` 共用）。
fn button_rect() -> (i32, i32, i32, i32) {
    (W / 2 - BTN_W / 2, BTN_TOP, BTN_W, BTN_H)
}

/// 内联卡片的 `Close` 命中区（卡片右上角，整块头高、宽 160 好点）。
fn inline_close_rect(r: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    (r.0 + r.2 - 160, r.1, 160, INLINE_HDR)
}

// ---------------------------------------------------------------- 公式切分

// ---------------------------------------------------------------- 公式回流

/// 句号/问号/冒号这一类"说完了"的结尾——上一行以它们结尾时不再把公式拼回去。
fn ends_sentence(s: &str) -> bool {
    s.trim_end()
        .chars()
        .last()
        .map(|c| matches!(c, '。' | '．' | '.' | '！' | '!' | '？' | '?' | '：' | ':' | '；' | ';'))
        .unwrap_or(true)
}

/// 拼接两段文字：中文/标点之间不插空格，英文之间插一个。
fn join_piece(a: &str, b: &str) -> String {
    let a = a.trim_end();
    let b = b.trim_start();
    if a.is_empty() {
        return b.to_string();
    }
    if b.is_empty() {
        return a.to_string();
    }
    let no_space = |c: char| {
        matches!(
            c,
            '，' | '。' | '；' | '：' | '？' | '！' | '、' | '）' | '】' | '」' | '』' | '》'
                | ')' | ']' | '}' | ',' | '.' | ';' | ':' | '?' | '!' | '…'
        )
    };
    let last = a.chars().last().unwrap_or(' ');
    let first = b.chars().next().unwrap_or(' ');
    if no_space(first) {
        // 后面紧跟中文标点：不插空格
        format!("{a}{b}")
    } else if a.contains('$') || b.contains('$') {
        // 公式和文字之间留一个空格（中文夹公式也一样，好读）
        format!("{a} {b}")
    } else if is_cjk(last) || is_cjk(first) {
        format!("{a}{b}")
    } else {
        format!("{a} {b}")
    }
}

/// 把"整行只有一个 `$公式$`"并回相邻文字行（排版回流）。
///
/// 模型很爱这么换行：
/// ```text
/// 验证：被积函数
/// $xe^{-x}$
/// 在
/// $[0,1]$
/// 上从
/// ```
/// 一条一行会变成"一行一个公式"，一张卡片一半是空白。规则（真机验收口径）：
/// * 空行 = 段落分隔，永远不跨；
/// * 只有当**上一行或这一行**含公式时才考虑拼（纯文字段落保持原样，别动列表）；
/// * 上一行不以句号/问号/冒号结尾；
/// * 拼起来的**总宽放得下**（用调用方的测量函数）；公式本身就超宽的不拼；
/// * 拼完继续试（所以 `文字 + 公式 + 文字 + 公式…` 会连成一整段，再交给 `wrap` 折行）。
///
/// 纯函数：`measure` 由调用方给（测试里可以用一个简单的宽度模型）。
pub fn join_lone_formulas<F>(text: &str, width: i32, measure: F) -> String
where
    F: Fn(&str) -> i32,
{
    let mut out: Vec<String> = Vec::new();
    for raw in text.split('\n') {
        let t = raw.trim();
        if t.is_empty() {
            // 段落分隔：保留空行，并且不让下一段并上来
            out.push(String::new());
            continue;
        }
        let cur_has_math = t.contains('$');
        let mut merged = false;
        if let Some(prev) = out.last() {
            let prev = prev.trim_end();
            if !prev.is_empty()
                && (prev.contains('$') || cur_has_math)
                && !ends_sentence(prev)
                && measure(&join_piece(prev, t)) <= width
            {
                let joined = join_piece(prev, t);
                *out.last_mut().unwrap() = joined;
                merged = true;
            }
        }
        if !merged {
            out.push(t.to_string());
        }
    }
    out.join("\n")
}

/// 把回答切成「文字 / 公式」两段。`$…$` 成对才算公式；
/// 未闭合的 `$` 留在文字里（流式中途出现半个公式很常见）。
pub fn split_math(s: &str) -> Vec<Seg> {
    let mut out: Vec<Seg> = Vec::new();
    let mut buf = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        let (head, tail) = rest.split_at(i);
        buf.push_str(head);
        let after = &tail[1..];
        match after.find('$') {
            Some(j) => {
                let formula = &after[..j];
                if formula.trim().is_empty() {
                    // `$$`：当普通文字，避免把空公式送去 TeX
                    buf.push('$');
                    buf.push_str(formula);
                    buf.push('$');
                } else {
                    if !buf.is_empty() {
                        out.push(Seg::Text(std::mem::take(&mut buf)));
                    }
                    out.push(Seg::Math(formula.trim().to_string()));
                }
                rest = &after[j + 1..];
            }
            None => {
                buf.push('$');
                buf.push_str(after);
                rest = "";
                break;
            }
        }
    }
    buf.push_str(rest);
    if !buf.is_empty() {
        out.push(Seg::Text(buf));
    }
    out
}

/// 把攒着的一行提交成 `Item::Line`（空行不留）。
fn flush_line(out: &mut Vec<Item>, line: &mut Vec<Run>, line_w: &mut f32) {
    if line.is_empty() {
        return;
    }
    out.push(Item::Line(std::mem::take(line)));
    *line_w = 0.0;
}

/// 估一段文字的宽度（和 `wrap` 用同一个模型）。
fn est_w(s: &str) -> f32 {
    let mut w = 0.0f32;
    for c in s.chars() {
        w += char_w(c, FONT);
    }
    w
}

/// 画一行（文字 + 行内公式），返回这一行末尾的 x（流式光标用）。
///
/// `clip` 只作用于公式图（文字没法按像素裁，调用方负责只画整行都在视口里的）。
fn draw_runs(
    fb: &mut Fb,
    text: &mut Option<Text>,
    mut x: i32,
    base: i32,
    runs: &[Run],
    imgs: &HashMap<String, Img>,
    failed: &HashSet<String>,
    clip: Option<(i32, i32)>,
) -> i32 {
    for (idx, r) in runs.iter().enumerate() {
        match r {
            Run::Text(s) => {
                if s.is_empty() {
                    continue;
                }
                match text.as_mut() {
                    Some(t) => {
                        t.draw(fb, x, base, FONT, s, INK);
                        x += t.width(FONT, s) as i32;
                    }
                    None => x += est_w(s) as i32,
                }
            }
            Run::Math { key, dw, dh } => {
                // 公式后面紧跟中文标点（，。；：）时不留空：不然会出现" $ ，"的怪缝
                let next_is_punct = match runs.get(idx + 1) {
                    Some(Run::Text(t)) => t
                        .chars()
                        .next()
                        .map(|c| matches!(c, '，' | '。' | '；' | '：' | '、' | '）' | '！' | '？'))
                        .unwrap_or(false),
                    _ => false,
                };
                let gap = if next_is_punct {
                    0
                } else {
                    INLINE_MATH_GAP as i32
                };
                if let Some(img) = imgs.get(key) {
                    let top = inline_math_top(base, *dh);
                    match clip {
                        Some((y0, y1)) => blit_scaled_clip(fb, x, top, *dw, *dh, img, y0, y1),
                        None => blit_scaled(fb, x, top, *dw, *dh, img),
                    }
                    x += *dw + gap;
                } else if failed.contains(key) {
                    if let Some(t) = text.as_mut() {
                        t.draw(fb, x, base, FONT, "[formula]", INK);
                        x += t.width(FONT, "[formula]") as i32;
                    }
                } else {
                    // 还在取图：留出宽度，后面的字别跳
                    x += *dw + gap;
                }
            }
        }
    }
    x
}

/// 公式图显示尺寸：宽度不超过正文宽、高度不超过 [`math_max_h`]，**只缩不放**。
///
/// 三个约束取最小的那个比例（>1 就按 1，小图保持自然尺寸）。这样
/// `$[0,1]$` 这种本来就小的公式原样画，`\int_0^1 rac{\ln x}{x}dx` 这种
/// 高的公式缩到 73 px 高，和正文一个量级。
#[cfg_attr(not(test), allow(dead_code))]
fn math_box(w: usize, h: usize) -> (i32, i32) {
    math_box_in(WRAP_W, w, h)
}

/// 同 [`math_box`]，但宽度上限由调用方给（内联卡片的折行宽度和整屏不一样）。
fn math_box_in(max_w: i32, w: usize, h: usize) -> (i32, i32) {
    math_box_cap(max_w, math_cap_h("x"), w, h)
}

/// 按源码取上限的版本：单符号 1.15×、复杂 1.6×；**宽度优先**，高度不够的下限放大。
fn math_box_cap(max_w: i32, cap_h: i32, w: usize, h: usize) -> (i32, i32) {
    if w == 0 || h == 0 {
        return (0, 0);
    }
    let (dw, dh) = (w as f32, h as f32);
    let s = (max_w as f32 / dw).min(cap_h as f32 / dh).min(1.0);
    let (mut sw, mut sh) = (dw * s, dh * s);
    // 下限：太小的等比放大到 LINE_H/2（太小的公式 16 级灰上根本看不清）
    let floor = MATH_H_FLOOR as f32;
    if sh < floor {
        let k = floor / sh.max(1.0);
        sw *= k;
        sh = floor;
        if sw > max_w as f32 {
            sw = max_w as f32;
        }
    }
    (
        (sw.round() as i32).max(1),
        (sh.round() as i32).max(1),
    )
}

/// `Fb::blit_gray` 只支持原尺寸，这里补最近邻缩放（灰度值直接查表）。
fn blit_scaled(fb: &mut Fb, x: i32, y: i32, dw: i32, dh: i32, img: &Img) {
    blit_gray_scaled(fb, x, y, dw, dh, img.w, img.h, &img.gray, i32::MIN, i32::MAX);
}

/// [`blit_scaled`] 的裸缓冲版本：别的面板（错题屏、墨迹回复）手里是
/// `(w, h, &[u8])` 而不是 [`Img`]，不必为了调一次 blit 去 clone 一整张图。
pub fn blit_gray_scaled(
    fb: &mut Fb,
    x: i32,
    y: i32,
    dw: i32,
    dh: i32,
    sw: usize,
    sh: usize,
    gray: &[u8],
    y0: i32,
    y1: i32,
) {
    if dw <= 0 || dh <= 0 || sw == 0 || sh == 0 || gray.len() < sw * sh {
        return;
    }
    if dw as usize == sw && dh as usize == sh {
        // 1:1 也按行裁：`blit_gray` 不会裁，负 y 会从上往下画进屏幕
        for row in 0..dh {
            let dy = y + row;
            if dy < y0 || dy >= y1 {
                continue;
            }
            fb.blit_gray(x, dy, dw, 1, &gray[row as usize * sw..]);
        }
        return;
    }
    for row in 0..dh {
        let dy = y + row;
        if dy < y0 || dy >= y1 {
            continue;
        }
        let sy = (row as usize * sh / dh as usize).min(sh - 1);
        let Some(dst) = fb.row_mut(dy, x, dw) else {
            continue;
        };
        for (i, d) in dst.iter_mut().enumerate() {
            let sx = (i * sw / dw as usize).min(sw - 1);
            *d = gray565(gray[sy * sw + sx]);
        }
    }
}

/// 同 [`blit_scaled`]，但**只写 `y0..y1` 这些行**（半开区间）。
///
/// 内联卡片的正文区必须严格裁在卡片里：一张 212 px 高的公式图跨在
/// 正文上边界时，没裁剪会画到 y≈-66，把页面顶部和顶栏的 `Live Note`
/// 按钮一起盖掉（真机 BUG 4）。缩略图的采样行不变，只是跳过目标行。
fn blit_scaled_clip(
    fb: &mut Fb,
    x: i32,
    y: i32,
    dw: i32,
    dh: i32,
    img: &Img,
    y0: i32,
    y1: i32,
) {
    blit_gray_scaled(fb, x, y, dw, dh, img.w, img.h, &img.gray, y0, y1);
}

// ---------------------------------------------------------------- 折行

/// 断行：中文按字、英文按词，贪心填满一行；超长单词从中间断。
///
/// 纯函数（不碰字体），宽度按 [`char_w`] 的估算模型来；`paint` 用的是同一个
/// `wrap`，所以折出来的行和画出来的行一致。估算偏宽一点点，宁可提前换行也
/// 不让正文压到右边距。
pub fn wrap(text: &str, width: i32, size: f32) -> Vec<String> {
    let max = width.max(1) as f32;
    let mut out: Vec<String> = Vec::new();
    let mut line = String::new();
    let mut line_w = 0.0f32;
    let space_w = char_w(' ', size);
    for a in atoms(text, size, max) {
        if a.brk {
            out.push(std::mem::take(&mut line));
            line_w = 0.0;
            continue;
        }
        if a.text.is_empty() {
            continue;
        }
        let extra = if a.sep && !line.is_empty() { space_w } else { 0.0 };
        if !line.is_empty() && line_w + extra + a.w > max {
            out.push(std::mem::take(&mut line));
            line.push_str(&a.text);
            line_w = a.w;
        } else {
            if extra > 0.0 {
                line.push(' ');
                line_w += space_w;
            }
            line.push_str(&a.text);
            line_w += a.w;
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}

struct Atom {
    text: String,
    w: f32,
    /// 这个 atom 前面原来有空格
    sep: bool,
    /// 硬换行
    brk: bool,
}

fn atoms(text: &str, size: f32, max: f32) -> Vec<Atom> {
    let mut out: Vec<Atom> = Vec::new();
    let mut word = String::new();
    let mut word_w = 0.0f32;
    let mut word_sep = false;
    let mut pending_sep = false;
    for ch in text.chars() {
        if ch == '\n' {
            push_word(&mut out, &mut word, &mut word_w, word_sep, max, size);
            word_sep = false;
            pending_sep = false;
            out.push(Atom {
                text: String::new(),
                w: 0.0,
                sep: false,
                brk: true,
            });
        } else if ch.is_whitespace() {
            push_word(&mut out, &mut word, &mut word_w, word_sep, max, size);
            word_sep = false;
            pending_sep = true;
        } else if is_cjk(ch) {
            push_word(&mut out, &mut word, &mut word_w, word_sep, max, size);
            word_sep = false;
            out.push(Atom {
                text: ch.to_string(),
                w: char_w(ch, size),
                sep: pending_sep && !out.is_empty(),
                brk: false,
            });
            pending_sep = false;
        } else {
            if word.is_empty() {
                word_sep = pending_sep;
                pending_sep = false;
            }
            word.push(ch);
            word_w += char_w(ch, size);
        }
    }
    push_word(&mut out, &mut word, &mut word_w, word_sep, max, size);
    out
}

fn push_word(
    out: &mut Vec<Atom>,
    word: &mut String,
    word_w: &mut f32,
    sep: bool,
    max: f32,
    size: f32,
) {
    if word.is_empty() {
        return;
    }
    if *word_w <= max {
        out.push(Atom {
            text: std::mem::take(word),
            w: *word_w,
            sep,
            brk: false,
        });
        *word_w = 0.0;
        return;
    }
    // 超长单词：从中间硬断，每段都塞得进一行（否则会一整个词顶出右边距）
    let mut chunk = String::new();
    let mut chunk_w = 0.0f32;
    let mut first = true;
    for ch in word.chars() {
        let cw = char_w(ch, size);
        if !chunk.is_empty() && chunk_w + cw > max {
            out.push(Atom {
                text: std::mem::take(&mut chunk),
                w: chunk_w,
                sep: sep && first,
                brk: false,
            });
            first = false;
            chunk_w = 0.0;
        }
        chunk.push(ch);
        chunk_w += cw;
    }
    if !chunk.is_empty() {
        out.push(Atom {
            text: chunk,
            w: chunk_w,
            sep: sep && first,
            brk: false,
        });
    }
    word.clear();
    *word_w = 0.0;
}

/// 单字符宽度估算（像素）。系数按设备字体实测反推（`Text::width`）：
/// 汉字 32 号约 33 px、小写字母约 20 px、空格 9.6 px。
fn char_w(ch: char, size: f32) -> f32 {
    if is_cjk(ch) {
        return size * 1.04;
    }
    match ch {
        ' ' => size * 0.31,
        'i' | 'l' | 'j' | 'I' | 't' | 'f' | 'r' | '.' | ',' | '\'' | '"' | ':' | ';' | '!'
        | '|' | '(' | ')' | '[' | ']' | '{' | '}' | '/' | '\\' | '-' | '_' | '=' => size * 0.38,
        'm' | 'w' | 'M' | 'W' | '@' | '—' | '…' => size * 1.06,
        'A'..='Z' => size * 0.82,
        '0'..='9' => size * 0.62,
        _ => size * 0.70,
    }
}

/// 汉字/全角标点：这些可以逐字断行。
fn is_cjk(ch: char) -> bool {
    matches!(ch as u32,
        0x1100..=0x115F
        | 0x2E80..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x2FFFD
    )
}

// ---------------------------------------------------------------- 网络

/// 起 `agent/ask`，把 SSE 逐条变成 [`Ev`]。
fn stream_thread(body: String, tx: Sender<Ev>) {
    let reader = match net::open_stream("POST", "/api/v1/agent/ask", body.as_bytes(), ASK_TIMEOUT) {
        Ok(r) => r,
        Err(e) => {
            let _ = tx.send(Ev::Fail(e));
            return;
        }
    };
    // 留一份 socket 给主线程：Stop 时 shutdown 能把下面阻塞的 read 立刻唤醒
    match reader.get_ref().try_clone() {
        Ok(s) => {
            if tx.send(Ev::Opened(s)).is_err() {
                return;
            }
        }
        Err(_) => return,
    }
    read_sse(reader, tx);
}

/// 按行读 SSE，直到 `done` / `error` / 连接断。
fn read_sse(mut rd: std::io::BufReader<TcpStream>, tx: Sender<Ev>) {
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut event = String::new();
    let mut data = String::new();
    let mut got_terminal = false;
    loop {
        buf.clear();
        match rd.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let raw = String::from_utf8_lossy(&buf);
        let line = raw.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            // 空行 = 一条事件结束
            if !data.is_empty() {
                if dispatch(&event, data.trim(), &tx) {
                    got_terminal = true;
                    break;
                }
                data.clear();
            }
            event.clear();
            continue;
        }
        if line.starts_with(':') {
            // 心跳（`: keepalive`）
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if !got_terminal {
        // 连接断了却没收到 done/error：别让面板永远停在"回答中"
        let _ = tx.send(Ev::Fail("stream closed".into()));
    }
}

/// 一条 SSE 事件 → [`Ev`]。返回 true = 这一轮结束，别再读了。
fn dispatch(event: &str, payload: &str, tx: &Sender<Ev>) -> bool {
    let v: serde_json::Value = serde_json::from_str(payload).unwrap_or(serde_json::Value::Null);
    match event {
        "token" => match v.get("t").and_then(|x| x.as_str()) {
            Some(t) if !t.is_empty() => tx.send(Ev::Chunk(t.to_string())).is_err(),
            _ => false,
        },
        "done" => {
            let usage = v.get("usage").and_then(|u| {
                Some((
                    u.get("in")?.as_u64()?,
                    u.get("out")?.as_u64()?,
                ))
            });
            let published = v.get("pdf").and_then(|x| x.as_str()).is_some();
            let _ = tx.send(Ev::Done { usage, published });
            true
        }
        "error" => {
            let msg = v
                .get("message")
                .and_then(|x| x.as_str())
                .unwrap_or("agent error");
            let _ = tx.send(Ev::Fail(msg.to_string()));
            true
        }
        _ => false,
    }
}

/// 取一张公式图：`GET /api/v1/tex?f=…&scale=2` → PNG → 灰度。
fn fetch_tex(formula: &str) -> Option<Img> {
    let path = format!("/api/v1/tex?f={}&scale=2", urlencode(formula));
    let rd = net::open_stream("GET", &path, &[], TEX_TIMEOUT).ok()?;
    let mut png = Vec::new();
    // open_stream 发的请求带 `Connection: close`，这里读到 EOF 就行
    rd.take(8 * 1024 * 1024).read_to_end(&mut png).ok()?;
    if png.is_empty() {
        return None;
    }
    let (w, h, gray) = png_to_gray(&png)?;
    Some(Img::new(w, h, gray))
}

/// URL 查询串转义：只留 RFC 3986 的 unreserved，其它（含 `+ = ^ \` 和中文）
/// 一律 `%XX`。TeX 里 `+`、`&`、`#` 全是语法字符，绝不能原样进 query。
fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------- PNG 解码

/// 8 位灰度 PNG → `(w, h, 灰度缓冲)`（255 = 白）。解不了返回 `None`。
///
/// 只支持 8 位、无隔行、color type 0（灰度）/ 2（真彩）/ 6（真彩+alpha）；
/// filter 0–4 全实现。公式图是 ImageMagick 出的灰度 PNG，正好落在支持范围内。
/// 调色板、16 位、隔行一律拒——宁可画 `[formula]` 也不画花屏。
pub fn png_to_gray(b: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    if b.len() < 8 || &b[..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let (mut w, mut h) = (0usize, 0usize);
    let (mut depth, mut color, mut interlace) = (0u8, 0u8, 0u8);
    let mut have_ihdr = false;
    let mut idat: Vec<u8> = Vec::new();
    let mut pos = 8usize;
    while pos + 8 <= b.len() {
        let len = u32::from_be_bytes([b[pos], b[pos + 1], b[pos + 2], b[pos + 3]]) as usize;
        let kind = &b[pos + 4..pos + 8];
        let start = pos + 8;
        let end = start.checked_add(len)?;
        if end.checked_add(4)? > b.len() {
            return None;
        }
        match kind {
            b"IHDR" => {
                if have_ihdr || len < 13 {
                    return None;
                }
                w = u32::from_be_bytes([b[start], b[start + 1], b[start + 2], b[start + 3]]) as usize;
                h = u32::from_be_bytes([b[start + 4], b[start + 5], b[start + 6], b[start + 7]])
                    as usize;
                depth = b[start + 8];
                color = b[start + 9];
                interlace = b[start + 12];
                have_ihdr = true;
            }
            b"IDAT" => idat.extend_from_slice(&b[start..end]),
            b"IEND" => break,
            _ => {}
        }
        pos = end + 4;
    }
    if !have_ihdr || idat.is_empty() || depth != 8 || interlace != 0 {
        return None;
    }
    let bpp = match color {
        0 => 1,
        2 => 3,
        6 => 4,
        _ => return None,
    };
    if w == 0 || h == 0 || w > 4096 || h > 4096 || w * h > 8_000_000 {
        return None;
    }
    let stride = w.checked_mul(bpp)?;
    let need = h.checked_mul(stride + 1)?;
    let raw = miniz_oxide::inflate::decompress_to_vec_zlib_with_limit(&idat, need).ok()?;
    if raw.len() < need {
        return None;
    }

    let mut prev = vec![0u8; stride];
    let mut cur = vec![0u8; stride];
    let mut out = vec![0u8; w * h];
    for y in 0..h {
        let base = y * (stride + 1);
        let ft = raw[base];
        cur.copy_from_slice(&raw[base + 1..base + 1 + stride]);
        for x in 0..stride {
            let a = if x >= bpp { cur[x - bpp] } else { 0 };
            let up = prev[x];
            let c = if x >= bpp { prev[x - bpp] } else { 0 };
            cur[x] = match ft {
                0 => cur[x],
                1 => cur[x].wrapping_add(a),
                2 => cur[x].wrapping_add(up),
                3 => cur[x].wrapping_add(((a as u16 + up as u16) / 2) as u8),
                4 => cur[x].wrapping_add(paeth(a, up, c)),
                _ => return None,
            };
        }
        let row = &mut out[y * w..(y + 1) * w];
        for x in 0..w {
            row[x] = match color {
                0 => cur[x],
                2 => luma(cur[x * 3], cur[x * 3 + 1], cur[x * 3 + 2]),
                6 => {
                    // 压到白底上（公式图通常是透明底黑字）
                    let al = cur[x * 4 + 3] as u32;
                    let l = luma(cur[x * 4], cur[x * 4 + 1], cur[x * 4 + 2]) as u32;
                    ((l * al + 255 * (255 - al)) / 255) as u8
                }
                _ => return None,
            };
        }
        prev.copy_from_slice(&cur);
    }
    Some((w, h, out))
}

fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let pa = (p - a as i16).abs();
    let pb = (p - b as i16).abs();
    let pc = (p - c as i16).abs();
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

fn luma(r: u8, g: u8, b: u8) -> u8 {
    ((r as u32 * 77 + g as u32 * 150 + b as u32 * 29) >> 8) as u8
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    /// 折行后的实测宽度（用同一套估算模型，保证断言可复现）。
    fn width_of(s: &str, size: f32) -> f32 {
        s.chars().map(|c| char_w(c, size)).sum()
    }

    // ------------------------------------------------------------ PNG

    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }

    fn put_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    }

    /// 按给定的 filter 把每行编码后组一张 PNG（测试专用）。
    fn build_png(w: usize, h: usize, color: u8, bpp: usize, filters: &[u8], rows: &[&[u8]]) -> Vec<u8> {
        assert_eq!(filters.len(), h);
        let stride = w * bpp;
        let mut scan = Vec::with_capacity((stride + 1) * h);
        let mut prev = vec![0u8; stride];
        for y in 0..h {
            let cur = rows[y];
            let f = filters[y];
            scan.push(f);
            for x in 0..stride {
                let a = if x >= bpp { cur[x - bpp] } else { 0 };
                let up = prev[x];
                let c = if x >= bpp { prev[x - bpp] } else { 0 };
                let pred = match f {
                    1 => a,
                    2 => up,
                    3 => ((a as u16 + up as u16) / 2) as u8,
                    4 => paeth(a, up, c),
                    _ => 0,
                };
                scan.push(cur[x].wrapping_sub(pred));
            }
            prev.copy_from_slice(cur);
        }
        let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&(w as u32).to_be_bytes());
        ihdr.extend_from_slice(&(h as u32).to_be_bytes());
        ihdr.extend_from_slice(&[8, color, 0, 0, 0]);
        put_chunk(&mut out, b"IHDR", &ihdr);
        put_chunk(
            &mut out,
            b"IDAT",
            &miniz_oxide::deflate::compress_to_vec_zlib(&scan, 6),
        );
        put_chunk(&mut out, b"IEND", &[]);
        out
    }

    /// 单测 1（C8 必做）：`share::png_gray` 出的 8×4 灰度图，解回来逐字节相等。
    #[test]
    fn png_gray_round_trip() {
        let (w, h) = (8usize, 4usize);
        // 别用全黑/全白：filter 和插值都测不出来
        let gray: Vec<u8> = (0..w * h).map(|i| (i * 37 % 254 + 1) as u8).collect();
        let png = crate::share::png_gray(&gray, w, h);
        assert!(!png.is_empty(), "png_gray 没出图");
        let (dw, dh, out) = png_to_gray(&png).expect("解不回来");
        assert_eq!((dw, dh), (w, h));
        assert_eq!(out, gray);
    }

    /// 单测 2：filter 0–4 全实现（png_gray 只会写 0，其它四种在这里手搓）。
    #[test]
    fn png_filters_one_to_four_decode() {
        let (w, h) = (8usize, 4usize);
        let rows: [Vec<u8>; 4] = [
            vec![10, 20, 30, 40, 50, 60, 70, 80],
            vec![200, 190, 180, 170, 160, 150, 140, 130],
            vec![0, 255, 0, 255, 0, 255, 0, 255],
            vec![90, 91, 92, 93, 94, 95, 96, 97],
        ];
        let refs: Vec<&[u8]> = rows.iter().map(|r| r.as_slice()).collect();
        // 第 0 行用 filter 1（Sub），后面 2/3/4 各一次
        let png = build_png(w, h, 0, 1, &[1, 2, 3, 4], &refs);
        let (dw, dh, out) = png_to_gray(&png).expect("filter 1-4 解不回来");
        assert_eq!((dw, dh), (w, h));
        assert_eq!(out, rows.concat());
    }

    /// 单测 3：真彩（type 2）按亮度转灰；真彩+alpha（type 6）压到白底。
    #[test]
    fn png_truecolor_and_alpha() {
        let row = [255u8, 0, 0, 0, 255, 0]; // 红、绿
        let png = build_png(2, 1, 2, 3, &[0], &[&row]);
        let (_, _, out) = png_to_gray(&png).expect("真彩解不回来");
        assert_eq!(out, vec![luma(255, 0, 0), luma(0, 255, 0)]);

        // alpha=0 的红 + alpha=255 的红：前者压成白
        let row = [255u8, 0, 0, 0, 255, 0, 0, 255];
        let png = build_png(2, 1, 6, 4, &[0], &[&row]);
        let (_, _, out) = png_to_gray(&png).expect("带 alpha 的解不回来");
        assert_eq!(out, vec![255, luma(255, 0, 0)]);
    }

    /// 单测 4：坏输入/不支持的类型返回 `None`，不 panic。
    #[test]
    fn png_rejects_garbage_and_interlaced() {
        assert!(png_to_gray(&[]).is_none());
        assert!(png_to_gray(b"not a png at all").is_none());
        let png = build_png(1, 1, 0, 1, &[0], &[&[42]]);
        assert!(png_to_gray(&png).is_some());
        // 只剩签名 + IHDR 头：没有 IDAT，解不了
        assert!(png_to_gray(&png[..20]).is_none());
        // 隔行：IHDR 第 12 字节改成 1
        let mut png = build_png(1, 1, 0, 1, &[0], &[&[42]]);
        let ihdr = 8 + 8;
        png[ihdr + 12] = 1;
        assert!(png_to_gray(&png).is_none());
        // 调色板图（color type 3）不支持
        let png = build_png(1, 1, 3, 1, &[0], &[&[0]]);
        assert!(png_to_gray(&png).is_none());
    }

    // ------------------------------------------------------------ 折行

    /// 单测 5（C8 必做）：中英混排，每行都不超过给定宽度，内容不丢。
    #[test]
    fn wrap_mixes_cjk_and_latin() {
        let text = "单位圆是 x^2 + y^2 = 1 的图像，所有满足这个方程的点 (x, y) 都在圆周上。";
        let lines = wrap(text, 400, FONT);
        assert!(lines.len() > 2, "400 宽应该折成多行：{lines:?}");
        for l in &lines {
            assert!(width_of(l, FONT) <= 400.0, "行超宽：{l:?}");
        }
        // 空格被吃掉了：拼回去（去掉空格）应当等于原文（去掉空格）
        let joined: String = lines.join("");
        let strip = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        assert_eq!(strip(&joined), strip(text));
    }

    /// 单测 6：超长英文单词从中间断，不断在宽度外。
    #[test]
    fn wrap_breaks_long_word() {
        let word = "supercalifragilisticexpialidocious_and_more";
        let lines = wrap(word, 120, FONT);
        assert!(lines.len() > 1, "超长词没断开：{lines:?}");
        for l in &lines {
            assert!(width_of(l, FONT) <= 120.0, "行超宽：{l:?}");
        }
        assert_eq!(lines.concat(), word);
    }

    /// 单测 7：空行保留（段落之间要留白），空串不折出行。
    #[test]
    fn wrap_keeps_blank_lines() {
        assert_eq!(wrap("first\n\nsecond", 500, FONT), vec!["first", "", "second"]);
        // 结尾的换行不额外多出一行空行
        assert_eq!(wrap("only\n", 500, FONT), vec!["only"]);
        assert!(wrap("", 500, FONT).is_empty());
    }

    // ------------------------------------------------------------ 公式切分

    /// 单测 8（C8 必做）：成对 `$…$` 切成 Math，其它是 Text。
    #[test]
    fn split_math_takes_pairs() {
        assert_eq!(
            split_math("单位圆是 $x^2+y^2=1$ 的图像"),
            vec![
                Seg::Text("单位圆是 ".into()),
                Seg::Math("x^2+y^2=1".into()),
                Seg::Text(" 的图像".into()),
            ]
        );
        assert_eq!(
            split_math("$a$ 和 $b$"),
            vec![
                Seg::Math("a".into()),
                Seg::Text(" 和 ".into()),
                Seg::Math("b".into()),
            ]
        );
        // 没有公式 / 空串
        assert_eq!(split_math("普通文本"), vec![Seg::Text("普通文本".into())]);
        assert!(split_math("").is_empty());
        // `$$` 不当空公式处理，整段留在文字里
        assert_eq!(split_math("$$"), vec![Seg::Text("$$".into())]);
    }

    /// 单测 9：未闭合的 `$` 留在文字里（流式中途只有半个公式）。
    #[test]
    fn split_math_keeps_unclosed_dollar() {
        assert_eq!(split_math("价格 $5 元"), vec![Seg::Text("价格 $5 元".into())]);
        assert_eq!(
            split_math("$x^2$ 然后 $y"),
            vec![
                Seg::Math("x^2".into()),
                Seg::Text(" 然后 $y".into()),
            ]
        );
    }

    /// 单测 10：公式图尺寸。
    ///
    /// 真机两轮反馈合成现在这套规则：
    /// * 以前死写 200 → `$0$` 画成半屏大数字（BUG 2）；
    /// * 上一版统一 1.6×（74）→ 单符号还是比正文大一倍；
    /// * 现在：单符号/单数字 1.15×（37），复杂公式 1.6×（74），
    ///   宽度优先、只缩不放（低于行高一半的放大到 23 免得看不清）。
    #[test]
    fn math_box_scales_to_panel() {
        assert_eq!(math_cap_h("0"), 37, "单数字：46×1.15≈37");
        assert_eq!(math_cap_h("[0,1]"), 37, "只有标点+2 个数字，也算单符号");
        assert_eq!(math_cap_h("x_i"), 37);
        assert_eq!(math_cap_h("\\frac{1}{3}"), 74, "高结构走 1.6×");
        assert_eq!(math_cap_h("\\int_0^1 x^2 dx"), 74);
        assert_eq!(math_cap_h("e^{-1} \\approx 0.368"), 74, "多符号不算单符号");
        assert!(math_cap_h("0") < math_cap_h("\\frac{1}{2}"));
        assert_eq!(MATH_H_FLOOR, 23, "下限 = 行高一半");
        assert!(math_cap_h("0") >= MATH_H_FLOOR);

        // 复杂公式：宽度优先缩、高度封顶 74
        let (w, h) = math_box_cap(WRAP_W, 74, 400, 800);
        assert_eq!(h, 74);
        assert_eq!(w, 37, "400 × 74/800");
        // 超宽：宽度缩到正文宽
        let (w, h) = math_box_cap(WRAP_W, 74, WRAP_W as usize + 100, 200);
        assert!(w <= WRAP_W);
        assert!(h <= 74);
        // 单符号：同一张图，上限 37
        let (_, h) = math_box_cap(WRAP_W, math_cap_h("0"), 60, 120);
        assert_eq!(h, 37, "单符号必须落在 1.15× 以内");
        // 很小的图放大到下限（等比）
        let (w, h) = math_box_cap(WRAP_W, 74, 30, 12);
        assert_eq!(h, MATH_H_FLOOR, "太小的放大到行高一半");
        assert!(w > 30, "放大是等比的");
        // 已经小于上限的大图不动
        assert_eq!(math_box_cap(WRAP_W, 74, 120, 40), (120, 40));
    }

    // ------------------------------------------------------------ 命中区

    /// 单测 11（C8 必做）：Close / Stop / Retry 的命中区间。
    #[test]
    fn hit_close_stop_retry() {
        let mut p = Ai::new();
        p.seed(Mode::Live, "什么是单位圆？", "x^2+y^2=1");
        // 空闲 → Retry
        let lay = p.layout();
        assert_eq!(lay.len(), 2);
        assert_eq!(lay[0].0, Act::Close);
        assert_eq!(lay[1].0, Act::Retry);
        assert_eq!(p.hit(1350.0, 20.0), Some(Act::Close));
        let (bx, by, bw, bh) = button_rect();
        assert!(bw >= 80 && bh >= 80, "按钮要够手指点");
        assert_eq!(p.hit((bx + bw / 2) as f32, (by + bh / 2) as f32), Some(Act::Retry));
        // 按钮外、正文里的空白没有动作
        assert_eq!(p.hit(700.0, 900.0), None);
        assert_eq!(p.hit(10.0, 10.0), None, "黑头左边不是 Close");

        // 回答中 → Stop
        p.busy = true;
        assert_eq!(p.hit((bx + bw / 2) as f32, (by + bh / 2) as f32), Some(Act::Stop));
        assert_eq!(p.hit(1350.0, 20.0), Some(Act::Close));
        // 黑头之外的 Close 区不再命中
        assert_eq!(p.hit(1350.0, 60.0), None);
    }

    // ------------------------------------------------------------ 流式拼接

    /// 单测 12（C8 必做）：push_chunk 多次后 answer 是完整拼接。
    #[test]
    fn push_chunk_concatenates_in_order() {
        let mut p = Ai::new();
        p.seed(Mode::Live, "", "");
        p.push_chunk("单位");
        p.push_chunk("圆是 ");
        p.push_chunk("");
        p.push_chunk("$x^2+y^2=1$");
        assert_eq!(p.answer, "单位圆是 $x^2+y^2=1$");
        assert!(p.needs_redraw());
        // 切公式：文字 + 公式
        assert_eq!(
            split_math(&p.answer),
            vec![
                Seg::Text("单位圆是 ".into()),
                Seg::Math("x^2+y^2=1".into()),
            ]
        );
    }

    /// 单测 13：滚动的上下边界，以及"贴底才跟随"的语义。
    #[test]
    fn scroll_clamps_and_follows_bottom() {
        let mut p = Ai::new();
        p.seed(Mode::Live, "问题", &"一行\n".repeat(100));
        p.rebuild();
        let max = p.max_scroll();
        assert!(max > 0, "100 行应该超出一屏");
        p.apply(Act::Scroll(100_000));
        assert_eq!(p.scroll, max);
        p.apply(Act::Scroll(10));
        assert_eq!(p.scroll, max, "到底了再加也不动");
        p.apply(Act::Scroll(-100_000));
        assert_eq!(p.scroll, 0);
        // 往上滚之后不再自动跟底
        assert!(!p.follow);
    }

    /// 单测 14：整屏绘制不 panic；黑头/底条是实的，公式图落在正文里。
    #[test]
    fn paint_draws_header_and_formula_block() {
        let mut p = Ai::new();
        p.seed(Mode::Live, "单位圆", "单位圆：$x^2+y^2=1$。");
        // 200×80 的占位公式图：只有一圈黑边
        let (iw, ih) = (200usize, 80usize);
        let mut gray = vec![255u8; iw * ih];
        for x in 0..iw {
            gray[x] = 0;
            gray[(ih - 1) * iw + x] = 0;
        }
        for y in 0..ih {
            gray[y * iw] = 0;
            gray[y * iw + iw - 1] = 0;
        }
        let png = crate::share::png_gray(&gray, iw, ih);
        assert!(p.seed_png("x^2+y^2=1", &png), "占位公式图解码失败");

        let mut fb = Fb::open_mem();
        p.paint(&mut fb);
        assert_eq!(fb.get(700, 10), 0, "黑头没画上");
        assert_eq!(fb.get(700, 20), 0, "黑头没画满高");
        assert_eq!(fb.get(700, BODY_BOT + 100), 255, "底条不是白的");
        // 公式**不再独占一块**：它和"单位圆："排在同一行，这一行被撑高到 74+12
        let (dw, dh) = math_box_cap(WRAP_W, math_cap_h("x^2+y^2=1"), 200, 80);
        assert_eq!(dh, 74, "复杂公式按 1.6 × 行高");
        let lh = LINE_H.max(dh + 12);
        let base = BODY_TOP + lh - 13;
        let top = inline_math_top(base, dh);
        let dark = (MARGIN..MARGIN + dw + 400)
            .filter(|&x| fb.get(x, top) == 0)
            .count();
        assert!(dark > 100, "行内公式的上边框要画在这一行（{dark} px）");
        assert_eq!(fb.get(MARGIN + dw / 2, top + dh / 2), 255, "公式图内部该是白的");
        assert!(!p.needs_redraw(), "paint 之后不该再要求重绘");
    }

    /// 单测 16：SSE 事件 → 内部事件（token 增量 / done 用量 / error 文案 / 其它忽略）。
    #[test]
    fn sse_events_map_to_callbacks() {
        let (tx, rx) = mpsc::channel();
        assert!(!dispatch("token", r#"{"t":"你好"}"#, &tx));
        assert!(!dispatch("thinking", r#"{"x":1}"#, &tx), "未知事件不算结束");
        match rx.try_recv() {
            Ok(Ev::Chunk(t)) => assert_eq!(t, "你好"),
            _ => panic!("token 没转成 Chunk"),
        }
        assert!(rx.try_recv().is_err(), "未知事件不该产生消息");

        // done：带用量，且告诉调用方"别再读了"
        let (tx, rx) = mpsc::channel();
        assert!(dispatch(
            "done",
            r#"{"session":"s","usage":{"in":3,"out":7}}"#,
            &tx
        ));
        match rx.try_recv() {
            Ok(Ev::Done { usage, .. }) => assert_eq!(usage, Some((3, 7))),
            _ => panic!("done 没转成 Done"),
        }

        // error：文案透传
        let (tx, rx) = mpsc::channel();
        assert!(dispatch("error", r#"{"message":"agent busy"}"#, &tx));
        match rx.try_recv() {
            Ok(Ev::Fail(m)) => assert_eq!(m, "agent busy"),
            _ => panic!("error 没转成 Fail"),
        }

        // 坏 JSON / 空文本：不 panic，也不产生事件
        let (tx, rx) = mpsc::channel();
        assert!(!dispatch("token", "{not json", &tx));
        assert!(!dispatch("token", r#"{"t":""}"#, &tx));
        assert!(rx.try_recv().is_err());
    }

    /// 单测 17：按行读 SSE——心跳/注释行跳过，读到 done 立刻停（服务器是
    /// keep-alive，后面还挂着数据也不该继续读），并且不等连接关闭。
    #[test]
    fn read_sse_stops_at_done() {
        use std::io::Write;
        let srv = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = srv.local_addr().expect("addr");
        let writer = std::thread::spawn(move || {
            let (mut s, _) = srv.accept().expect("accept");
            let _ = s.write_all(b": keepalive\n\nevent: token\ndata: {\"t\":\"a\"}\n\n");
            let _ = s.write_all(b"event: token\ndata: {\"t\":\"b\"}\n\n");
            let _ = s.write_all(
                b"event: done\ndata: {\"session\":\"s\",\"usage\":{\"in\":1,\"out\":2}}\n\n",
            );
            // done 之后还继续写：客户端应该已经走人，写失败也无所谓
            for _ in 0..100 {
                if s.write_all(b"event: token\ndata: {\"t\":\"x\"}\n\n").is_err() {
                    break;
                }
            }
            std::thread::sleep(Duration::from_millis(100));
        });
        let client = TcpStream::connect(addr).expect("connect");
        let (tx, rx) = mpsc::channel();
        read_sse(std::io::BufReader::new(client), tx);
        let mut text = String::new();
        let mut done = None;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Ev::Chunk(t) => text.push_str(&t),
                Ev::Done { usage, .. } => done = Some(usage),
                _ => {}
            }
        }
        assert_eq!(text, "ab");
        assert_eq!(done, Some(Some((1, 2))));
        writer.join().expect("writer");
    }

    /// 单测 15：URL 转义（`+ = ^ &` 和中文都不能原样进 query）。
    #[test]
    fn urlencode_escapes_tex_chars() {
        assert_eq!(urlencode("x^2+y^2=1"), "x%5E2%2By%5E2%3D1");
        assert_eq!(urlencode("a&b#c"), "a%26b%23c");
        assert_eq!(urlencode("中文"), "%E4%B8%AD%E6%96%87");
        assert_eq!(urlencode("A-Z_a.z~0"), "A-Z_a.z~0");
    }

    // ------------------------------------------------------------ 内联模式（D1）

    /// 框选提问的请求体：`rect` 和 `crop_blob` 必须在（服务器端靠它裁图）。
    /// 纯函数，不联网。
    #[test]
    fn request_body_carries_rect_and_crop_blob() {
        let mut p = Ai::new();
        p.mode = Mode::Select;
        p.doc = "abc-123".into();
        p.page = 5;
        p.question = "第 1 题错在哪？".into();
        p.image_blob = Some("sha-page".into());
        p.set_sel([120, 240, 700, 900], Some("sha-crop".into()));
        let v: serde_json::Value = serde_json::from_str(&p.request_body()).unwrap();
        assert_eq!(v["mode"], "select");
        assert_eq!(v["doc"], "abc-123");
        assert_eq!(v["page"], 5);
        assert_eq!(v["rect"], serde_json::json!([120, 240, 700, 900]));
        assert_eq!(v["image_blob"], "sha-page");
        assert_eq!(v["crop_blob"], "sha-crop");
        assert_eq!(v["text"], "第 1 题错在哪？");
        // 没裁图（理论上不会发生）时也不该把字段写成 null
        p.set_sel([0, 0, 10, 10], None);
        let v: serde_json::Value = serde_json::from_str(&p.request_body()).unwrap();
        assert!(v.get("crop_blob").is_none());
    }

    /// 老模式（live/review/organize）的请求体一个字段都没多。
    #[test]
    fn request_body_without_sel_is_unchanged() {
        let mut p = Ai::new();
        p.mode = Mode::Live;
        p.doc = "d".into();
        p.page = 1;
        p.question = "hi".into();
        let v: serde_json::Value = serde_json::from_str(&p.request_body()).unwrap();
        assert!(v.get("rect").is_none());
        assert!(v.get("crop_blob").is_none());
        assert_eq!(v["mode"], "live");
    }

    /// 内联模式有自己的折行宽度（卡片宽 - 2*内边距），不跟整屏抢。
    #[test]
    fn inline_mode_uses_its_own_wrap_width() {
        let mut p = Ai::new();
        assert_eq!(p.wrap_w, WRAP_W);
        let card = (150, 90, 1104, 840);
        p.set_inline(Some(card));
        assert_eq!(p.wrap_w, 1104 - 2 * INLINE_PAD);
        assert_eq!(p.inline_rect(), Some(card));
        assert!(p.view_h() < 840, "正文高度要扣掉头/底/内边距");
        p.set_inline(None);
        assert_eq!(p.wrap_w, WRAP_W);
        assert_eq!(p.view_h(), VIEW_H);
    }

    /// 卡片只在自己的矩形里画：外面一个像素都不许动。
    #[test]
    fn inline_paint_stays_inside_the_card() {
        let mut p = Ai::new();
        let card = (150, 90, 1104, 840);
        p.set_inline(Some(card));
        p.seed(Mode::Select, "问", "答");
        let mut fb = Fb::open_mem();
        // 先把整屏涂成一个不黑不白的灰，画完检查卡片外面还是原值
        fb.fill_rect(0, 0, W, H, gray565(128));
        let base = fb.get(10, 10);
        p.paint_inline(&mut fb);
        let (x, y, w, h) = card;
        for (px, py) in [(10, 10), (140, 500), (x + w + 5, y + 5), (700, y + h + 5)] {
            assert_eq!(fb.get(px, py), base, "({px},{py}) 在卡片外，不该被画到");
        }
        // 卡片里是白的（底）
        assert!(fb.get(x + 30, y + h / 2) > 200, "卡片里要铺白底");
        // 底边那条明显的分隔线
        assert!(fb.get(x + 100, y + h - 2) < 60, "卡片底边要有 4px 黑线");
        // Close 在右上角、只在头里
        assert_eq!(p.inline_hit((x + w - 30) as f32, (y + 20) as f32), Some(Act::Close));
        assert_eq!(p.inline_hit((x + 30) as f32, (y + h / 2) as f32), None);
    }

    // ------------------------------------------------------------ 公式回流

    /// 一个简单的宽度模型：汉字 33、ASCII 18、公式按源码长度估（测试够用）。
    fn meas(s: &str) -> i32 {
        let mut w = 0;
        let mut rest = s;
        while let Some(i) = rest.find('$') {
            let (head, tail) = rest.split_at(i);
            w += est_w(head) as i32;
            let after = &tail[1..];
            match after.find('$') {
                Some(j) => {
                    // 公式：每个字符 20 px，最少 40
                    w += (after[..j].chars().count() as i32 * 20).max(40);
                    rest = &after[j + 1..];
                }
                None => {
                    rest = "";
                }
            }
        }
        w + est_w(rest) as i32
    }

    /// ①单公式行（只有 `$…$`）并回上一行文字（放得下）。
    #[test]
    fn lone_formula_joins_the_previous_text_line() {
        let src = "验证：被积函数\n$0$\n在\n$[0,1]$";
        let out = join_lone_formulas(src, 1200, meas);
        assert_eq!(out, "验证：被积函数 $0$ 在 $[0,1]$");
        // 换行没了 → 交给 wrap 折行
        assert!(!out.contains('\n'));
    }

    /// ②放不下就不合并（保持原来的换行）。
    #[test]
    fn lone_formula_not_joined_when_too_wide() {
        let src = "验证：被积函数被积函数被积函数\n$0$";
        // 量出来一定超过 200 → 不并
        let out = join_lone_formulas(src, 200, meas);
        assert_eq!(out, "验证：被积函数被积函数被积函数\n$0$");
        // 同一段文字，宽度给够 → 并
        let out = join_lone_formulas(src, 2000, meas);
        assert_eq!(out, "验证：被积函数被积函数被积函数 $0$");
    }

    /// ③超长公式（本身比正文宽）不合并，保持独占一行。
    #[test]
    fn overlong_formula_stays_on_its_own_line() {
        let src = "看这个\n$\\frac{AAAAAAAAAAAAAAAAAAAAAAAA}{BBBBBBBBBBBBBBBBBBBB}$";
        let out = join_lone_formulas(src, 300, meas);
        assert!(out.contains('\n'), "超宽公式不能并进文字行：{out:?}");
    }

    /// 句号/问号/冒号结尾的行不并（一句话说完了）。
    #[test]
    fn sentence_end_blocks_the_join() {
        for end in ["答。", "为什么？", "如下：", "ok."] {
            let src = format!("{end}\n$0$");
            let out = join_lone_formulas(&src, 1200, meas);
            assert!(out.contains('\n'), "{end:?} 之后不该并：{out:?}");
        }
        // 句中没有标点 → 并
        let out = join_lone_formulas("设函数\n$0$", 1200, meas);
        assert_eq!(out, "设函数 $0$");
    }

    /// 只有"公式相关"的行才回流：纯文字段落、空行、列表都不能被动。
    #[test]
    fn plain_text_paragraphs_are_left_alone() {
        let src = "第一行\n第二行\n\n第三段第一行\n第三段第二行";
        assert_eq!(join_lone_formulas(src, 9999, meas), src);
    }

    /// ④单符号公式的高度上限 = 1.15 × 正文字号（37），不是 1.6 × 行高。
    #[test]
    fn single_symbol_formula_is_body_text_size() {
        assert_eq!(math_cap_h("0"), 37);
        assert_eq!(math_cap_h("[0,1]"), 37);
        assert_eq!(math_cap_h("x"), 37);
        // 一张 60×120 的图（真机那种"0 被画成 55 高"）缩到 37
        let (w, h) = math_box_cap(WRAP_W, math_cap_h("0"), 60, 120);
        assert_eq!(h, 37);
        assert_eq!(w, 19, "等比：60 × 37/120 ≈ 19");
        assert!(h as f32 <= FONT * 1.15 + 0.5, "单符号不许超过 1.15 × 字号");
        // 复杂公式还是 74
        assert_eq!(math_cap_h("\\frac{1}{3}"), 74);
    }

    /// 排版：模型那种"一段话里每个公式独占一行"要回流成 2~4 行，
    /// 而且公式和文字**在同一行**（`Run::Math` 和 `Run::Text` 混排）。
    #[test]
    fn layout_flows_inline_formulas_into_text() {
        let mut p = Ai::new();
        p.set_inline(Some((150, 90, 1104, 840)));
        // 公式图按真机的样子给：单符号方一点、长公式扁一点，都能排进行内
        for k in ["0", "[0,1]", "0.264"] {
            p.imgs.insert(k.to_string(), Img::new(40, 40, vec![0u8; 1600]));
        }
        p.imgs.insert(
            "e^{-1} \\approx 0.368".to_string(),
            Img::new(200, 30, vec![0u8; 6000]),
        );
        p.seed(
            Mode::Select,
            "q",
            "验证：被积函数\n$0$\n在\n$[0,1]$\n上从\n$0$\n增到\n$e^{-1} \\approx 0.368$\n再回落，平均值约\n$0.264$\n，与上述结果一致。",
        );
        p.rebuild();
        let lines = p.body.len();
        assert!(lines <= 4, "回流后应该只剩 2~4 行，实际 {lines} 行");
        // 每一行都得有字（不许出现"一行只有一个公式"）
        for it in &p.body {
            if let Item::Line(runs) = it {
                let has_text = runs
                    .iter()
                    .any(|r| matches!(r, Run::Text(s) if !s.trim().is_empty()));
                assert!(has_text, "还有一行只有公式：{runs:?}");
            }
        }
        // 至少一行是"文字 + 公式"混排
        let mixed = p.body.iter().any(|it| {
            matches!(it, Item::Line(runs)
                if runs.iter().any(|r| matches!(r, Run::Math { .. })))
        });
        assert!(mixed, "公式没有排进行内");
    }

    /// BUG 4：跨在卡片上下边界的**公式图不许画到卡片外**。
    ///
    /// 公式图最高 212 px（74 + 上下留白），正文区顶 146、卡片顶 90：不裁剪时
    /// 它会把 y≈-66..146 全画一遍，把页面顶部和顶栏 `Live Note` 按钮盖掉。
    #[test]
    fn math_image_never_bleeds_out_of_the_card() {
        let mut p = Ai::new();
        let (cx, cy, cw, ch) = (150, 90, 1104, 840);
        p.set_inline(Some((cx, cy, cw, ch)));
        // 300×600 的纯黑公式图（缩到 74 高、37 宽）
        p.imgs.insert(
            "BIG".to_string(),
            Img::new(300, 600, vec![0u8; 300 * 600]),
        );
        p.seed(Mode::Select, "q", "$BIG$");
        // 让这个公式块跨在正文上边界：yy = 146 + 28 - scroll，图顶 = yy + 6
        // scroll=100 → 图顶 80（卡片顶 90 之上 10 px），图底 154（正文区里 8 px）
        p.scroll = 100;
        p.rebuild();
        assert_eq!(p.body.len(), 1);
        let mut fb = Fb::open_mem();
        fb.fill_rect(0, 0, W, H, gray565(128));
        let base = fb.get(700, 10);
        p.paint_inline(&mut fb);
        // 卡片上方：一个像素都不许动（原来是公式图的白色底盖上去）
        for y in (0..cy).step_by(7) {
            for x in (cx..cx + cw).step_by(37) {
                assert_eq!(fb.get(x, y), base, "公式画到了卡片上方 ({x},{y})");
            }
        }
        // 卡片下方
        for y in ((cy + ch + 1)..H.min(cy + ch + 300)).step_by(7) {
            for x in (cx..cx + cw).step_by(37) {
                assert_eq!(fb.get(x, y), base, "公式画到了卡片下方 ({x},{y})");
            }
        }
        // 不滚的时候公式完整落在正文区里，要看得见
        p.scroll = 0;
        let mut fb2 = Fb::open_mem();
        fb2.fill_rect(0, 0, W, H, gray565(255));
        p.paint_inline(&mut fb2);
        let dark = (cy + 56..cy + 56 + 220)
            .flat_map(|y| (cx + 20..cx + 120).map(move |x| (x, y)))
            .filter(|&(x, y)| fb2.get(x, y) < 80)
            .count();
        assert!(dark > 300, "正文区里要看得见公式（{dark} px）");
    }

    /// 卡片头是**黑底白字**（BUG 1：白底卡片在白 PDF 上看着像"整页刷白"）。
    #[test]
    fn inline_header_is_black_on_white_text() {
        let mut p = Ai::new();
        let (cx, cy, cw, _) = (150, 90, 1104, 840);
        p.set_inline(Some((cx, cy, cw, 400)));
        p.seed(Mode::Select, "q", "答");
        let mut fb = Fb::open_mem();
        p.paint_inline(&mut fb);
        // 表头左右两端是黑底
        assert!(fb.get(cx + 5, cy + 5) < 40, "表头黑底");
        assert!(fb.get(cx + cw - 5, cy + 5) < 40, "表头黑底（右上）");
        // Close 在表头里是白字（反色）
        let white = (cx + cw - 160..cx + cw - 20)
            .flat_map(|x| (cy + 10..cy + 50).map(move |y| (x, y)))
            .filter(|&(x, y)| fb.get(x, y) > 200)
            .count();
        assert!(white > 40, "Close 要是白字（{white} px）");
        // 正文区是白纸（在黑表头下面）
        assert!(fb.get(cx + 60, cy + 200) > 200, "正文白底");
    }

    /// 卡片高度跟着内容走：`Thinking…` 最矮，长回答封顶到卡片高。
    #[test]
    fn inline_needed_height_follows_content() {
        let mut p = Ai::new();
        p.set_inline(Some((150, 90, 1104, 840)));
        p.show_pending();
        let pending = p.inline_needed_height();
        assert!(pending <= INLINE_HDR + INLINE_SEP + 2 * INLINE_PAD + LINE_H * 2);
        let mut ans = String::new();
        for _ in 0..400 {
            ans.push_str("一行很长的回答。\n");
        }
        p.seed(Mode::Select, "q", &ans);
        let long = p.inline_needed_height();
        assert!(long > 840, "长回答需要的高度会超过卡片上限（调用方再夹）");
        assert!(long > pending);
    }

    /// `show_pending`：卡片立刻立起来、显示"思考中"，还没有正文。
    #[test]
    fn pending_card_shows_before_the_first_token() {
        let mut p = Ai::new();
        p.set_inline(Some((150, 90, 1104, 840)));
        p.show_pending();
        assert!(p.open && p.busy && p.answer.is_empty());
        assert!(p.needs_redraw());
        let mut fb = Fb::open_mem();
        p.paint_inline(&mut fb);
        // 正文区有"Thinking…"的墨（不是纯白）
        let mut dark = 0;
        for y in 150..230 {
            for x in 180..420 {
                // MUTED = 130 的灰字，阈值放到 180
                if fb.get(x, y) < 180 {
                    dark += 1;
                }
            }
        }
        assert!(dark > 30, "要画出 Thinking…（{dark} px）");
    }
}
