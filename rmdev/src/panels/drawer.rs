//! 库视图顶部下拉抽屉（官方 `res_06b82ab.qml` 的 `DocumentDrawer`）。
//!
//! 官方是 `TabControl` + 一个 `ListView`：三个 tab（`Recent` / `Favorites` /
//! `Tags`）一次只显示一个区，`View all` 只挂在 Tags 区。本任务契约要**三区并列、
//! 每区右上 `View all`**，所以这里把三区上下排进半屏抽屉，每区一行卡片；区头右侧
//! 是官方那个 `View all`（原文 `qsTr("View all")`，官方 `IconButton` +
//! `icon_chevron_right`）。
//!
//! 文案全部照官方 QML 抄（`res_06b82ab.qml`）：
//!
//! * 区名 = `tabModel` 的 `Recent` / `Favorites` / `Tags`；
//! * 空态（`:187` 那条三目）：`No recent documents`、`No favorited documents`、
//!   Tags 区那段长文案 `You can tag pages, files, and folders to find them
//!   quicker. Tagged content will appear right here.`。
//!
//! 几何（宽 1404、抽屉 = 屏幕一半 936 高）：
//!
//! | 元素 | 位置 |
//! |---|---|
//! | 抓手 | 居中 120×6，y 19 |
//! | 区标题行 | 高 76；区名基线 区顶+52，左 32；`View all` 右 32 |
//! | 卡片行 | 高 196，4 列，左右边距 32，列距 22 |
//! | 区间隔 | 24 |
//!
//! 约定同 `nbsettings` / `pagesel`：`sections()` / `rows()` / `hit()` 是纯布局，
//! `paint()` 只照着画；`main.rs` 只把手指事件翻译成 [`Drawer::down`] /
//! [`Drawer::moving`] / [`Drawer::up`]。

use crate::doc::{Book, Library};
use crate::fb::{gray565, Fb};
use crate::ui::text::Text;
use crate::ui::widgets;

/// 屏幕顶部这一条里往下拖才拉抽屉（库的顶栏按钮也在这一条里，点按不受影响）。
pub const TOP_ZONE_H: i32 = 120;
/// 下拉超过这么多像素才展开。
pub const DRAG_MIN: i32 = 200;

/// 抽屉顶部抓手带
const GRAB_H: i32 = 44;
const GRAB_W: i32 = 120;
const GRAB_LINE_H: i32 = 6;

/// 每区标题行（区名 + `View all`）
const SEC_HDR_H: i32 = 76;
/// 卡片高（一区只放一行）
const CARD_H: i32 = 196;
/// 区间隔
const SEC_GAP: i32 = 24;
/// 左右边距
const MARGIN: i32 = 32;
/// 卡片列距
const COL_GAP: i32 = 22;
/// 每行几张卡
pub const COLS: i32 = 4;
/// `View all` 命中宽（文字 + 右侧箭头）
const VIEW_ALL_W: i32 = 220;
/// 一区里显示几张（= 每行张数；半屏放不下第二行）
pub const PER_SEC: usize = COLS as usize;

/// 官方 `View all`（`res_06b82ab.qml:272`）
pub const VIEW_ALL: &str = "View all";

const FONT_TITLE: f32 = 36.0;
const FONT_VIEW_ALL: f32 = 26.0;
const FONT_CARD: f32 = 26.0;
const FONT_EMPTY: f32 = 26.0;
/// 空态文字的行距（官方 `lineHeight: 1.5`，26 号字 → 39，取 40）
const EMPTY_LINE: i32 = 40;

pub type Rect = (i32, i32, i32, i32);

/// 抽屉的三个区。官方 `DocumentDrawer.Tab`（`Recent` / `Favorites` / `Tags`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sec {
    Recent,
    Favorites,
    Tags,
}

impl Sec {
    pub const ALL: [Sec; 3] = [Sec::Recent, Sec::Favorites, Sec::Tags];

