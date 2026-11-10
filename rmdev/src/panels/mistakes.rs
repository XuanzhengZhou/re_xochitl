//! 错题档案（C13 设备端）：整屏列表，数据来自自建服务器 `/api/v1/profile`。
//!
//! 入口在库的侧栏 `Mistakes`（左上 ☰ 打开的那条）。点进去是整屏列表，
//! 左上角返回箭头回库。结构：
//!
//! | 区域 | 几何 |
//! |---|---|
//! | 黑头：返回箭头 + `Mistakes` + 条数 | 0..120（= 上安全区） |
//! | 列表 | 120..1680（13 行 × 120） |
//! | 滚动提示 | 1692..1732（还有上/下没看到时才画） |
//! | 底部安全区 | 1752..1872（留空） |
//!
//! 每行的视觉编码（16 级灰没有颜色，只能靠形状和深浅）：
//!
//! * 左 33 起 24×24 的优先级标记：`high` = 实心方块、`normal` = 空心方块、
//!   `low` = 一条短横线；
//! * 72 起一行元信息：`错 N 次`（只有 N > 1 才画）+ `难度 d`（只画 1..=5）；
//! * 再往右是题目原文（单行，超宽用 `Text::ellipsize` 截断加 `…`）；
//! * 行右端 `Learned` 按钮（150×80，笔和手指都能点）；已学会的行整行浅灰、
//!   不画按钮。
//!
//! 网络（同 `d5`/`c12` 的约定）：
//!
//! * 拉列表 / 标记学会都在后台线程里跑，UI 线程只 `poll()` 里的 `try_recv`；
//! * 没配对 → 一行 `Pair a server first`；失败 → 一行 `Could not load mistakes`；
//!   不 panic、不弹模态；
//! * 排序完全照服务器给的顺序（优先级 → 最近时间），这里不重排。

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use serde::Deserialize;

use crate::fb::{gray565, Fb};
use crate::net::{self, Config};
use crate::panels::{ai, teximg};
use crate::ui::text::Text;
use crate::ui::widgets;

/// 顶部黑头（= 上安全区，返回箭头和标题都在这一条里）
pub const HDR_H: i32 = 120;
/// 行高：手指好点
pub const ROW_H: i32 = 120;
/// 列表顶边
pub const LIST_TOP: i32 = HDR_H;
/// 底部安全区（状态栏那条，列表不占）
pub const SAFE_BOTTOM: i32 = 120;
/// 手指/笔上下拖多少像素滚一行（和 `panels::toc` 同一档手感）
pub const DRAG_STEP: f32 = 90.0;

/// 面板是整页 sheet：真机永远是 1404×1872，命中判定用这组常量。
const SCREEN_W: i32 = 1404;
const SCREEN_H: i32 = 1872;

const MARGIN: i32 = 33;
const MARK: i32 = 24;
const META_X: i32 = 72;
/// 元信息和题目之间的空
const META_GAP: i32 = 24;
const BTN_W: i32 = 150;
const BTN_H: i32 = 80;
const BTN_MARGIN: i32 = 32;
/// 题目右边和按钮之间留的空
const TEXT_GAP: i32 = 20;

const FONT_TITLE: f32 = 40.0;
const FONT_COUNT: f32 = 36.0;
/// 题目原文里行内公式的目标高度（行高 120、正文 27 px，公式留一点余量）。
const MATH_H: i32 = 38;
const FONT_TEXT: f32 = 27.0;
const FONT_META: f32 = 24.0;
const FONT_BTN: f32 = 24.0;
const FONT_STATE: f32 = 32.0;

const INK: u8 = 20;
/// 已学会的行整行浅灰（不是虚线）
const LEARNED_INK: u8 = 178;
const MUTED: u8 = 110;
/// 行底分隔线
const RULE: u8 = 228;

/// 标题（官方没有这个界面，串自拟）。
pub const TITLE: &str = "Mistakes";
/// 空列表整屏中间那行（自拟）。
pub const EMPTY: &str = "No mistakes yet";
/// 没配对：复用仓库里既有的那句（`c12_grade_page` / D5 也在用）。
pub const PAIR_FIRST: &str = "Pair a server first";
/// 拉列表失败（自拟，英文一行）。
pub const LOAD_FAILED: &str = "Could not load mistakes";
/// 标记学会成功的 toast（自拟）。
pub const MARKED: &str = "Marked as learned";
/// 标记学会失败（自拟；只弹通知条，不弹模态）。
pub const MARK_FAILED: &str = "Could not mark as learned";
const LOADING: &str = "Loading";
const LEARNED: &str = "Learned";

/// `GET /api/v1/profile` 的一条。只收界面要用的字段，其它字段（`doc`、
/// `subject`、`first_seen`…）serde 默认忽略；缺字段一律走默认值，不 panic。
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct Problem {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub difficulty: Option<i64>,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub priority: String,
    /// 服务器一定给；缺省当 1（第一次错，不显示"错 N 次"）
    #[serde(default = "one")]
    pub repeat_count: i64,
}

fn one() -> i64 {
    1
}

/// 优先级只有三档；认不出的（缺字段、写错）按 `normal` 画。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Priority {
    High,
    Normal,
    Low,
}

impl Priority {
    /// 服务器串 → 三档（缺字段/写错都按 `normal`，画出来至少有个方块）。
    pub fn of(s: &str) -> Priority {
        match s.trim().to_ascii_lowercase().as_str() {
            "high" => Priority::High,
            "low" => Priority::Low,
            _ => Priority::Normal,
        }
    }
}

/// `错 N 次`：只有 N > 1 才显示（第一次错不算"重复"）。
pub fn repeat_label(n: i64) -> Option<String> {
    (n > 1).then(|| format!("错 {n} 次"))
}

/// `难度 d`：只认 1..=5；字段缺省（`None`）或越界都不显示。
pub fn difficulty_label(d: Option<i64>) -> Option<String> {
    d.filter(|d| (1..=5).contains(d))
        .map(|d| format!("难度 {d}"))
}

/// 一行元信息：两段用 `·` 隔开；都没有就是空串（连位置都不占）。
pub fn meta_label(p: &Problem) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = repeat_label(p.repeat_count) {
        parts.push(s);
    }
    if let Some(s) = difficulty_label(p.difficulty) {
        parts.push(s);
    }
    parts.join(" · ")
}

