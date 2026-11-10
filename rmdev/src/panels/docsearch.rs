//! 文档内搜索（官方 `SearchResultsHeader` = `res_066d01c.qml`）。
//!
//! 官方结构：搜索栏是文档视图顶部的一条 `HeaderActions`，不放独立页面 ——
//! 左边 `Cancel`、放大镜 + 输入框、中间结果计数 `Result %1 of %2` /
//! `No results found`、右边 `Page only` 勾选与 `Search` 按钮；有结果时右侧换成
//! `Previous` / `Next`（QML `actions`）。输入框获得焦点时键盘贴底
//! （`Keyboard.showRequested()`），提交后收起键盘（`Keyboard.hideRequested()`）。
//!
//! 搜索结果来自当前文档每一页的文字层（`library.load_texts`，即我们的
//! `texts-<pageid>.json`），大小写不敏感；`Page only` 只搜当前页。
//! 命中记录是（页号、该页第几段文字、字符区间），绘制高亮在 `main.rs` 里做
//! （那里才有页面坐标变换和字体宽度）。
//!
//! 和 `docsettings` 一样：布局/命中/状态都在这个模块里（可单测），`main.rs`
//! 只负责喂数据、翻页和局部刷新。

use crate::fb::{gray565, Fb};
use crate::ui::keyboard::Keyboard;
use crate::ui::text::Text;
use crate::ui::widgets;

/// 搜索条高度（和文档视图顶部 header 一致：118）。
pub const HDR_H: i32 = 118;

/// 搜索条打开时正文的纵向位移。
///
/// 官方 `SearchResultsHeader` 是**盖在页面上**的一层，但我们的正文从 y=0
/// 开始画，第一行会被搜索条压住。打开搜索时整页内容下移 `HDR_H`，关掉恢复。
/// 纯函数：`main.rs` 的绘制、命中高亮和局部刷新共用同一份，保证不会差一像素。
pub fn body_offset(open: bool) -> i32 {
    if open {
        HDR_H
    } else {
        0
    }
}

/// 文字基线/高亮框的纵向平移（`open = false` 时原样返回）。
pub fn shift_y(y: i32, open: bool) -> i32 {
    y + body_offset(open)
}

/// 高亮矩形的纵向平移：宽高不变，只动 y。
pub fn shift_rect(r: (i32, i32, i32, i32), open: bool) -> (i32, i32, i32, i32) {
    (r.0, shift_y(r.1, open), r.2, r.3)
}

/// 一次命中：第 `page` 页、该页第 `item` 段文字、字符区间 `[start, end)`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hit {
    pub page: usize,
    pub item: usize,
    pub start: usize,
    pub end: usize,
}

/// 搜索条上的可点区域。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Cancel,
    Input,
    PageOnly,
    Search,
    Previous,
    Next,
}

/// 搜索条布局（纯几何，不碰 Fb）。返回的矩形同时用于绘制和命中。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bar {
    pub cancel: (i32, i32, i32, i32),
    /// 文字区（点它 = 重新聚焦 / 收起键盘）
    pub input: (i32, i32, i32, i32),
    pub page_only: (i32, i32, i32, i32),
    pub search: (i32, i32, i32, i32),
    pub previous: (i32, i32, i32, i32),
    pub next: (i32, i32, i32, i32),
}

/// 固定的几档字号（官方 fontSizeH1 36 / H2 28 / Body1 28 的量级）。
const FONT_HDR: f32 = 33.0;
const FONT_FIELD: f32 = 34.0;
const FONT_LABEL: f32 = 30.0;
/// 输入框文字左缘：放大镜（图标盒 x=190..230）右边再留一点。
const INPUT_X: i32 = 245;
/// 没有结果文字时输入文字能画到哪。
const INPUT_R: i32 = 900;
/// 勾选框 40×40、文字右移 55。
const CHECK_X: i32 = 945;
const CHECK_Y: i32 = 39;
const CHECK: i32 = 40;

pub struct DocSearch {
    open: bool,
    /// 搜索输入（官方 `searchField`）
    pub kb: Keyboard,
    /// 键盘是否贴底。打开时自动聚焦（官方 `searchField.forceActiveFocus()`），
    /// 提交后收起（官方 `Keyboard.hideRequested()`）。
    kb_open: bool,
    page_only: bool,
    /// 上一次真正提交的查询（官方 `committedSearchInput`）。
    committed: String,
    hits: Vec<Hit>,
    /// 文档后端（PDF/EPUB）命中的片段，与 `hits` 一一对应。
    /// 这类命中没有字符区间（后端只给页号 + 片段），所以页内不高亮。
    doc_snips: Vec<String>,
    cur: usize,
    /// 分步扫描（`None` = 没在搜）。文档文字层是一页一页扫的，
    /// 每 tick 一页 —— 原来是同步扫全文档，进度和 `Cancel` 都没机会响应。
    scan: Option<Scan>,
    /// 进度条上一次画出来时扫到了第几页（限流用，见 `scan_paint_due`）
    painted: usize,
}