    /// 区名 = 官方 tab 标题
    pub fn title(self) -> &'static str {
        match self {
            Sec::Recent => "Recent",
            Sec::Favorites => "Favorites",
            Sec::Tags => "Tags",
        }
    }

    /// 空态文案（官方 `res_06b82ab.qml:187` 的三目原文）
    pub fn empty(self) -> &'static str {
        match self {
            Sec::Recent => "No recent documents",
            Sec::Favorites => "No favorited documents",
            Sec::Tags => {
                "You can tag pages, files, and folders to find them quicker. \
                 Tagged content will appear right here."
            }
        }
    }

}

/// 抽屉里的点击结果。`ViewAll` / `Pick` 都带区号，`Pick` 再带区里第几项。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    /// 区头右上 `View all` → 切到对应的库视图
    ViewAll(Sec),
    /// 点卡片 / 标签胶囊 → 打开第 i 项
    Pick(Sec, usize),
    /// 点在抽屉外面 → 关
    Close,
}

/// 一个分区的几何：整区、区头右上的 `View all`、卡片行。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Section {
    pub sec: Sec,
    /// 整个分区（标题行 + 卡片行）
    pub rect: Rect,
    /// 区头右上 `View all` 的可点矩形
    pub view_all: Rect,
    /// 卡片行
    pub row: Rect,
}

/// 一行卡片。`cards` 里是 `(区里第几项, 矩形)`。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub y: i32,
    pub cards: Vec<(usize, Rect)>,
}

pub struct Drawer {
    open: bool,
    /// 这次下拉的起点（屏幕坐标）。`None` = 不是从顶部区开始的，不认。
    drag_from: Option<(f32, f32)>,
    /// 这次下拉已经往下走了多少（只算"下 > 横"的位移）
    drag_dy: i32,
    /// 三区的数据。Recent / Favorites 存文档，Tags 存标签名。
    recent: Vec<Book>,
    favorites: Vec<Book>,
    tags: Vec<String>,
}

impl Drawer {
    pub fn new() -> Drawer {
        Drawer {
            open: false,
            drag_from: None,
            drag_dy: 0,
            recent: Vec::new(),
            favorites: Vec::new(),
            tags: Vec::new(),
        }
    }

    /// 抽屉高度 = 屏幕一半（官方抽屉也是压住上面半屏）。
    pub fn height(h: i32) -> i32 {
        h / 2
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self) {
        self.open = true;
        self.drag_from = None;
        self.drag_dy = 0;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.drag_from = None;
        self.drag_dy = 0;
    }

    /// 开抽屉时抓一次数据（Recent 按 `lastModified` 倒序，Favorites = pinned）。
    /// 只在打开时扫一遍库：画的时候不再重扫。
    pub fn reload(&mut self, lib: &Library) {
        self.recent = lib.recent(PER_SEC);
        self.favorites = lib
            .pinned()
            .into_iter()
            .filter(|b| b.doc_type == "DocumentType")
            .take(PER_SEC)
            .collect();
        self.tags = lib.tag_labels().into_iter().take(PER_SEC).collect();
    }

    /// 某区有几项。
    pub fn count(&self, sec: Sec) -> usize {
        match sec {
            Sec::Recent => self.recent.len(),
            Sec::Favorites => self.favorites.len(),
            Sec::Tags => self.tags.len(),
        }
    }

    /// 按 `Act::Pick` 的区号/下标取文档。Tags 区不是文档，返回 `None`。
    pub fn book(&self, sec: Sec, i: usize) -> Option<&Book> {
        match sec {
            Sec::Recent => self.recent.get(i),
            Sec::Favorites => self.favorites.get(i),
            Sec::Tags => None,
        }
    }

    /// 按 `Act::Pick` 的区号/下标取标签名。
    pub fn tag(&self, i: usize) -> Option<&str> {
        self.tags.get(i).map(|s| s.as_str())
    }

