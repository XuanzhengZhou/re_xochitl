//! Live Note 实时模式（C11）：笔记页底部的流式文字带。
//!
//! # 交互
//!
//! 笔记页顶栏（右上角 × 左边）有一个 `Live Note` 开关。开着的时候：
//!
//! * 屏幕下方出现一条固定高度（[`BAND_H`]）的文字带，顶上一条 2px 黑分隔线；
//! * 上半部照常写字 —— 笔的 down/move/up 一路走原路，墨迹层/渲染/自动存盘都不受影响；
//! * 带内上下拖动（手指或笔都行）滚动历史，右上角 `Close` 关掉模式；
//! * 每次抬笔后防抖 [`DEBOUNCE_MS`]，到点且页面内容相对上次请求有变化就起一条流。
//!
//! # 数据怎么来
//!
//! `POST /api/v1/agent/ask`（和 C8 那个整屏回答面板同一个端点），body：
//!
//! ```json
//! {"mode":"live","doc":"<uuid>","page":0,"image_blob":"<整页 PNG 的 sha>","text":"<最后一行的文字>"}
//! ```
//!
//! 整页 PNG 是后台线程用 `Canvas` 把当前页笔迹重画进一块内存帧缓冲、再
//! [`crate::share::png_gray`] 编出来的（和真机屏幕上看到的墨迹同一套光栅化），
//! 然后走 [`crate::net::put_blob`] 传上去。
//!
//! 响应用 [`crate::net::open_stream`] 的 SSE 读：`event: token` / `done` / `error`，
//! `:` 开头的心跳忽略。**逐 token 追加**到文字带里。
//!
//! # 单飞与取消
//!
//! 主线程只碰 [`Gate`]：抬笔 → 记时刻 → 每帧问一次"到点没有"。真起请求时
//! 先把上一条流的 socket `shutdown` 掉、把收货端 `Receiver` 丢掉 —— 旧线程下一次
//! `send` 就失败、自己退出，两条流不会同时在写文字带。同一时刻最多一条流。
//!
//! # 性能
//!
//! 主循环每帧只做 `try_recv`（不阻塞）+ 到点重画一次；重画限频 [`PAINT_MS`]，
//! 而且**只刷文字带那一块**（A2 波形 ≈ 141 ms），绝不整屏重绘。结构变化
//! （开关模式 / 出错 / 滚动）才用 GC16，那一档能自愈残影。
//!
//! 文字一律用 **tone 0（纯黑）** 画：A2 是二值波形，驱不动中间灰
//! （`src/fb.rs` 和 `src/panel/wbf.rs` 都记着这个坑）。

use std::io::BufRead;
use std::net::{Shutdown, TcpStream};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use crate::fb::{
    gray565, Fb, UPDATE_MODE_PARTIAL, WAVEFORM_MODE_A2, WAVEFORM_MODE_GC16,
};
use crate::net;
use crate::ui::text::Text;
use crate::ui::widgets;

// ---------------------------------------------------------------- 几何

/// 底部状态条高度。
///
/// 以前这里是一条 420 px 的文字带，把整段回答显示在带子里。用户要的是
/// "回答长在纸上"，带子只负责说**现在在干什么**，所以压成一条窄条。
pub const BAND_H: i32 = 96;
/// 带子上沿那条分隔线。
pub const DIVIDER: i32 = 2;
/// 带内左右边距。
pub const PAD_X: i32 = 32;
/// 顶部那一行（左边 `Live Note`、右边 `Close`）的高度。
pub const HEAD_H: i32 = 64;
/// 正文字号。
pub const FONT: f32 = 30.0;
/// 正文行高。
pub const LINE_H: i32 = 44;
/// 行内基线（相对行顶边）。
pub const LINE_BASE: i32 = 34;
/// 正文区底下留白。
pub const BOT_PAD: i32 = 20;
/// 上半部至少留这么多可写区域（屏幕太矮时按它压带子）。
pub const MIN_TOP: i32 = 200;
/// 顶栏按钮尺寸与位置（右上角，× 左边）。
pub const BTN_W: i32 = 340;
pub const BTN_H: i32 = 76;
pub const BTN_TOP: i32 = 18;
/// 按钮右边留的余量：× 的命中区是 1300..1380（见 `ui_tap`），别撞上。
pub const BTN_RIGHT_GAP: i32 = 112;
/// `Close` 小按钮。
pub const CLOSE_W: i32 = 132;
pub const CLOSE_H: i32 = 56;

/// 流式 token 的刷新波形：**A2**（12 相位 ≈ 141 ms）。
///
/// 文字带里所有像素都是纯黑/纸白（正文 tone 0），A2 正好推得动；GC16 要 46
/// 相位 ≈ 540 ms，拿来刷 token 就根本"流"不起来。
pub const WF_STREAM: u32 = WAVEFORM_MODE_A2;
/// 结构变化（开关模式 / 出错 / 滚动 / 被盖掉后补画）用 GC16：46 相位，
/// 但它会重绘"目标没变"的像素，能自愈 A2 留下的残影。
pub const WF_FULL: u32 = WAVEFORM_MODE_GC16;
/// 局部刷新（`UPDATE_MODE_FULL` 会整块重刷一遍，文字带只要局部）。
pub const UPD_PARTIAL: u32 = UPDATE_MODE_PARTIAL;

/// 文字带高度：不超过 [`BAND_H`]，也保证上半部还剩 [`MIN_TOP`]。
pub fn band_h(h: i32) -> i32 {
    BAND_H.min((h - MIN_TOP).max(0))
}

/// 文字带上沿（含分隔线）的 y。
pub fn band_top(h: i32) -> i32 {
    h - band_h(h)
}

/// 文字带整块矩形（含分隔线）。
pub fn band_rect(w: i32, h: i32) -> (i32, i32, i32, i32) {
    (0, band_top(h), w, band_h(h))
}

/// 落点在不在文字带里（下半屏）。
pub fn in_band(x: f32, y: f32, w: i32, h: i32) -> bool {
    let (t, bh) = (band_top(h), band_h(h));
    x >= 0.0 && x < w as f32 && y >= t as f32 && (y as i32) < t + bh
}

/// `Close` 小按钮：文字带右上角。
pub fn close_rect(w: i32, h: i32) -> (i32, i32, i32, i32) {
    let t = band_top(h) + (HEAD_H - CLOSE_H) / 2;
    (w - PAD_X - CLOSE_W, t, CLOSE_W, CLOSE_H)
}

/// 落点在不在 `Close` 上。
pub fn in_close(x: f32, y: f32, w: i32, h: i32) -> bool {
    let (cx, cy, cw, ch) = close_rect(w, h);
    x >= cx as f32 && x < (cx + cw) as f32 && y >= cy as f32 && y < (cy + ch) as f32
}

/// 正文可画区（带内，去掉顶部那一行）。
pub fn content_rect(w: i32, h: i32) -> (i32, i32, i32, i32) {
    (
        PAD_X,
        band_top(h) + HEAD_H,
        (w - 2 * PAD_X).max(1),
        (band_h(h) - HEAD_H - BOT_PAD).max(1),
    )
}

/// 第 `row` 行（0 = 可见的第一行）的基线 y。
pub fn line_y(content_y: i32, row: i32) -> i32 {
    content_y + row * LINE_H + LINE_BASE
}

/// 顶栏 `Live Note` 开关的矩形。
pub fn btn_rect(w: i32) -> (i32, i32, i32, i32) {
    (w - BTN_RIGHT_GAP - BTN_W, BTN_TOP, BTN_W, BTN_H)
}

/// 落点在不在顶栏按钮上。
pub fn in_btn(x: f32, y: f32, w: i32) -> bool {
    let (bx, by, bw, bh) = btn_rect(w);
    x >= bx as f32 && x < (bx + bw) as f32 && y >= by as f32 && y < (by + bh) as f32
}

// ---------------------------------------------------------------- 折行