/// 已经学会的那一行：整行浅灰、不画 `Learned` 按钮。
pub fn is_learned(p: &Problem) -> bool {
    p.status.trim().eq_ignore_ascii_case("learned")
}

fn one_line(s: &str) -> String {
    // 题目原文字段是模型从手写里转述的，可能带换行；这一页只有单行。
    s.chars()
        .map(|c| if c == '\n' || c == '\r' { ' ' } else { c })
        .collect()
}

/// 题目文字的起点与最大宽度：元信息后面空 `META_GAP`，右边给按钮让位。
///
/// 纯函数，`panels::mistakes` 的单测直接拿它算"截断后不许超过右边界"。
pub fn text_span(w: i32, meta_w: i32) -> (i32, i32) {
    let x = META_X + if meta_w > 0 { meta_w + META_GAP } else { 0 };
    let right = w - BTN_MARGIN - BTN_W - TEXT_GAP;
    (x, (right - x).max(60))
}

/// 一行 `Learned` 按钮的矩形（纯函数）。
pub fn learned_btn(w: i32, y: i32) -> (i32, i32, i32, i32) {
    (w - BTN_MARGIN - BTN_W, y + (ROW_H - BTN_H) / 2, BTN_W, BTN_H)
}

/// 一屏能放几整行：上下各留 120 的安全区，1404×1872 上是 13 行。
pub fn rows_visible(h: i32) -> usize {
    ((h - LIST_TOP - SAFE_BOTTOM).max(0) / ROW_H) as usize
}

/// 解析 `GET /api/v1/profile` 的返回。空列表 / 缺 `problems` 都算空；
/// 非法 JSON（含 `problems` 不是数组）报错，绝不 panic。
pub fn parse_list(body: &str) -> Result<Vec<Problem>, String> {
    #[derive(Deserialize)]
    struct ListResp {
        #[serde(default)]
        problems: Vec<Problem>,
    }
    let resp: ListResp = serde_json::from_str(body).map_err(|e| format!("bad profile JSON: {e}"))?;
    Ok(resp.problems)
}

/// 点到了什么。`Learned` 带的是**全局行号**（含已经滚上去的行）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Back,
    Learned(usize),
}

/// `poll()` 往主循环报的事件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// 列表拉到了（空列表也算成功）
    Loaded,
    /// 拉列表失败，附英文原因（只写日志，界面固定一行 `Could not load mistakes`）
    LoadFailed(String),
    /// 标记学会的回复
    Learned { id: i64, ok: bool, why: String },
}

enum Reply {
    List(Result<Vec<Problem>, String>),
    Learned { id: i64, ok: bool, why: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    Loading,
    Ready(Vec<Problem>),
    NoServer,
    Failed,
}

pub struct MistakesPanel {
    open: bool,
    state: State,
    /// 列表滚动首行（全局下标）
    top: usize,
    /// 配对配置（`learn` 拼 URL/token 用；没配对就是 `None`）
    cfg: Option<Config>,
    /// 一次只跑一趟网络；`Some` = 还有请求在路上
    rx: Option<Receiver<Reply>>,
    /// 正在跑的那趟是"标记学会"的话，记下 id（线程断了也能报对事件）
    learn_id: Option<i64>,
    /// 按下但还没抬：`(x, y, 拖动过没有)`。拖动过就不算点击。
    press: Option<(f32, f32, bool)>,
    /// 上次画的时候 [`teximg::epoch`] 是多少：公式图落地了就要补一帧
    tex_epoch: u64,
}

impl MistakesPanel {
    pub fn new() -> MistakesPanel {
        MistakesPanel {
            open: false,
            state: State::Loading,
            top: 0,
            cfg: None,
            rx: None,
            learn_id: None,
            press: None,
            tex_epoch: teximg::epoch(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// 打开整屏列表：没配对直接显示 `Pair a server first`，配对了就起后台线程拉。
    /// `cfg` 由调用方 `net::Config::load()` 传进来（单测可以传默认值，不读全局文件）。
    pub fn open(&mut self, cfg: Config) {
        self.open = true;
        self.top = 0;
        self.press = None;
        self.learn_id = None;
        self.rx = None;
        if !cfg.configured() {
            self.cfg = None;
            self.state = State::NoServer;
            return;
        }
        self.state = State::Loading;
        self.rx = Some(spawn_list(cfg.clone()));
        self.cfg = Some(cfg);
    }

    pub fn close(&mut self) {
        self.open = false;
        self.press = None;
    }

    /// 假数据/单测用：直接进"列表已就绪"状态（不连网）。
    pub fn show_ready(&mut self, list: Vec<Problem>) {
        self.open = true;
        self.top = 0;
        self.state = State::Ready(list);
    }

    /// 列表里还有公式没落地时返回 `true`，且每落地一张图只返回一次。
    ///
    /// 公式图是**异步**取的（[`teximg`] 的后台线程），第一帧只能画
    /// `[formula]` 占位。调用方每帧问一次，拿到 `true` 就重画一次——不能每帧
    /// 都重画，那会把 e-ink 刷爆。
    pub fn needs_repaint(&mut self) -> bool {
        let has_math = matches!(&self.state, State::Ready(v)
            if v.iter().any(|p| p.text.contains('$') || teximg::latexify(&p.text).contains('$')));
        if !has_math {
            return false;
        }
        let e = teximg::epoch();
        if e == self.tex_epoch {
            return false;
        }
        self.tex_epoch = e;
        true
    }

    /// 空态/错误态那行字；有列表时是 `None`（`draw` 不画居中行）。
    pub fn status_line(&self) -> Option<&'static str> {
        match &self.state {
            State::Loading => Some(LOADING),
            State::NoServer => Some(PAIR_FIRST),
            State::Failed => Some(LOAD_FAILED),
            State::Ready(v) if v.is_empty() => Some(EMPTY),
            State::Ready(_) => None,
        }
    }

    fn problems(&self) -> &[Problem] {
        match &self.state {
            State::Ready(v) => v,
            _ => &[],
        }
    }

    /// 当前滚到的窗口（测试/出图直接看内容）。
    pub fn visible(&self) -> &[Problem] {
        let v = self.problems();
        let end = (self.top + rows_visible(SCREEN_H)).min(v.len());
        if self.top >= end {
            return &[];
        }
        &v[self.top..end]
    }

    /// 滚 `step` 行（正数 = 往后看）。到顶/到底返回 false（主程序不必重画）。
    pub fn scroll(&mut self, step: i32) -> bool {
        let max = self
            .problems()
            .len()
            .saturating_sub(rows_visible(SCREEN_H).max(1));
        let before = self.top;
        self.top = (self.top as i32 + step).clamp(0, max as i32) as usize;
        self.top != before
    }

    /// 出图用：直接跳到第 `top` 行（夹在界内）。
    pub fn scroll_to(&mut self, top: usize) {
        let max = self
            .problems()
            .len()
            .saturating_sub(rows_visible(SCREEN_H).max(1));
        self.top = top.min(max);
    }

    /// 命中判定：左上 120×120 = 返回；行右端 150×80 = 标记学会。
    /// 已学会的行不响应按钮（没有按钮可点）。越界一律 `None`。
    pub fn hit_at(&self, x: i32, y: i32) -> Option<Act> {
        if x < 0 || y < 0 || x >= SCREEN_W {
            return None;
        }
        if x < HDR_H && y < HDR_H {
            return Some(Act::Back);
        }
        if y < LIST_TOP {
            return None;
        }
        let k = ((y - LIST_TOP) / ROW_H) as usize;
        if k >= rows_visible(SCREEN_H) {
            return None;
        }
        let i = self.top + k;
        let p = self.problems().get(i)?;
        if is_learned(p) {
            return None;
        }
        let (bx, by, bw, bh) = learned_btn(SCREEN_W, LIST_TOP + k as i32 * ROW_H);
        if x >= bx && x < bx + bw && y >= by && y < by + bh {
            return Some(Act::Learned(i));
        }
        None
    }

    /// 命中行 → 要 POST 的那条 id（纯函数，单测直接断言"只对命中行发请求"）。
    pub fn target_id(&self, act: Act) -> Option<i64> {
        match act {
            Act::Learned(i) => {
                let p = self.problems().get(i)?;
                (!is_learned(p)).then_some(p.id)
            }
            Act::Back => None,
        }
    }

    /// 标记第 `i` 行为已学会：后台 `POST /api/v1/profile/<id>/learned`。
    /// 返回发出去的 id。已经在跑别的请求 / 已学会 / 没配对时返回 `None`。
    pub fn learn(&mut self, i: usize) -> Option<i64> {
        if self.rx.is_some() {
            return None;
        }
        let id = self.target_id(Act::Learned(i))?;
        let cfg = self.cfg.clone()?;
        let url = format!("{}/api/v1/profile/{id}/learned", cfg.base());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let reply = match net::request(
                "POST",
                &url,
                &cfg.token,
                None,
                &[],
                Duration::from_secs(20),
            ) {
                Ok(r) if r.status == 200 => Reply::Learned {
                    id,
                    ok: true,
                    why: String::new(),
                },
                Ok(r) => Reply::Learned {
                    id,
                    ok: false,
                    why: r.error(),
                },
                Err(e) => Reply::Learned {
                    id,
                    ok: false,
                    why: e,
                },
            };
            let _ = tx.send(reply);
        });
        self.rx = Some(rx);
        self.learn_id = Some(id);
        Some(id)
    }

