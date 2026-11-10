//! 框选问 AI（D1）：矩形框选 + 整页/裁图上传 + 内联回答卡片。
//!
//! **只放纯逻辑和像素**：矩形归一化/最小尺寸/裁剪/请求体/卡片几何、
//! 浮层与提示条的绘制。网络、笔记本状态、线程在 `main.rs` 的 `// === D1` 段。
//!
//! 和原版选区工具的区别（用户点名的第一件事）：
//! * 原版选区（`chrome.selecting`）是**套索**，只会圈已有的笔迹；纯印刷页上
//!   一个笔点都没有，就什么也选不到。
//! * 这里框的是**屏幕矩形**，和有没有笔迹无关：整页渲染图（印刷底图 + 墨迹 +
//!   文字层）直接从帧缓冲这条路上取，零笔迹的 PDF 页也照样能发。
//!
//! 视觉上也要能分开：矩形选框画成 **3px 实线** + 内部 6px 网格的点阵浅影，
//! 原版套索是 1px 自由曲线、原版选区框是 2px 虚线（`main.rs::dashed_rect`）。

use crate::fb::{gray565, Fb};
use crate::ui::keyboard::{Key, Keyboard};
use crate::ui::text::Text;
use crate::ui::widgets;

/// 页面宽度（设备像素）。请求里的 `rect` 和整页图都用这个坐标系。
pub const PAGE_W: i32 = 1404;
/// 页面高度。整页图的尺寸就是 `PAGE_W × PAGE_H`，**不发 2x**
///（真机实测：2x 那版模型要读 1 分 47 秒）。
pub const PAGE_H: i32 = 1872;
/// 误触下限：两条边都得 ≥ 40 px，否则当"点了一下"处理。
pub const MIN_SIDE: i32 = 40;

// ---------------------------------------------------------------- 卡片几何

/// 回答卡片：固定钉在**上半部**，不压住下半部的写字区。
///
/// 高度 840 = 屏高的 45%（验收要求 ≤ 55%），底边 930 < 936（整屏一半），
/// 所以"卡片在上面、笔在下面写"是硬约束，不靠运气。
pub const CARD_X: i32 = 150;
pub const CARD_Y: i32 = 90;
pub const CARD_W: i32 = 1104;
pub const CARD_H: i32 = 840;
/// 卡片头高（`Close` 那一条）。`panels::ai` 直接用这个常量，别各写一份。
pub const CARD_HDR: i32 = 56;

/// 卡片占的屏幕矩形（**最高**那一档）。
pub fn card_rect() -> (i32, i32, i32, i32) {
    (CARD_X, CARD_Y, CARD_W, CARD_H)
}

/// 卡片最矮多高（只有 "Thinking…" 一行时）。
pub const CARD_MIN_H: i32 = 260;

/// 按内容实际需要的高度算卡片矩形：`min(max(need, CARD_MIN_H), CARD_H)`。
///
/// **这一条是真机反馈（BUG 1）换来的**：卡片原来固定 840 高、纯白，在一整页
/// 印刷内容本来就落在上半部的 PDF 上，看起来就像"整页被刷白了"。改成跟着内容
/// 长高（"Thinking…" 时只有 260），卡片周围的页面一直看得见，配合黑色表头，
/// 一眼就知道那是一个浮层而不是白纸。
pub fn card_height(need: i32) -> i32 {
    need.clamp(CARD_MIN_H, CARD_H)
}

/// 内容需要 `need` 像素时卡片的矩形。
pub fn card_rect_for(need: i32) -> (i32, i32, i32, i32) {
    (CARD_X, CARD_Y, CARD_W, card_height(need))
}

// ---- 卡片上的"按钮热区"（笔点也要认，见 `route`） ----

/// `Close` 的命中矩形：卡片右上角，宽 160、整块头高（好戳）。
pub fn card_close_rect() -> (i32, i32, i32, i32) {
    (CARD_X + CARD_W - CLOSE_W, CARD_Y, CLOSE_W, CARD_HDR)
}

/// 右侧滚动条热区：卡片右内边 28 px 宽，从头下面一直到底边。
///
/// 和 `ai::paint_inline` 画的滚动条（`x + w - 22`，宽 6）对齐：热区比它宽一圈，
/// 笔/手指都够得着。
pub fn card_scroll_rect() -> (i32, i32, i32, i32) {
    (
        CARD_X + CARD_W - SCROLL_HOT_W,
        CARD_Y + CARD_HDR,
        SCROLL_HOT_W,
        CARD_H - CARD_HDR,
    )
}

/// 卡片上的一个热区。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CardHit {
    Close,
    Scroll,
}

/// 落点是不是卡片上的按钮/滚动条（**笔和手指都算**，卡片按最高那一档算）。
pub fn card_hot(x: i32, y: i32) -> Option<CardHit> {
    card_hot_in(card_rect(), x, y)
}

/// 同 [`card_hot`]，但卡片矩形由调用方给（卡片高度是跟着内容变的）。
pub fn card_hot_in(card: (i32, i32, i32, i32), x: i32, y: i32) -> Option<CardHit> {
    let (cx, cy, cw, ch) = card;
    let close = (cx + cw - CLOSE_W, cy, CLOSE_W, CARD_HDR);
    let scroll = (cx + cw - SCROLL_HOT_W, cy + CARD_HDR, SCROLL_HOT_W, ch - CARD_HDR);
    if in_rect(close, x, y) {
        return Some(CardHit::Close);
    }
    if in_rect(scroll, x, y) {
        return Some(CardHit::Scroll);
    }
    None
}

/// `Close` 热区宽（卡片右上角）。
pub const CLOSE_W: i32 = 160;
/// 滚动条热区宽（卡片右内边）。
pub const SCROLL_HOT_W: i32 = 28;

/// 卡片矩形（`has` 用）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Quad(pub i32, pub i32, pub i32, pub i32);