    /// 手指在顶部区按下：记下拉起点。抽屉已经开着就不再认（否则会自己关自己）。
    pub fn down(&mut self, x: f32, y: f32) {
        self.drag_from = if !self.open && y < TOP_ZONE_H as f32 {
            Some((x, y))
        } else {
            None
        };
        self.drag_dy = 0;
    }

    /// 拖动中：只认"往下、且比横向走得远"的位移。
    pub fn moving(&mut self, x: f32, y: f32) {
        let Some((x0, y0)) = self.drag_from else {
            return;
        };
        let (dx, dy) = ((x - x0).abs(), y - y0);
        self.drag_dy = if dy > 0.0 && dy > dx { dy as i32 } else { 0 };
    }

    /// 抬手：超过 [`DRAG_MIN`] 就展开并返回 `true`（调用方要吞掉这次手势，
    /// 别再当成滚动/点击）。
    pub fn up(&mut self) -> bool {
        let from = self.drag_from.take();
        let dy = self.drag_dy;
        self.drag_dy = 0;
        if from.is_some() && dy > DRAG_MIN {
            self.open = true;
            true
        } else {
            false
        }
    }

    /// 三个分区的布局（从上到下：Recent / Favorites / Tags）。
    pub fn sections(w: i32) -> [Section; 3] {
        let mut out = [Section {
            sec: Sec::Recent,
            rect: (0, 0, 0, 0),
            view_all: (0, 0, 0, 0),
            row: (0, 0, 0, 0),
        }; 3];
        let mut y = GRAB_H;
        for (k, sec) in Sec::ALL.iter().enumerate() {
            let sh = SEC_HDR_H + CARD_H;
            out[k] = Section {
                sec: *sec,
                rect: (0, y, w, sh),
                view_all: (w - MARGIN - VIEW_ALL_W, y, VIEW_ALL_W, SEC_HDR_H),
                row: (MARGIN, y + SEC_HDR_H, w - 2 * MARGIN, CARD_H),
            };
            y += sh + SEC_GAP;
        }
        out
    }

    /// 卡片宽：4 列均分剩下的宽度。
    pub fn card_w(w: i32) -> i32 {
        ((w - 2 * MARGIN - (COLS - 1) * COL_GAP) / COLS).max(1)
    }

    /// 一个分区里 `n` 项怎么排：每行 4 张，`top` 是第一行的顶。
    pub fn rows(w: i32, top: i32, n: usize) -> Vec<Row> {
        let cw = Self::card_w(w);
        let mut out = Vec::new();
        let mut i = 0usize;
        let mut y = top;
        while i < n {
            let mut cards = Vec::with_capacity(COLS as usize);
            for c in 0..COLS {
                if i >= n {
                    break;
                }
                cards.push((i, (MARGIN + c * (cw + COL_GAP), y, cw, CARD_H)));
                i += 1;
            }
            out.push(Row { y, cards });
            y += CARD_H + COL_GAP;
        }
        out
    }

    /// 命中测试。抽屉收着时永远 `None`。
    ///
    /// * 抽屉外面（y ≥ 抽屉高）→ [`Act::Close`]；
    /// * 区头右上 → [`Act::ViewAll`]；
    /// * 卡片 → [`Act::Pick`]；
    /// * 抽屉里但没点到东西 → `None`（调用方照样吞掉，不关）。
    pub fn hit(&self, x: f32, y: f32, w: i32, h: i32) -> Option<Act> {
        if !self.open {
            return None;
        }
        if y >= Self::height(h) as f32 {
            return Some(Act::Close);
        }
        for s in Self::sections(w).iter() {
            if in_rect(x, y, s.view_all) {
                return Some(Act::ViewAll(s.sec));
            }
            let n = self.count(s.sec);
            if n == 0 {
                continue;
            }
            for row in Self::rows(w, s.row.1, n) {
                // 出区的行不画也不吃点击（半屏只放得下一行）
                if row.y + CARD_H > s.row.1 + s.row.3 {
                    break;
                }
                for (i, r) in row.cards.iter() {
                    if in_rect(x, y, *r) {
                        return Some(Act::Pick(s.sec, *i));
                    }
                }
            }
        }
        None
    }