    /// 每帧一次：非阻塞收后台结果。还没回来返回 `None`。
    pub fn poll(&mut self) -> Option<Event> {
        let rx = self.rx.as_ref()?;
        let reply = match rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return None,
            // 线程没了却没发结果（panic / 被杀）：按连接断了处理
            Err(TryRecvError::Disconnected) => match self.learn_id.take() {
                Some(id) => Reply::Learned {
                    id,
                    ok: false,
                    why: "connection lost".to_string(),
                },
                None => Reply::List(Err("connection lost".to_string())),
            },
        };
        self.rx = None;
        self.learn_id = None;
        match reply {
            Reply::List(Ok(list)) => {
                self.state = State::Ready(list);
                self.top = 0;
                Some(Event::Loaded)
            }
            Reply::List(Err(e)) => {
                self.state = State::Failed;
                Some(Event::LoadFailed(e))
            }
            Reply::Learned { id, ok, why } => {
                if ok {
                    if let State::Ready(v) = &mut self.state {
                        if let Some(p) = v.iter_mut().find(|p| p.id == id) {
                            p.status = "learned".to_string();
                        }
                    }
                }
                Some(Event::Learned { id, ok, why })
            }
        }
    }

    /// 输入：按下只记锚点（`Up` 不带坐标，点击位置用按下点）。
    pub fn down(&mut self, x: f32, y: f32) {
        self.press = Some((x, y, false));
    }

    /// 拖动：每 `DRAG_STEP` 滚一行（手指往下拖 = 看前面的行）。
    /// 返回 true = 滚动窗口真的动了，调用方需要重画。
    pub fn moved(&mut self, _x: f32, y: f32) -> bool {
        let Some((x0, y0, _)) = self.press else {
            return false;
        };
        let dy = y - y0;
        if dy.abs() < DRAG_STEP {
            return false;
        }
        let step = if dy > 0.0 { -1 } else { 1 };
        let changed = self.scroll(step);
        // 锚点跟着手走：一次拖动可以连续滚多行；`moved` 保证抬笔不算点击
        self.press = Some((x0, y, true));
        changed
    }

    /// 抬起：没拖动过才算点击，返回命中的动作。
    pub fn up(&mut self) -> Option<Act> {
        let (x, y, moved) = self.press.take()?;
        if moved {
            return None;
        }
        self.hit_at(x as i32, y as i32)
    }