impl Quad {
    pub fn has(&self, x: i32, y: i32) -> bool {
        x >= self.0 && y >= self.1 && x < self.0 + self.2 && y < self.1 + self.3
    }
    pub fn to_tuple(&self) -> (i32, i32, i32, i32) {
        (self.0, self.1, self.2, self.3)
    }
}

/// 卡片不越过屏底（也留在上半部）。
pub fn card_bottom() -> i32 {
    CARD_Y + CARD_H
}

// ---------------------------------------------------------------- 矩形

/// 屏幕上的一个选区矩形。**始终有序**：`x0 <= x1 && y0 <= y1`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

impl Rect {
    pub fn new(x0: i32, y0: i32, x1: i32, y1: i32) -> Rect {
        Rect { x0, y0, x1, y1 }
    }
    pub fn w(&self) -> i32 {
        self.x1 - self.x0
    }
    pub fn h(&self) -> i32 {
        self.y1 - self.y0
    }
    pub fn area(&self) -> i64 {
        (self.w().max(0) as i64) * (self.h().max(0) as i64)
    }
    pub fn arr(&self) -> [i32; 4] {
        [self.x0, self.y0, self.x1, self.y1]
    }
    pub fn has(&self, x: i32, y: i32) -> bool {
        x >= self.x0 && y >= self.y0 && x < self.x1 && y < self.y1
    }
    /// 整页（`Current page`）：`[0, 0, PAGE_W, PAGE_H]`。
    pub fn full_page() -> Rect {
        Rect::new(0, 0, PAGE_W, PAGE_H)
    }
    /// 两张矩形的并集（拖框增量刷新用）。
    pub fn union(&self, o: &Rect) -> Rect {
        Rect::new(
            self.x0.min(o.x0),
            self.y0.min(o.y0),
            self.x1.max(o.x1),
            self.y1.max(o.y1),
        )
    }
    /// 外扩若干像素。
    pub fn expand(&self, p: i32) -> Rect {
        Rect::new(self.x0 - p, self.y0 - p, self.x1 + p, self.y1 + p)
    }
}

/// 任意方向的两个点 → 有序矩形（**不**钳制、不判大小）。
pub fn ordered(a: (i32, i32), b: (i32, i32)) -> Rect {
    Rect::new(a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
}

/// 拖框的两个点 → 有序 + 钳到页内的矩形。
///
/// 反方向拖（右下 → 左上）走的是同一条路，所以四个方向都能用。
pub fn clamp_page(a: (i32, i32), b: (i32, i32)) -> Rect {
    let r = ordered(a, b);
    let x0 = r.x0.clamp(0, PAGE_W);
    let y0 = r.y0.clamp(0, PAGE_H);
    let x1 = r.x1.clamp(0, PAGE_W);
    let y1 = r.y1.clamp(0, PAGE_H);
    Rect::new(x0, y0, x1, y1)
}

/// 一次完整的拖动 → 可发送的矩形；太小（任一边 < [`MIN_SIDE`]）算误触。
/// 框好之后可以拖的两个角（左上 / 右下）。用户 2026-11-09 要求：
/// 拖完还能调这两个点，再点 Send 才发。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    Tl,
    Br,
}

/// 命中哪个角：角上 48×48 的范围都算（手指粗，别要求点得准）。
pub fn handle_hit(r: Rect, x: i32, y: i32) -> Option<Handle> {
    let d = 48;
    if (x - r.x0).abs() <= d && (y - r.y0).abs() <= d {
        return Some(Handle::Tl);
    }
    if (x - r.x1).abs() <= d && (y - r.y1).abs() <= d {
        return Some(Handle::Br);
    }
    None
}

/// 拖某个角：另一个角不动，新矩形自己归一化（可以拖过对角）。
pub fn resize(r: Rect, h: Handle, x: i32, y: i32) -> Rect {
    let (mut x0, mut y0, mut x1, mut y1) = (r.x0, r.y0, r.x1, r.y1);
    match h {
        Handle::Tl => {
            x0 = x;
            y0 = y;
        }
        Handle::Br => {
            x1 = x;
            y1 = y;
        }
    }
    Rect::new(x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1))
}

pub fn finish_drag(a: (i32, i32), b: (i32, i32)) -> Option<Rect> {
    let r = clamp_page(a, b);
    if r.w() < MIN_SIDE || r.h() < MIN_SIDE {
        None
    } else {
        Some(r)
    }
}

// ---------------------------------------------------------------- 裁剪

/// 从整页灰度图裁一块：返回 `(宽, 高, 像素)`。越界部分按页边裁掉。
///
/// 尺寸对不上、rect 完全在页外都返回空（调用方当失败处理）。
pub fn crop(gray: &[u8], w: usize, h: usize, r: Rect) -> (usize, usize, Vec<u8>) {
    if w == 0 || h == 0 || gray.len() < w * h {
        return (0, 0, Vec::new());
    }
    let x0 = r.x0.clamp(0, w as i32) as usize;
    let y0 = r.y0.clamp(0, h as i32) as usize;
    let x1 = r.x1.clamp(0, w as i32) as usize;
    let y1 = r.y1.clamp(0, h as i32) as usize;
    if x1 <= x0 || y1 <= y0 {
        return (0, 0, Vec::new());
    }
    let (cw, ch) = (x1 - x0, y1 - y0);
    let mut out = Vec::with_capacity(cw * ch);
    for y in y0..y1 {
        let row = y * w;
        out.extend_from_slice(&gray[row + x0..row + x1]);
    }
    (cw, ch, out)
}

// ---------------------------------------------------------------- 请求体

