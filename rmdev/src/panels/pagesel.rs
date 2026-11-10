//! 页总览多选（官方 `PagePicker` 的选中态 + `PagesModalAction` 动作条）。
//!
//! 纯状态机 + 纯几何，不碰帧缓冲：`main.rs` 照着 [`PageSel::hits`] 画、照着
//! [`PageSel::hit`] 轮询。文案都来自实机 carve 出来的 QML：
//!
//! * `Select all` / `Select none` —— `res_06a644c.qml`（PagePicker 的
//!   `TextHeaderSmall` 下划线按钮；全选后文案换成 `Select none`）；
//! * 动作条 5 项 `Tag` / `Move` / `Delete` / `Duplicate` / `Cancel` ——
//!   `res_067dfe1.qml` 的 `currentActions()`（`Table of contents` 是 T2 的，
//!   `Convert to text` / `Send` 依赖服务器，本任务不画）；
//! * 删除确认 `Delete page permanently?` —— 同文件 `ConfirmChangeDialog`，
//!   总览里原来那版就在用这句。
//!
//! 交互（照 `PagesModalAction`）：长按任意页进入多选并选中该页；多选态点击 =
//! 切换选中；选中 0 页时 `Tag` / `Move` / `Delete` 灰掉且不吃点击。

use std::collections::BTreeSet;

/// 总览顶栏高度（和 `main.rs` 的 `Pages` 标题栏一致）
pub const HDR_H: i32 = 118;
/// 底部动作条高度（官方 `NavigatorModalActionBase` 是 120）
pub const BAR_H: i32 = 120;
/// 删除确认框尺寸（沿用总览原来那版）
pub const DLG_W: i32 = 900;
pub const DLG_H: i32 = 380;
/// 确认框里两个按钮
const DLG_BTN_W: i32 = 380;
const DLG_BTN_H: i32 = 88;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    /// 点缩略图：切换该页选中
    Toggle(usize),
    /// 顶部 `Select all`
    All,
    /// 顶部 `Select none`
    None,
    Tag,
    Move,
    Delete,
    /// 动作条 `Duplicate`（计划 §6 的范围外，但动作条里官方有这一项）
    Duplicate,
    /// 删除确认框里的 `Delete`
    DeleteConfirm,
    /// 删除确认框里的 `Cancel`
    DeleteCancel,
    /// 动作条 `Cancel`：退出多选
    Close,
    /// P9 顶栏 `Go to page`：打开页码输入
    GoToPage,
    /// P9 顶栏 `Grid view` / `List view`：切换排布
    ToggleView,
    /// P9 标签胶囊（下标指向 `chrome_hits()` 传进来的 `filters`）
    TagFilter(usize),
}

pub struct PageSel {
    open: bool,
    sel: Vec<bool>,
    /// 进入多选时长按的那一页（官方 `PageSelection` 的 anchor，留作以后 shift 连选）
    anchor: Option<usize>,
    /// 页数（= 缩略图个数）
    pages: usize,
    /// 删除确认框开着
    confirming: bool,
    /// 正在给选中页打标签（`main.rs` 用 `Notebook.tag_edit` 那套键盘）
    tagging: bool,
}