    /// 画整页。调用方负责推屏（`refresh`）。
    pub fn draw(&self, fb: &mut Fb, text: &mut Option<Text>) {
        let (w, h) = (fb.width, fb.height);
        fb.fill_rect(0, 0, w, h, gray565(255));

        // ---------- 黑头：返回 | Mistakes | 条数
        fb.fill_rect(0, 0, w, HDR_H, gray565(0));
        widgets::back_arrow(fb, 44, 60, 22, 255);
        if let Some(t) = text.as_mut() {
            t.draw_bold(fb, 140, 78, FONT_TITLE, TITLE, 255);
            if let State::Ready(v) = &self.state {
                let n = v.len().to_string();
                let nw = t.width_bold(FONT_COUNT, &n) as i32;
                t.draw_bold(fb, w - 32 - nw, 78, FONT_COUNT, &n, 255);
            }
        }

        // ---------- 空态 / 错误态：整屏中间一行
        if let Some(line) = self.status_line() {
            if let Some(t) = text.as_mut() {
                let tw = t.width(FONT_STATE, line) as i32;
                t.draw(fb, (w - tw) / 2, h / 2, FONT_STATE, line, MUTED);
            }
            return;
        }

        // ---------- 行：服务器给的顺序，不重排
        let rows = rows_visible(h);
        let v = self.problems();
        for k in 0..rows {
            let i = self.top + k;
            let Some(p) = v.get(i) else { break };
            let y = LIST_TOP + k as i32 * ROW_H;
            draw_row(fb, text, w, y, p);
        }

        // ---------- 滚动提示：上下还有没看到的才画箭头
        let up = self.top > 0;
        let down = self.top + rows < v.len();
        let hy = LIST_TOP + rows as i32 * ROW_H + 12;
        let cx = w / 2 - 20;
        match (up, down) {
            (true, true) => {
                widgets::icon_arrow_up(fb, cx - 46, hy, 150);
                widgets::icon_arrow_down(fb, cx + 46, hy, 150);
            }
            (true, false) => widgets::icon_arrow_up(fb, cx, hy, 150),
            (false, true) => widgets::icon_arrow_down(fb, cx, hy, 150),
            (false, false) => {}
        }
    }
}

impl Default for MistakesPanel {
    fn default() -> Self {
        MistakesPanel::new()
    }
}

/// 画一行。`y` 是行顶。
fn draw_row(fb: &mut Fb, text: &mut Option<Text>, w: i32, y: i32, p: &Problem) {
    let ink = if is_learned(p) { LEARNED_INK } else { INK };

    // 优先级标记：实心方块 / 空心方块 / 短横线
    let my = y + (ROW_H - MARK) / 2;
    match Priority::of(&p.priority) {
        Priority::High => fb.fill_rect(MARGIN, my, MARK, MARK, gray565(ink)),
        Priority::Normal => widgets::outline_rect(fb, MARGIN, my, MARK, MARK, 2, ink),
        Priority::Low => fb.fill_rect(MARGIN, y + ROW_H / 2 - 2, MARK, 4, gray565(ink)),
    }

    // 元信息 + 题目原文（单行截断）
    let mut meta_w = 0;
    if let Some(t) = text.as_mut() {
        let meta = meta_label(p);
        if !meta.is_empty() {
            t.draw(fb, META_X, y + 76, FONT_META, &meta, ink);
            meta_w = t.width(FONT_META, &meta) as i32;
        }
        let (tx, max_w) = text_span(w, meta_w);
        let src = one_line(&p.text);
        draw_problem(fb, t, tx, y + 76, max_w, &src, ink);
    }

    // `Learned` 按钮：已学会的行不画（整行淡显就够了）
    if !is_learned(p) {
        let (bx, by, bw, bh) = learned_btn(w, y);
        widgets::outline_rect(fb, bx, by, bw, bh, 2, ink);
        if let Some(t) = text.as_mut() {
            let tw = t.width(FONT_BTN, LEARNED) as i32;
            t.draw(
                fb,
                bx + (bw - tw) / 2,
                by + 52,
                FONT_BTN,
                LEARNED,
                ink,
            );
        }
    }

    // 行底细线（学会的行也一样，只是它本来就浅）
    fb.fill_rect(MARGIN, y + ROW_H - 1, w - 2 * MARGIN, 1, gray565(RULE));
}

/// 画题目原文：`$…$` 里的公式按**公式图**排（走 [`teximg`] 的缓存），
/// 还没取到就先画 `[formula]` 占位，其余照旧单行截断加 `…`。
///
/// 直接用 `ellipsize` 画原文会把 `$2+2=3$` 原样显示出来——真机上一眼看上去
/// 像排版坏了。这里把公式换成真正的排版图，行高不变（`MATH_H`）。
///
/// 返回 `true` = 这行还有公式在路上，调用方在 [`teximg::epoch`] 变了以后要重画。
fn draw_problem(
    fb: &mut Fb,
    t: &mut Text,
    x: i32,
    base: i32,
    max_w: i32,
    src: &str,
    ink: u8,
) -> bool {
    let right = x + max_w;
    let mut pen = x;
    let mut pending = false;
    let mut cut = false;

    // 老数据是 Unicode 伪公式（`∫_{0}^{+∞} x e^{-x} dx`），先转成 `$…$`
    let src = teximg::latexify(src);
    for seg in ai::split_math(&src) {
        if cut {
            break;
        }
        match seg {
            ai::Seg::Text(s) => {
                if s.is_empty() {
                    continue;
                }
                let avail = (right - pen).max(0);
                if avail < 8 {
                    cut = true;
                    break;
                }
                let shown = t.ellipsize(FONT_TEXT, &s, avail);
                if shown.ends_with('…') {
                    cut = true;
                }
                t.draw(fb, pen, base, FONT_TEXT, &shown, ink);
                pen += t.width(FONT_TEXT, &shown) as i32;
            }
            ai::Seg::Math(f) => {
                let Some(img) = teximg::cached(&f, 2) else {
                    teximg::request(&f, 2);
                    pending = true;
                    // 占位：英文，和 C8 的 `[formula]` 一致（e-ink 上没有红色）
                    let avail = (right - pen).max(0);
                    let shown = t.ellipsize(FONT_TEXT, "[formula]", avail);
                    if shown.ends_with('…') {
                        cut = true;
                    }
                    t.draw(fb, pen, base, FONT_TEXT, &shown, ink);
                    pen += t.width(FONT_TEXT, &shown) as i32;
                    continue;
                };
                // 公式图带 padding，先裁掉；再按剩余宽度决定缩到多高
                let c = img.cropped();
                if c.w == 0 {
                    continue;
                }
                let room = (right - pen).max(0);
                let mut h = MATH_H;
                let mut s = c.scaled_to_h(h as usize);
                if s.w as i32 > room {
                    // 放不下就等比缩，矮到 16 px 还不下就截断
                    h = ((room as f32) * (c.h as f32) / (c.w as f32)).floor() as i32;
                    if h < 16 {
                        cut = true;
                        break;
                    }
                    s = c.scaled_to_h(h as usize);
                }
                if s.w as i32 > room {
                    cut = true;
                    break;
                }
                // 基线对齐：公式底边压到文字基线上方 6 px（同 C8 行内公式）
                let top = base - s.h as i32 + 6;
                ai::blit_gray_scaled(
                    fb,
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
                pen += s.w as i32 + MATH_GAP;
            }
        }
    }
    if cut {
        let ell = "…";
        let avail = (right - pen).max(0);
        if avail >= t.width(FONT_TEXT, ell) as i32 {
            t.draw(fb, pen, base, FONT_TEXT, ell, ink);
        }
    }
    pending
}

/// 行内公式与前后文字之间留的空。
const MATH_GAP: i32 = 8;

fn spawn_list(cfg: Config) -> Receiver<Reply> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // `all`：已学会的也要在列表里（整行淡显），只拉 open 就永远看不到淡显行
        let url = format!("{}/api/v1/profile?status=all&limit=200", cfg.base());
        let r = match net::request(
            "GET",
            &url,
            &cfg.token,
            None,
            &[],
            Duration::from_secs(20),
        ) {
            Ok(resp) if resp.status == 200 => parse_list(&resp.text()),
            Ok(resp) => Err(resp.error()),
            Err(e) => Err(e),
        };
        let _ = tx.send(Reply::List(r));
    });
    rx
}