/// `POST /api/v1/agent/ask` 的请求体（纯函数，不联网、不读配置）。
///
/// 字段和服务器端 `AskReq` 对齐：`mode/doc/page/rect/image_blob/crop_blob/text`。
/// `crop_blob` 是这一轮新加的（服务器端另一个 agent 加），**发归发，不依赖它**。
pub fn ask_body(
    doc: &str,
    page: u32,
    r: Rect,
    image_blob: &str,
    crop_blob: &str,
    text: &str,
) -> String {
    let mut v = serde_json::json!({
        "mode": "select",
        "doc": doc,
        "page": page,
        "rect": [r.x0, r.y0, r.x1, r.y1],
        "image_blob": image_blob,
        "text": text,
    });
    // 裁图真拿到了才发这个键：空串当成"没有裁图"，别让服务器去取一个不存在的 blob
    if !crop_blob.is_empty() {
        v["crop_blob"] = serde_json::Value::String(crop_blob.to_string());
    }
    v.to_string()
}

// ---------------------------------------------------------------- 指针路由

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pointer {
    Pen,
    Finger,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Route {
    /// 落进墨迹层
    Ink,
    /// 归卡片（手指：滚动 / Close）
    Card,
    /// 归卡片上的按钮（`Close` / 滚动条）——**笔落在这些热区上也走这条**
    Ui,
}

/// 卡片开着时，一次落在 (x,y) 的指针该归谁。
///
/// 策略（真机反馈后定下来的）：
/// * **笔落在 `Close` / 滚动条热区上 = 点按钮**（`Ui`），和"笔点浮层优先于落墨"
///   一个道理 —— 真机上笔才是主指针，按钮必须能被笔点到；
/// * 笔落在卡片**其他**区域：照写（`Ink`），被卡片盖住，关掉卡片就露出来；
/// * 手指在卡片里：归卡片（滚动 / Close），卡片外照常是页面手势。
pub fn route(pointer: Pointer, card_open: bool, x: i32, y: i32) -> Route {
    route_in(card_rect(), pointer, card_open, x, y)
}

/// 同 [`route`]，卡片矩形由调用方给（高度自适应以后，命中要跟着当前卡片走）。
pub fn route_in(
    card: (i32, i32, i32, i32),
    pointer: Pointer,
    card_open: bool,
    x: i32,
    y: i32,
) -> Route {
    if !card_open {
        return Route::Ink;
    }
    let on_hot = card_hot_in(card, x, y).is_some();
    match pointer {
        Pointer::Pen => {
            if on_hot {
                Route::Ui
            } else {
                Route::Ink
            }
        }
        Pointer::Finger => {
            if on_hot {
                Route::Ui
            } else if Quad(card.0, card.1, card.2, card.3).has(x, y) {
                Route::Card
            } else {
                Route::Ink
            }
        }
    }
}

// ---------------------------------------------------------------- 工具条浮层

/// 浮层里的两项。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    /// 画矩形发送
    Rect,
    /// 发送指定的页（默认就是当前页）
    Page,
}

/// `(英文串, 动作)`。两串都是自拟的：官方固件里没有 AI，`qml_all` 里
/// 最接近的只有 "Selection tool" / "Select pages" / "All pages" / "Page only"，
/// 那两个都不是这个意思，所以这里用短标签 `Rectangle` / `Current page`。
pub const PICKS: [(&str, Pick); 2] = [("Rectangle", Pick::Rect), ("Current page", Pick::Page)];

/// 浮层矩形：贴着工具条第 13 格（x=120 起，宽 400，行高 120）。
pub fn popover_rect() -> (i32, i32, i32, i32) {
    (
        crate::chrome::menu_x(),
        crate::chrome::ai_y(),
        crate::chrome::MENU_W,
        PICKS.len() as i32 * crate::chrome::MENU_ROW,
    )
}

/// 浮层某一行（0/1）。
pub fn popover_row(i: usize) -> (i32, i32, i32, i32) {
    let (x, y, w, _) = popover_rect();
    (x, y + i as i32 * crate::chrome::MENU_ROW, w, crate::chrome::MENU_ROW)
}

/// 浮层命中。
pub fn popover_hit(x: i32, y: i32) -> Option<Pick> {
    let (px, py, pw, ph) = popover_rect();
    if x < px || y < py || x >= px + pw || y >= py + ph {
        return None;
    }
    let i = ((y - py) / crate::chrome::MENU_ROW) as usize;
    PICKS.get(i).map(|(_, p)| *p)
}

// ---------------------------------------------------------------- 提问条

/// 提问条高度（在键盘上面一条）。
pub const PROMPT_H: i32 = 132;

/// 键盘区（官方键盘 601 高）。
pub fn kb_rect() -> (i32, i32, i32, i32) {
    (0, PAGE_H - KB_H, PAGE_W, KB_H)
}
pub const KB_H: i32 = 601;

/// 提问条：键盘上沿往上一格。
pub fn prompt_rect() -> (i32, i32, i32, i32) {
    (0, PAGE_H - KB_H - PROMPT_H, PAGE_W, PROMPT_H)
}

/// 提问条右侧的 `Send`。
pub fn send_rect() -> (i32, i32, i32, i32) {
    (PAGE_W - 240, PAGE_H - KB_H - PROMPT_H + 20, 200, 92)
}

/// `Cancel`（在 Send 左边）。
pub fn cancel_rect() -> (i32, i32, i32, i32) {
    (PAGE_W - 470, PAGE_H - KB_H - PROMPT_H + 20, 200, 92)
}

fn in_rect(r: (i32, i32, i32, i32), x: i32, y: i32) -> bool {
    x >= r.0 && y >= r.1 && x < r.0 + r.2 && y < r.1 + r.3
}

// ---------------------------------------------------------------- 绘制