/// 单字符宽度估算（像素）。系数按设备字体实测反推（和 `panels::ai` 同一套）：
/// 汉字 30 号约 31 px、小写字母约 21 px、空格 9.3 px。
pub fn char_w(ch: char, size: f32) -> f32 {
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

/// 汉字/全角标点：可以逐字断行。
pub fn is_cjk(ch: char) -> bool {
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

/// 折行：`\n` 强制换行；CJK 逐字可断，西文按词断，一个词长过整行就硬断。
pub fn wrap(text: &str, max_w: f32) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<String> = Vec::new();
    for para in text.split('\n') {
        if para.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut cur = String::new();
        let mut w = 0.0f32;
        // 词 = 一串非 CJK 非空白字符，或单个 CJK 字符
        let mut it = para.char_indices().peekable();
        while let Some((_, ch)) = it.next() {
            if is_cjk(ch) {
                let cw = char_w(ch, FONT);
                if w + cw > max_w && !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    w = 0.0;
                }
                cur.push(ch);
                w += cw;
                continue;
            }
            if ch == ' ' {
                // 行尾的空格不占位置
                if !cur.is_empty() {
                    let cw = char_w(ch, FONT);
                    if w + cw > max_w {
                        out.push(std::mem::take(&mut cur));
                        w = 0.0;
                    } else {
                        cur.push(ch);
                        w += cw;
                    }
                }
                continue;
            }
            // 西文词：先把整词量一遍
            let start = cur.len();
            let mut word = String::new();
            word.push(ch);
            while let Some((_, c2)) = it.peek().copied() {
                if is_cjk(c2) || c2 == ' ' {
                    break;
                }
                word.push(c2);
                it.next();
            }
            let ww: f32 = word.chars().map(|c| char_w(c, FONT)).sum();
            if w + ww <= max_w {
                cur.push_str(&word);
                w += ww;
                continue;
            }
            // 放不下：先收当前行，再决定整词换行还是硬断
            if start > 0 {
                out.push(std::mem::take(&mut cur));
                w = 0.0;
            }
            if ww <= max_w {
                cur.push_str(&word);
                w = ww;
                continue;
            }
            for c in word.chars() {
                let cw = char_w(c, FONT);
                if w + cw > max_w && !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    w = 0.0;
                }
                cur.push(c);
                w += cw;
            }
        }
        out.push(cur);
    }
    // 末尾的空行（文本以 `\n` 结尾）不算一行
    if out.len() > 1 && out.last().map(|s| s.is_empty()).unwrap_or(false) {
        out.pop();
    }
    out
}

/// 正文区一屏能放几行。
pub fn rows_per_view(view_h: i32) -> usize {
    (view_h / LINE_H).max(1) as usize
}

/// 最多能往上滚多少像素（按**整行**算：滚到头正好第一行贴顶）。
pub fn max_scroll(n: usize, view_h: i32) -> i32 {
    let cap = rows_per_view(view_h);
    (n.saturating_sub(cap) as i32) * LINE_H
}

/// 从底部往上滚 `scroll` 像素后，可见的行区间 `(首行, 行数)`。
///
/// `scroll` = 0 表示贴着最新一行（跟随流式）。
pub fn visible(n: usize, view_h: i32, scroll: i32) -> (usize, usize) {
    if n == 0 {
        return (0, 0);
    }
    let cap = rows_per_view(view_h);
    let s = scroll.clamp(0, max_scroll(n, view_h));
    // 往上滚了多少行就是往回退多少行（按整行算，半行归到更靠下那行）
    let back = (s / LINE_H) as usize;
    let end = n.saturating_sub(back);
    let first = end.saturating_sub(cap);
    (first, end - first)
}

/// 把 `scroll` 夹进合法范围。
pub fn clamp_scroll(n: usize, view_h: i32, scroll: i32) -> i32 {
    scroll.clamp(0, max_scroll(n, view_h))
}

// ---------------------------------------------------------------- 防抖 / 单飞 / 限频

/// 抬笔后等多久才起请求。
pub const DEBOUNCE_MS: u64 = 1200;
/// 两次重画文字带之间至少隔多久。
pub const PAINT_MS: u64 = 60;

/// [`Gate::poll`] 的结论。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    /// 没有流在跑：直接起。
    Start,
    /// 已经有流在跑：取消它再起新的。
    CancelAndStart,
}

/// 抬笔防抖 + 单飞 + 重画限频。**全部用注入的时间**，方便单测。
#[derive(Clone, Debug)]
pub struct Gate {
    /// 抬笔时刻（`None` = 没有待发请求）
    pending: Option<u64>,
    /// 有几条流在跑（正常只会是 0 或 1）
    busy: u32,
    /// 累计起了几条（测试用）
    starts: u32,
    /// 累计取消了几条（测试用）
    cancels: u32,
    /// 上一次重画文字带的时刻
    last_paint: u64,
    /// 有内容变了、等着重画
    dirty: bool,
}

impl Default for Gate {
    fn default() -> Self {
        Gate::new()
    }
}

impl Gate {
    pub const fn new() -> Gate {
        Gate {
            pending: None,
            busy: 0,
            starts: 0,
            cancels: 0,
            last_paint: 0,
            dirty: false,
        }
    }

    /// 抬笔：重置防抖计时（防抖期间又写一笔 = 从头再等）。
    pub fn pen_up(&mut self, now: u64) {
        self.pending = Some(now);
    }

    /// 防抖到点没有。`changed` = 页面内容相对上次请求有没有变。
    pub fn poll(&mut self, now: u64, changed: bool) -> Option<Act> {
        let t0 = self.pending?;
        if now < t0 + DEBOUNCE_MS {
            return None;
        }
        // 到点这一下就把 pending 取走：不然后面每帧都会重新判一次
        self.pending = None;
        if !changed {
            return None;
        }
        let act = if self.busy > 0 {
            self.cancels += 1;
            Act::CancelAndStart
        } else {
            Act::Start
        };
        self.busy = 1;
        self.starts += 1;
        Some(act)
    }

    /// 一条流结束（done / error / 被取消）。取消由下一次 `poll` 记账。
    pub fn finished(&mut self) {
        self.busy = 0;
    }

    /// 直接起流（不走 `poll`）时也要把单飞标上。
    pub fn mark_busy(&mut self) {
        self.busy = 1;
    }

    pub fn busy(&self) -> u32 {
        self.busy
    }

    /// 有抬笔在等防抖到点（便宜，给"要不要算页面指纹"用）。
    pub fn pending(&self) -> bool {
        self.pending.is_some()
    }

    pub fn starts(&self) -> u32 {
        self.starts
    }

    pub fn cancels(&self) -> u32 {
        self.cancels
    }

    /// 来了 token：标脏，等限频窗口放行。
    pub fn token(&mut self) {
        self.dirty = true;
    }

    /// 到点该重画了吗。60ms 里来多少 token 都只放行一次。
    pub fn take_paint(&mut self, now: u64) -> bool {
        if !self.dirty || now < self.last_paint + PAINT_MS {
            return false;
        }
        self.dirty = false;
        self.last_paint = now;
        true
    }
}

// ---------------------------------------------------------------- SSE

/// 一条 SSE 事件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SseEv {
    None,
    Token(String),
    /// 结束。`Some((in, out))` = `done` 事件里带了 `usage`。
    Done(Option<(u64, u64)>),
    Err(String),
}

/// SSE 行解析：`event:` / `data:` 攒到空行成一条事件，`:` 开头当心跳丢掉。
#[derive(Clone, Debug, Default)]
pub struct Sse {
    event: String,
    data: String,
}

impl Sse {
    pub const fn new() -> Sse {
        Sse {
            event: String::new(),
            data: String::new(),
        }
    }