impl PageSel {
    pub fn new() -> PageSel {
        PageSel {
            open: false,
            sel: Vec::new(),
            anchor: None,
            pages: 0,
            confirming: false,
            tagging: false,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn pages(&self) -> usize {
        self.pages
    }

    pub fn anchor(&self) -> Option<usize> {
        self.anchor
    }

    /// 进入多选：清空旧的选中，只选中长按的那一页。
    pub fn show(&mut self, pages: usize, cur: usize) {
        self.open = true;
        self.pages = pages;
        self.sel = vec![false; pages];
        if let Some(s) = self.sel.get_mut(cur) {
            *s = true;
        }
        self.anchor = if cur < pages { Some(cur) } else { None };
        self.confirming = false;
        self.tagging = false;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.sel.clear();
        self.pages = 0;
        self.anchor = None;
        self.confirming = false;
        self.tagging = false;
    }

    /// 选中/取消一页。越界忽略。
    pub fn toggle(&mut self, i: usize) {
        if let Some(s) = self.sel.get_mut(i) {
            *s = !*s;
        }
    }

    pub fn select_all(&mut self) {
        for s in self.sel.iter_mut() {
            *s = true;
        }
    }

    pub fn select_none(&mut self) {
        for s in self.sel.iter_mut() {
            *s = false;
        }
    }

    /// 只选中给定的页（页标签筛选开着时的 `Select all`：官方只选 model 里的页）。
    pub fn select_only(&mut self, idx: &[usize]) {
        self.select_none();
        for &i in idx {
            if let Some(s) = self.sel.get_mut(i) {
                *s = true;
            }
        }
    }

    /// 按下标升序（`BTreeSet` 天然有序且去重）。
    pub fn selected(&self) -> Vec<usize> {
        self.sel
            .iter()
            .enumerate()
            .filter(|(_, &on)| on)
            .map(|(i, _)| i)
            .collect()
    }

    pub fn count(&self) -> usize {
        self.sel.iter().filter(|&&on| on).count()
    }

    pub fn is_selected(&self, i: usize) -> bool {
        self.sel.get(i).copied().unwrap_or(false)
    }

    /// 全选了（决定顶栏画 `Select all` 还是 `Select none`）。
    pub fn all_selected(&self) -> bool {
        !self.sel.is_empty() && self.sel.iter().all(|&on| on)
    }

    /// 顶部那个下划线按钮该发哪个动作。
    pub fn all_button(&self) -> Act {
        if self.all_selected() {
            Act::None
        } else {
            Act::All
        }
    }

    pub fn confirming(&self) -> bool {
        self.confirming
    }

    pub fn ask_delete(&mut self) {
        self.confirming = true;
    }

    pub fn cancel_delete(&mut self) {
        self.confirming = false;
    }

    pub fn tagging(&self) -> bool {
        self.tagging
    }

    pub fn set_tagging(&mut self, on: bool) {
        self.tagging = on;
    }

    /// 删除确认框的矩形（`(x, y, w, h)`）。
    pub fn delete_dialog(w: i32, h: i32) -> (i32, i32, i32, i32) {
        ((w - DLG_W) / 2, (h - DLG_H) / 2, DLG_W, DLG_H)
    }

    /// 可点区域，顺序 = 绘制顺序。灰掉的 `Tag` / `Move` / `Delete` 不出现
    /// （点不到）；删除确认开着时只剩确认框那两个按钮（模态）。
    pub fn hits(&self, w: i32, h: i32) -> Vec<((i32, i32, i32, i32), Act)> {
        if !self.open {
            return Vec::new();
        }
        if self.confirming {
            let (bx, by, bw, bh) = Self::delete_dialog(w, h);
            let y2 = by + bh - 150;
            return vec![
                ((bx + 48, y2, DLG_BTN_W, DLG_BTN_H), Act::DeleteCancel),
                (
                    (bx + bw - 48 - DLG_BTN_W, y2, DLG_BTN_W, DLG_BTN_H),
                    Act::DeleteConfirm,
                ),
            ];
        }
        let mut v = Vec::with_capacity(6);
        // 顶栏右边那个下划线按钮（右边距 33，和标题同一行）
        v.push(((w - 33 - 320, 0, 320, HDR_H), self.all_button()));
        // 底部动作条：5 项等分
        let bar: [(Act, bool); 5] = [
            (Act::Tag, self.count() > 0),
            (Act::Move, self.count() > 0),
            (Act::Delete, self.count() > 0),
            (Act::Duplicate, true),
            (Act::Close, true),
        ];
        let by = h - BAR_H;
        for (i, (act, on)) in bar.iter().enumerate() {
            if !on {
                continue;
            }
            let x0 = w * i as i32 / 5;
            let x1 = w * (i as i32 + 1) / 5;
            v.push(((x0, by, x1 - x0, BAR_H), *act));
        }
        v
    }

    /// 命中测试（后画的优先，和别的面板一致）。
    pub fn hit(&self, x: f32, y: f32, w: i32, h: i32) -> Option<Act> {
        hit_in(&self.hits(w, h), x, y)
    }

    /// 多选移动的执行序列：把选中的页当**一整块**搬到 `to`（0 基，块的首叶落在
    /// 这一页；放不下就贴到末尾），返回按目标下标升序的 `(from, to)`，逐个喂给
    /// `library::move_page`。
    ///
    /// 先算出"最终该长什么样"，再用选择排序的思路倒推单页移动 —— 这样多选时
    /// 块内部顺序不会像"每页都挪到同一个下标"那样反过来。单页是常见情况，
    /// 直接一步到位（不用挪三次）。
    pub fn move_plan(&self, to: usize) -> Vec<(usize, usize)> {
        let n = self.pages;
        let sel: BTreeSet<usize> = self.selected().into_iter().collect();
        if n == 0 || sel.is_empty() {
            return Vec::new();
        }
        let to = to.min(n - 1);
        if sel.len() == 1 {
            let from = *sel.iter().next().unwrap();
            return if from == to { Vec::new() } else { vec![(from, to)] };
        }
        // 目标顺序：抽掉选中的页，把这一块插到 to（放不下贴末尾）
        let rest: Vec<usize> = (0..n).filter(|i| !sel.contains(i)).collect();
        let start = to.min(rest.len());
        let mut want: Vec<usize> = rest[..start].to_vec();
        want.extend(sel.iter().copied());
        want.extend(rest[start..].to_vec());

        let mut order: Vec<usize> = (0..n).collect();
        let mut out = Vec::new();
        for (i, &target) in want.iter().enumerate() {
            if order[i] == target {
                continue;
            }
            let Some(from) = order.iter().position(|&v| v == target) else {
                continue;
            };
            order.remove(from);
            order.insert(i, target);
            out.push((from, i));
        }
        out
    }
}

impl Default for PageSel {
    fn default() -> Self {
        PageSel::new()
    }
}

/// 一组命中区里的命中测试（后压的优先，和 `widgets::Hits` 一条规则）。
pub fn hit_in(hs: &[((i32, i32, i32, i32), Act)], x: f32, y: f32) -> Option<Act> {
    hs.iter()
        .rev()
        .find(|(r, _)| {
            x >= r.0 as f32
                && y >= r.1 as f32
                && x < (r.0 + r.2) as f32
                && y < (r.1 + r.3) as f32
        })
        .map(|(_, a)| *a)
}

// ---------------------------------------------------------------- P9：视图 / 跳页 / 标签筛选
//
// 官方 `res_06a644c.qml` 的顶栏：`Go to page`（IconButton）、`ContentDropdown`
// 在 `Grid view` / `List view` 之间切、底部 `TagFilter` 胶囊（`bottomColumn`）。
// 这里只放纯几何 / 纯解析，`main.rs` 照着画、照着命中。

/// 页总览的两种排布。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Grid,
    List,
}

impl View {
    /// 点一下切换。
    pub fn toggle(self) -> View {
        match self {
            View::Grid => View::List,
            View::List => View::Grid,
        }
    }