impl DocSearch {
    pub fn new() -> DocSearch {
        DocSearch {
            open: false,
            kb: Keyboard::new(),
            kb_open: false,
            page_only: false,
            committed: String::new(),
            hits: Vec::new(),
            doc_snips: Vec::new(),
            cur: 0,
            scan: None,
            painted: 0,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn kb_open(&self) -> bool {
        self.kb_open
    }

    /// 打开搜索条：清空上一次的查询与结果，输入框自动聚焦、键盘贴底。
    pub fn show(&mut self) {
        self.open = true;
        self.kb_open = true;
        self.kb.text.clear();
        self.committed.clear();
        self.hits.clear();
        self.doc_snips.clear();
        self.cur = 0;
        self.scan = None;
        self.painted = 0;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.kb_open = false;
        self.committed.clear();
        self.hits.clear();
        self.doc_snips.clear();
        self.cur = 0;
        self.scan = None;
        self.painted = 0;
    }

    /// 打开/收起键盘（点输入框）。收起时不清查询。
    pub fn toggle_keyboard(&mut self) {
        self.kb_open = !self.kb_open;
    }

    /// 点了 `Page only`。
    pub fn toggle_page_only(&mut self) {
        self.page_only = !self.page_only;
    }

    pub fn page_only(&self) -> bool {
        self.page_only
    }

    pub fn committed(&self) -> &str {
        &self.committed
    }

    /// 改过输入：官方 `onTextChanged` 会取消当前结果。返回改之前有没有结果
    /// （调用方要据此把旧高亮擦掉）。
    pub fn edited(&mut self) -> bool {
        let had = !self.hits.is_empty() || !self.committed.is_empty() || self.scan.is_some();
        self.committed.clear();
        self.hits.clear();
        self.doc_snips.clear();
        self.cur = 0;
        // 官方 `onTextChanged: searchController.cancel()`
        self.scan = None;
        had
    }

    /// 提交搜索。空查询不搜（官方 `text.trim().length > 0` 才搜）。
    /// 返回命中数；提交后收起键盘。
    pub fn submit(&mut self, pages: &[Vec<String>], current_page: usize) -> usize {
        self.kb_open = false;
        self.committed = self.kb.text.trim().to_string();
        self.hits.clear();
        self.doc_snips.clear();
        self.cur = 0;
        self.scan = None;
        if self.committed.is_empty() {
            return 0;
        }
        let only = if self.page_only {
            Some(current_page)
        } else {
            None
        };
        self.hits = find_all(pages, &self.committed, only);
        self.hits.len()
    }

    /// 文档后端的命中（PDF 文字层 / 电子书正文）。
    ///
    /// 后端只给 `(页号, 片段)`，没有字符区间，所以这里的 `item` 用一个哨兵
    /// `usize::MAX`：`main.rs` 的页内高亮查找 `texts.get(item)` 会自然返回 None，
    /// 不会画错位置。计数 / `Previous` / `Next` / 跳页全部照常工作。
    pub fn submit_doc(&mut self, hits: Vec<(usize, String)>, current_page: usize) -> usize {
        self.kb_open = false;
        self.committed = self.kb.text.trim().to_string();
        self.hits.clear();
        self.doc_snips.clear();
        self.cur = 0;
        self.scan = None;
        if self.committed.is_empty() {
            return 0;
        }
        let only = if self.page_only {
            Some(current_page)
        } else {
            None
        };
        for (page, snip) in hits {
            if let Some(p) = only {
                if page != p {
                    continue;
                }
            }
            self.hits.push(Hit {
                page,
                item: usize::MAX,
                start: 0,
                end: 0,
            });
            self.doc_snips.push(snip);
        }
        self.hits.len()
    }

    // ------------------------------------------------ 分步扫描（P13）
    //
    // 文字层搜索原来是一次性 `find_all` 全文档：页多的时候整个主循环卡住，
    // 进度画不出来、`Cancel` 也点不动。现在拆成"每 tick 一页"：
    // `start_scan` → 反复 `scan_page` + `scan_take` → `scan_page` 返回 `None`
    // → 调用方 `cancel_scan()` 收尾（`scanning()` 才变 false）。
    // 进度文案/进度条/刷新限流都是纯函数，见 [`Scan`] / [`progress_text`]。

    /// 开始分步扫描。空查询或没页可扫时返回 false（不进入扫描态）。
    ///
    /// `only` 是 `Page only`：只扫那一页。`total` 是文档总页数
    /// （官方进度文案的分母就是 `documentView.pageCount`）。
    pub fn start_scan(&mut self, query: &str, total: usize, only: Option<usize>) -> bool {
        self.kb_open = false;
        self.committed = query.trim().to_string();
        self.hits.clear();
        self.doc_snips.clear();
        self.cur = 0;
        self.scan = None;
        self.painted = 0;
        if self.committed.is_empty() || total == 0 {
            return false;
        }
        self.scan = Some(Scan::new(total, only));
        true
    }

    /// 还在分步扫描。
    pub fn scanning(&self) -> bool {
        self.scan.is_some()
    }

    /// 取下一张要搜的页（0 基）。扫完返回 `None`（此时 `scanning()` 仍是 true，
    /// 由调用方在取不到页时收尾，这样"最后一页的结果"也画得出来）。
    pub fn scan_page(&mut self) -> Option<usize> {
        self.scan.as_mut()?.next_page()
    }

    /// 收下一张页的命中：`find_all` 只看这一页，页号由扫描器给。
    pub fn scan_take(&mut self, page: usize, texts: &[String]) {
        let one = [texts.to_vec()];
        let mut hs = find_all(&one, &self.committed, None);
        for h in &mut hs {
            h.page = page;
        }
        self.hits.extend(hs);
    }

    /// `Cancel` 中途取消扫描（已经搜到的结果保留，官方 cancel 也保留）。
    pub fn cancel_scan(&mut self) {
        self.scan = None;
    }

    /// 底部进度条要画的文字（官方 `searchProgressionText`）。
    /// `Page only` 时官方不显示进度条（`SearchResultsFooter.visible:
    /// _searchResultsHeader.running && !_searchResultsHeader.pageOnly`），这里一致。
    pub fn scan_label(&self) -> Option<String> {
        let s = self.scan?;
        if self.page_only {
            return None;
        }
        s.label()
    }

    /// 进度 0..=1（官方 `progress: lastSearchedPage / pageCount`）。
    pub fn scan_progress(&self) -> Option<f32> {
        let s = self.scan?;
        if self.page_only {
            return None;
        }
        Some(s.fraction())
    }

    /// 这一次进度该刷屏吗：页号变了、而且跨过 `PAINT_STEPS` 分之一才刷。
    ///
    /// 一页刷一次的话，100 页的文档要刷 100 次 GC16（每次十几毫秒），比原来
    /// 同步搜还慢；`Searching page` 上的页码也不需要一页一跳。
    pub fn scan_paint_due(&mut self) -> bool {
        let Some(s) = self.scan else { return false };
        if s.next <= self.painted {
            return false;
        }
        let step = (s.total / PAINT_STEPS).max(1);
        if s.next - self.painted >= step || s.next >= s.total {
            self.painted = s.next;
            return true;
        }
        false
    }

    /// 画底部进度条（官方 `SearchResultsFooter`）：黑底白字
    /// `Searching page %1 of %2` + 一条按比例点亮的进度线。
    /// 返回刷新的矩形，没在扫描时返回 `None`。
    pub fn paint_progress(
        &self,
        fb: &mut Fb,
        text: &mut Text,
        w: i32,
        h: i32,
    ) -> Option<(i32, i32, i32, i32)> {
        let label = self.scan_label()?;
        let frac = self.scan_progress()?;
        let r = progress_rect(w, h);
        let (x, y, bw, bh) = r;
        fb.fill_rect(x, y, bw, bh, gray565(0));
        text.draw_bold(fb, x + 30, y + 46, FONT_LABEL, &label, 250);
        // 进度线：4px，离底 8px，按比例点亮；最少给 4px，免得太短看不见
        let bar = (bw - 60) as f32;
        let lit = (bar * frac).max(4.0) as i32;
        fb.fill_rect(x + 30, y + bh - 12, lit, 4, gray565(250));
        Some(r)
    }

    /// 当前命中的片段（文档后端才有）。
    pub fn snippet(&self) -> Option<&str> {
        self.doc_snips.get(self.cur).map(|s| s.as_str())
    }

    pub fn hits(&self) -> &[Hit] {
        &self.hits
    }

    pub fn count(&self) -> usize {
        self.hits.len()
    }

    pub fn current(&self) -> Option<Hit> {
        self.hits.get(self.cur).copied()
    }

    pub fn index(&self) -> usize {
        self.cur
    }

    /// `Next`：绕回到第一条（官方是 incrementCurrentResultIndex）。
    pub fn next(&mut self) -> Option<Hit> {
        if self.hits.is_empty() {
            return None;
        }
        self.cur = (self.cur + 1) % self.hits.len();
        self.current()
    }

    /// `Previous`：从第一条绕到最后一条。
    pub fn prev(&mut self) -> Option<Hit> {
        if self.hits.is_empty() {
            return None;
        }
        self.cur = (self.cur + self.hits.len() - 1) % self.hits.len();
        self.current()
    }

    /// 结果行文字（官方 `resultsLabel.text`）；还没提交时是空串。
    ///
    /// 官方那段的顺序是：有结果 → `Result %1 of %2`；还在搜 → `Searching...`；
    /// 否则 `No results found`。扫描中已经出结果时也显示结果计数（官方同样）。
    pub fn result_label(&self) -> String {
        if self.committed.is_empty() {
            return String::new();
        }
        if self.hits.is_empty() {
            if self.scan.is_some() {
                return "Searching...".to_string();
            }
            return "No results found".to_string();
        }
        format!("Result {} of {}", self.cur + 1, self.hits.len())
    }

    /// 搜索条上各控件的矩形。结果出现时 `Page only` / `Search` 换成
    /// `Previous` / `Next`（官方 `visible` 条件），矩形两套都算出来。
    pub fn bar(&self, w: i32) -> Bar {
        Bar {
            cancel: (0, 0, 180, HDR_H),
            input: (180, 0, 560, HDR_H),
            page_only: (CHECK_X - 25, 14, 250, 90),
            search: (w - 233, 19, 200, 80),
            previous: (w - 444, 19, 200, 80),
            next: (w - 234, 19, 200, 80),
        }
    }

    /// 命中测试。结果为空时 `Page only` / `Search` 有效，有结果时
    /// `Previous` / `Next` 有效（和绘制一致）。
    pub fn hit(&self, x: f32, y: f32, w: i32) -> Option<Act> {
        let b = self.bar(w);
        let inside = |r: (i32, i32, i32, i32)| {
            x >= r.0 as f32
                && y >= r.1 as f32
                && x < (r.0 + r.2) as f32
                && y < (r.1 + r.3) as f32
        };
        if inside(b.cancel) {
            return Some(Act::Cancel);
        }
        if inside(b.input) {
            return Some(Act::Input);
        }
        if self.hits.is_empty() {
            if inside(b.page_only) {
                return Some(Act::PageOnly);
            }
            if inside(b.search) {
                return Some(Act::Search);
            }
        } else {
            if inside(b.previous) {
                return Some(Act::Previous);
            }
            if inside(b.next) {
                return Some(Act::Next);
            }
        }
        None
    }

    /// 画搜索条（不含键盘；键盘由 `main` 调 `ui::keyboard` 画在底部）。
    pub fn paint(&self, fb: &mut Fb, text: &mut Text, w: i32) {
        fb.fill_rect(0, 0, w, HDR_H, gray565(0));

        // Cancel（官方 rejectText，粗体白字）
        text.draw_bold(fb, 32, 74, FONT_HDR, "Cancel", 250);

        // 放大镜（官方 Icons.icon_lens 40×40：镜片环 + 左下柄）
        widgets::ring(fb, 213, 55, 16, 4, 250);
        widgets::line(fb, 202, 66, 191, 78, 4, 250);

        // 中间的结果计数（先算宽度：输入文字要给它让位）
        let label = self.result_label();
        let label_w = if label.is_empty() {
            0
        } else {
            text.width(FONT_LABEL, &label) as i32
        };
        let label_left = (w - label_w) / 2;

        // 输入框：有字就画字，空着且键盘收起时画占位 "Search here"
        let q = self.kb.text.clone();
        let right = if label.is_empty() {
            INPUT_R
        } else {
            (label_left - 24).max(INPUT_X + 60)
        };
        let max_w = right - INPUT_X;
        if q.is_empty() {
            // 官方占位文案（QML 里焦点态不画，这里一直留着更好认）
            text.draw(fb, INPUT_X, 72, FONT_FIELD, "Search here", 150);
            if self.kb_open {
                // 焦点在输入框：占位文案后面画一根光标（没有闪烁动画）
                let tw = text.width(FONT_FIELD, "Search here") as i32;
                fb.fill_rect(INPUT_X + tw + 3, 50, 2, 34, gray565(250));
            }
        } else {
            let shown = text.ellipsize(FONT_FIELD, &q, max_w);
            text.draw(fb, INPUT_X, 72, FONT_FIELD, &shown, 250);
            if self.kb_open {
                let tw = text.width(FONT_FIELD, &shown) as i32;
                fb.fill_rect(INPUT_X + tw + 3, 50, 2, 34, gray565(250));
            }
        }

        if !label.is_empty() {
            text.draw(fb, label_left, 72, FONT_LABEL, &label, 250);
        }

        // 右侧：没有结果时是 Page only + Search；有结果时是 Previous / Next
        if self.hits.is_empty() {
            self.paint_checkbox(fb, text);
            self.paint_button(fb, text, self.bar(w).search, "Search", true);
        } else {
            self.paint_button(fb, text, self.bar(w).previous, "Previous", true);
            self.paint_button(fb, text, self.bar(w).next, "Next", true);
        }
    }

    fn paint_checkbox(&self, fb: &mut Fb, text: &mut Text) {
        let c = gray565(250);
        // 40×40 白描边方框（官方 CheckBox 反白态）
        widgets::outline_rect(fb, CHECK_X, CHECK_Y, CHECK, CHECK, 3, 250);
        if self.page_only {
            fb.fill_rect(CHECK_X + 10, CHECK_Y + 10, CHECK - 20, CHECK - 20, c);
        }
        text.draw(fb, CHECK_X + CHECK + 15, 72, FONT_LABEL, "Page only", 250);
    }

    /// 反白的头部按钮（官方 `ButtonHeader inverted`）：白底黑字。
    fn paint_button(
        &self,
        fb: &mut Fb,
        text: &mut Text,
        r: (i32, i32, i32, i32),
        label: &str,
        on: bool,
    ) {
        fb.fill_rect(r.0, r.1, r.2, r.3, gray565(if on { 250 } else { 120 }));
        let tw = text.width_bold(FONT_LABEL, label) as i32;
        text.draw_bold(
            fb,
            r.0 + (r.2 - tw) / 2,
            r.1 + r.3 / 2 + 12,
            FONT_LABEL,
            label,
            0,
        );
    }
}

impl Default for DocSearch {
    fn default() -> Self {
        DocSearch::new()
    }
}

// ---------------------------------------------------------------- 匹配

/// 在整本文档的文字层里找 `query`（大小写不敏感）。
///
/// `pages[p]` 是第 p 页的文字段落；`only = Some(p)` 时只搜那一页（`Page only`）。
/// 返回顺序 = 页号、段落下标、字符位置，方便 `Previous`/`Next` 顺序跳。
pub fn find_all(pages: &[Vec<String>], query: &str, only: Option<usize>) -> Vec<Hit> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (p, items) in pages.iter().enumerate() {
        if only.is_some_and(|o| o != p) {
            continue;
        }
        for (i, s) in items.iter().enumerate() {
            let (folded, map) = fold(s);
            for (byte, _) in folded.match_indices(&q) {
                let start = map.get(byte).copied().unwrap_or(0);
                let end = map
                    .get(byte + q.len())
                    .copied()
                    .unwrap_or_else(|| s.chars().count());
                out.push(Hit {
                    page: p,
                    item: i,
                    start,
                    end,
                });
            }
        }
    }
    out
}

/// 小写化并记下每个小写字节对应原字符串的**字符下标**。
///
/// 大小写转换会改变长度（`İ` → `i̇`），所以不能直接拿小写串的下标去切原串。
fn fold(s: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(s.len());
    let mut map = Vec::with_capacity(s.len());
    for (ci, ch) in s.chars().enumerate() {
        for lc in ch.to_lowercase() {
            for _ in 0..lc.len_utf8() {
                map.push(ci);
            }
            out.push(lc);
        }
    }
    (out, map)
}

// ---------------------------------------------------------------- 分步扫描（P13）

/// 进度条整块刷新多少次（一页刷一次太费；见 `DocSearch::scan_paint_due`）。
const PAINT_STEPS: usize = 20;

/// 官方网站原文：`res_066d01c.qml:31`
/// `qsTr("Searching page %1 of %2").arg(lastSearchedPage + 1).arg(pageCount)`。
///
/// `page` 是**正在搜的那一页**（1 基，官方就是 `lastSearchedPage + 1`），
/// `total` 是文档页数。两个都夹到合法范围，页码永远不会写出 `1 of 0` 这种。
pub fn progress_text(page: usize, total: usize) -> String {
    let total = total.max(1);
    let page = page.clamp(1, total);
    format!("Searching page {page} of {total}")
}

/// 底部进度条的矩形：整条贴底、72 高（官方 `SearchResultsFooter` 就在
/// `anchors.bottom`，高度和 `navigatorStatusbarHeight` 一致）。
pub fn progress_rect(w: i32, h: i32) -> (i32, i32, i32, i32) {
    (0, h - 72, w, 72)
}

/// 分步搜索的游标：一次只给一页，扫完为止。
///
/// 这是"进度状态机"的全部状态 —— 纯数据、纯函数，单测不用碰帧缓冲。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scan {
    /// 下一张要搜的页（0 基，同时也就是"已经派出去几页"）
    pub next: usize,
    /// 文档总页数（官方进度分母 `pageCount`）
    pub total: usize,
    /// `Page only`：只搜这一页，其它页直接跳过
    pub only: Option<usize>,
}