    /// 喂一行（可以带 `\r\n`）。
    pub fn feed(&mut self, raw: &str) -> SseEv {
        let line = raw.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if self.event.is_empty() {
                self.data.clear();
                return SseEv::None;
            }
            let ev = std::mem::take(&mut self.event);
            let data = std::mem::take(&mut self.data);
            return match ev.as_str() {
                "token" => match serde_json::from_str::<serde_json::Value>(data.trim()) {
                    // 半截 JSON（连接断了 / 分包还没到齐）就丢掉这一条，别报错
                    Ok(v) => match v.get("t").and_then(|x| x.as_str()) {
                        Some(t) if !t.is_empty() => SseEv::Token(t.to_string()),
                        _ => SseEv::None,
                    },
                    Err(_) => SseEv::None,
                },
                "done" => SseEv::Done(usage_of(data.trim())),
                "error" => {
                    let msg = serde_json::from_str::<serde_json::Value>(data.trim())
                        .ok()
                        .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from))
                        .unwrap_or_else(|| "agent error".to_string());
                    SseEv::Err(msg)
                }
                _ => SseEv::None,
            };
        }
        if line.starts_with(':') {
            return SseEv::None; // 心跳
        }
        if let Some(rest) = line.strip_prefix("event:") {
            self.event = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !self.data.is_empty() {
                self.data.push('\n');
            }
            self.data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
        SseEv::None
    }
}

/// `done` 事件里的用量 `(in, out)`。
fn usage_of(data: &str) -> Option<(u64, u64)> {
    let v: serde_json::Value = serde_json::from_str(data).ok()?;
    let u = v.get("usage")?;
    Some((u.get("in")?.as_u64()?, u.get("out")?.as_u64()?))
}

// ---------------------------------------------------------------- 整页 PNG

/// 整页灰度 → PNG。
///
/// **整页图由调用方给**（main.rs 的 `d6_start_live` 走 `d1_page_gray`：印刷底图
/// `pdf_bg` × 墨迹 + 文字层，和屏幕上同一套合成）。这里只管编码 —— 曾经这里
/// 只画 `visible_lines()`，真机上 PDF 习题页被模型读成「这页基本是空白」，
/// 因为题目在底图上、不在笔迹里。
pub fn page_png(gray: &[u8]) -> Vec<u8> {
    let n = crate::fb::W * crate::fb::H;
    if gray.len() != n {
        return Vec::new();
    }
    crate::share::png_gray(gray, crate::fb::W, crate::fb::H)
}

// ---------------------------------------------------------------- 后台线程

/// 流里的错误一律翻成这一行英文（不弹模态，只写在文字带里）。
pub const REASON_NET: &str = "Could not reach the server";
pub const REASON_CLOSED: &str = "Stream closed";

/// 后台线程 → 主线程的消息。
enum Ev {
    /// 连上了：把 socket 留给主线程，取消时 `shutdown` 能唤醒阻塞的 read
    Opened(TcpStream),
    Token(String),
    Done(Option<(u64, u64)>),
    Fail(String),
}

fn worker(doc: String, page: usize, text: String, gray: Vec<u8>, tx: Sender<Ev>) {
    let png = page_png(&gray);
    if png.is_empty() {
        let _ = tx.send(Ev::Fail("Image encode failed".into()));
        return;
    }
    let cfg = net::Config::load();
    if !cfg.configured() {
        let _ = tx.send(Ev::Fail(REASON_NET.into()));
        return;
    }
    let sha = match net::put_blob(&cfg, &png) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[live] 上传整页失败：{e}");
            let _ = tx.send(Ev::Fail(REASON_NET.into()));
            return;
        }
    };
    let body = serde_json::json!({
        "mode": "live",
        "doc": doc,
        "page": page,
        "image_blob": sha,
        "text": text,
    })
    .to_string();
    let rd = match net::open_stream(
        "POST",
        "/api/v1/agent/ask",
        body.as_bytes(),
        Duration::from_secs(180),
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[live] 开流失败：{e}");
            let _ = tx.send(Ev::Fail(REASON_NET.into()));
            return;
        }
    };
    match rd.get_ref().try_clone() {
        Ok(s) => {
            // 留一份给主线程：取消时 `shutdown` 能把下面阻塞的 read 立刻唤醒
            let keep = s.try_clone().ok();
            if tx.send(Ev::Opened(s)).is_err() {
                if let Some(k) = keep {
                    let _ = k.shutdown(Shutdown::Both);
                }
                return;
            }
        }
        Err(_) => {
            let _ = tx.send(Ev::Fail(REASON_NET.into()));
            return;
        }
    }
    read_sse(rd, tx);
}

/// 按行读 SSE，直到 `done` / `error` / 连接断。
fn read_sse(mut rd: std::io::BufReader<TcpStream>, tx: Sender<Ev>) {
    let mut buf: Vec<u8> = Vec::with_capacity(256);
    let mut sse = Sse::new();
    loop {
        buf.clear();
        match rd.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let line = String::from_utf8_lossy(&buf).to_string();
        match sse.feed(&line) {
            SseEv::None => {}
            SseEv::Token(t) => {
                if tx.send(Ev::Token(t)).is_err() {
                    return; // 被取消了：收货端已经丢掉
                }
            }
            SseEv::Done(usage) => {
                let _ = tx.send(Ev::Done(usage));
                return;
            }
            SseEv::Err(m) => {
                let _ = tx.send(Ev::Fail(m));
                return;
            }
        }
    }
    let _ = tx.send(Ev::Fail(REASON_CLOSED.into()));
}

// ---------------------------------------------------------------- 状态

/// 文字带的运行时状态（UI 线程独占，网络在后台线程）。
struct St {
    /// 模式开着（文字带在）
    on: bool,
    /// 流式累积的文本
    text: String,
    /// 出错时那一行（新请求开始时清掉）
    err: Option<String>,
    /// 从底部往上滚了多少像素（0 = 跟到最新）
    scroll: i32,
    gate: Gate,
    rx: Option<Receiver<Ev>>,
    sock: Option<TcpStream>,
    /// 上一次请求用的页面指纹（内容没变就不再问）
    last_fp: Option<u64>,
    /// 拖动滚动：(上一 y, 按下时的 scroll)
    drag: Option<(f32, i32)>,
    /// 按下被我们吃掉了：跟着的抬笔也要吃掉
    grabbed: bool,
    /// 需要 GC16 重画（结构变化）
    need_full: bool,
    /// 上一次画的时候文字带是不是可见的（用来判断"刚出现 / 刚消失"）
    painted: bool,
    /// 顶栏按钮该不该在屏上
    btn: bool,
    /// 这一轮请求起来的时刻（`fb::now_ms`），用来打"首字/完成"耗时
    start_ms: u64,
    /// 这一轮收到过 token 没有（没收到 = 带子里显示 `Thinking…`）
    got_token: bool,
    /// `Still working…` 已经画出来了没有（8 s 后补一行，只补一次）
    still_shown: bool,
    /// `done` 事件里的用量
    usage: Option<(u64, u64)>,
    /// 出图/单测用：假装已经等了这么久（进程刚起时 `now - start_ms` 到不了 8 s）。
    /// 生产路径永远是 0。
    waited_hint: u64,
    /// 这一轮的回答到齐了，等主循环取走（渲染成墨迹写进页面）
    reply_out: Option<String>,
    /// 上一轮回答变成墨迹的条数（状态条上显示 `Wrote 2409 strokes` 之类）
    last_ink: Option<usize>,
    /// 上一轮回答的字数（`done` 时算好：正文搬走以后就没得数了）
    reply_chars: usize,
}

const fn new_st() -> St {
    St {
        on: false,
        text: String::new(),
        err: None,
        scroll: 0,
        gate: Gate::new(),
        rx: None,
        sock: None,
        last_fp: None,
        drag: None,
        grabbed: false,
        need_full: true,
        painted: false,
        btn: false,
        start_ms: 0,
        got_token: false,
        still_shown: false,
        usage: None,
        waited_hint: 0,
        reply_out: None,
        last_ink: None,
        reply_chars: 0,
    }
}

