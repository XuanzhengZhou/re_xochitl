//! Table of contents 面板（页总览顶部那一行 `Table of contents`）。
//!
//! 官方 `res_067046e.qml`：整页 `Background` + `Header`（标题 `Table of contents`，
//! 拒绝键回到页总览），下面一列章节（`ListView`），每行是
//! `TextHeaderRegular`（按 `level` 缩进 `level * 50`、当前章加粗）+ 右侧
//! `TextCaption` 页码（`modelData.page + 1`），行底一条细线。
//! 数据来自 `Doc::toc()`：重排格式（EPUB/TXT/MOBI/FB2/DOCX）各自把大纲映射成
//! `Toc { depth, title, page }`。
//!
//! 行高与缩进按官方界面实测值定：行高 120（手指好点）、缩进 1→0 / 2→40 / 3→80、
//! 当前页那一章整行反白（黑底白字）、空目录居中灰字、条目多于可视行时上下拖动滚动。
//! 约定同其它面板：`hits()` 是纯布局（可单测），`draw()` 只照着画。

use crate::doc::Toc;
use crate::fb::{gray565, Fb};
use crate::ui::text::Text;

/// 黑头高（与 `nbsettings` 一致）
pub const HDR_H: i32 = 118;
/// 条目行高
pub const ROW_H: i32 = 120;
/// 列表顶边（黑头 + 2px 分线）
pub const LIST_TOP: i32 = HDR_H + 2;
/// 手指/笔上下拖多少像素滚一行
pub const DRAG_STEP: f32 = 80.0;

/// 面板是整页 sheet，屏幕固定 1404×1872；`show()` 拿不到帧缓冲尺寸，
/// 算"打开时把当前章滚进视野"时用这个当屏高（真机永远是它）。
const SCREEN_H: i32 = 1872;

const FONT_HDR: f32 = 33.0;
const FONT_ITEM: f32 = 25.0;
const FONT_EMPTY: f32 = 30.0;
/// 正文黑 / 灰
const INK: u8 = 20;
const MUTED: u8 = 130;
/// 行底细线
const RULE: u8 = 225;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    /// 点第几项（`items` 的下标）
    Pick(usize),
    /// 左上角 `Cancel`
    Close,
}

pub struct TocPanel {
    open: bool,
    items: Vec<Toc>,
    /// 当前页
    cur: usize,
    /// 列表滚动首项
    top: usize,
    /// 当前页对应的目录项
    cur_item: Option<usize>,
}