impl Scan {
    pub fn new(total: usize, only: Option<usize>) -> Scan {
        Scan {
            next: 0,
            total,
            only,
        }
    }

    /// 取下一页并推进游标。扫完（或 `only` 只扫一页、已经给过）返回 `None`。
    pub fn next_page(&mut self) -> Option<usize> {
        while self.next < self.total {
            let p = self.next;
            self.next += 1;
            match self.only {
                Some(o) if o != p => continue,
                _ => return Some(p),
            }
        }
        None
    }

    /// 进度文案；没页可扫返回 `None`。
    ///
    /// 派完最后一页时 `next == total`，文案仍然是 `Searching page N of N`
    /// （主循环下一圈才收尾，这时进度条不该突然消失一帧）。
    pub fn label(&self) -> Option<String> {
        if self.total == 0 {
            return None;
        }
        Some(progress_text(self.next, self.total))
    }

    /// 进度 0..=1。
    pub fn fraction(&self) -> f32 {
        if self.total == 0 {
            return 1.0;
        }
        (self.next as f32 / self.total as f32).clamp(0.0, 1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pages(v: &[&[&str]]) -> Vec<Vec<String>> {
        v.iter()
            .map(|p| p.iter().map(|s| s.to_string()).collect())
            .collect()
    }

    /// 大小写不敏感（查询大写 / 原文小写都能中）。
    #[test]
    fn case_insensitive() {
        let p = pages(&[&["Hello World"], &["say hello"]]);
        let hits = find_all(&p, "hello", None);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0], Hit { page: 0, item: 0, start: 0, end: 5 });
        assert_eq!(hits[1], Hit { page: 1, item: 0, start: 4, end: 9 });
        // 反过来：查询小写、原文大写
        let p = pages(&[&["HELLO"]]);
        assert_eq!(find_all(&p, "HeLLo", None).len(), 1);
    }