static ST: Mutex<St> = Mutex::new(new_st());

/// 统一时间轴（和 `tick` / `fb::now_ms` 同一个 0 点）。
fn now_ms() -> u64 {
    crate::fb::now_ms() as u64
}

/// 等首字等到该补 `Still working…` 了吗（纯函数，方便单测）。
///
/// 只在一个条件下补：**正在等首字**（`busy` 且还没 `got_token`）、已经等满
/// [`THINKING_MS`]、而且还没补过。补过就不再刷（`still_shown`）。
pub fn still_due(busy: bool, got_token: bool, still_shown: bool, waited_ms: u64) -> bool {
    busy && !got_token && !still_shown && waited_ms >= THINKING_MS
}

/// 等这么久还没首字，就在 `Thinking…` 下面补一行 `Still working…`。
///
/// 真机上 dsh 冷启动 + 读整页图要 **30 s+** 才出首字，带子里一直只有
/// `Thinking…` 会让人以为卡死；补一行浅一点的英文，**不中断请求**。
pub const THINKING_MS: u64 = 8_000;

fn lock() -> std::sync::MutexGuard<'static, St> {
    ST.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------- 对外接口（UI 线程）

/// 模式开着没有。
pub fn is_on() -> bool {
    lock().on
}

/// 开/关模式。开的时候清空文字带，等第一次抬笔。
pub fn set_on(on: bool) {
    let mut st = lock();
    if st.on == on {
        return;
    }
    st.on = on;
    st.need_full = true;
    st.text.clear();
    st.err = None;
    st.scroll = 0;
    st.last_fp = None;
    if !on {
        cancel(&mut st);
        st.gate.finished();
    }
    eprintln!("[live] {}", if on { "开" } else { "关" });
}

/// 抬笔了（笔记页里、没在拖文字带）。
pub fn pen_up(now: u64) {
    let mut st = lock();
    if !st.on {
        return;
    }
    st.gate.pen_up(now);
}

/// 有抬笔在等防抖。为 false 时调用方连页面指纹都不用算。
pub fn start_pending() -> bool {
    let st = lock();
    st.on && st.gate.pending()
}

/// 防抖到点、内容也变了 → 返回 true，调用方去取笔迹并 [`start`]。
pub fn take_start(now: u64, fp: u64) -> bool {
    let mut st = lock();
    if !st.on {
        return false;
    }
    let changed = st.last_fp != Some(fp);
    let hit = match st.gate.poll(now, changed) {
        Some(Act::Start) => true,
        Some(Act::CancelAndStart) => {
            // 单飞：先把旧的掐掉，再起新的
            cancel(&mut st);
            true
        }
        None => false,
    };
    if hit {
        st.last_fp = Some(fp);
    }
    hit
}

/// 起一条流（会先取消上一条）。
///
/// `gray` 是**整页灰度**（1404×1872，含印刷底图和文字层），由调用方在 UI 线程
/// 用 `d1_page_gray` 合成；编码/上传/读流全在后台线程。
pub fn start(doc: &str, page: usize, text: &str, gray: Vec<u8>) {
    let mut st = lock();
    cancel(&mut st);
    st.gate.mark_busy();
    st.text.clear();
    st.err = None;
    st.scroll = 0;
    st.need_full = true;
    st.start_ms = now_ms();
    st.waited_hint = 0;
    st.got_token = false;
    st.still_shown = false;
    st.usage = None;
    let (tx, rx) = mpsc::channel();
    st.rx = Some(rx);
    let dark = gray.iter().filter(|v| **v < 200).count();
    eprintln!(
        "[live] 起请求 doc={doc} page={page} 整页 {}×{} 有墨 {} px text={:?}",
        crate::fb::W,
        crate::fb::H,
        dark,
        text
    );
    let (doc, text) = (doc.to_string(), text.to_string());
    thread::spawn(move || worker(doc, page, text, gray, tx));
}

/// 取消当前流：关 socket + 丢接收端。返回 true = 真取消了一条。
fn cancel(st: &mut St) -> bool {
    let busy = st.gate.busy() > 0;
    if let Some(s) = st.sock.take() {
        let _ = s.shutdown(Shutdown::Both);
    }
    st.rx = None;
    if busy {
        st.gate.finished();
        eprintln!("[live] 取消旧的流");
        true
    } else {
        false
    }
}

/// 收后台事件（**不阻塞**）。返回 true = 文字带内容变了，要重画。
pub fn pump() -> bool {
    let mut st = lock();
    let mut evs: Vec<Ev> = Vec::new();
    if let Some(rx) = st.rx.as_ref() {
        while let Ok(ev) = rx.try_recv() {
            evs.push(ev);
        }
    }
    let mut changed = false;
    for ev in evs {
        match ev {
            Ev::Opened(s) => {
                if st.gate.busy() > 0 {
                    st.sock = Some(s);
                } else {
                    let _ = s.shutdown(Shutdown::Both);
                }
            }
            Ev::Token(t) => {
                if st.gate.busy() == 0 {
                    continue;
                }
                if !st.got_token {
                    st.got_token = true;
                    eprintln!("[live] 首字 {}ms", now_ms().saturating_sub(st.start_ms));
                }
                st.text.push_str(&t);
                st.gate.token();
                changed = true;
            }
            Ev::Done(usage) => {
                if st.gate.busy() > 0 {
                    st.gate.finished();
                    st.sock = None;
                    st.rx = None;
                    st.need_full = true;
                    st.gate.token();
                    changed = true;
                    st.usage = usage;
                    // 回答到齐：搬给主循环去变墨迹（带子里不再显示正文）。
                    // **先数字符再搬**：搬完 `st.text` 是空的，下面那句日志会印成 0 字。
                    let chars = st.text.chars().count();
                    if !st.text.trim().is_empty() {
                        st.reply_out = Some(std::mem::take(&mut st.text));
                    }
                    st.reply_chars = chars;
                    match usage {
                        Some((i, o)) => eprintln!(
                            "[live] 完成 {}ms in={i} out={o}（{} 字）",
                            now_ms().saturating_sub(st.start_ms),
                            st.reply_chars
                        ),
                        None => eprintln!(
                            "[live] 完成 {}ms（{} 字）",
                            now_ms().saturating_sub(st.start_ms),
                            st.reply_chars
                        ),
                    }
                }
            }
            Ev::Fail(m) => {
                if st.gate.busy() > 0 {
                    eprintln!(
                        "[live] 失败 {}ms：{m}",
                        now_ms().saturating_sub(st.start_ms)
                    );
                    st.gate.finished();
                    st.sock = None;
                    st.rx = None;
                    st.err = Some(m);
                    st.need_full = true;
                    st.gate.token();
                    changed = true;
                }
            }
        }
    }
    changed
}

/// 到点该重画了没有；`Some(true)` = 要 GC16。
pub fn want_paint(now: u64) -> Option<bool> {
    let mut st = lock();
    if !st.on {
        return None;
    }
    // `Thinking…` 挂满 8 s：补一行 `Still working…`（只补一次，中间灰要 GC16）
    if st.still_due(now) {
        st.need_full = true;
        st.gate.token();
    }
    if !st.gate.take_paint(now) {
        return None;
    }
    let full = std::mem::take(&mut st.need_full);
    Some(full)
}

impl St {
    /// 该补 `Still working…` 了吗（在等首字、已经等了 8 s、还没画过）。
    fn still_due(&self, now: u64) -> bool {
        let waited = now.saturating_sub(self.start_ms).max(self.waited_hint);
        self.on
            && still_due(
                self.gate.busy() > 0,
                self.got_token,
                self.still_shown,
                waited,
            )
    }
}

/// 文字带该不该在屏上（模式开着、而且上一帧真画过）。
pub fn should_be_painted() -> bool {
    let st = lock();
    st.on && st.painted
}