impl TocPanel {
    pub fn new() -> TocPanel {
        TocPanel {
            open: false,
            items: Vec::new(),
            cur: 0,
            top: 0,
            cur_item: None,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// 打开面板。`cur_page` = 当前页；当前页那一章反白，并把它滚进视野
    /// （官方 `ensureCurrentChapterInView`）。
    pub fn show(&mut self, items: Vec<Toc>, cur_page: usize) {
        self.open = true;
        self.cur = cur_page;
        self.items = items;
        self.cur_item = find_cur(&self.items, self.cur);
        // 当前章上面留一项做上下文，底边不留白
        let vis = rows_visible(SCREEN_H).max(1);
        let max = self.items.len().saturating_sub(vis);
        self.top = match self.cur_item {
            Some(i) if cur_page > 0 => i.saturating_sub(1),
            _ => 0,
        }
        .min(max);
    }

    pub fn close(&mut self) {
        self.open = false;
    }

    /// 点条目：返回要跳的页号（同时关面板）。越界返回 `None`。
    pub fn pick(&mut self, i: usize) -> Option<usize> {
        let page = self.items.get(i)?.page;
        self.open = false;
        Some(page)
    }

    /// 上下滚 `step` 行（正数 = 往后看）。返回是否真的动了
    /// （条目不够一屏、或已经到头时不动，主程序就不必重画）。
    pub fn scroll(&mut self, step: i32, h: i32) -> bool {
        let max = self.items.len().saturating_sub(rows_visible(h).max(1));
        let before = self.top;
        self.top = (self.top as i32 + step).clamp(0, max as i32) as usize;
        self.top != before
    }

    /// 命中区。返回 `Vec<((x,y,w,h), Act)>`，顺序 = 绘制顺序。
    pub fn hits(&self, w: i32, h: i32) -> Vec<((i32, i32, i32, i32), Act)> {
        let mut v = Vec::with_capacity(rows_visible(h) + 1);
        v.push(((0, 0, 240, HDR_H), Act::Close));
        for k in 0..rows_visible(h) {
            let i = self.top + k;
            if i >= self.items.len() {
                break;
            }
            let y = LIST_TOP + k as i32 * ROW_H;
            v.push(((0, y, w, ROW_H), Act::Pick(i)));
        }
        v
    }

    /// 画整页。调用方保证打开状态；`fb` 会被整块刷白。
    pub fn draw(&mut self, fb: &mut Fb, text: &mut Option<Text>) {
        let (w, h) = (fb.width, fb.height);
        fb.fill_rect(0, 0, w, h, gray565(255));

        // ---------- 黑头：Cancel | Table of contents
        fb.fill_rect(0, 0, w, HDR_H, gray565(0));
        if let Some(t) = text.as_mut() {
            t.draw_bold(fb, 32, 74, FONT_HDR, "Cancel", 255);
            let title = "Table of contents";
            let tw = t.width_bold(FONT_HDR, title);
            t.draw_bold(
                fb,
                (w as f32 / 2.0 - tw / 2.0) as i32,
                74,
                FONT_HDR,
                title,
                255,
            );
        }

        // ---------- 空目录：居中灰字
        if self.items.is_empty() {
            if let Some(t) = text.as_mut() {
                let msg = "No table of contents";
                let tw = t.width(FONT_EMPTY, msg);
                t.draw(
                    fb,
                    (w as f32 / 2.0 - tw / 2.0) as i32,
                    h / 2,
                    FONT_EMPTY,
                    msg,
                    MUTED,
                );
            }
            return;
        }

        // ---------- 条目：当前章整行反白 + 右侧页码 + 行底细线
        for k in 0..rows_visible(h) {
            let i = self.top + k;
            if i >= self.items.len() {
                break;
            }
            let it = &self.items[i];
            let y = LIST_TOP + k as i32 * ROW_H;
            let sel = self.cur_item == Some(i);
            if sel {
                fb.fill_rect(0, y, w, ROW_H, gray565(0));
            }
            let tone = if sel { 255 } else { INK };
            let x = 33 + indent(it.depth);
            let page = (it.page + 1).to_string();
            if let Some(t) = text.as_mut() {
                let page_w = t.width(FONT_ITEM, &page) as i32;
                // 标题右边要给页码让出位置（页码右缘留 33）
                let max_w = (w - 33 - page_w - 33 - indent(it.depth)).max(80);
                let title = t.ellipsize(FONT_ITEM, &it.title, max_w);
                if sel {
                    t.draw_bold(fb, x, y + 74, FONT_ITEM, &title, tone);
                } else {
                    t.draw(fb, x, y + 74, FONT_ITEM, &title, tone);
                }
                t.draw(fb, w - 33 - page_w, y + 74, FONT_ITEM, &page, tone);
            }
            if !sel {
                fb.fill_rect(x, y + ROW_H - 1, w - x, 1, gray565(RULE));
            }
        }
    }
}

impl Default for TocPanel {
    fn default() -> Self {
        TocPanel::new()
    }
}

/// 一屏能放下几整行。
fn rows_visible(h: i32) -> usize {
    ((h - LIST_TOP).max(0) / ROW_H) as usize
}

/// `depth` → 左缩进（1→0 / 2→40 / 3→80，再深的按 80 封顶）。
fn indent(depth: u8) -> i32 {
    ((depth.saturating_sub(1)) as i32 * 40).min(80)
}

/// 当前页落在哪一章：最后一个 `page <= cur` 的条目（官方 `isCurrentChapter`）。
fn find_cur(items: &[Toc], cur: usize) -> Option<usize> {
    let mut found = None;
    for (i, it) in items.iter().enumerate() {
        if it.page <= cur {
            found = Some(i);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(n: usize) -> Vec<Toc> {
        (0..n)
            .map(|i| Toc {
                depth: (i % 3 + 1) as u8,
                title: format!("Chapter {}", i + 1),
                page: i * 10,
            })
            .collect()
    }

    /// 单测 1：10 项 → 10 个条目命中区 + 1 个 `Cancel`，y 递增且都在屏内、
    /// 每个都够手指点的大小。
    #[test]
    fn hits_cover_close_and_every_row() {
        let mut p = TocPanel::new();
        p.show(items(10), 0);
        let hs = p.hits(1404, 1872);
        assert_eq!(hs.len(), 11);
        assert_eq!(hs[0].1, Act::Close);
        let mut prev = -1;
        for (r, act) in &hs {
            assert!(r.1 > prev, "{act:?} 的 y 没递增");
            prev = r.1;
            assert!(r.0 >= 0 && r.1 >= 0 && r.0 + r.2 <= 1404 && r.1 + r.3 <= 1872);
            assert!(r.2 >= 80 && r.3 >= 80, "{act:?} 命中区 {r:?} 太小");
        }
        for (k, (_, act)) in hs.iter().skip(1).enumerate() {
            assert_eq!(*act, Act::Pick(k));
        }
    }

    /// 单测 2：`pick(3)` 返回第 4 项的页号，面板随即关闭。
    #[test]
    fn pick_returns_page_and_closes() {
        let mut p = TocPanel::new();
        p.show(items(10), 0);
        assert!(p.is_open());
        assert_eq!(p.pick(3), Some(30));
        assert!(!p.is_open());
        assert_eq!(p.pick(99), None);
    }

    /// 单测 3：当前页 → 目录项（章首页算自己那章，两章之间算前一项）。
    #[test]
    fn cur_item_is_the_chapter_containing_the_page() {
        let mut p = TocPanel::new();
        p.show(items(10), 0);
        assert_eq!(p.cur_item, Some(0));
        p.show(items(10), 10);
        assert_eq!(p.cur_item, Some(1)); // 章首页
        p.show(items(10), 15);
        assert_eq!(p.cur_item, Some(1)); // 10..20 之间 → 前一项
        p.show(items(10), 35);
        assert_eq!(p.cur_item, Some(3)); // 30..40
        p.show(items(10), 90);
        assert_eq!(p.cur_item, Some(9)); // 最后一章之后还是最后一项
    }

    /// 单测 4：空目录只有一个 `Cancel`，画一整页也不 panic（灰字居中）。
    #[test]
    fn empty_toc_draws_without_rows() {
        let mut p = TocPanel::new();
        p.show(Vec::new(), 0);
        assert!(p.is_open());
        assert_eq!(p.cur_item, None);
        let hs = p.hits(1404, 1872);
        assert_eq!(hs.len(), 1);
        assert_eq!(hs[0].1, Act::Close);
        let mut fb = Fb::open_mem();
        let mut text = None;
        p.draw(&mut fb, &mut text);
        assert_eq!(fb.get(702, 60), 0, "黑头没画上");
        assert_eq!(fb.get(702, 936), 255, "空目录只有字，底还是白的");
    }

    /// 单测 5：滚动窗口在界内；当前章整行反白（黑底）。
    #[test]
    fn scroll_and_current_row_inverted() {
        let mut p = TocPanel::new();
        p.show(items(40), 15); // 当前章 = 下标 1，top 里留一项
        assert!(p.scroll(1, 1872));
        assert_eq!(p.hits(1404, 1872)[1].1, Act::Pick(1));
        assert!(p.scroll(99, 1872)); // 能滚到最后
        let hs = p.hits(1404, 1872);
        assert_eq!(hs.last().unwrap().1, Act::Pick(39));
        assert!(p.scroll(-99, 1872));
        assert_eq!(p.hits(1404, 1872)[1].1, Act::Pick(0));
        assert!(!p.scroll(-1, 1872), "到顶了不该再动");

        // 反白：第 2 项（下标 1）整行黑底，其它行白底
        let mut p = TocPanel::new();
        p.show(items(10), 15);
        let mut fb = Fb::open_mem();
        let mut text = None;
        p.draw(&mut fb, &mut text);
        assert_eq!(fb.get(700, LIST_TOP + 120 + 60), 0, "当前章整行没反白");
        assert_eq!(fb.get(700, LIST_TOP + 60), 255);
    }
}