    /// 跨页：按页序返回，页内按段序。
    #[test]
    fn across_pages_in_order() {
        let p = pages(&[
            &["alpha", "alpha beta"],
            &["gamma"],
            &["alpha again"],
        ]);
        let hits = find_all(&p, "alpha", None);
        assert_eq!(hits.len(), 3);
        assert_eq!((hits[0].page, hits[0].item, hits[0].start), (0, 0, 0));
        assert_eq!((hits[1].page, hits[1].item, hits[1].start), (0, 1, 0));
        assert_eq!((hits[2].page, hits[2].item, hits[2].start), (2, 0, 0));
    }

    /// 同一段里多次出现都算；`Page only` 只搜指定页。
    #[test]
    fn multiple_occurrences_and_page_only() {
        let p = pages(&[&["tok tok"], &["tok"]]);
        assert_eq!(find_all(&p, "tok", None).len(), 3);
        let only = find_all(&p, "tok", Some(1));
        assert_eq!(only.len(), 1);
        assert_eq!(only[0].page, 1);
        assert!(find_all(&p, "tok", Some(9)).is_empty());
    }

    /// 空查询/纯空格不搜；没命中返回空。
    #[test]
    fn empty_query_and_miss() {
        let p = pages(&[&["hello"]]);
        assert!(find_all(&p, "", None).is_empty());
        assert!(find_all(&p, "   ", None).is_empty());
        assert!(find_all(&p, "absent", None).is_empty());
    }