/// 文字带"还在屏上"的探针点：`(x, y, 期望灰度)`。
///
/// **只看结构像素**（2px 分隔线、带子最底那条留白、`Close` 的 2px 边框），
/// 不看正文 —— 正文写什么都有可能。左右两端 + 中间都要查：真机上踩过的坑就是
/// `Chrome::draw_in` 把 x<120 那一列整列刷白，只查中点的话查不出来（`Live N`
/// 被啃成 `ote`，探针却还是"好的"）。
pub fn band_probes(w: i32, h: i32) -> Vec<(i32, i32, u8)> {
    let t = band_top(h);
    let mut v = Vec::with_capacity(10);
    // 分隔线：左端 / 1/4 / 中点 / 3/4 / 右端
    for x in [0, w / 4, w / 2, w * 3 / 4, w - 1] {
        v.push((x, t + 1, 0));
    }
    // 带子最底下那一行必须还是纸白（正文区到不了那里）
    for k in 1..4 {
        let x = w / 4 * k;
        v.push((x, h - 6, 255));
    }
    // `Close` 的边框：上边中点 + 左边中点
    let (cx, cy, cw, ch) = close_rect(w, h);
    v.push((cx + cw / 2, cy, 0));
    v.push((cx, cy + ch / 2, 0));
    v
}

/// 采到的值全对？
pub fn probes_ok(vals: &[u8], want: &[(i32, i32, u8)]) -> bool {
    if vals.len() != want.len() {
        return false;
    }
    vals.iter()
        .zip(want.iter())
        .all(|(got, (_, _, w))| (*got as i32 - *w as i32).abs() < 48)
}

/// 屏上那些探针还对不对（每帧十几像素，比区域重算便宜得多）。
///
/// 文字带已经画进 `paint_overlays`（正常 overlay 路径），所以整屏/局部重绘
/// 都会被正确补回来；这里只剩一种情况：**笔迹本身划进带子**——墨迹的局部刷新
/// 不走 `paint_overlays`，刷过来就把带子啃一块。探针发现后 60 ms 内补画。
pub fn looks_painted(fb: &Fb, w: i32, h: i32) -> bool {
    let probes = band_probes(w, h);
    let vals: Vec<u8> = probes
        .iter()
        .map(|(x, y, _)| fb.get(*x, *y))
        .collect();
    probes_ok(&vals, &probes)
}

/// 强制重画一次（被人盖掉了 / 状态刚变）。
pub fn force_repaint() {
    let mut st = lock();
    st.need_full = true;
    st.gate.token();
}

/// 状态条上那一句英文（官方口径，不自造）。
///
/// 回答本身**不再显示在带子里**——它已经变成纸上的墨迹了。带子只回答
/// "现在在干什么"，以及"刚写进去多少"。
pub fn status_text() -> &'static str {
    let st = lock();
    if let Some(e) = &st.err {
        // 失败原因可能很长：状态条只放固定一句，细节在日志里
        let _ = e;
        return "Could not reach the server";
    }
    if st.gate.busy() > 0 {
        return if st.got_token {
            "Writing to page…"
        } else {
            "Thinking…"
        };
    }
    match st.last_ink {
        Some(_) => "Done",
        None => "Listening…",
    }
}

/// 重画状态条：只写帧缓冲，**不推面板**（刷哪块由调用方定）。
pub fn paint_band(fb: &mut Fb, text: &mut Option<Text>, w: i32, h: i32) {
    let st = lock();
    let t = band_top(h);
    let bh = band_h(h);
    // 底色 + 顶部一条分隔线
    fb.fill_rect(0, t, w, bh, gray565(255));
    fb.fill_rect(0, t, w, DIVIDER, gray565(0));

    // 左边：一个 24×24 的状态块。忙 = 实心（在推），闲 = 空心（在听）。
    let busy = st.gate.busy() > 0;
    let by = t + (bh - 24) / 2;
    if busy {
        fb.fill_rect(PAD_X, by, 24, 24, gray565(0));
    } else {
        widgets::outline_rect(fb, PAD_X, by, 24, 24, 2, 0);
    }

    // 中间一行：`Live Note` + 状态
    let status = status_text_locked(&st);
    if let Some(tx) = text.as_mut() {
        let lw = tx.width(26.0, "Live Note") as i32;
        tx.draw(fb, PAD_X + 40, t + bh / 2 + 10, 26.0, "Live Note", 0);
        tx.draw(fb, PAD_X + 40 + lw + 22, t + bh / 2 + 10, 24.0, status, 90);
    }

    // 右边：`Close`
    let (cx, cy, cw, ch) = close_rect(w, h);
    widgets::outline_rect(fb, cx, cy, cw, ch, 2, 0);
    if let Some(tx) = text.as_mut() {
        let sw = tx.width(24.0, "Close") as i32;
        tx.draw(fb, cx + (cw - sw) / 2, cy + 36, 24.0, "Close", 0);
    }
}

/// 已经拿到锁的版本（`paint_band` 里不能再 `lock()`，会自锁）。
fn status_text_locked(st: &St) -> &'static str {
    if st.err.is_some() {
        return "Could not reach the server";
    }
    if st.gate.busy() > 0 {
        return if st.got_token {
            "Writing to page…"
        } else {
            "Thinking…"
        };
    }
    if st.last_ink.is_some() {
        "Done"
    } else {
        "Listening…"
    }
}

/// 取走这一轮的回答（主循环拿去渲染成墨迹）。只给一次。
pub fn take_reply() -> Option<String> {
    let mut st = lock();
    st.reply_out.take()
}

/// 记下"这一轮写进去多少条墨迹"，状态条上会显示 `Done`。
pub fn set_last_ink(n: usize) {
    let mut st = lock();
    st.last_ink = Some(n);
}

/// 开新一轮时清掉上一轮的 `Done`。
pub fn clear_last_ink() {
    let mut st = lock();
    st.last_ink = None;
}

/// 顶栏 `Live Note` 开关（只写帧缓冲）。`selected` = 开着。
pub fn paint_btn(fb: &mut Fb, text: &mut Option<Text>, w: i32, selected: bool) {
    let (bx, by, bw, bh) = btn_rect(w);
    if selected {
        // 选中：整块反白 + 4px 粗边框（16 级灰上没有颜色，只能靠这个）
        fb.fill_rect(bx, by, bw, bh, gray565(0));
        widgets::outline_rect(fb, bx, by, bw, bh, 4, 0);
        if let Some(tx) = text.as_mut() {
            let sw = tx.width(30.0, "Live Note") as i32;
            tx.draw(fb, bx + (bw - sw) / 2, by + 52, 30.0, "Live Note", 255);
        }
    } else {
        fb.fill_rect(bx, by, bw, bh, gray565(255));
        widgets::outline_rect(fb, bx, by, bw, bh, 2, 0);
        if let Some(tx) = text.as_mut() {
            let sw = tx.width(30.0, "Live Note") as i32;
            tx.draw(fb, bx + (bw - sw) / 2, by + 52, 30.0, "Live Note", 0);
        }
    }
}

/// 顶栏按钮还在屏上吗（被整屏重绘刷掉了就返回 false）。
///
/// 两处探针：外框那一行必须是黑的（选中/未选中都画 2px 边），内芯按状态对。
/// 只看内芯不够 —— 未选中的按钮是白底，纸面本来就是白的，探不出来。
pub fn btn_looks_painted(fb: &Fb, w: i32, selected: bool) -> bool {
    let (bx, by, bw, _) = btn_rect(w);
    let border = fb.get(bx + bw / 2, by + 1) < 40;
    let inner = fb.get(bx + 8, by + 8);
    let want = if selected { inner < 40 } else { inner > 200 };
    border && want
}

/// 画完标记"已经在屏上了"。
pub fn mark_painted(painted: bool) {
    lock().painted = painted;
}

// ---------------------------------------------------------------- 输入