    /// 画抽屉。背景/卡片框总会画（没字库时至少把下面挡住）；文字要先加载字库。
    pub fn paint(&self, fb: &mut Fb, text: &mut Option<Text>, w: i32, h: i32) {
        if !self.open {
            return;
        }
        let dh = Self::height(h);
        fb.fill_rect(0, 0, w, dh, gray565(255));
        // 抓手（官方抽屉顶上的那条拖动提示）
        fb.fill_rect(
            (w - GRAB_W) / 2,
            (GRAB_H - GRAB_LINE_H) / 2,
            GRAB_W,
            GRAB_LINE_H,
            gray565(200),
        );

        for s in Self::sections(w).iter() {
            let top = s.rect.1;
            if let Some(t) = text.as_mut() {
                t.draw_bold(fb, MARGIN, top + 52, FONT_TITLE, s.sec.title(), 20);
                // `View all`：右对齐，右边跟官方那个 chevron
                let vx = s.view_all.0 + s.view_all.2;
                let lw = t.width_bold(FONT_VIEW_ALL, VIEW_ALL) as i32;
                t.draw_bold(fb, vx - 46 - lw, top + 48, FONT_VIEW_ALL, VIEW_ALL, 20);
            }
            widgets::chevron_right(fb, s.view_all.0 + s.view_all.2 - 26, top + 38, 9, 20);
            // 区头下面一条 1px 灰线（和列表行的 hairline 一个色）
            fb.fill_rect(MARGIN, top + SEC_HDR_H - 1, w - 2 * MARGIN, 1, gray565(225));

            self.paint_cards(*s, fb, text, w);
        }

        // 官方 `Background` 的底线：2px 黑
        fb.fill_rect(0, dh - 2, w, 2, gray565(0));
    }

    /// 一行卡片（Recent / Favorites 是文档名，Tags 是标签名）：名字居中一行，
    /// 超宽中间省略。
    fn paint_cards(&self, s: Section, fb: &mut Fb, text: &mut Option<Text>, w: i32) {
        let names: Vec<&str> = match s.sec {
            Sec::Recent => self.recent.iter().map(|b| b.name.as_str()).collect(),
            Sec::Favorites => self.favorites.iter().map(|b| b.name.as_str()).collect(),
            Sec::Tags => self.tags.iter().map(|t| t.as_str()).collect(),
        };
        if names.is_empty() {
            self.paint_empty(s, fb, text, w);
            return;
        }
        let cw = Self::card_w(w);
        for row in Self::rows(w, s.row.1, names.len()) {
            if row.y + CARD_H > s.row.1 + s.row.3 {
                break;
            }
            for (i, r) in row.cards.iter() {
                widgets::outline_rect(fb, r.0, r.1, r.2, r.3, 2, 20);
                let Some(name) = names.get(*i).copied() else { continue };
                if let Some(t) = text.as_mut() {
                    let shown = t.ellipsize(FONT_CARD, name, cw - 32);
                    let tw = t.width(FONT_CARD, &shown) as i32;
                    t.draw(
                        fb,
                        r.0 + (cw - tw) / 2,
                        r.1 + CARD_H / 2 + 9,
                        FONT_CARD,
                        &shown,
                        20,
                    );
                }
            }
        }
    }

    /// 空态：官方那句居中（Tags 那句长，按行折）。
    fn paint_empty(&self, s: Section, fb: &mut Fb, text: &mut Option<Text>, w: i32) {
        let Some(t) = text.as_mut() else { return };
        // 26 号 Maison Neue 一个字符平均 ~14px，这里按 16 算，留一点余量。
        let max_chars = ((w - 2 * MARGIN) / 16).max(20) as usize;
        let lines: Vec<String> = wrap(s.sec.empty(), max_chars)
            .into_iter()
            .map(|l| l.to_string())
            .collect();
        let n = lines.len() as i32;
        // 多行时整块以卡片行中心对称
        let y0 = s.row.1 + CARD_H / 2 + 9 - (n - 1) * EMPTY_LINE / 2;
        for (i, line) in lines.iter().enumerate() {
            let tw = t.width(FONT_EMPTY, line) as i32;
            t.draw(
                fb,
                (w - tw) / 2,
                y0 + i as i32 * EMPTY_LINE,
                FONT_EMPTY,
                line,
                100,
            );
        }
    }
}