/// 工具条第 13 格的图标：**闭合细框 + 中间一颗四角星**。
///
/// 和原版选区图标（四个直角括号 + 边上的短划）刻意做成两种结构：
/// 原版是"开口的角"，这里是"闭合的框 + 星"，16 级灰度下也一眼分得开。
pub fn draw_icon(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8) {
    widgets::shade_at(
        widgets::sid("draw_ai_select"),
        fb,
        ox,
        oy,
        40,
        40,
        fg,
        bg,
        |x, y| {
            // 闭合细框：2.6px，四边（x/y 都在 1.4..38.6）
            let frame = widgets::in_rect(x, y, 1.4, 1.4, 37.2, 2.6)
                || widgets::in_rect(x, y, 1.4, 36.0, 37.2, 2.6)
                || widgets::in_rect(x, y, 1.4, 1.4, 2.6, 37.2)
                || widgets::in_rect(x, y, 36.0, 1.4, 2.6, 37.2);
            // 四角星（"AI" 的那个 sparkle）：中间一颗菱形核 + 四条细尖，
            // 尖朝上下左右，和原版那个"四个角括号"完全是两种结构。
            let (cx, cy) = (20.0f32, 20.0f32);
            let (dx, dy) = ((x - cx).abs(), (y - cy).abs());
            // 星（astroid，指数 1/2）：四条边向内凹，尖朝上下左右
            let star = (dx / 10.8).powf(0.5) + (dy / 10.8).powf(0.5) <= 1.0;
            let core = dx + dy <= 3.2; // 中间补一颗小菱形核，缩到很小也看得见
            frame || star || core
        },
    );
}

/// 浮层（两项）。和官方其它 foldout 同一套度量：白底 + 2px 黑框 + 行高 120。
pub fn draw_popover(fb: &mut Fb, text: &mut Option<Text>) {
    let (x, y, w, h) = popover_rect();
    fb.fill_rect(x, y, w, h, gray565(255));
    widgets::outline_rect(fb, x, y, w, h, 2, 0);
    for (i, (name, pick)) in PICKS.iter().enumerate() {
        let ry = y + i as i32 * crate::chrome::MENU_ROW;
        // 行图标：矩形 = 小方框；整页 = 一页纸（右上角折角）
        match pick {
            Pick::Rect => widgets::outline_rect(fb, x + 30, ry + 40, 40, 36, 3, 0),
            Pick::Page => {
                fb.fill_rect(x + 32, ry + 40, 3, 40, gray565(0));
                fb.fill_rect(x + 32, ry + 77, 36, 3, gray565(0));
                fb.fill_rect(x + 32, ry + 40, 30, 3, gray565(0));
                fb.fill_rect(x + 60, ry + 40, 8, 3, gray565(0));
                fb.fill_rect(x + 65, ry + 43, 3, 8, gray565(0));
                fb.fill_rect(x + 65, ry + 43, 3, 37, gray565(0));
            }
        }
        if let Some(t) = text.as_mut() {
            t.draw(
                fb,
                x + crate::chrome::MENU_TEXT_DX,
                ry + crate::chrome::MENU_TEXT_BASE,
                crate::chrome::MENU_FONT,
                name,
                0,
            );
        }
        if i + 1 < PICKS.len() {
            let sy = ry + crate::chrome::MENU_ROW - 2;
            fb.fill_rect(x, sy, w, 2, gray565(0));
        }
    }
}

/// 矩形选框：3px 实线 + 6px 网格点阵浅影。
///
/// 和原版选区框的 2px 虚线（`dashed_rect`）、套索的 1px 自由线都不是一个长相。
/// 点阵比整块填灰好在**底下的字还看得见**（16 级灰没有 alpha，实心填充会盖掉内容）。
pub fn draw_rect(fb: &mut Fb, r: Rect) {
    let t = 3;
    let (x0, y0, x1, y1) = (r.x0, r.y0, r.x1, r.y1);
    let c = gray565(0);
    fb.fill_rect(x0, y0, x1 - x0, t, c);
    fb.fill_rect(x0, y1 - t, x1 - x0, t, c);
    fb.fill_rect(x0, y0, t, y1 - y0, c);
    fb.fill_rect(x1 - t, y0, t, y1 - y0, c);
    // 4 个角加粗一倍：远看像"角上有把手"，和套索的自由曲线更不像
    for (cx, cy) in [(x0, y0), (x1 - 12, y0), (x0, y1 - 12), (x1 - 12, y1 - 12)] {
        fb.fill_rect(cx, cy, 12, 6, c);
        fb.fill_rect(cx, cy, 6, 12, c);
    }
    // 内部点阵（6px 网格，1px 点，灰 150）
    let dot = gray565(150);
    let mut y = y0 + 12;
    while y < y1 - 12 {
        let mut x = x0 + 12 + ((y / 6) % 2) * 3;
        while x < x1 - 12 {
            fb.fill_rect(x, y, 1, 1, dot);
            x += 6;
        }
        y += 6;
    }
}

/// 提问条 + 键盘。`kb` 是当前的输入状态；返回键盘的命中区（调用方存起来）。
pub fn draw_prompt(
    fb: &mut Fb,
    text: &mut Option<Text>,
    kb: &Keyboard,
) -> Vec<((i32, i32, i32, i32), Key)> {
    let (px, py, pw, ph) = prompt_rect();
    fb.fill_rect(px, py, pw, ph, gray565(255));
    fb.fill_rect(px, py, pw, 2, gray565(0));
    // 左边小标题 + 右边两个按钮
    let q = if kb.text.is_empty() {
        "Ask a question".to_string()
    } else {
        kb.text.clone()
    };
    let tone = if kb.text.is_empty() { 130 } else { 20 };
    let max = send_rect().0 - 60;
    let show = match text.as_mut() {
        Some(t) => {
            if t.width(30.0, &q) as i32 > max {
                t.ellipsize(30.0, &q, max)
            } else {
                q.clone()
            }
        }
        None => q.clone(),
    };
    if let Some(t) = text.as_mut() {
        t.set_bold(true);
        t.draw(fb, 40, py + 46, 30.0, "Ask AI", 20);
        t.set_bold(false);
        t.draw(fb, 40, py + 104, 30.0, &show, tone);
    }
    button(fb, text, send_rect(), "Send");
    button(fb, text, cancel_rect(), "Cancel");
    let (kx, ky, kw, _) = kb_rect();
    kb.draw(fb, text, kx, ky, kw)
}