/// 带内的命中。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BandHit {
    Close,
    Scroll,
}

/// 文字带上的命中（模式关着就恒 None）。
pub fn hit_band(x: f32, y: f32, w: i32, h: i32) -> Option<BandHit> {
    if !lock().on || !in_band(x, y, w, h) {
        return None;
    }
    if in_close(x, y, w, h) {
        Some(BandHit::Close)
    } else {
        Some(BandHit::Scroll)
    }
}

/// 按下开始拖：`(当前 y, 按下时的 scroll)`。
pub fn drag_start(y: f32) {
    let mut st = lock();
    st.drag = Some((y, st.scroll));
    st.grabbed = true;
}

/// 拖动（`y` 变大 = 往下拖 = 看更早的内容）。
pub fn drag_move(y: f32, w: i32, h: i32) {
    let mut st = lock();
    let Some((y0, s0)) = st.drag else { return };
    st.drag = Some((y, s0));
    let (_, _, tw, th) = content_rect(w, h);
    let n = wrap(&st.text, (tw - 10) as f32).len();
    let want = s0 + (y - y0) as i32;
    let s = clamp_scroll(n, th, want);
    if s != st.scroll {
        st.scroll = s;
        st.gate.token();
        st.need_full = true;
    }
}

/// 松手。
pub fn drag_end() -> bool {
    let mut st = lock();
    st.drag = None;
    let g = st.grabbed;
    st.grabbed = false;
    g
}

/// 上一次按下是我们吃掉的吗（决定抬笔要不要一起吃掉）。
pub fn grabbed() -> bool {
    let st = lock();
    st.grabbed || st.drag.is_some()
}

/// 出图用：直接摆一段假文本（不连服务器）。
pub fn seed_demo(text: &str) {
    let mut st = lock();
    st.on = true;
    st.text = text.to_string();
    st.err = None;
    // 出图要露出第一行：滚到顶。真流式是 `scroll = 0`（跟到最新）。
    st.scroll = i32::MAX / 2;
    st.order_demo();
}

/// `--render live:on` 的假流式文本（几行数学解释，正好多到能滚）。
pub const DEMO_TEXT: &str = "The curl of a vector field measures how much the field rotates \
around each point, and Stokes' theorem turns that local number into a global one.\n\
\n\
Stokes: the circulation of F around a closed loop equals the flux of curl F through \
any surface the loop bounds.\n\
\n\
Step 1. Take a small square of side h in the xy plane and walk around it.\n\
Step 2. Divide the circulation by h squared; the limit is the vertical component of \
the curl.\n\
Step 3. Sum over a grid: interior edges cancel in pairs, so only the boundary is \
left. That cancellation is the whole proof.";

/// 出图用：摆成"请求刚起来、还在等首字"的样子（`Thinking…` + `Still working…`）。
pub fn seed_thinking(waited_ms: u64) {
    let mut st = lock();
    st.on = true;
    st.text.clear();
    st.err = None;
    st.scroll = 0;
    st.order_demo();
    st.gate.mark_busy();
    st.gate.token();
    st.need_full = true;
    st.start_ms = now_ms().saturating_sub(waited_ms);
    st.waited_hint = waited_ms;
    st.got_token = false;
    st.still_shown = false;
}

/// 顶栏按钮现在该不该在屏上（工具条收起时收走）。
pub fn btn_shown() -> bool {
    lock().btn
}

/// 记下顶栏按钮该不该在屏上。
pub fn set_btn(on: bool) {
    lock().btn = on;
}

/// 按下一个"归我们"的点：跟着的抬指/抬笔也吃掉。
pub fn grab() {
    lock().grabbed = true;
}

impl St {
    /// 出图时把"已经在流"的状态摆好（`Streaming` 那个标和滚动条都要画出来）。
    fn order_demo(&mut self) {
        self.need_full = true;
        self.painted = false;
        self.gate = Gate::new();
    }
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    const H: i32 = 1872;
    const W: i32 = 1404;

    // ---- 几何 ----

    #[test]
    fn band_sits_at_the_bottom_of_the_screen() {
        let (x, y, w, h) = band_rect(W, H);
        assert_eq!(x, 0, "带子贴左边，整条横穿");
        assert_eq!(w, W);
        assert_eq!(y, H - h, "带子贴底：上沿 + 高度 == 屏高");
        assert_eq!(y + h, H, "不能越过屏底");
        assert_eq!(h, BAND_H);
        assert!(
            (80..=140).contains(&h),
            "状态条要窄（回答已经写到纸上去了）：{h}"
        );
    }

    #[test]
    fn upper_half_is_still_writable() {
        let top = band_top(H);
        assert_eq!(top, H - BAND_H);
        assert!(top > 0, "上半部可写区域必须 > 0");
        // 状态条只占一条窄边，可写区域比以前的 420 px 带子大得多
        assert!(top > 1700, "状态条不该占地：top={top}");
        assert!(top - crate::chrome::W >= 0);
    }

    #[test]
    fn hit_inside_and_outside_the_band() {
        let t = band_top(H) as f32;
        // 带内
        assert!(in_band(10.0, t + 1.0, W, H));
        assert!(in_band(700.0, t + 40.0, W, H));
        assert!(in_band(10.0, H as f32 - 1.0, W, H));
        // 带外：上面一个像素、右边一个像素、负坐标
        assert!(!in_band(10.0, t - 1.0, W, H));
        assert!(!in_band(700.0, 100.0, W, H));
        assert!(!in_band(W as f32, t + 1.0, W, H));
        assert!(!in_band(-1.0, t + 1.0, W, H));
    }

    #[test]
    fn close_button_is_inside_the_band_at_its_top_right() {
        let (cx, cy, cw, ch) = close_rect(W, H);
        assert!(in_close(cx as f32 + 1.0, cy as f32 + 1.0, W, H));
        assert!(in_close(cx as f32 + cw as f32 - 1.0, cy as f32 + ch as f32 - 1.0, W, H));
        // 整块都在带里
        assert!(in_band(cx as f32 + 1.0, cy as f32 + 1.0, W, H));
        assert!(cy >= band_top(H) + DIVIDER);
        assert_eq!(cx + cw, W - PAD_X, "Close 贴右边距");
        // 带外的不算
        assert!(!in_close(cx as f32 - 1.0, cy as f32 + 1.0, W, H));
        assert!(!in_close(cx as f32 + 1.0, (cy + ch) as f32 + 1.0, W, H));
    }

    #[test]
    fn top_bar_button_does_not_collide_with_the_close_cross() {
        let (bx, by, bw, bh) = btn_rect(W);
        assert!(bx > crate::chrome::W, "按钮在工具条右边");
        assert!(bx + bw <= 1300, "× 的命中区从 1300 开始，别撞上");
        assert_eq!((by, bh), (BTN_TOP, BTN_H));
        assert!(in_btn(bx as f32 + 1.0, by as f32 + 1.0, W));
        assert!(!in_btn(bx as f32 - 1.0, by as f32 + 1.0, W));
        assert!(!in_btn(bx as f32 + 1.0, 1000.0, W), "跟文字带是分开的");
        assert!(!in_band(bx as f32 + 1.0, by as f32 + 1.0, W, H));
    }

    #[test]
    fn tall_band_on_a_short_screen_still_leaves_a_top() {
        // 屏比带子还矮：要压到留下 MIN_TOP，而不是画到屏幕外面去
        let h = 420;
        let bh = band_h(h);
        assert!(band_top(h) >= MIN_TOP || bh == 0);
        assert!(band_top(h) + bh == h);
        assert!(content_rect(W, h).3 > 0);
    }

    // ---- 折行 / 裁剪 ----