// ---------------------------------------------------------------- 出图

/// `--render mistakes:demo|empty|unpaired[:<top>]`：整屏出图（不连网、不读库）。
///
/// * `demo`（默认）：假数据 15 条（一屏 13 行 → 有 2 行要滚）：high / normal /
///   low 都有，多条 `repeat_count > 1`，两条 `learned`，一条长中文 + 一条超长
///   英文单词专门用来看截断；
/// * `demo:2`：滚到第 3 行，用来看翻页后的内容；
/// * `empty`：`No mistakes yet`；
/// * `unpaired`：没配对时的 `Pair a server first`（不建连接）。
pub fn render_demo(spec: &str, out: &str) {
    let spec = spec.trim_start_matches(':');
    let mut panel = MistakesPanel::new();
    if spec == "empty" {
        panel.show_ready(Vec::new());
    } else if spec == "unpaired" {
        panel.open(Config::default());
    } else {
        panel.show_ready(demo_problems());
        // `demo` / `demo:2` / `2` 都认：最后一段是数字就滚到那一行
        if let Some(top) = spec.rsplit(':').next().and_then(|s| s.parse::<usize>().ok()) {
            panel.scroll_to(top);
        }
    }
    let mut fb = Fb::open_mem();
    let mut text = Text::load();
    panel.draw(&mut fb, &mut text);
    // 公式图是后台线程取的：出图前等它落地（最多 6 s），不然只能看到占位。
    // 真机上这块由 `needs_repaint()` + 主循环重画补，这里是一次性出图，只能等。
    for _ in 0..120 {
        if !teximg::inflight() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !teximg::inflight() {
        panel.draw(&mut fb, &mut text);
    }
    match fb.dump_pgm(out) {
        Ok(()) => eprintln!(
            "[render] mistakes:{} → {out}（{} 条，滚到第 {} 行）",
            if spec.is_empty() { "demo" } else { spec },
            panel.problems().len(),
            panel.top + 1
        ),
        Err(e) => eprintln!("[render] 写 {out} 失败：{e}"),
    }
}

fn demo_problems() -> Vec<Problem> {
    let mut v = Vec::new();
    let mut push = |priority: &str,
                    repeat: i64,
                    difficulty: Option<i64>,
                    status: &str,
                    text: &str| {
        let id = v.len() as i64 + 1;
        v.push(Problem {
            id,
            text: text.to_string(),
            difficulty,
            status: status.to_string(),
            priority: priority.to_string(),
            repeat_count: repeat,
        });
    };
    push(
        "high",
        3,
        Some(5),
        "open",
        // 真实服务器存的老形状：Unicode 伪公式（走 `teximg::latexify` 转换）
        "1. 计算定积分：∫_{0}^{1} (3x^2 - 2x + 1) dx",
    );
    push(
        "high",
        2,
        Some(4),
        "open",
        // 新形状：已经是 `$…$` 的 TeX（直接送去排）
        r"2. 计算定积分：$\int_{0}^{\pi/2} \sin x \cos x\,dx$",
    );
    push(
        "high",
        1,
        Some(5),
        "open",
        "Supercalifragilisticexpialidocious-antidisestablishmentarianism-pneumonoultramicroscopicsilicovolcanoconiosis",
    );
    push(
        "normal",
        2,
        Some(3),
        "open",
        "解方程 2sin²x - 3sinx + 1 = 0，其中 x ∈ [0, 2π]。",
    );
    push(
        "normal",
        1,
        Some(2),
        "open",
        "计算定积分 ∫₀¹ (3x² + 2x + 1) dx。",
    );
    push(
        "normal",
        1,
        None,
        "open",
        "证明：任意三个连续整数之积都能被 6 整除。",
    );
    push(
        "normal",
        4,
        Some(3),
        "open",
        "在直角坐标系中，直线 l: y = kx + b 与圆 x² + y² = 4 相切，求 b² - 4k² 的值。",
    );
    push(
        "normal",
        1,
        Some(1),
        "open",
        "已知集合 A={x|x²-3x+2<0}，B={x|x>a}，若 A⊆B，求 a 的取值范围。",
    );
    push("low", 1, Some(2), "open", "化简：sin(π/2 + α) + cos(π - α)。");
    push(
        "low",
        1,
        Some(4),
        "open",
        "求函数 y = log₂(x² - 4x + 5) 的单调区间。",
    );
    push(
        "low",
        1,
        Some(3),
        "open",
        "已知等差数列 {a_n} 满足 a_1=2，a_3+a_5=18，求前 n 项和 S_n。",
    );
    push(
        "high",
        2,
        Some(3),
        "learned",
        "若 x + 1/x = 3，求 x² + 1/x² 的值。",
    );
    push(
        "normal",
        1,
        Some(5),
        "open",
        "一物体从高处以初速度 v₀ 水平抛出，不计空气阻力，求落地时间与水平射程。",
    );
    push(
        "high",
        2,
        Some(4),
        "open",
        "求过点 (1,2) 且与直线 2x - y + 3 = 0 垂直的直线方程。",
    );
    push(
        "normal",
        1,
        Some(2),
        "learned",
        "已知复数 z 满足 |z-1|=1，求 |z| 的最大值。",
    );
    v
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    /// 服务器 `Problem::json()` 的真实形状（字段一个不少，中文题面照真库那种写法）。
    const FIXTURE: &str = r#"{"count":2,"problems":[
        {"id":4,"doc":"12345678-90ab-cdef-1234-567890abcdef","page":7,
         "text":"已知函数 f(x)=x^3-3x+1，求 f(x) 在区间 [-2,2] 上的最大值与最小值。",
         "subject":"数学","difficulty":5,"status":"open","priority":"high",
         "repeat_count":2,"first_seen":"2026-11-08T09:12:00Z",
         "last_seen":"2026-11-09T01:58:50Z","learned_at":null,"note":null,
         "fingerprint":"9f2c4d8e0a1b3c5d7e9f1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d5e6f"},
        {"id":5,"doc":"12345678-90ab-cdef-1234-567890abcdef","page":9,
         "text":"化简：sin(π/2 + α) + cos(π - α)。",
         "subject":"数学","difficulty":2,"status":"learned","priority":"low",
         "repeat_count":1,"first_seen":"2026-11-08T09:12:00Z",
         "last_seen":"2026-11-08T09:12:00Z","learned_at":"2026-11-09T01:59:00Z",
         "note":null,"fingerprint":"11aa22bb33cc44dd55ee66ff7700889911aa22bb33cc44dd55ee66ff77008899"}
    ]}"#;

    fn probs(n: usize) -> Vec<Problem> {
        (0..n)
            .map(|i| Problem {
                id: 100 + i as i64,
                text: format!("第 {} 题：求导。", i + 1),
                difficulty: Some((i % 5 + 1) as i64),
                // 这一组全 open：学会态在 `drawing_never_panics…` 里单独造
                status: "open".to_string(),
                priority: ["high", "normal", "low"][i % 3].to_string(),
                repeat_count: (i % 3) as i64 + 1,
            })
            .collect()
    }

    fn panel(n: usize) -> MistakesPanel {
        let mut p = MistakesPanel::new();
        p.show_ready(probs(n));
        p
    }

    // ---------- JSON

    /// 单测 1：真形状（中文题面 + 15 个字段）能解析，字段对得上。
    #[test]
    fn parses_real_server_shape() {
        let v = parse_list(FIXTURE).expect("真实形状应该能解析");
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].id, 4);
        assert_eq!(v[0].difficulty, Some(5));
        assert_eq!(v[0].priority, "high");
        assert_eq!(v[0].repeat_count, 2);
        assert_eq!(v[0].status, "open");
        assert!(v[0].text.contains("区间 [-2,2]"));
        assert!(is_learned(&v[1]));
        assert_eq!(meta_label(&v[0]), "错 2 次 · 难度 5");
        assert_eq!(meta_label(&v[1]), "难度 2");
    }

    /// 单测 2：空列表 / 缺 `problems` 字段都算空。
    #[test]
    fn parses_empty_and_missing_problems() {
        for raw in [
            r#"{"count":0,"problems":[]}"#,
            r#"{"problems":[]}"#,
            r#"{"count":0}"#,
            "{}",
        ] {
            assert!(parse_list(raw).unwrap().is_empty(), "{raw}");
        }
    }

    /// 单测 3：字段缺失走默认值（不 panic，界面能画）。
    #[test]
    fn missing_fields_use_defaults() {
        let v = parse_list(r#"{"problems":[{"id":7,"text":"只有题面"}]}"#).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].id, 7);
        assert_eq!(v[0].text, "只有题面");
        assert_eq!(v[0].difficulty, None);
        assert_eq!(v[0].repeat_count, 1);
        assert_eq!(v[0].status, "");
        assert_eq!(v[0].priority, "");
        // 元信息全空 → 一个字都不占
        assert_eq!(meta_label(&v[0]), "");
    }

    /// 单测 4：非法 JSON 一律报错，不 panic。
    #[test]
    fn invalid_json_is_error() {
        for raw in [
            "",
            "没有 JSON",
            r#"{"problems":[}"#,
            "}{",
            r#"{"problems":3}"#,
            r#"{"problems":[{"id":"x"}]}"#,
        ] {
            assert!(parse_list(raw).is_err(), "{raw} 应该报错");
        }
    }

    // ---------- 行布局

    /// 单测 5：1404×1872 一屏 13 行；第一行贴 120，最后一行 y=1560。
    #[test]
    fn rows_and_y_positions() {
        assert_eq!(rows_visible(SCREEN_H), 13);
        assert_eq!(rows_visible(0), 0);
        assert_eq!(rows_visible(LIST_TOP + SAFE_BOTTOM), 0);
        let p = panel(14);
        let v = p.visible();
        assert_eq!(v.len(), 13);
        assert_eq!(v[0].id, 100);
        assert_eq!(v[12].id, 112);
        assert_eq!(LIST_TOP, 120);
        assert_eq!(LIST_TOP + 12 * ROW_H, 1560);
        // 第 14 条（下标 13）要滚动才看得到
        assert!(p.hit_at(1300, LIST_TOP + 13 * ROW_H + 60).is_none());
    }

    /// 单测 6：点第 k 行的 `Learned` → 只对第 k 行那条 id 发请求。
    #[test]
    fn learned_button_targets_only_that_row() {
        let p = panel(14);
        for k in 0..13 {
            let y = LIST_TOP + k as i32 * ROW_H;
            let (bx, by, bw, bh) = learned_btn(SCREEN_W, y);
            let act = p
                .hit_at(bx + bw / 2, by + bh / 2)
                .expect("按钮中心该命中");
            assert_eq!(act, Act::Learned(k), "第 {k} 行");
            assert_eq!(p.target_id(act), Some(100 + k as i64), "第 {k} 行的 id");
        }
        // 行内的空白（不在按钮上）不算点击
        assert_eq!(p.hit_at(600, LIST_TOP + 60), None);
        // 底部安全区 / 标题带下方 / 越界都不命中
        assert_eq!(p.hit_at(600, 1800), None);
        assert_eq!(p.hit_at(600, 100), None);
        assert_eq!(p.hit_at(-5, 300), None);
        assert_eq!(p.hit_at(SCREEN_W + 10, 300), None);
        // 左上角返回
        assert_eq!(p.hit_at(60, 60), Some(Act::Back));
        assert_eq!(p.target_id(Act::Back), None);
    }

    /// 单测 7：滚一行后窗口内容变了、命中号跟着变；到底/到顶夹住。
    #[test]
    fn scroll_changes_rows_and_clamps() {
        let mut p = panel(14);
        assert_eq!(p.visible()[0].id, 100);
        assert!(p.scroll(1));
        assert_eq!(p.visible()[0].id, 101, "滚一行后第一行应该是第 2 条");
        assert_eq!(p.hit_at(1300, LIST_TOP + 20 + 40), Some(Act::Learned(1)));
        assert_eq!(p.hit_at(1300, LIST_TOP + 20 + 40).and_then(|a| p.target_id(a)), Some(101));

        // 14 条 - 13 行 = 最多滚 1 行；再滚不动
        assert!(!p.scroll(1));
        assert!(p.scroll(-9));
        assert!(!p.scroll(-1), "到顶了不该再动");

        // 跳转越界夹在界内
        p.scroll_to(99);
        assert_eq!(p.top, 1);
        p.scroll_to(0);
        assert_eq!(p.top, 0);
    }

    /// 单测 8：空列表 / 越界窗口 / 各种状态画一整页都不 panic。
    #[test]
    fn drawing_never_panics_and_marks_learned_dim() {
        let mut fb = Fb::open_mem();
        let mut text = None;
        for n in [0usize, 1, 3, 14] {
            let mut p = panel(n);
            p.scroll_to(999);
            p.draw(&mut fb, &mut text);
            p.scroll_to(0);
            p.draw(&mut fb, &mut text);
        }
        // 学会的行整行浅灰（用纯函数断言色号，不依赖字体）
        let mut l = probs(1)[0].clone();
        assert_eq!(if is_learned(&l) { LEARNED_INK } else { INK }, INK);
        l.status = "learned".to_string();
        assert_eq!(if is_learned(&l) { LEARNED_INK } else { INK }, LEARNED_INK);
        // 已学会的行没有按钮可点
        let mut p = MistakesPanel::new();
        p.show_ready(vec![l]);
        assert_eq!(p.hit_at(1300, LIST_TOP + 20 + 40), None);
    }

    // ---------- 显示规则

    /// 单测 9：优先级三档、次数、难度的显示规则。
    #[test]
    fn priority_and_labels_follow_the_rules() {
        assert_eq!(Priority::of("high"), Priority::High);
        assert_eq!(Priority::of("NORMAL"), Priority::Normal);
        assert_eq!(Priority::of("low"), Priority::Low);
        assert_eq!(Priority::of(""), Priority::Normal);
        assert_eq!(Priority::of("urgent"), Priority::Normal);

        assert_eq!(repeat_label(1), None, "N=1 不显示");
        assert_eq!(repeat_label(0), None);
        assert_eq!(repeat_label(2), Some("错 2 次".to_string()));
        assert_eq!(repeat_label(17), Some("错 17 次".to_string()));

        assert_eq!(difficulty_label(None), None, "难度缺失不显示");
        assert_eq!(difficulty_label(Some(0)), None);
        assert_eq!(difficulty_label(Some(6)), None);
        assert_eq!(difficulty_label(Some(1)), Some("难度 1".to_string()));
        assert_eq!(difficulty_label(Some(5)), Some("难度 5".to_string()));

        let mk = |r, d| Problem {
            repeat_count: r,
            difficulty: d,
            ..Problem::default()
        };
        assert_eq!(meta_label(&mk(1, None)), "");
        assert_eq!(meta_label(&mk(3, Some(4))), "错 3 次 · 难度 4");
    }

    // ---------- 截断

    /// 单测 10：中文 60 字 + 超长英文单词，截断后不超出右边界（用 `ui::text` 量宽）。
    #[test]
    fn long_text_is_ellipsized_before_the_button() {
        let Some(mut t) = Text::load() else {
            eprintln!("本机没字体，跳过测宽");
            return;
        };
        let meta = "错 3 次 · 难度 5";
        let meta_w = t.width(FONT_META, meta) as i32;
        let (x, max_w) = text_span(SCREEN_W, meta_w);
        // 右边界 = 按钮左缘 - TEXT_GAP
        let right = SCREEN_W - BTN_MARGIN - BTN_W - TEXT_GAP;
        assert_eq!(x, META_X + meta_w + META_GAP);

        let zh: String = "求函数在给定区间上的最大值与最小值，并说明取到最值时自变量的取值。"
            .repeat(3)
            .chars()
            .take(60)
            .collect();
        assert_eq!(zh.chars().count(), 60, "测试前提：正好 60 个汉字");
        let en = "Supercalifragilisticexpialidociousantidisestablishmentarianismpneumonoultramicroscopicsilicovolcanoconiosis";
        for src in [zh.as_str(), en] {
            let shown = t.ellipsize(FONT_TEXT, src, max_w);
            assert!(shown.ends_with('…'), "超宽应该截断：{shown}");
            assert!(
                x + t.width(FONT_TEXT, &shown) as i32 <= right,
                "截断后压到按钮了：{shown}"
            );
        }
        // 短题不截断
        let short = "求导。";
        assert_eq!(t.ellipsize(FONT_TEXT, short, max_w), short);
    }

    // ---------- 状态与网络

    /// 单测 11：没配对只显示一行 `Pair a server first`，绝不起后台线程。
    #[test]
    fn unpaired_shows_pair_first_and_spawns_nothing() {
        let mut p = MistakesPanel::new();
        p.open(Config::default());
        assert!(p.is_open());
        assert_eq!(p.status_line(), Some(PAIR_FIRST));
        assert!(p.rx.is_none(), "没配对不该发请求");
        assert!(p.poll().is_none());
        assert!(p.problems().is_empty());
        assert_eq!(p.hit_at(60, 60), Some(Act::Back));
        p.close();
        assert!(!p.is_open());
    }

    /// 单测 12：空列表 = `No mistakes yet`；有数据时没有居中状态行。
    #[test]
    fn empty_list_has_a_center_line() {
        let mut p = MistakesPanel::new();
        p.show_ready(Vec::new());
        assert_eq!(p.status_line(), Some(EMPTY));
        p.show_ready(probs(1));
        assert_eq!(p.status_line(), None);
        // 学会失败也只报事件，不 panic
        assert_eq!(
            Event::Learned {
                id: 1,
                ok: false,
                why: "HTTP 500".into()
            }
            .clone(),
            Event::Learned {
                id: 1,
                ok: false,
                why: "HTTP 500".into()
            }
        );
    }

    /// 单测 13：拖动每 90 px 滚一行，拖动过就不算点击；按一下不动 = 点击。
    #[test]
    fn drag_scrolls_and_is_not_a_tap() {
        let mut p = panel(14);
        assert_eq!(p.visible()[0].id, 100);
        p.down(600.0, 600.0);
        assert!(!p.moved(600.0, 650.0), "没到 90px 不该动");
        assert!(p.moved(600.0, 500.0), "往上拖 100px 应该滚一行");
        assert_eq!(p.visible()[0].id, 101);
        assert_eq!(p.up(), None, "拖动过不算点击");

        // 按一下 `Learned` 按钮再抬：算点击
        let (bx, by, bw, bh) = learned_btn(SCREEN_W, LIST_TOP);
        p.down((bx + bw / 2) as f32, (by + bh / 2) as f32);
        assert_eq!(p.up(), Some(Act::Learned(1)));
    }

    /// 单测 14：出图（假数据 15 条）能写出 1404×1872 的 PGM。
    #[test]
    fn demo_renders_a_full_pgm() {
        let out = std::env::temp_dir().join(format!("rmdev-mistakes-{}.pgm", std::process::id()));
        let out = out.to_string_lossy().into_owned();
        let _ = std::fs::remove_file(&out);
        render_demo("demo", &out);
        let bytes = std::fs::read(&out).expect("PGM 应该写出来");
        let hdr = b"P5\n1404 1872\n255\n";
        assert!(bytes.starts_with(hdr), "PGM 头不对");
        assert_eq!(bytes.len(), hdr.len() + 1404 * 1872);
        // 黑头有墨、列表区不是全白
        let px = &bytes[hdr.len()..];
        let dark = |x0: i32, y0: i32, x1: i32, y1: i32| {
            let mut n = 0;
            for y in y0..y1 {
                for x in x0..x1 {
                    if px[(y as usize) * 1404 + x as usize] < 128 {
                        n += 1;
                    }
                }
            }
            n
        };
        assert!(dark(0, 0, 1404, 120) > 10000, "黑头没画上");
        assert!(dark(0, 120, 1404, 1680) > 1000, "列表行没画上");
        assert_eq!(dark(0, 1760, 1404, 1870), 0, "底部安全区该是空的");
        let _ = std::fs::remove_file(&out);
    }

    // ---------- 网络：本机 mock HTTP 服务器（不连真机、不连服务器）

    /// 只接一次请求的假服务器，返回 `(端口, 线程)`；线程返回收到的请求头。
    fn serve_once(body: &'static str) -> (u16, std::thread::JoinHandle<String>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1");
        let port = l.local_addr().expect("local_addr").port();
        l.set_nonblocking(true).expect("nonblocking");
        let h = std::thread::spawn(move || {
            use std::io::{Read, Write};
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            loop {
                match l.accept() {
                    Ok((mut s, _)) => {
                        s.set_read_timeout(Some(Duration::from_secs(2))).ok();
                        let mut req = Vec::new();
                        let mut b = [0u8; 512];
                        loop {
                            let n = s.read(&mut b).unwrap_or(0);
                            if n == 0 {
                                break;
                            }
                            req.extend_from_slice(&b[..n]);
                            if req.windows(4).any(|w| w == b"\r\n\r\n") {
                                break;
                            }
                        }
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\
                             Connection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ = s.write_all(resp.as_bytes());
                        return String::from_utf8_lossy(&req).into_owned();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if std::time::Instant::now() >= deadline {
                            return String::new();
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return String::new(),
                }
            }
        });
        (port, h)
    }

    fn cfg_for(port: u16) -> Config {
        Config {
            url: format!("http://127.0.0.1:{port}"),
            token: "tok".to_string(),
            ..Config::default()
        }
    }

    /// 等后台回复（最多 2 s；UI 里是每帧 `try_recv`，这里只是轮询）。
    fn wait_event(p: &mut MistakesPanel) -> Option<Event> {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if let Some(e) = p.poll() {
                return Some(e);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        None
    }

    /// 单测 15：后台线程真发 `GET ...status=all&limit=200`、带 token、回来能解析。
    #[test]
    fn background_list_request_uses_all_and_parses() {
        let (port, srv) = serve_once(FIXTURE);
        let mut p = MistakesPanel::new();
        p.open(cfg_for(port));
        assert_eq!(p.status_line(), Some(LOADING));
        assert_eq!(wait_event(&mut p), Some(Event::Loaded));
        let req = srv.join().expect("假服务器线程");
        assert!(
            req.starts_with("GET /api/v1/profile?status=all&limit=200 HTTP/1.1"),
            "请求行不对：{req}"
        );
        assert!(req.contains("Authorization: Bearer tok"), "没带 token：{req}");
        assert_eq!(p.problems().len(), 2);
        assert_eq!(p.status_line(), None);
    }

    /// 单测 16：点 `Learned` 只 POST 命中那一行的 id；成功后这一行就地变淡。
    #[test]
    fn learned_posts_only_the_hit_id_and_dims_it() {
        let (port, srv) = serve_once(r#"{"problem":{"id":100}}"#);
        let mut p = MistakesPanel::new();
        p.show_ready(probs(14));
        p.cfg = Some(cfg_for(port));
        assert_eq!(p.learn(0), Some(100));
        match wait_event(&mut p).expect("应该有回复") {
            Event::Learned { id, ok, .. } => {
                assert_eq!(id, 100);
                assert!(ok);
            }
            other => panic!("事件不对：{other:?}"),
        }
        let req = srv.join().expect("假服务器线程");
        assert!(
            req.starts_with("POST /api/v1/profile/100/learned HTTP/1.1"),
            "请求行不对：{req}"
        );
        assert_eq!(p.problems()[0].status, "learned");
        // 学会后这一行没有按钮可点了
        assert!(p.hit_at(1300, LIST_TOP + 60).is_none());
    }
}