    /// 按钮文案是**当前**视图名：官方
    /// `closedText: pagePickerPanel.listMode ? qsTr("List view") : qsTr("Grid view")`。
    pub fn label(self) -> &'static str {
        match self {
            View::Grid => "Grid view",
            View::List => "List view",
        }
    }
}

impl Default for View {
    fn default() -> View {
        View::Grid
    }
}

/// 顶栏 `Go to page` 的按钮框：标题 `Pages` 之后、`Table of contents` 之前。
const GO_X: i32 = 240;
const GO_W: i32 = 240;
/// 顶栏视图切换按钮（`Grid view` / `List view`）。
const VIEW_X: i32 = GO_X + GO_W + 20;
const VIEW_W: i32 = 200;
/// 顶栏按钮字号（和 `Select all` 同一档）。
pub const TOP_BTN_SIZE: f32 = 30.0;

/// 列表视图一行的高度（官方 `ListView` 一行：`Page N` + 页标签胶囊）。
pub const LIST_ROW_H: i32 = 120;
/// 列表行的左右外边距（官方 `ListView` 的 `leftMargin = designGridVerticalGutter - designGridMargin = 18`）。
pub const LIST_MARGIN: i32 = 18;
/// 列表顶部从顶栏那条 2px 横线下面开始。
const LIST_TOP: i32 = HDR_H + 2;
/// 列表底部留白（和网格视图的 `h - 120` 一致）。
const LIST_BOTTOM: i32 = 120;