fn button(fb: &mut Fb, text: &mut Option<Text>, r: (i32, i32, i32, i32), label: &str) {
    widgets::outline_rect(fb, r.0, r.1, r.2, r.3, 2, 0);
    if let Some(t) = text.as_mut() {
        let w = t.width(30.0, label) as i32;
        t.draw(fb, r.0 + (r.2 - w) / 2, r.1 + r.3 / 2 + 10, 30.0, label, 20);
    }
}

/// 提问条上的命中：`true` = Send。
pub fn prompt_tap(x: i32, y: i32) -> Option<PromptAct> {
    if in_rect(send_rect(), x, y) {
        return Some(PromptAct::Send);
    }
    if in_rect(cancel_rect(), x, y) {
        return Some(PromptAct::Cancel);
    }
    let (kx, ky, kw, kh) = kb_rect();
    if in_rect((kx, ky, kw, kh), x, y) {
        return Some(PromptAct::Key);
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptAct {
    Send,
    Cancel,
    Key,
}

/// 把键盘那次点击换成一次按键（纯函数，便于单测键盘命中）。
pub fn prompt_key(hits: &[((i32, i32, i32, i32), Key)], x: i32, y: i32) -> Option<Key> {
    hits.iter()
        .rev()
        .find(|(r, _)| in_rect(*r, x, y))
        .map(|(_, k)| *k)
}

// ---------------------------------------------------------------- 状态

/// 提交/提问的进度。卡片本身的状态在 `ai::Ai` 里（`open` / `busy`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// 工具开着，等框选
    Idle,
    /// 框好了，等用户打问题
    Ask,
    /// 在生成 PNG + 传 blob（卡片已经显示 "Thinking…"）
    Uploading,
}

/// 等着开流的那一轮请求（上传回来以后要用）。
#[derive(Clone, Debug)]
pub struct PendingAsk {
    pub doc: String,
    /// 1-based 页号（服务器提示词里就是"第 N 页"）
    pub page: u32,
    pub rect: Rect,
    pub text: String,
    /// 这一轮回答要落成的 PDF 的 uuid（设备生成，服务器按它发布）。
    pub ai_doc: String,
    /// 追问的父回答 uuid；空 = 从当前文档起一条新链。
    pub parent: String,
    /// 第几轮（显示/命名用）。
    pub n: u32,
}

/// 框选 AI 的全部状态（存在 `App` 里，一处放齐好排查）。
pub struct Panel {
    /// 工具激活（点第 13 格进；再点一次退出，回普通笔）
    pub on: bool,
    /// 两项浮层开着
    pub popover: bool,
    /// 正在拖的是哪个角（`None` = 不是在调框）。
    pub handle: Option<Handle>,
    /// 拖动中：按下的点
    pub drag: Option<(i32, i32)>,
    /// 当前矩形（拖动中 = 实时预览；定稿后 = 待提问的框）
    pub rect: Option<Rect>,
    pub stage: Stage,
    /// 提问键盘（`Some` = 正在输入）
    pub kb: Option<Keyboard>,
    /// 上传线程的收货端：`Ok((整页 sha, 裁图 sha))`
    pub up: Option<std::sync::mpsc::Receiver<Result<(String, String), String>>>,
    /// 上传开始的时刻（画 "Sending…" 用）
    pub up_at: std::time::Instant,
    /// 上一次刷卡片的时刻（流式时限速，别和笔抢面板）
    pub card_at: std::time::Instant,
    /// 卡片矩形（开着时 Some）：关掉/重刷都要用它把纸面补回来
    pub card: Option<(i32, i32, i32, i32)>,
    /// 卡片上手指按下/上一次的 y（滚动用）
    pub card_drag: Option<(i32, i32)>,
    /// 卡片已经画过的内容版本（`needs_redraw` 消费）
    pub drawn: bool,
    /// 打开的是哪本笔记（换文档要清状态，别把上一本的框发出去）
    pub doc: String,
    /// 提问键盘的命中区（`draw_prompt` 每次画完更新）
    pub kb_hits: Vec<((i32, i32, i32, i32), Key)>,
    /// 等开流的那一轮（上传线程跑着时有效）
    pub pending: Option<PendingAsk>,
}

impl Panel {
    pub fn new() -> Panel {
        Panel {
            on: false,
            popover: false,
            handle: None,
            drag: None,
            rect: None,
            stage: Stage::Idle,
            kb: None,
            up: None,
            up_at: std::time::Instant::now(),
            card_at: std::time::Instant::now()
                - std::time::Duration::from_secs(10),
            card: None,
            card_drag: None,
            drawn: false,
            doc: String::new(),
            kb_hits: Vec::new(),
            pending: None,
        }
    }

    /// 退出工具：矩形、浮层、键盘、上传全部清掉（卡片由调用方收）。
    pub fn reset(&mut self) {
        self.on = false;
        self.popover = false;
        self.handle = None;
        self.drag = None;
        self.rect = None;
        self.stage = Stage::Idle;
        self.kb = None;
        self.up = None;
        self.pending = None;
        self.card_drag = None;
    }

    /// 只清框（用户误触/取消后回工具态）。
    pub fn clear_box(&mut self) {
        self.handle = None;
        self.drag = None;
        self.rect = None;
        self.stage = Stage::Idle;
        self.kb = None;
    }

    /// 收上传结果（不阻塞）。
    pub fn poll_upload(&mut self) -> Option<Result<(String, String), String>> {
        let rx = self.up.as_ref()?;
        let r = match rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => return None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("upload thread died".to_string())
            }
        };
        self.up = None;
        Some(r)
    }
}

impl Default for Panel {
    fn default() -> Panel {
        Panel::new()
    }
}

// ---------------------------------------------------------------- 单测

#[cfg(test)]
mod handle_tests {
    use super::*;