    #[test]
    fn wrapped_lines_fit_the_band_width() {
        let mut s = String::new();
        for _ in 0..12 {
            s.push_str("The curl of a vector field measures how much it rotates ");
        }
        let max_w = (W - 2 * PAD_X - 10) as f32;
        let lines = wrap(&s, max_w);
        assert!(lines.len() > 3, "应该折成多行：{}", lines.len());
        for l in &lines {
            let w: f32 = l.chars().map(|c| char_w(c, FONT)).sum();
            assert!(w <= max_w + 0.5, "行超宽 {w} > {max_w}：{l:?}");
        }
    }

    #[test]
    fn wrap_breaks_hard_on_a_long_word_and_keeps_newlines() {
        let long = "x".repeat(400);
        let lines = wrap(&long, 200.0);
        assert!(lines.len() > 1);
        for l in &lines {
            let w: f32 = l.chars().map(|c| char_w(c, FONT)).sum();
            assert!(w <= 200.5, "硬断之后还超宽：{w}");
        }
        assert_eq!(wrap("first\n\nsecond", 500.0), vec!["first", "", "second"]);
        assert_eq!(wrap("only\n", 500.0), vec!["only"]);
        assert!(wrap("", 500.0).is_empty());
        // CJK 逐字可断
        let cjk = "汉字可以在任意位置折行";
        let lines = wrap(cjk, 60.0);
        assert!(lines.len() > 1);
        assert_eq!(lines.concat(), cjk);
    }

    /// 单测里直接改状态：`St` 是模块私有的，测试同模块可以锁。
    fn st_set_text(t: &str) {
        lock().text = t.to_string();
    }

    fn st_set_busy(b: bool) {
        let mut st = lock();
        if b {
            st.gate.mark_busy();
        } else {
            st.gate.finished();
        }
    }

    #[test]
    fn only_visible_lines_are_drawn_and_never_above_the_band() {
        // 新契约：状态条只画在 `band_top` 以下。以前这里测的是"正文滚动的可见行"，
        // 正文搬到纸上以后没有滚动这回事了 —— 但**不许画到带子上面**这条不能丢。
        let mut fb = Fb::open_mem();
        let mut text = Text::load();
        fb.clear(gray565(255));
        set_on(true);
        let long: String = (0..400).map(|i| format!("line {i}\n")).collect();
        st_set_text(&long);
        paint_band(&mut fb, &mut text, W, H);
        let top = band_top(H);
        for y in 0..top {
            for x in (0..W).step_by(97) {
                assert_eq!(fb.get(x, y), 255, "状态条画到 ({x},{y}) 去了（在带上沿以上）");
            }
        }
        assert!(
            (top..H).any(|y| (0..W).step_by(11).any(|x| fb.get(x, y) < 200)),
            "状态条得真画点东西出来"
        );
        set_on(false);
    }

    #[test]
    fn long_stream_text_does_not_paint_over_the_upper_half() {
        // 流式中间态（还没 Done）也不能画到带子上沿以上
        let mut fb = Fb::open_mem();
        let mut text = Text::load();
        for n in [0usize, 1, 200, 4000] {
            fb.clear(gray565(255));
            set_on(true);
            st_set_text(&"流式文字 ".repeat(n));
            st_set_busy(true);
            paint_band(&mut fb, &mut text, W, H);
            let top = band_top(H);
            for y in 0..top {
                for x in (0..W).step_by(131) {
                    assert_eq!(fb.get(x, y), 255, "n={n} 画到 ({x},{y}) 去了");
                }
            }
            st_set_busy(false);
            set_on(false);
        }
    }

    // ---- 防抖 / 单飞 / 限频 ----

    #[test]
    fn pen_up_fires_exactly_once_after_the_debounce() {
        let mut g = Gate::new();
        g.pen_up(1_000);
        // 没到点
        assert_eq!(g.poll(1_000 + DEBOUNCE_MS - 1, true), None);
        // 正好到点：只触发一次
        assert_eq!(g.poll(1_000 + DEBOUNCE_MS, true), Some(Act::Start));
        assert_eq!(g.starts(), 1);
        for t in 0..20 {
            assert_eq!(g.poll(1_000 + DEBOUNCE_MS + t * 10, true), None);
        }
        assert_eq!(g.starts(), 1, "一次抬笔只起一条流");
        // 内容没变：到点也不请求
        let mut g = Gate::new();
        g.pen_up(5_000);
        assert_eq!(g.poll(5_000 + DEBOUNCE_MS, false), None);
        assert_eq!(g.starts(), 0);
    }

    #[test]
    fn writing_during_the_debounce_restarts_the_timer() {
        let mut g = Gate::new();
        g.pen_up(0);
        // 防抖期间又抬了一笔
        g.pen_up(600);
        // 原来的 1200 到点了，但计时器已经重置 → 不该触发
        assert_eq!(g.poll(1_200, true), None);
        assert_eq!(g.poll(1_799, true), None);
        assert_eq!(g.poll(1_800, true), Some(Act::Start));
        assert_eq!(g.starts(), 1, "连着写几笔也只起一条流");
    }

    #[test]
    fn a_new_trigger_while_streaming_cancels_the_old_one() {
        let mut g = Gate::new();
        g.pen_up(0);
        assert_eq!(g.poll(DEBOUNCE_MS, true), Some(Act::Start));
        assert_eq!(g.busy(), 1);
        // 流还在跑，又写完一笔
        g.pen_up(2_000);
        assert_eq!(
            g.poll(2_000 + DEBOUNCE_MS, true),
            Some(Act::CancelAndStart)
        );
        assert_eq!(g.busy(), 1, "严格单飞：同一时刻只有一条");
        assert_eq!(g.starts(), 2);
        assert_eq!(g.cancels(), 1);
        // 结束以后不会再自己起一条
        g.finished();
        assert_eq!(g.busy(), 0);
        assert_eq!(g.poll(9_999, true), None);
        assert_eq!(g.starts(), 2);
    }

    #[test]
    fn repaint_is_throttled_to_one_per_window() {
        let mut g = Gate::new();
        // 60ms 里来了 4 个 token
        for t in [0u64, 10, 20, 30] {
            g.token();
            assert!(!g.take_paint(t), "窗口内不该重画（t={t}）");
        }
        assert!(g.take_paint(60), "到 60ms 才放行一次");
        assert!(!g.take_paint(60), "同一时刻不会再放一次");
        // 之后再来 token：又要等一个窗口
        g.token();
        assert!(!g.take_paint(100));
        assert!(g.take_paint(120));
        assert!(!g.take_paint(200), "没新东西就不重画");
    }

    // ---- SSE ----

    fn feed_all(s: &mut Sse, lines: &[&str]) -> Vec<SseEv> {
        let mut out = Vec::new();
        for l in lines {
            let ev = s.feed(l);
            if ev != SseEv::None {
                out.push(ev);
            }
        }
        out
    }

    #[test]
    fn sse_parses_token_done_and_error() {
        let mut s = Sse::new();
        assert_eq!(
            feed_all(
                &mut s,
                &[
                    "event: token",
                    "data: {\"t\":\"Hel\"}",
                    "",
                    "event: token",
                    "data: {\"t\":\"lo\"}",
                    "",
                    "event: done",
                    "data: {\"usage\":{\"in\":1,\"out\":2}}",
                    "",
                ]
            ),
            vec![
                SseEv::Token("Hel".into()),
                SseEv::Token("lo".into()),
                // usage 要跟着 done 一起带回来（带子里/日志里都要用）
                SseEv::Done(Some((1, 2)))
            ]
        );
        let mut s = Sse::new();
        assert_eq!(
            feed_all(
                &mut s,
                &[
                    "event: error",
                    "data: {\"message\":\"boom\"}",
                    "",
                ]
            ),
            vec![SseEv::Err("boom".into())]
        );
    }