/// 列表视图的行矩形（`(x, y, w, h)`，按页序）。放不下的行不返回
/// （和网格视图一样是"一屏到底、不滚动"）。
pub fn list_rows(w: i32, h: i32, n: usize) -> Vec<(i32, i32, i32, i32)> {
    let avail = (h - LIST_TOP - LIST_BOTTOM).max(0);
    let fit = (avail / LIST_ROW_H) as usize;
    let rw = (w - 2 * LIST_MARGIN).max(0);
    (0..n.min(fit))
        .map(|i| (LIST_MARGIN, LIST_TOP + i as i32 * LIST_ROW_H, rw, LIST_ROW_H))
        .collect()
}

/// `Go to page` 输入框里的一串字 → **1 基**页号。
///
/// 官方输入框挂的是 `IntValidator { bottom: 1; top: document.pageCount }`，
/// 所以非数字、0、越界一律算无效（返回 `None`，面板不关、只提示范围）。
pub fn parse_page(s: &str, total: usize) -> Option<usize> {
    let t = s.trim();
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let p: usize = t.parse().ok()?;
    if p >= 1 && p <= total {
        Some(p)
    } else {
        None
    }
}

/// 按页标签筛页。
///
/// `tags` 就是 `library.page_tags()` 的形状 `(pageId, 页标签)`，但**下标要按页序**
/// （`main.rs` 先用 `page_ids()` 对齐再传进来）。`None` = 不过滤（全部页），
/// `Some(t)` = 只留带 `t` 的页。返回升序页号。
pub fn filter_pages(tags: &[(String, Vec<String>)], tag: Option<&str>) -> Vec<usize> {
    match tag {
        None => (0..tags.len()).collect(),
        Some(t) => tags
            .iter()
            .enumerate()
            .filter(|(_, (_, ts))| ts.iter().any(|x| x == t))
            .map(|(i, _)| i)
            .collect(),
    }
}

/// 标签胶囊（官方 `TagFilter` / `TagPill`）高度，和 Tags 视图同款。
pub const TAG_PILL_H: i32 = 46;
/// 胶囊间距 / 左边距 / 胶囊内边距（`TagPill: width = text + 50`）。
const TAG_GAP: i32 = 20;
const TAG_X0: i32 = 33;
const TAG_PAD: i32 = 50;
const TAG_MIN_W: i32 = 80;
/// 胶囊行距屏幕底边的距离（`+ Page` 那颗按钮在右下，别撞上）。
const TAG_BOTTOM: i32 = 70;
/// 右下角留给 `+ Page` 的宽度（它在 `w - 40 - 200`）。
const TAG_RIGHT_RESERVE: i32 = 300;