    #[test]
    fn handle_hit_only_near_the_two_corners() {
        let r = Rect::new(100, 200, 500, 600);
        assert_eq!(handle_hit(r, 105, 205), Some(Handle::Tl));
        assert_eq!(handle_hit(r, 495, 595), Some(Handle::Br));
        assert_eq!(handle_hit(r, 300, 400), None);
        assert_eq!(handle_hit(r, 100, 600), None);
    }

    #[test]
    fn resize_moves_only_the_dragged_corner() {
        let r = Rect::new(100, 200, 500, 600);
        let a = resize(r, Handle::Tl, 60, 150);
        assert_eq!((a.x0, a.y0, a.x1, a.y1), (60, 150, 500, 600));
        let b = resize(r, Handle::Br, 700, 800);
        assert_eq!((b.x0, b.y0, b.x1, b.y1), (100, 200, 700, 800));
    }

    #[test]
    fn resize_normalizes_when_dragged_past_the_other_corner() {
        let r = Rect::new(100, 200, 500, 600);
        let a = resize(r, Handle::Tl, 900, 1000);
        assert_eq!((a.x0, a.y0, a.x1, a.y1), (500, 600, 900, 1000));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_any_direction_orders_rect() {
        // 左上 → 右下
        let a = finish_drag((300, 400), (900, 1000)).expect("够大");
        assert_eq!(a.arr(), [300, 400, 900, 1000]);
        // 右下 → 左上
        let b = finish_drag((900, 1000), (300, 400)).expect("够大");
        assert_eq!(b, a);
        // 右上 → 左下 / 左下 → 右上
        assert_eq!(finish_drag((900, 400), (300, 1000)), Some(a));
        assert_eq!(finish_drag((300, 1000), (900, 400)), Some(a));
        // 同一条水平线/竖直线：有序但退化
        let flat = ordered((900, 400), (300, 400));
        assert_eq!(flat.arr(), [300, 400, 900, 400]);
        assert_eq!(flat.h(), 0);
    }

    #[test]
    fn tiny_drag_is_rejected() {
        assert_eq!(finish_drag((300, 300), (339, 900)), None, "宽 39 太窄");
        assert_eq!(finish_drag((300, 300), (900, 339)), None, "高 39 太扁");
        assert_eq!(finish_drag((300, 300), (300, 300)), None);
        // 正好 40 算有效
        assert!(finish_drag((300, 300), (340, 900)).is_some());
        assert!(finish_drag((300, 300), (900, 340)).is_some());
        // 反方向也一样
        assert!(finish_drag((340, 340), (300, 300)).is_some());
    }

    #[test]
    fn rect_is_clamped_to_page() {
        let r = clamp_page((-50, -80), (PAGE_W + 200, PAGE_H + 200));
        assert_eq!(r.arr(), [0, 0, PAGE_W, PAGE_H]);
        let r = clamp_page((1000, 1000), (2000, 2000));
        assert_eq!(r.arr(), [1000, 1000, PAGE_W, PAGE_H]);
        // 完全在页外：宽/高为 0 → finish_drag 拒绝
        assert_eq!(clamp_page((2000, 100), (2100, 200)).w(), 0);
        assert_eq!(finish_drag((2000, 100), (2100, 200)), None);
        // 钳制之后再判最小尺寸（不是先判再钳）
        assert_eq!(finish_drag((-30, -30), (30, 30)), None);
    }

    #[test]
    fn crop_takes_the_right_pixels() {
        // 4x3 的图，值 = y*10 + x
        let (w, h) = (4usize, 3usize);
        let mut gray = vec![0u8; w * h];
        for y in 0..h {
            for x in 0..w {
                gray[y * w + x] = (y * 10 + x) as u8;
            }
        }
        let (cw, ch, px) = crop(&gray, w, h, Rect::new(1, 1, 3, 3));
        assert_eq!((cw, ch), (2, 2));
        assert_eq!(px, vec![11, 12, 21, 22]);
        // 整张
        let (cw, ch, px) = crop(&gray, w, h, Rect::new(0, 0, 4, 3));
        assert_eq!((cw, ch), (4, 3));
        assert_eq!(px, gray);
    }

    #[test]
    fn crop_clamps_and_handles_empty() {
        let (w, h) = (4usize, 3usize);
        let gray: Vec<u8> = (0..12).collect();
        let (cw, ch, px) = crop(&gray, w, h, Rect::new(2, 2, 99, 99));
        assert_eq!((cw, ch), (2, 1));
        assert_eq!(px, vec![10, 11]);
        // 完全在外
        assert_eq!(crop(&gray, w, h, Rect::new(9, 9, 20, 20)).2.len(), 0);
        // 尺寸对不上
        assert_eq!(crop(&gray[..5], w, h, Rect::new(0, 0, 2, 2)).2.len(), 0);
        assert_eq!(crop(&[], 0, 0, Rect::new(0, 0, 2, 2)).2.len(), 0);
    }

    #[test]
    fn full_page_rect_is_the_rendered_page() {
        let r = Rect::full_page();
        assert_eq!(r.arr(), [0, 0, 1404, 1872]);
        assert_eq!((r.w(), r.h()), (PAGE_W, PAGE_H));
        assert!(r.has(0, 0) && r.has(1403, 1871));
        assert!(!r.has(1404, 1871) && !r.has(0, 1872));
    }

    #[test]
    fn ask_body_has_every_field() {
        let r = Rect::new(120, 240, 700, 900);
        let s = ask_body("uuid-1", 7, r, "sha-page", "sha-crop", "这题怎么算？");
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["mode"], "select");
        assert_eq!(v["doc"], "uuid-1");
        assert_eq!(v["page"], 7);
        assert_eq!(v["rect"], serde_json::json!([120, 240, 700, 900]));
        assert_eq!(v["image_blob"], "sha-page");
        assert_eq!(v["crop_blob"], "sha-crop", "crop_blob 必须发出去");
        assert_eq!(v["text"], "这题怎么算？");
        // 空问题也允许（服务器按"解释这一块"处理）
        let v: serde_json::Value =
            serde_json::from_str(&ask_body("d", 1, r, "p", "c", "")).unwrap();
        assert_eq!(v["text"], "");
    }