    #[test]
    fn sse_ignores_heartbeats_and_half_json() {
        let mut s = Sse::new();
        assert_eq!(feed_all(&mut s, &[": keepalive", "", ": ping"]), vec![]);
        // 半截 JSON：不 panic、不吐 token
        assert_eq!(
            feed_all(
                &mut s,
                &["event: token", "data: {\"t\":\"hel", "", "event: token", "data: nope", ""]
            ),
            vec![]
        );
        // 收到半截之后还能继续正常解析
        assert_eq!(
            feed_all(&mut s, &["event: token", "data: {\"t\":\"ok\"}", ""]),
            vec![SseEv::Token("ok".into())]
        );
        // t 不是字符串 / 空串：丢掉
        assert_eq!(
            feed_all(&mut s, &["event: token", "data: {\"t\":1}", ""]),
            vec![]
        );
        assert_eq!(
            feed_all(&mut s, &["event: token", "data: {\"t\":\"\"}", ""]),
            vec![]
        );
    }

    #[test]
    fn sse_tolerates_crlf_and_multi_line_data() {
        let mut s = Sse::new();
        assert_eq!(
            feed_all(
                &mut s,
                &[
                    "event: token\r",
                    "data: {\"t\":\"hi\"}\r",
                    "\r",
                ]
            ),
            vec![SseEv::Token("hi".into())]
        );
        // JSON 里的转义换行照样原样还原
        assert_eq!(
            feed_all(&mut s, &["event: token", "data: {\"t\":\"a\\nb\"}", ""]),
            vec![SseEv::Token("a\nb".into())]
        );
        // 两个 data 行拼起来（SSE 规范：中间插换行）→ JSON 还是好的
        let mut s = Sse::new();
        assert_eq!(
            feed_all(
                &mut s,
                &[
                    "event: token",
                    "data: {\"t\":\"x\"}",
                    "data: ",
                    "",
                ]
            ),
            vec![SseEv::Token("x".into())]
        );
        // done 没带 data 也算一条
        let mut s = Sse::new();
        assert_eq!(
            feed_all(&mut s, &["event: done", ""]),
            vec![SseEv::Done(None)]
        );
        // 没有 event 的裸 data 行不算事件
        let mut s = Sse::new();
        assert_eq!(feed_all(&mut s, &["data: {\"t\":\"z\"}", ""]), vec![]);
    }

    // ---- 整页 PNG ----

    /// 整页灰度里**印刷底图**必须先被合成进来（调用方给的就是合成好的那张），
    /// 编码之后解回来还得在。真机上踩过的坑：只画笔迹 → PDF 习题页被模型读成
    /// 「这页基本是空白」。
    #[test]
    fn page_png_keeps_the_printed_page() {
        let (w, h) = (crate::fb::W, crate::fb::H);
        let mut gray = vec![255u8; w * h];
        // 印刷内容：每 40 行一道 12px 的"字行"
        for y in (0..h).step_by(40) {
            for dy in 0..12 {
                for x in 200..1200 {
                    gray[(y + dy) * w + x] = 60;
                }
            }
        }
        // 一笔手写（黑）+ 一块灰（铅笔）
        for x in 300..900 {
            gray[1700 * w + x] = 0;
        }
        let png = page_png(&gray);
        assert!(!png.is_empty(), "整页要能编出 PNG");
        let (pw, ph, back) = crate::panels::ai::png_to_gray(&png).expect("PNG 要能解回来");
        assert_eq!((pw, ph), (w, h), "整页图就是 1404×1872，不做 2x");
        // 印刷字行（用户要问的题目）必须还在：第 80..91 行是一道字行
        assert!(back[85 * w + 700] < 120, "印刷底图丢了（{}）", back[85 * w + 700]);
        assert_eq!(back[85 * w + 700], gray[85 * w + 700]);
        // 字行之间仍然是纸白
        assert!(back[100 * w + 700] > 200);
        // 笔迹也在
        assert!(back[1700 * w + 600] < 80, "墨迹丢了");
        // 尺寸不对的灰度直接拒（别拿半张图去问模型）
        assert!(page_png(&vec![255u8; 10]).is_empty());
    }

    // ---- 等首字的提示 ----

    /// 8 s 才补 `Still working…`；补过一次就不补；首字来了也不补。
    #[test]
    fn still_working_appears_once_after_eight_seconds() {
        assert!(!still_due(true, false, false, 0));
        assert!(!still_due(true, false, false, THINKING_MS - 1));
        assert!(still_due(true, false, false, THINKING_MS), "满 8 s 要补");
        assert!(still_due(true, false, false, THINKING_MS + 30_000));
        // 补过了就不再刷（否则每帧都在强制重画）
        assert!(!still_due(true, false, true, THINKING_MS + 1));
        // 首字已经来了：正文在流，不补
        assert!(!still_due(true, true, false, THINKING_MS + 1));
        // 没在等（空闲/出错/关掉）：不补
        assert!(!still_due(false, false, false, THINKING_MS + 1));
    }

    // ---- 探针 ----

    /// 探针只挑结构像素，而且**左右两端都要有**（左边那一列被工具条刷白是历史 bug）。
    #[test]
    fn band_probes_cover_the_whole_band() {
        let probes = band_probes(W, H);
        assert!(probes.len() >= 8, "探针太少（{}）", probes.len());
        let t = band_top(H);
        // 分隔线上从左到右都有探针：左端、右端、中点
        let divider: Vec<i32> = probes.iter().filter(|(_, y, _)| *y == t + 1).map(|(x, _, _)| *x).collect();
        assert!(divider.contains(&0), "分隔线左端要有探针（{divider:?}）");
        assert!(divider.contains(&(W - 1)), "分隔线右端要有探针");
        assert!(divider.iter().any(|x| *x == W / 2), "分隔线中点要有探针");
        // 全部落在带子里、且期望值只有纯黑/纸白（A2 只驱得动这两档）
        for (x, y, want) in &probes {
            assert!((0..W).contains(x) && (0..H).contains(y), "探针越界 {x},{y}");
            assert!(*y >= t && *y < H, "探针跑到带子上面了：{x},{y}");
            assert!(*want == 0 || *want == 255, "期望值只能是纯黑/纸白");
        }
        // 最底那一行在正文区下面（正文不会画到那儿，永远是纸白）
        let (_, cy, _, ch) = content_rect(W, H);
        assert!(cy + ch < H - 6, "带底留白要给探针留出来");
    }

    /// 真画一条带子：探针要过；把左边那一列刷白（真机上工具条干的事）要能发现；
    /// 再画一遍就恢复 —— 这就是 "整页重绘之后带子仍然完整" 的最小模型。
    #[test]
    fn painted_band_passes_probes_and_survives_a_column_wipe() {
        let mut fb = Fb::open_mem();
        // 先铺一层"页面"（灰阶，方便区分）
        fb.fill_rect(0, 0, W, H, gray565(190));
        let mut no_text: Option<Text> = None;
        set_on(true);
        paint_band(&mut fb, &mut no_text, W, H);
        assert!(looks_painted(&fb, W, H), "自己画的带子必须过探针");
        // 工具条把 x<120 整列刷白（`Chrome::draw_in` 就是 fill_rect(0,0,120,屏高)）
        fb.fill_rect(0, 0, crate::chrome::W, H, gray565(255));
        assert!(
            !looks_painted(&fb, W, H),
            "左边那一条被刷白必须能发现（真机上就是这么被啃掉 `Live N` 的）"
        );
        // 正常 overlay 路径 = 重画一遍带子
        paint_band(&mut fb, &mut no_text, W, H);
        assert!(looks_painted(&fb, W, H), "补画之后就完整了");
        // 分隔线左端、带子左下角、Close 边框都是对的
        assert!(fb.get(0, band_top(H) + 1) < 40);
        assert!(fb.get(0, H - 6) > 200);
        // 只把带底中间一小块涂成页面灰，也要能发现（区域采样，不是一个点）
        let mid = W / 2;
        fb.fill_rect(mid - 6, H - 8, 12, 6, gray565(120));
        assert!(!looks_painted(&fb, W, H), "带底被盖也要能发现");
        set_on(false);
        mark_painted(false);
    }
}