impl Default for Drawer {
    fn default() -> Self {
        Drawer::new()
    }
}

fn in_rect(x: f32, y: f32, r: Rect) -> bool {
    x >= r.0 as f32 && y >= r.1 as f32 && x < (r.0 + r.2) as f32 && y < (r.1 + r.3) as f32
}

/// 按 `max_chars` 折行（纯函数，只认空格，够官方那几句英文用）。
///
/// 折出来的每一行都不超过 `max_chars`；把结果按空格接回去和原文一致。
pub fn wrap(s: &str, max_chars: usize) -> Vec<&str> {
    let max = max_chars.max(1);
    let mut out: Vec<&str> = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        if rest.chars().count() <= max {
            out.push(rest);
            break;
        }
        // 在窗口内找最后一个空格断行；一个词都塞不下就硬切
        let cut = rest
            .char_indices()
            .take(max + 1)
            .filter(|(_, c)| *c == ' ')
            .map(|(i, _)| i)
            .last();
        let (line, tail) = match cut {
            Some(i) => (&rest[..i], rest[i + 1..].trim_start()),
            None => {
                let i = rest
                    .char_indices()
                    .nth(max)
                    .map(|(i, _)| i)
                    .unwrap_or(rest.len());
                (&rest[..i], &rest[i..])
            }
        };
        out.push(line);
        rest = tail;
    }
    if out.is_empty() {
        out.push("");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::library::new_uuid;

    fn tmp_lib(tag: &str) -> Library {
        let dir = std::env::temp_dir().join(format!("rmdev-drawer-{tag}-{}", new_uuid("t")));
        std::fs::create_dir_all(&dir).unwrap();
        Library::with_root(&dir)
    }

    /// 抽屉高 = 半屏，且设备上三区都塞得下（不然最后一区被切掉）。
    #[test]
    fn height_is_half_screen_and_fits() {
        assert_eq!(Drawer::height(1872), 936);
        assert_eq!(Drawer::height(1001), 500);
        let s = Drawer::sections(1404);
        let bottom = s[2].rect.1 + s[2].rect.3;
        assert!(
            bottom <= Drawer::height(1872),
            "三区底 {bottom} 超出半屏 {}",
            Drawer::height(1872)
        );
        // 抓手 + 三区 + 两个间隔 = 抽屉内容高
        let want = GRAB_H + 3 * (SEC_HDR_H + CARD_H) + 2 * SEC_GAP;
        assert_eq!(bottom, want);
    }

    /// 三区从上到下是 Recent / Favorites / Tags，互不重叠，`View all` 在各自右上。
    #[test]
    fn sections_stack_with_view_all_top_right() {
        let w = 1404;
        let s = Drawer::sections(w);
        let ids: Vec<Sec> = s.iter().map(|x| x.sec).collect();
        assert_eq!(ids, vec![Sec::Recent, Sec::Favorites, Sec::Tags]);
        for (i, sec) in s.iter().enumerate() {
            assert_eq!(sec.rect.0, 0);
            assert_eq!(sec.rect.2, w);
            if i > 0 {
                assert_eq!(s[i - 1].rect.1 + s[i - 1].rect.3 + SEC_GAP, sec.rect.1);
            }
            // View all 贴着右边、在区头那一行里
            assert_eq!(sec.view_all.0 + sec.view_all.2, w - MARGIN);
            assert_eq!(sec.view_all.1, sec.rect.1);
            assert!(sec.view_all.2 >= 200 && sec.view_all.3 >= 76);
            // 卡片行在区头下面
            assert_eq!(sec.row.1, sec.rect.1 + SEC_HDR_H);
            assert_eq!(sec.row.0, MARGIN);
        }
    }

    /// 卡片行：4 列均分、列不重叠、多于 4 项就往下折行。
    #[test]
    fn rows_wrap_four_per_row() {
        let w = 1404;
        let r = Drawer::rows(w, 200, 4);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].y, 200);
        assert_eq!(r[0].cards.len(), 4);
        let cw = Drawer::card_w(w);
        for (k, (i, rect)) in r[0].cards.iter().enumerate() {
            assert_eq!(*i, k);
            assert_eq!(rect.2, cw);
            assert_eq!(rect.3, CARD_H);
            assert_eq!(rect.0, MARGIN + k as i32 * (cw + COL_GAP));
            assert!(rect.0 + rect.2 <= w - MARGIN);
        }
        // 9 项 → 三行（4/4/1），第二行接在第一行下面
        let r = Drawer::rows(w, 200, 9);
        assert_eq!(r.len(), 3);
        assert_eq!(r[1].y, 200 + CARD_H + COL_GAP);
        assert_eq!(r[2].cards.len(), 1);
        assert_eq!(r[2].cards[0].0, 8);
        // 0 项 = 一行都没有
        assert!(Drawer::rows(w, 0, 0).is_empty());
    }

    /// 命中：View all / 卡片 / 抽屉外 Close / 抽屉内空白 None。
    #[test]
    fn hit_view_all_pick_and_close() {
        let lib = tmp_lib("hit");
        let a = lib.create_book("笔记 A").unwrap();
        let b = lib.create_book("笔记 B").unwrap();
        let _ = lib.set_pinned(&b, true);
        let mut d = Drawer::new();
        assert_eq!(d.hit(700.0, 10.0, 1404, 1872), None, "收着时不吃点击");
        d.reload(&lib);
        d.open();
        let s = Drawer::sections(1404);
        // 区头右上
        let va = s[1].view_all;
        assert_eq!(
            d.hit((va.0 + va.2 / 2) as f32, (va.1 + va.3 / 2) as f32, 1404, 1872),
            Some(Act::ViewAll(Sec::Favorites))
        );
        // 第一张卡
        let card = Drawer::rows(1404, s[0].row.1, 1)[0].cards[0].1;
        assert_eq!(
            d.hit((card.0 + 10) as f32, (card.1 + 10) as f32, 1404, 1872),
            Some(Act::Pick(Sec::Recent, 0))
        );
        let got = d.book(Sec::Recent, 0).unwrap();
        assert!(got.uuid == a || got.uuid == b);
        // 抽屉外 → 关
        assert_eq!(d.hit(700.0, 1000.0, 1404, 1872), Some(Act::Close));
        // 抽屉里的空白（抓手带）→ None，但调用方照样吞掉
        assert_eq!(d.hit(700.0, 10.0, 1404, 1872), None);
    }

    /// 空区不吐 Pick，只留 View all。
    #[test]
    fn empty_section_has_only_view_all() {
        let lib = tmp_lib("empty");
        let mut d = Drawer::new();
        d.reload(&lib);
        d.open();
        let s = Drawer::sections(1404);
        for sec in s.iter() {
            assert_eq!(d.count(sec.sec), 0);
            let card_x = (sec.row.0 + 10) as f32;
            let card_y = (sec.row.1 + 10) as f32;
            assert_eq!(d.hit(card_x, card_y, 1404, 1872), None, "{:?}", sec.sec);
        }
    }

    /// 空态文案就是官方 QML 里那三句（一个字符都不许改）。
    #[test]
    fn empty_text_is_official() {
        assert_eq!(Sec::Recent.title(), "Recent");
        assert_eq!(Sec::Favorites.title(), "Favorites");
        assert_eq!(Sec::Tags.title(), "Tags");
        assert_eq!(Sec::Recent.empty(), "No recent documents");
        assert_eq!(Sec::Favorites.empty(), "No favorited documents");
        assert_eq!(
            Sec::Tags.empty(),
            "You can tag pages, files, and folders to find them quicker. \
             Tagged content will appear right here."
        );
        assert_eq!(VIEW_ALL, "View all");
    }

    /// 折行：每行不超宽、不丢词、接回去和原文一致。
    #[test]
    fn wrap_keeps_words_and_width() {
        let s = Sec::Tags.empty();
        let lines = wrap(s, 60);
        assert!(lines.len() > 1, "Tags 那句得折行");
        for l in &lines {
            assert!(l.chars().count() <= 60, "{l:?} 超宽");
            assert!(!l.is_empty());
        }
        assert_eq!(lines.join(" "), s);
        // 短的句子不折
        assert_eq!(wrap("No recent documents", 60), vec!["No recent documents"]);
        // 一个长词也切得开
        let long = "a".repeat(30);
        assert_eq!(wrap(&long, 10), vec!["aaaaaaaaaa"; 3]);
        assert_eq!(wrap("", 10), vec![""]);
    }

    /// 下拉手势：只有从顶部 120px 内往下走、且超过 200px 才展开。
    #[test]
    fn drag_from_top_zone_and_min_distance() {
        let mut d = Drawer::new();
        // 顶部区内下拉 240px → 展开
        d.down(700.0, 60.0);
        d.moving(700.0, 200.0);
        d.moving(700.0, 300.0);
        assert!(d.up());
        assert!(d.is_open());
        // 再来一次不够长 → 不展开（而且已经开着，down 根本不记）
        let mut d = Drawer::new();
        d.down(700.0, 60.0);
        d.moving(700.0, 120.0);
        assert!(!d.up());
        assert!(!d.is_open());
        // 刚好 200 不算过（官方阈值按"超过"）
        d.down(700.0, 60.0);
        d.moving(700.0, 260.0);
        assert!(!d.up());
        // 顶部区外面按下 → 一律不认
        d.down(700.0, 400.0);
        d.moving(700.0, 900.0);
        assert!(!d.up());
        // 横向拖更远 → 不算下拉
        d.down(700.0, 60.0);
        d.moving(1100.0, 300.0);
        assert!(!d.up());
        // 往上拖 → 不算
        d.down(700.0, 100.0);
        d.moving(700.0, 900.0);
        d.moving(700.0, 50.0);
        assert!(!d.up());
    }

    /// 开/关来回切，状态不残留；开着时 down 不记起点。
    #[test]
    fn open_close_resets_drag_state() {
        let mut d = Drawer::new();
        d.down(700.0, 60.0);
        d.moving(700.0, 400.0);
        assert!(d.up());
        assert!(d.is_open());
        d.close();
        assert!(!d.is_open());
        // 开着的时候手指在顶部区按下：不记起点，抬手也不会"再开一次"
        d.open();
        d.down(700.0, 60.0);
        d.moving(700.0, 400.0);
        assert!(!d.up());
        assert!(d.is_open());
    }

    /// 数据：Recent 最多 4 项、Favorites 只要文档（文件夹不进抽屉）。
    #[test]
    fn reload_takes_per_section_limit() {
        let lib = tmp_lib("reload");
        for i in 0..6 {
            let _ = lib.create_book(&format!("笔记 {i}")).unwrap();
        }
        let dir = lib.create_folder("文件夹", "").unwrap();
        let _ = lib.set_pinned(&dir, true);
        let mut d = Drawer::new();
        d.reload(&lib);
        assert_eq!(d.count(Sec::Recent), PER_SEC);
        assert_eq!(d.count(Sec::Favorites), 0, "pin 的是文件夹，抽屉只放文档");
        assert_eq!(d.count(Sec::Tags), 0);
        assert!(d.tag(0).is_none());
        assert!(d.book(Sec::Tags, 0).is_none());
    }
}