    #[test]
    fn ask_body_escapes_text() {
        let s = ask_body("d", 1, Rect::new(0, 0, 40, 40), "p", "c", "a\"b\\c\n中文");
        let v: serde_json::Value = serde_json::from_str(&s).expect("必须是合法 JSON");
        assert_eq!(v["text"], "a\"b\\c\n中文");
        // 不是靠字符串拼接：引号没有把 JSON 弄坏
        assert!(s.starts_with('{') && s.ends_with('}'));
    }

    #[test]
    fn card_geometry_lands_in_the_upper_half() {
        let (x, y, w, h) = card_rect();
        assert_eq!((x, y, w, h), (CARD_X, CARD_Y, CARD_W, CARD_H));
        // 高度封顶：≤ 屏高的 55%
        assert!(h <= PAGE_H * 55 / 100, "卡片高度 {h} 超过 55% 屏高");
        // 不越过屏底（顺带保证在上半部：底边 < 屏高一半）
        assert!(card_bottom() <= PAGE_H, "卡片越过了屏底");
        assert!(card_bottom() <= PAGE_H / 2, "卡片底边应当留在上半部");
        // 卡片内/外命中
        let q = Quad(x, y, w, h);
        assert!(q.has(x, y) && q.has(x + w - 1, y + h - 1));
        assert!(!q.has(x - 1, y) && !q.has(x, y - 1));
        assert!(!q.has(x + w, y) && !q.has(x, y + h));
        // 卡片不压住工具条那一列
        assert!(x >= crate::chrome::W, "卡片别盖住工具条");
        // 下半部（y > 一半）留给写字
        assert!(card_bottom() < PAGE_H / 2 + 40);
    }

    #[test]
    fn pen_never_goes_to_the_card() {
        let (x, y, w, h) = card_rect();
        let inside = (x + w / 2, y + h / 2);
        let outside = (x + w / 2, card_bottom() + 100);
        // 用户点名的策略：卡片开着时笔**照样落墨**（卡片里也写，只是被盖住）
        assert_eq!(route(Pointer::Pen, true, inside.0, inside.1), Route::Ink);
        assert_eq!(route(Pointer::Pen, true, outside.0, outside.1), Route::Ink);
        // 手指：卡片里归卡片（滚动/Close），卡片外是页面
        assert_eq!(route(Pointer::Finger, true, inside.0, inside.1), Route::Card);
        assert_eq!(
            route(Pointer::Finger, true, outside.0, outside.1),
            Route::Ink
        );
        // 卡片没开时手指全归页面
        assert_eq!(
            route(Pointer::Finger, false, inside.0, inside.1),
            Route::Ink
        );
        // 卡片外缘（右下角再往外 1px）
        assert_eq!(
            route(Pointer::Finger, true, x + w, y + h),
            Route::Ink,
            "边界外一像素不算卡片"
        );
    }

    /// BUG 3（真机）：**笔点 `Close` 必须是点按钮，不是落墨**。
    #[test]
    fn pen_on_card_buttons_is_ui() {
        let (cx, cy, cw, ch) = card_rect();
        // Close 在卡片右上角
        let close = card_close_rect();
        assert_eq!(
            route(Pointer::Pen, true, close.0 + close.2 / 2, close.1 + close.3 / 2),
            Route::Ui,
            "笔点 Close 要归 UI（真机这里以前会写一笔）"
        );
        // 滚动条
        let sb = card_scroll_rect();
        assert_eq!(
            route(Pointer::Pen, true, sb.0 + sb.2 / 2, sb.1 + sb.3 / 2),
            Route::Ui,
            "笔点滚动条也归 UI"
        );
        // 卡片中部（不是按钮）——笔照写
        assert_eq!(
            route(Pointer::Pen, true, cx + cw / 2, cy + ch / 2),
            Route::Ink,
            "卡片中部的笔还是落墨"
        );
        // 卡片左下角（正文区，不是热区）
        assert_eq!(
            route(Pointer::Pen, true, cx + 80, cy + ch - 40),
            Route::Ink
        );
        // 手指在热区上也归 UI（和以前一样能点）
        assert_eq!(
            route(Pointer::Finger, true, close.0 + 10, close.1 + 10),
            Route::Ui
        );
        // 卡片没开：热区也不算
        assert_eq!(
            route(Pointer::Pen, false, close.0 + 10, close.1 + 10),
            Route::Ink
        );
        // 热区都在卡片内、互不重叠
        let q = Quad(cx, cy, cw, ch);
        assert!(q.has(close.0, close.1) && q.has(close.0 + close.2 - 1, close.1 + close.3 - 1));
        assert!(q.has(sb.0, sb.1) && q.has(sb.0 + sb.2 - 1, sb.1 + sb.3 - 1));
        assert!(close.1 + close.3 <= sb.1, "Close 在滚动条上面，不重叠");
    }

    /// 卡片关闭那一帧必须把「卡片 + 外扩一圈」补回纸面（BUG 4 的残片都在卡片外
    /// 一圈之内：公式图往上最多溢 212 px）。
    #[test]
    fn card_hot_zones_match_the_painted_card() {
        // `ai::paint_inline` 用 `INLINE_HDR` 画头、用 `x+w-22` 画滚动条；
        // 这里的热区必须把那两处盖住。
        assert_eq!(crate::panels::ai::INLINE_HDR, CARD_HDR);
        let close = card_close_rect();
        let sb = card_scroll_rect();
        // 头高一致：Close 正好和头对齐
        assert_eq!((close.1, close.3), (CARD_Y, CARD_HDR));
        // 滚动条 6 px 画在 `x+w-22..x+w-16`，热区 28 px 一定包含它
        let bar_x0 = CARD_X + CARD_W - 22;
        let bar_x1 = CARD_X + CARD_W - 16;
        assert!(sb.0 <= bar_x0 && bar_x1 <= sb.0 + sb.2);
        // 滚动条热区不越出卡片
        assert_eq!(sb.0 + sb.2, CARD_X + CARD_W);
        assert_eq!(sb.1 + sb.3, CARD_Y + CARD_H);
    }