    /// 非 ASCII：小写化改变长度时也要切在原串的正确字符位置。
    #[test]
    fn unicode_offsets() {
        let p = pages(&[&["Ärger im Büro"]]);
        let hits = find_all(&p, "ärger", None);
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].start, hits[0].end), (0, 5));
        // 中文不需要大小写，但位置要按字符算
        let p = pages(&[&["第一页有搜索框"]]);
        let hits = find_all(&p, "搜索", None);
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].start, hits[0].end), (4, 6));
    }

    /// 提交 / 编辑 / 前进后退的状态机。
    #[test]
    fn state_machine() {
        let p = pages(&[&["one two"], &["two three"], &["three four"]]);
        let mut s = DocSearch::new();
        s.show();
        assert!(s.is_open() && s.kb_open());
        assert_eq!(s.result_label(), "");
        s.kb.text = "two".into();
        assert_eq!(s.submit(&p, 0), 2);
        assert!(!s.kb_open(), "提交后键盘收起");
        assert_eq!(s.result_label(), "Result 1 of 2");
        assert_eq!(s.current(), Some(Hit { page: 0, item: 0, start: 4, end: 7 }));
        assert_eq!(s.next().unwrap().page, 1);
        assert_eq!(s.result_label(), "Result 2 of 2");
        // 绕回第一条
        assert_eq!(s.next().unwrap().page, 0);
        // 从第一条往前 → 最后一条
        assert_eq!(s.prev().unwrap().page, 1);
        // 改输入：结果与计数清空
        s.kb_open = true;
        assert!(s.edited());
        assert_eq!(s.result_label(), "");
        assert_eq!(s.count(), 0);
        // 空查询不搜
        s.kb.text = "  ".into();
        assert_eq!(s.submit(&p, 0), 0);
        assert_eq!(s.result_label(), "");
    }

    /// `Page only` 只搜当前页，切换后重新提交会换结果。
    #[test]
    fn page_only_scope() {
        let p = pages(&[&["two"], &["two"]]);
        let mut s = DocSearch::new();
        s.show();
        s.kb.text = "two".into();
        assert_eq!(s.submit(&p, 1), 2);
        s.toggle_page_only();
        assert!(s.page_only());
        assert_eq!(s.submit(&p, 1), 1);
        assert_eq!(s.current().unwrap().page, 1);
    }

    /// 无结果时给出明确文案（官方 `No results found`）。
    #[test]
    fn no_results_label() {
        let p = pages(&[&["hello"]]);
        let mut s = DocSearch::new();
        s.show();
        s.kb.text = "zzz".into();
        assert_eq!(s.submit(&p, 0), 0);
        assert_eq!(s.result_label(), "No results found");
        assert!(s.current().is_none());
        assert_eq!(s.next(), None);
        assert_eq!(s.prev(), None);
    }

    /// 命中区都 ≥ 80×80，两套按钮互不重叠，且都在屏内。
    #[test]
    fn hit_areas_are_big_enough() {
        let s = DocSearch::new();
        let b = s.bar(1404);
        for r in [b.cancel, b.input, b.page_only, b.search, b.previous, b.next] {
            assert!(r.2 >= 80 && r.3 >= 80, "{r:?} 太小");
            assert!(r.0 >= 0 && r.0 + r.2 <= 1404);
            assert!(r.1 >= 0 && r.1 + r.3 <= HDR_H);
        }
        // Page only 和 Search 不叠；Previous 和 Next 不叠
        let overlap = |a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)| {
            a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3
        };
        assert!(!overlap(b.page_only, b.search));
        assert!(!overlap(b.previous, b.next));
    }

    /// 搜索条占用的高度正好是正文下移量；关掉后必须精确归零。
    #[test]
    fn body_offset_matches_header_height() {
        assert_eq!(body_offset(false), 0);
        assert_eq!(body_offset(true), HDR_H);
        assert_eq!(HDR_H, 118);
        assert_eq!(shift_y(0, true), HDR_H);
        assert_eq!(shift_y(0, false), 0);
        assert_eq!(shift_y(900, false), 900);
        // 打开再关闭：回到原来的 y，不能累积漂移
        let y = 777;
        assert_eq!(shift_y(shift_y(y, true), false) - HDR_H, y);
    }

    /// 高亮框跟着正文一起下移，宽高不变；关掉后回到原位。
    #[test]
    fn highlight_rect_follows_the_body() {
        let r = (120, 300, 80, 42);
        assert_eq!(shift_rect(r, false), r);
        assert_eq!(shift_rect(r, true), (120, 300 + HDR_H, 80, 42));
        let back = shift_rect(shift_rect(r, true), false);
        assert_eq!((back.0, back.1 - HDR_H, back.2, back.3), r);
    }

    /// 命中分发：没有结果时能点到 Search/Page only，有结果时是 Previous/Next。
    #[test]
    fn hit_dispatch() {
        let p = pages(&[&["hello world"]]);
        let mut s = DocSearch::new();
        s.show();
        let b = s.bar(1404);
        let mid = |r: (i32, i32, i32, i32)| ((r.0 + r.2 / 2) as f32, (r.1 + r.3 / 2) as f32);
        let (x, y) = mid(b.search);
        assert_eq!(s.hit(x, y, 1404), Some(Act::Search));
        let (x, y) = mid(b.page_only);
        assert_eq!(s.hit(x, y, 1404), Some(Act::PageOnly));
        let (x, y) = mid(b.cancel);
        assert_eq!(s.hit(x, y, 1404), Some(Act::Cancel));
        let (x, y) = mid(b.input);
        assert_eq!(s.hit(x, y, 1404), Some(Act::Input));
        assert_eq!(s.hit(700.0, 900.0, 1404), None); // 页面区域
        s.kb.text = "hello".into();
        s.submit(&p, 0);
        assert_eq!(s.hit(x, y, 1404), Some(Act::Input));
        let (x, y) = mid(b.previous);
        assert_eq!(s.hit(x, y, 1404), Some(Act::Previous));
        let (x, y) = mid(b.next);
        assert_eq!(s.hit(x, y, 1404), Some(Act::Next));
    }

    /// 文档后端（PDF/EPUB）：只有页号 + 片段，没有字符区间。
    #[test]
    fn submit_doc_keeps_page_only_and_snippet() {
        let mut s = DocSearch::new();
        s.show();
        s.kb.text = "needle".into();
        let n = s.submit_doc(
            vec![(0, "a needle here".into()), (3, "another needle".into())],
            0,
        );
        assert_eq!(n, 2);
        assert_eq!(s.count(), 2);
        assert_eq!(s.current().unwrap().page, 0);
        assert_eq!(s.current().unwrap().item, usize::MAX);
        assert_eq!(s.snippet(), Some("a needle here"));
        // Next 绕到第二页，片段跟着换
        assert_eq!(s.next().unwrap().page, 3);
        assert_eq!(s.snippet(), Some("another needle"));
        // Page only 只留当前页
        s.show();
        s.kb.text = "needle".into();
        s.toggle_page_only();
        let n = s.submit_doc(vec![(0, "x".into()), (3, "y".into())], 3);
        assert_eq!(n, 1);
        assert_eq!(s.current().unwrap().page, 3);
        // 空查询不产生命中
        s.show();
        let n = s.submit_doc(vec![(0, "x".into())], 0);
        assert_eq!(n, 0);
        assert_eq!(s.snippet(), None);
    }

    // ---- 分步扫描（P13） ---------------------------------------------------

    /// 进度文案是官方原文 `Searching page %1 of %2`，页码夹在 1..=total。
    #[test]
    fn progress_text_is_the_official_string() {
        assert_eq!(progress_text(1, 3), "Searching page 1 of 3");
        assert_eq!(progress_text(3, 3), "Searching page 3 of 3");
        // 越界的两头都不许写出怪数
        assert_eq!(progress_text(0, 3), "Searching page 1 of 3");
        assert_eq!(progress_text(9, 3), "Searching page 3 of 3");
        assert_eq!(progress_text(1, 0), "Searching page 1 of 1");
    }

    /// 进度条是底部整条（官方 `SearchResultsFooter`）。
    #[test]
    fn progress_bar_sits_at_the_bottom() {
        let (x, y, w, h) = progress_rect(1404, 1872);
        assert_eq!((x, y, w, h), (0, 1800, 1404, 72));
        assert_eq!(y + h, 1872, "要贴着屏幕底边");
    }

    /// 游标一页一页走，走完返回 None；进度按页数递增。
    #[test]
    fn scan_walks_every_page_in_order() {
        let mut s = Scan::new(4, None);
        assert_eq!(s.next_page(), Some(0));
        assert_eq!(s.label(), Some("Searching page 1 of 4".to_string()));
        assert!((s.fraction() - 0.25).abs() < 1e-6);
        assert_eq!(s.next_page(), Some(1));
        assert_eq!(s.next_page(), Some(2));
        assert_eq!(s.next_page(), Some(3));
        assert_eq!(s.label(), Some("Searching page 4 of 4".to_string()));
        assert!((s.fraction() - 1.0).abs() < 1e-6);
        assert_eq!(s.next_page(), None, "走完必须停下来");
        assert_eq!(s.next_page(), None, "再问还是 None");
        // 派完最后一页、还没收尾：文案停在最后一页（`cancel_scan` 后才会消失）
        assert_eq!(s.label(), Some("Searching page 4 of 4".to_string()));
    }

    /// `Page only` 只派那一页出去。
    #[test]
    fn scan_page_only_hands_out_one_page() {
        let mut s = Scan::new(5, Some(2));
        assert_eq!(s.next_page(), Some(2));
        assert_eq!(s.next_page(), None);
        // 越界的 only：一页都不派
        let mut s = Scan::new(2, Some(9));
        assert_eq!(s.next_page(), None);
    }

    /// 完整一轮：每 tick 一页、命中逐页累积，扫完才给最终计数。
    #[test]
    fn stepped_search_collects_hits_page_by_page() {
        let p = pages(&[&["alpha beta"], &["gamma"], &["alpha again"]]);
        let mut s = DocSearch::new();
        s.show();
        s.kb.text = "alpha".into();
        assert!(s.start_scan("alpha", p.len(), None));
        assert!(s.scanning());
        assert!(!s.kb_open(), "提交后键盘要收起");
        // 还没搜出结果：官方显示 `Searching...`
        assert_eq!(s.result_label(), "Searching...");
        assert_eq!(s.scan_label(), Some("Searching page 1 of 3".to_string()));
        // 一页一页来
        let mut got = 0;
        while let Some(page) = s.scan_page() {
            s.scan_take(page, &p[page]);
            got += 1;
            if got == 2 {
                // 已经有两页的结果了，计数要跟着长
                assert_eq!(s.result_label(), "Result 1 of 1");
            }
        }
        assert_eq!(got, 3, "三页都得搜到");
        assert_eq!(s.scan_page(), None);
        assert_eq!(s.count(), 2);
        assert_eq!(s.result_label(), "Result 1 of 2");
        assert_eq!(s.current().unwrap().page, 0);
        assert_eq!(s.next().unwrap().page, 2);
    }

    /// `Page only` 的扫描不给进度条；`Cancel` 只停扫描、保留已有结果。
    #[test]
    fn page_only_scan_has_no_progress_bar_and_cancel_stops_it() {
        let p = pages(&[&["alpha"], &["alpha"], &["alpha"]]);
        let mut s = DocSearch::new();
        s.show();
        s.kb.text = "alpha".into();
        s.toggle_page_only();
        assert!(s.start_scan("alpha", 3, Some(1)));
        assert_eq!(s.scan_label(), None, "Page only 官方不画进度条");
        assert_eq!(s.scan_progress(), None);
        assert_eq!(s.scan_page(), Some(1));
        s.scan_take(1, &p[1]);
        assert_eq!(s.count(), 1);
        // 中途取消：还在搜的状态立刻结束，结果保留
        s.cancel_scan();
        assert!(!s.scanning());
        assert_eq!(s.count(), 1);
        assert_eq!(s.result_label(), "Result 1 of 1");
    }

    /// 空查询 / 零页 / 改输入：都不进扫描态。
    #[test]
    fn scan_never_starts_for_empty_query() {
        let mut s = DocSearch::new();
        s.show();
        assert!(!s.start_scan("   ", 3, None));
        assert!(!s.scanning());
        s.kb.text = "x".into();
        assert!(!s.start_scan("x", 0, None), "没有页可扫");
        assert!(s.start_scan("x", 2, None));
        // 改输入 = 官方 `onTextChanged: searchController.cancel()`
        s.kb_open = true;
        assert!(s.edited());
        assert!(!s.scanning());
        assert_eq!(s.scan_label(), None);
    }

    /// 进度刷新限流：小文档一页一刷，大文档按 1/20 刷。
    #[test]
    fn scan_paint_is_throttled_by_page_count() {
        // 3 页：每页都刷（step = max(3/20, 1) = 1）
        let mut s = DocSearch::new();
        s.show();
        s.kb.text = "x".into();
        assert!(s.start_scan("x", 3, None));
        for i in 0..3 {
            assert_eq!(s.scan_page(), Some(i));
            assert!(s.scan_paint_due());
        }
        // 100 页：step = 5，只在 5/10/.../100 页刷
        let mut s = DocSearch::new();
        s.show();
        s.kb.text = "x".into();
        assert!(s.start_scan("x", 100, None));
        let mut fired = 0;
        while s.scan_page().is_some() {
            if s.scan_paint_due() {
                fired += 1;
            }
        }
        assert!(
            (2..=21).contains(&fired),
            "100 页最多刷 20 次左右，实际 {fired} 次"
        );
        assert!(fired >= 2, "大文档至少要能看到几次进度");
    }
}