impl PageSel {
    /// P9 在**非多选**总览态的三组命中区：顶栏 `Go to page`、视图切换、
    /// 底部标签胶囊。多选态它们都收起（官方 `visible: !selecting`），
    /// 所以没并进 [`PageSel::hits`]，由 `main.rs` 单独取。
    ///
    /// `filters` 是这一份文档的页标签 `(标签名, 文字宽)` —— 胶囊宽度要跟文字
    /// 对齐，而量文字得用 `ui::text::Text`，面板这边拿不到，只能让调用方量好传进来。
    pub fn chrome_hits(
        &self,
        w: i32,
        h: i32,
        filters: &[(String, i32)],
    ) -> Vec<((i32, i32, i32, i32), Act)> {
        let mut v = Vec::with_capacity(2 + filters.len());
        v.push(((GO_X, 0, GO_W, HDR_H), Act::GoToPage));
        v.push(((VIEW_X, 0, VIEW_W, HDR_H), Act::ToggleView));
        let y = h - TAG_PILL_H - TAG_BOTTOM;
        let right = w - TAG_RIGHT_RESERVE;
        let mut x = TAG_X0;
        for (i, (_, tw)) in filters.iter().enumerate() {
            let pw = (*tw + TAG_PAD).max(TAG_MIN_W);
            // 一行放不下就不画了（官方是 Flow，会换行；总览里没滚动，换行会盖住缩略图）
            if x + pw > right {
                break;
            }
            v.push(((x, y, pw, TAG_PILL_H), Act::TagFilter(i)));
            x += pw + TAG_GAP;
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(pages: usize, cur: usize) -> PageSel {
        let mut s = PageSel::new();
        s.show(pages, cur);
        s
    }

    /// 长按进入 → 只有那一页选中；关闭后状态清干净。
    #[test]
    fn show_selects_current_then_close_resets() {
        let mut s = shown(5, 2);
        assert!(s.is_open());
        assert_eq!(s.pages(), 5);
        assert_eq!(s.selected(), vec![2]);
        assert_eq!(s.anchor(), Some(2));
        assert_eq!(s.count(), 1);
        assert!(!s.all_selected());
        // 越界的长按（空文档）不 panic
        let mut s2 = PageSel::new();
        s2.show(0, 0);
        assert!(s2.selected().is_empty());
        assert_eq!(s2.anchor(), None);
        assert_eq!(s2.all_button(), Act::All);

        s.close();
        assert!(!s.is_open() && s.count() == 0 && s.pages() == 0);
        assert!(s.hits(1404, 1872).is_empty());
    }

    /// `toggle` 两次回到原状；越界是空操作。
    #[test]
    fn toggle_is_stable_and_ignores_out_of_range() {
        let mut s = shown(4, 0);
        assert_eq!(s.selected(), vec![0]);
        s.toggle(3);
        assert_eq!(s.selected(), vec![0, 3], "selected() 必须升序");
        s.toggle(3);
        assert_eq!(s.selected(), vec![0], "再 toggle 一次回到原状");
        s.toggle(9);
        assert_eq!(s.selected(), vec![0], "越界不能改状态");
        assert!(!s.is_selected(9));
        s.toggle(0);
        assert_eq!(s.count(), 0);
    }

    /// 全选 / 全不选 / 顶栏按钮文案跟着变。
    #[test]
    fn select_all_and_none() {
        let mut s = shown(6, 1);
        assert_eq!(s.all_button(), Act::All);
        s.select_all();
        assert_eq!(s.count(), 6);
        assert_eq!(s.selected(), vec![0, 1, 2, 3, 4, 5]);
        assert!(s.all_selected());
        assert_eq!(s.all_button(), Act::None);
        s.select_none();
        assert_eq!(s.count(), 0);
        assert!(!s.all_selected());
        assert_eq!(s.all_button(), Act::All);
    }

    /// 筛选态下的全选：只选列出来的页，越界忽略（官方 `pageSelection.setPageNumbers(model)`）。
    #[test]
    fn select_only_keeps_listed_pages() {
        let mut s = shown(6, 0);
        s.select_only(&[0, 3, 5, 9]);
        assert_eq!(s.selected(), vec![0, 3, 5], "越界的不算");
        assert!(!s.all_selected(), "没全选，顶栏还得是 Select all");
        assert_eq!(s.all_button(), Act::All);
        s.select_only(&[]);
        assert_eq!(s.count(), 0);
    }

    /// 动作条：选中 0 页时 Tag/Move/Delete 点不到；选中后 5 项都在屏内、
    /// 互不重叠、命中区 ≥ 80×80。
    #[test]
    fn hits_layout_and_disabled_actions() {
        let mut s = shown(12, 0);
        s.select_none();
        let w = 1404;
        let h = 1872;

        let hs = s.hits(w, h);
        let acts: Vec<Act> = hs.iter().map(|(_, a)| *a).collect();
        assert!(acts.contains(&Act::All));
        assert!(!acts.contains(&Act::Tag));
        assert!(!acts.contains(&Act::Move));
        assert!(!acts.contains(&Act::Delete));
        assert!(acts.contains(&Act::Close) && acts.contains(&Act::Duplicate));

        s.toggle(5);
        let hs = s.hits(w, h);
        for act in [Act::Tag, Act::Move, Act::Delete, Act::Duplicate, Act::Close] {
            assert!(hs.iter().any(|(_, a)| *a == act), "{act:?} 没画出来");
        }
        for (r, act) in &hs {
            assert!(r.2 >= 80 && r.3 >= 80, "{act:?} 命中区 {r:?} 太小");
            assert!(r.0 >= 0 && r.0 + r.2 <= w, "{act:?} 出屏");
            assert!(r.1 >= 0 && r.1 + r.3 <= h, "{act:?} 出屏");
        }
        // 动作条 5 项互不重叠
        let bar: Vec<(i32, i32, i32, i32)> = hs
            .iter()
            .filter(|(_, a)| matches!(a, Act::Tag | Act::Move | Act::Delete | Act::Duplicate | Act::Close))
            .map(|(r, _)| *r)
            .collect();
        assert_eq!(bar.len(), 5);
        for i in 0..bar.len() {
            for j in i + 1..bar.len() {
                let (a, b) = (bar[i], bar[j]);
                assert!(
                    !(a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3),
                    "动作条 {i}/{j} 叠了"
                );
            }
        }
    }

    /// 命中分发：顶栏按钮跟状态走；确认态只剩两个按钮（模态）。
    #[test]
    fn hit_dispatch_and_delete_confirm() {
        let (w, h) = (1404, 1872);
        let mut s = shown(5, 0);
        // 顶栏按钮中点
        assert_eq!(s.hit((w - 33 - 320 + 160) as f32, 60.0, w, h), Some(Act::All));
        s.select_all();
        assert_eq!(s.hit((w - 33 - 320 + 160) as f32, 60.0, w, h), Some(Act::None));
        // 动作条 Tag（第一格中点）
        s.select_none();
        s.toggle(1);
        assert_eq!(s.hit((w / 10) as f32, (h - BAR_H / 2) as f32, w, h), Some(Act::Tag));
        // 页面区域（缩略图由 main 管）不算面板命中
        assert_eq!(s.hit(700.0, 600.0, w, h), None);

        s.ask_delete();
        assert!(s.confirming());
        let hs = s.hits(w, h);
        assert_eq!(hs.len(), 2, "确认框是模态的");
        let (bx, by, _bw, bh) = PageSel::delete_dialog(w, h);
        assert_eq!(hs[0].0, (bx + 48, by + bh - 150, DLG_BTN_W, DLG_BTN_H));
        assert_eq!(s.hit((bx + 48 + 100) as f32, (by + bh - 150 + 40) as f32, w, h), Some(Act::DeleteCancel));
        s.cancel_delete();
        assert!(!s.confirming());
        assert_eq!(s.hit((w / 10) as f32, (h - BAR_H / 2) as f32, w, h), Some(Act::Tag));
    }

    /// 多选移动：整块搬走、块内顺序不变、返回的序列真的能走通。
    #[test]
    fn move_plan_keeps_block_order() {
        // 5 页里选中 [1,3]，搬到最后 → [0,2,4,1,3]
        let mut s = shown(5, 1);
        s.toggle(3);
        assert_eq!(s.selected(), vec![1, 3]);
        let plan = s.move_plan(4);
        let mut order: Vec<usize> = (0..5).collect();
        for (from, to) in &plan {
            let e = order.remove(*from);
            order.insert(*to, e);
        }
        assert_eq!(order, vec![0, 2, 4, 1, 3]);
        assert_eq!(plan.len(), 2, "两页最多两步");
        // 目标下标升序（依次移动）
        assert!(plan.windows(2).all(|p| p[0].1 <= p[1].1));

        // 搬到头 → [1,3,0,2,4]
        let plan = s.move_plan(0);
        let mut order: Vec<usize> = (0..5).collect();
        for (from, to) in &plan {
            let e = order.remove(*from);
            order.insert(*to, e);
        }
        assert_eq!(order, vec![1, 3, 0, 2, 4]);

        // 单页：等价于一次 move_page
        let one = shown(5, 1);
        assert_eq!(one.move_plan(4), vec![(1, 4)]);
        assert_eq!(one.move_plan(9), vec![(1, 4)], "目标越界贴到最后一页");
        // 就在原地 → 不用动
        assert!(one.move_plan(1).is_empty());
        // 空选 / 空文档
        let empty = PageSel::new();
        assert!(empty.move_plan(0).is_empty());
    }

    // ---------------------------------------------------------------- P9

    /// 视图切换：默认网格、文案跟**当前**视图、切两次回原状。
    #[test]
    fn view_toggle_and_labels() {
        assert_eq!(View::default(), View::Grid);
        assert_eq!(View::Grid.label(), "Grid view");
        assert_eq!(View::List.label(), "List view");
        assert_eq!(View::Grid.toggle(), View::List);
        assert_eq!(View::List.toggle(), View::Grid);
        assert_eq!(View::Grid.toggle().toggle(), View::Grid);
    }

    /// `Go to page`：1 基、只收纯数字、0 / 越界 / 非数字 → `None`。
    #[test]
    fn parse_page_is_one_based_and_bounded() {
        assert_eq!(parse_page("1", 10), Some(1));
        assert_eq!(parse_page("10", 10), Some(10));
        assert_eq!(parse_page(" 3 ", 10), Some(3), "两端空白要容忍");
        assert_eq!(parse_page("0", 10), None, "页号从 1 起");
        assert_eq!(parse_page("11", 10), None, "越界");
        assert_eq!(parse_page("99", 10), None, "越界");
        assert_eq!(parse_page("abc", 10), None);
        assert_eq!(parse_page("1.5", 10), None);
        assert_eq!(parse_page("-1", 10), None);
        assert_eq!(parse_page("", 10), None);
        assert_eq!(parse_page("3", 0), None, "空文档没有页可跳");
    }

    /// 标签筛选：`None` 全给、`Some` 只给带这个标签的页、大小写敏感。
    #[test]
    fn filter_pages_by_tag() {
        let tags = vec![
            ("p0".to_string(), vec!["Ideas".to_string()]),
            ("p1".to_string(), Vec::new()),
            (
                "p2".to_string(),
                vec!["Ideas".to_string(), "Draft".to_string()],
            ),
            ("p3".to_string(), vec!["Draft".to_string()]),
        ];
        assert_eq!(filter_pages(&tags, None), vec![0, 1, 2, 3]);
        assert_eq!(filter_pages(&tags, Some("Ideas")), vec![0, 2]);
        assert_eq!(filter_pages(&tags, Some("Draft")), vec![2, 3]);
        assert_eq!(filter_pages(&tags, Some("ideas")), Vec::<usize>::new());
        assert_eq!(filter_pages(&tags, Some("Nope")), Vec::<usize>::new());
        assert_eq!(filter_pages(&[], None), Vec::<usize>::new());
        assert_eq!(filter_pages(&[], Some("Ideas")), Vec::<usize>::new());
    }

    /// 列表行：等宽等距、不出屏、放不下的行不返回、不 panic。
    #[test]
    fn list_rows_layout() {
        let (w, h) = (1404, 1872);
        let rows = list_rows(w, h, 40);
        let fit = ((h - LIST_TOP - LIST_BOTTOM) / LIST_ROW_H) as usize;
        assert_eq!(rows.len(), fit, "一屏该放 {fit} 行");
        assert!(fit >= 10, "列表一屏至少得有 10 行");
        assert_eq!(
            rows[0],
            (LIST_MARGIN, LIST_TOP, w - 2 * LIST_MARGIN, LIST_ROW_H)
        );
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(r.1, LIST_TOP + i as i32 * LIST_ROW_H, "行距 = 行高");
            assert!(r.0 >= 0 && r.0 + r.2 <= w, "行 {i} 横向出屏");
            assert!(r.1 >= 0 && r.1 + r.3 <= h, "行 {i} 纵向出屏");
        }
        assert!(rows.windows(2).all(|p| p[0].1 + p[0].3 <= p[1].1));
        // 页少就少给；空文档 / 屏幕太矮都是一行都没有
        assert_eq!(list_rows(w, h, 3).len(), 3);
        assert!(list_rows(w, h, 0).is_empty());
        assert!(list_rows(w, 200, 5).is_empty());
        assert!(list_rows(0, h, 5).iter().all(|r| r.2 == 0));
    }

    /// P9 命中区：顶栏两颗按钮 + 底部标签胶囊都在屏内、互不重叠、点得中。
    #[test]
    fn chrome_hits_geometry_and_dispatch() {
        let (w, h) = (1404, 1872);
        let s = PageSel::new();
        // 没有页标签时只有顶栏两颗
        let hs = s.chrome_hits(w, h, &[]);
        assert_eq!(hs.len(), 2);
        assert!(matches!(hs[0].1, Act::GoToPage));
        assert!(matches!(hs[1].1, Act::ToggleView));
        for (r, act) in &hs {
            assert!(r.0 >= 220, "{act:?} 压到返回箭头了");
            assert!(r.0 + r.2 <= w, "{act:?} 出屏");
            assert!(r.3 >= 80, "{act:?} 够不着");
            assert_eq!(r.1, 0);
        }
        // 两颗按钮不叠、也不跟标题抢位置
        let (a, b) = (hs[0].0, hs[1].0);
        assert!(a.0 + a.2 <= b.0, "Go to page 和视图按钮叠了");
        assert_eq!(
            hit_in(&hs, (a.0 + a.2 / 2) as f32, 60.0),
            Some(Act::GoToPage)
        );
        assert_eq!(
            hit_in(&hs, (b.0 + b.2 / 2) as f32, 60.0),
            Some(Act::ToggleView)
        );

        // 有页标签：胶囊排在最底下，从左边开始，让开右下角 `+ Page`
        let filters = vec![
            ("Ideas".to_string(), 90),
            ("Draft".to_string(), 110),
            ("Long tag".to_string(), 200),
        ];
        let hs = s.chrome_hits(w, h, &filters);
        assert_eq!(hs.len(), 2 + filters.len());
        assert_eq!(hs[2].0.2, 90 + TAG_PAD, "胶囊宽 = 文字 + 50");
        let mut last_right = 0;
        for (i, (r, act)) in hs[2..].iter().enumerate() {
            assert_eq!(*act, Act::TagFilter(i));
            assert_eq!(r.3, TAG_PILL_H);
            assert!(r.1 > 1700 && r.1 + r.3 < h, "胶囊没贴在底部：{r:?}");
            assert!(r.0 + r.2 <= w - TAG_RIGHT_RESERVE, "胶囊盖到 + Page 了");
            assert!(r.0 >= last_right, "胶囊顺序 / 重叠不对");
            last_right = r.0 + r.2;
            assert_eq!(
                hit_in(&hs, (r.0 + r.2 / 2) as f32, (r.1 + 20) as f32),
                Some(Act::TagFilter(i))
            );
        }
        // 屏幕窄了只放得下一部分，剩下的不画（不重叠、不 panic）
        let narrow = s.chrome_hits(500, h, &filters);
        assert!(narrow.len() > 2 && narrow.len() < hs.len());
        assert!(narrow[2..].iter().all(|(r, _)| r.0 + r.2 <= 500));
        // 空文档 / 无用标签时不会给出胶囊
        assert_eq!(s.chrome_hits(w, h, &[]).len(), 2);
    }
}