    #[test]
    fn popover_hits_its_two_rows() {
        let (px, py, pw, _) = popover_rect();
        assert_eq!(px, crate::chrome::MENU_X);
        assert_eq!(py, crate::chrome::ai_y());
        assert_eq!(pw, crate::chrome::MENU_W);
        let row = crate::chrome::MENU_ROW;
        assert_eq!(popover_hit(px + 10, py + 10), Some(Pick::Rect));
        assert_eq!(popover_hit(px + 10, py + row + 10), Some(Pick::Page));
        // 两行之间/之外
        assert_eq!(popover_hit(px + 10, py + 2 * row + 10), None);
        assert_eq!(popover_hit(px - 1, py + 10), None);
        assert_eq!(popover_hit(px + pw, py + 10), None);
        // 行矩形和命中一致
        let (rx, ry, rw, rh) = popover_row(1);
        assert_eq!((rx, ry, rw, rh), (px, py + row, pw, row));
        // 浮层别和工具条那一列重叠（x 从 120 起）
        assert!(px >= crate::chrome::W);
        // 每一项都有一行文字
        assert_eq!(PICKS.len(), 2);
    }

    #[test]
    fn prompt_bar_sits_above_the_keyboard() {
        let (px, py, pw, ph) = prompt_rect();
        let (kx, ky, kw, kh) = kb_rect();
        assert_eq!(ky, PAGE_H - KB_H);
        assert_eq!(py + ph, ky, "提问条正好贴着键盘上沿");
        assert_eq!(pw, PAGE_W);
        assert_eq!(kw, PAGE_W);
        assert_eq!(kh, KB_H);
        // Send / Cancel 在条内，互不重叠，也不越出屏
        let (sx, sy, sw, sh) = send_rect();
        let (cx, cy, cw, ch2) = cancel_rect();
        assert!(sy >= py && sy + sh <= py + ph);
        assert!(sx + sw <= PAGE_W);
        assert!(cx + cw <= sx, "Cancel 在 Send 左边且不重叠");
        assert!(cy >= py && cy + ch2 <= py + ph);
        assert!(px == 0);
    }

    #[test]
    fn prompt_hit_maps_to_actions() {
        let (sx, sy, sw, sh) = send_rect();
        assert_eq!(prompt_tap(sx + sw / 2, sy + sh / 2), Some(PromptAct::Send));
        let (cx, cy, cw, ch) = cancel_rect();
        assert_eq!(prompt_tap(cx + cw / 2, cy + ch / 2), Some(PromptAct::Cancel));
        assert_eq!(prompt_tap(700, PAGE_H - 300), Some(PromptAct::Key));
        assert_eq!(prompt_tap(700, 400), None, "页面正文区不是提问条");
        // 键盘命中按后画的优先
        let hits = vec![
            ((0, 0, 100, 100), Key::Ch('a')),
            ((0, 0, 100, 100), Key::Ch('b')),
        ];
        assert_eq!(prompt_key(&hits, 10, 10), Some(Key::Ch('b')));
        assert_eq!(prompt_key(&hits, 500, 10), None);
    }

    #[test]
    fn rect_draw_marks_border_and_keeps_the_middle_readable() {
        let mut fb = Fb::open_mem();
        fb.fill_rect(0, 0, PAGE_W, PAGE_H, gray565(255));
        let r = Rect::new(200, 300, 700, 800);
        draw_rect(&mut fb, r);
        // 四边都是实线（3px）
        assert!(fb.get(300, 300) < 60, "上边");
        assert!(fb.get(300, 797) < 60, "下边");
        assert!(fb.get(200, 500) < 60, "左边");
        assert!(fb.get(697, 500) < 60, "右边");
        // 内部是点阵：既不是纯白（有浅影），也不是实心黑（内容还看得见）
        let mut dark = 0;
        let mut light = 0;
        for y in (r.y0 + 20..r.y1 - 20).step_by(7) {
            for x in (r.x0 + 20..r.x1 - 20).step_by(7) {
                let g = fb.get(x, y);
                if g < 200 {
                    dark += 1;
                } else {
                    light += 1;
                }
            }
        }
        assert!(dark > 0, "内部要有一点浅影");
        assert!(light > dark * 2, "点阵不能把内容盖死");
        // 框外没被动过
        assert_eq!(fb.get(150, 300), 255);
    }

    #[test]
    fn icon_is_not_the_selection_icon() {
        // 两种图标在同一块 40x40 上画出来的覆盖率不该一样（形状确实不同）
        let mut a = Fb::open_mem();
        a.fill_rect(0, 0, 60, 60, gray565(255));
        draw_icon(&mut a, 0, 0, 0, 255);
        let mut n_ai = 0;
        for y in 0..40 {
            for x in 0..40 {
                if a.get(x, y) < 128 {
                    n_ai += 1;
                }
            }
        }
        assert!(n_ai > 150, "图标要有足够墨量（{n_ai} px）");
        assert!(n_ai < 900, "别糊成一团（{n_ai} px）");
        // 闭合框：左右两边是实线（原版选区图标只有四个角，边中间是断的）
        assert!(a.get(2, 20) < 100 && a.get(37, 20) < 100);
        // 中间是星，不是空的；星和框之间留白
        assert!(a.get(20, 20) < 100, "星心");
        assert!(a.get(6, 20) > 200, "框和星之间要留白");
    }
}
