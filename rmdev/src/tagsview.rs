//! Tags 视图（官方 `res_069e2e9.qml` / 截图 `12-sidebar-tags.png`）。
//!
//! 布局全部按官方截图逐像素量：
//!
//! | 元素 | 位置 |
//! |---|---|
//! | 标题 `Tags` | 36→40 号粗体，左边距 30（墨迹 31），基线 196 |
//! | 排序 `Last updated ⌄` | 文字右端 1299，箭头 1326..1349，基线 189 |
//! | tabs | 顶 228、高 120；文字基线 310；选中条 6px（342..347），全宽底线 2px（346..347） |
//! | 胶囊行 | 顶 368，左右 40 起，行高 46、间距 20 |
//! | 空态 | 图标 90 顶 842；`No tags yet` 基线 1007；说明首行 1068、行距 55 |
//!
//! 卡片沿用库网格的 426×568（`10-library.png` 实测：x=33、宽 426、高 568、列距 30、
//! 3 列）。spec §0 表里的 342/510 是另一版固件的 Values，和这台设备的截图对不上，
//! 按「截图优先」处理。
//!
//! 这里只放**视图本身**：状态 + 绘制 + 动作分发。`main.rs` 里只有 `mod`、`App` 的
//! 一个字段、几处调用。

use rmdev::doc::pdf::Pdf;
use rmdev::doc::Book;
use rmdev::fb::{gray565, Fb, UPDATE_MODE_FULL, UPDATE_MODE_PARTIAL, WAVEFORM_MODE_GC16};
use rmdev::ui::keyboard::{Key, Keyboard};
use rmdev::ui::widgets;

use crate::{App, COL_W, COLS, HEADER_H, NAV_MARGIN, TILE_GAP, TILE_H};

// ---------------------------------------------------------------- 动作号

// 2600 号段（2100..2200 是搜索结果 A_RESULT0，别撞）
const A_TAB0: u32 = 2600; // +0 All / +1 Pages / +2 Documents
const A_SORT: u32 = 2610;
const A_SORT_ITEM: u32 = 2611;
const A_PILL0: u32 = 2620; // +i（最多 80 个标签）
const A_TILE0: u32 = 2750; // +i（i>=100 是集合行）
const A_HOLD_RENAME: u32 = 2850;
const A_HOLD_DELETE: u32 = 2851;
const A_HOLD_CLOSE: u32 = 2852;
const A_DEL_YES: u32 = 2860;
const A_DEL_NO: u32 = 2861;
const A_REN_SAVE: u32 = 2870;
const A_REN_CANCEL: u32 = 2871;

/// 这些动作号归 Tags 视图，`main.rs` 只要把整段丢进来。
pub fn is_tags_action(a: u32) -> bool {
    (2600..2900).contains(&a)
}

// ---------------------------------------------------------------- 几何

const TITLE_BASE: i32 = 195;
const TABS_TOP: i32 = 228;
const TABS_H: i32 = 120;
const TABS_BASE: i32 = 310;
const TABS_RULE_Y: i32 = 346;
const IND_H: i32 = 6;
const FILTER_TOP: i32 = 368;
/// TagFilter 和内容之间：`spacing 20` + `Item height 4`
const FILTER_GAP: i32 = 24;
const PILL_H: i32 = 46;
const PILL_GAP: i32 = 20;
/// TagPill：`width = text + 50`
const PILL_PAD: i32 = 50;
/// 卡片行距（库网格同款：卡高 568 + 名字/副标题 190）
const ROW_H: i32 = TILE_H + 190;
/// 缩略图上的标签胶囊（`TagPillCaption`）：高 `Body3 + 18 = 40`
const TPILL_H: i32 = 40;
const TPILL_GAP: i32 = 10;

// ---------------------------------------------------------------- 状态

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    All,
    Pages,
    Documents,
}

impl Default for Tab {
    fn default() -> Tab {
        Tab::All
    }
}

/// 一页的标签。`page` 是 0 基页号（显示时 +1）。
#[derive(Clone, Debug)]
pub struct PageTag {
    pub uuid: String,
    pub name: String,
    pub page: usize,
    pub tags: Vec<String>,
    pub ts: u64,
}

#[derive(Default)]
pub struct TagsView {
    pub tab: Tab,
    /// 选中的胶囊（多个 = AND）
    pub selected: Vec<String>,
    pub sort_open: bool,
    /// 长按胶囊 → 底部操作条（`Edit tag`）
    pub hold: Option<String>,
    /// 删除确认框里的标签
    pub confirm: Option<String>,
    /// 改名：原标签 + 键盘
    pub rename: Option<(String, Keyboard)>,
    /// 下一次重绘只刷这一块（避免整屏闪）
    pub dirty: Option<(i32, i32, i32, i32)>,
    /// 上次画出来的胶囊命中区（长按要按落点找标签）
    pub pills: Vec<(String, i32, i32, i32, i32)>,
}

impl TagsView {
    /// 每次进入 Tags 视图：官方 `onVisibleChanged` 把 tab 归到 All。
    pub fn on_open(&mut self) {
        self.tab = Tab::All;
        self.sort_open = false;
        self.hold = None;
        self.confirm = None;
        self.rename = None;
        self.dirty = None;
        self.pills.clear();
    }
}

enum Cell<'a> {
    Doc(&'a Book),
    Page(&'a PageTag),
}

/// Tags 视图的数据：胶囊行的标签 + 三档内容（已按选中标签过滤）。
struct TagsData {
    labels: Vec<String>,
    collections: Vec<Book>,
    docs: Vec<Book>,
    pages: Vec<PageTag>,
}

/// 标签集合是否命中当前筛选（多个选中 = AND；空选 = 只要有标签就算命中）。
fn hit(tags: &[String], sel: &[String]) -> bool {
    !tags.is_empty() && sel.iter().all(|s| tags.iter().any(|t| t == s))
}

/// 实心胶囊：两个半圆 + 中间矩形（官方 `antialiasing: false`，硬边）。
fn fill_capsule(fb: &mut Fb, x: i32, y: i32, w: i32, h: i32, tone: u8) {
    let r = h / 2;
    fb.fill_rect(x + r, y, w - 2 * r, h, gray565(tone));
    widgets::disc(fb, x + r, y + r, r, tone);
    widgets::disc(fb, x + w - r, y + r, r, tone);
}

/// 胶囊描边：外面一颗实心、里面再盖一颗白底。`widgets::capsule` 画的是开关那种
/// 两头整圆的环，胶囊会多出两段竖弧，所以这里自己来。
fn stroke_capsule(fb: &mut Fb, x: i32, y: i32, w: i32, h: i32, t: i32, tone: u8) {
    fill_capsule(fb, x, y, w, h, tone);
    fill_capsule(fb, x + t, y + t, w - 2 * t, h - 2 * t, 255);
}

impl App {
    // ------------------------------------------------------------ 数据

    fn tags_data(&self) -> TagsData {
        let labels = self.library.tag_labels();
        let sel = &self.tags.selected;
        let entries = self.library.entries("", false);
        let mut by_uuid: std::collections::HashMap<&str, &Book> = Default::default();
        for b in &entries {
            by_uuid.insert(b.uuid.as_str(), b);
        }
        let mut docs = Vec::new();
        let mut collections = Vec::new();
        for b in &entries {
            if !hit(&self.library.tags_of(&b.uuid), sel) {
                continue;
            }
            if b.doc_type == "CollectionType" {
                collections.push(b.clone());
            } else {
                docs.push(b.clone());
            }
        }
        docs.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
        collections.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));

        let mut pages = Vec::new();
        for (uuid, page_id, tags) in self.library.all_page_tags() {
            if !hit(&tags, sel) {
                continue;
            }
            let Some(b) = by_uuid.get(uuid.as_str()) else {
                continue;
            };
            let Some(page) = self.library.page_ids(&uuid).iter().position(|p| p == &page_id) else {
                continue;
            };
            pages.push(PageTag {
                uuid: uuid.clone(),
                name: b.name.clone(),
                page,
                tags,
                ts: b.last_modified,
            });
        }
        pages.sort_by(|a, b| b.ts.cmp(&a.ts));
        TagsData {
            labels,
            collections,
            docs,
            pages,
        }
    }

    fn tags_cells<'a>(&self, data: &'a TagsData) -> Vec<Cell<'a>> {
        match self.tags.tab {
            Tab::Pages => data.pages.iter().map(Cell::Page).collect(),
            Tab::Documents => data.docs.iter().map(Cell::Doc).collect(),
            Tab::All => {
                let mut v: Vec<(u64, Cell)> = Vec::new();
                for d in &data.docs {
                    v.push((d.last_modified, Cell::Doc(d)));
                }
                for p in &data.pages {
                    v.push((p.ts, Cell::Page(p)));
                }
                v.sort_by(|a, b| b.0.cmp(&a.0));
                v.into_iter().map(|(_, c)| c).collect()
            }
        }
    }

    fn tags_text_w(&mut self, size: f32, s: &str) -> i32 {
        self.text
            .as_mut()
            .map(|t| t.width(size, s) as i32)
            .unwrap_or(0)
    }

    fn tags_text_wb(&mut self, size: f32, s: &str) -> i32 {
        self.text
            .as_mut()
            .map(|t| t.width_bold(size, s) as i32)
            .unwrap_or(0)
    }

    /// 胶囊行占几行（官方 `maxTagRows = 3`）。
    fn tags_pill_rows(&mut self, labels: &[String]) -> usize {
        let avail = self.fb.width - 60 - 20 - 20;
        let mut x = 0;
        let mut rows = 0;
        for name in labels {
            let w = self.tags_text_wb(28.0, name).max(50) + PILL_PAD;
            if rows == 0 {
                rows = 1;
            } else if x + w > avail {
                rows += 1;
                x = 0;
            }
            x += w + PILL_GAP;
        }
        rows.min(3)
    }

    /// TagFilter 的高度（官方：`max(60, tagsContent.height)`，内容 = 行高 + 上下各 10）。
    fn tags_filter_height(&mut self, labels: &[String]) -> i32 {
        let rows = self.tags_pill_rows(labels) as i32;
        if rows == 0 {
            return 60;
        }
        (rows * PILL_H + (rows - 1) * PILL_GAP + 20).max(60)
    }

    fn tags_grid_top(&mut self, data: &TagsData) -> i32 {
        FILTER_TOP + self.tags_filter_height(&data.labels) + FILTER_GAP
    }

    fn tags_content_height(&mut self, data: &TagsData, grid_top: i32) -> i32 {
        let mut top = grid_top;
        let crows = (data.collections.len() as i32 + COLS - 1) / COLS;
        if crows > 0 {
            // 集合区在网格 header 里，下面还多留 `extraSpaceUnderTagsHeader = 30`
            top += crows * 75 + 30;
        }
        let n = match self.tags.tab {
            Tab::Pages => data.pages.len(),
            Tab::Documents => data.docs.len(),
            Tab::All => data.pages.len() + data.docs.len(),
        } as i32;
        let rows = (n + COLS - 1) / COLS;
        top + rows * ROW_H
    }

    /// 内容还能往上滚多少像素
    pub fn tags_max_scroll(&mut self) -> i32 {
        let data = self.tags_data();
        let grid_top = self.tags_grid_top(&data);
        let content = self.tags_content_height(&data, grid_top);
        (content - (self.fb.height - 72)).max(0)
    }

    // ------------------------------------------------------------ 绘制

    /// 整个 Tags 视图：内容 → 空态 → 顶栏/标题/tabs/胶囊 → 弹层。
    ///
    /// 顶栏最后画（压住滚上来的卡片），和库的约定一致。刷新由 `draw_library`
    /// 统一收口（`tags_flush`）。
    pub fn draw_tags(&mut self) {
        let w = self.fb.width;
        let data = self.tags_data();
        let grid_top = self.tags_grid_top(&data);
        self.tags.pills.clear();
        if self.menu_open {
            // 侧栏抽屉压在上面，这时候只能整屏刷
            self.tags.dirty = None;
        }

        // ---------- 集合行（官方：网格 header，4 列 342×75；这里 3 列 426×75）
        let mut items_top = grid_top - self.scroll;
        if !data.collections.is_empty() {
            for (i, c) in data.collections.iter().enumerate() {
                let cx = NAV_MARGIN + (i as i32 % COLS) * (COL_W + TILE_GAP);
                let cy = items_top + (i as i32 / COLS) * 75;
                self.tags_collection_row(cx, cy, c);
                self.hits
                    .push(cx, cy, COL_W, 75, A_TILE0 + 100 + i as u32);
            }
            let crows = (data.collections.len() as i32 + COLS - 1) / COLS;
            items_top += crows * 75 + 30;
        }

        // ---------- 网格
        let cells = self.tags_cells(&data);
        for (i, cell) in cells.iter().enumerate() {
            let cx = NAV_MARGIN + (i as i32 % COLS) * (COL_W + TILE_GAP);
            let cy = items_top + (i as i32 / COLS) * ROW_H;
            if cy > self.fb.height - 100 {
                break;
            }
            if cy + TILE_H + 110 < grid_top {
                continue;
            }
            self.tags_card(cx, cy, cell);
            self.hits.push(cx, cy, COL_W, TILE_H + 110, A_TILE0 + i as u32);
        }

        // ---------- 空态
        if cells.is_empty() {
            self.tags_empty_state();
        }

        // ---------- header
        self.tags_header(w, &data, grid_top);

        // ---------- 排序下拉（官方只有一个 "Last updated"）
        if self.tags.sort_open {
            let bw = 342;
            let bx = w - 30 - bw;
            let by = 212;
            let bh = 80;
            self.fb.fill_rect(bx - 2, by - 2, bw + 4, bh + 4, gray565(0));
            self.fb.fill_rect(bx, by, bw, bh, gray565(255));
            widgets::disc(&mut self.fb, bx + 34, by + 40, 8, 20);
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, bx + 60, by + 54, 28.0, "Last updated", 20);
            }
            self.hits.push(bx, by, bw, bh, A_SORT_ITEM);
        }
    }

    fn tags_header(&mut self, w: i32, data: &TagsData, grid_top: i32) {
        // 白色标题带盖住滚上来的卡片（官方内容区在 header 下沿被裁掉）
        self.fb.fill_rect(0, 0, w, grid_top, gray565(255));

        // ---------- 顶栏：官方 Tags 视图只留 Menu / Quick sheets
        // （+ Folder / + Notebook 的显示条件不含 Tags，搜索按钮 `!tagsView.visible`）
        widgets::hamburger(&mut self.fb, 33, 44, 39, 20);
        self.hits.push(12, 0, 160, HEADER_H, crate::A_MENU);
        if let Some(t) = self.text.as_mut() {
            t.draw_bold(&mut self.fb, 88, 73, 36.0, "Menu", 20);
        }
        let x = 380;
        widgets::plus_circle(&mut self.fb, x + 20, 60, 20, 20);
        if let Some(t) = self.text.as_mut() {
            t.draw_bold(&mut self.fb, x + 55, 73, 36.0, "Quick sheets", 20);
        }
        let qw = self.tags_text_wb(36.0, "Quick sheets");
        self.hits.push(x - 8, 0, 48 + qw + 25, HEADER_H, crate::A_QUICK);
        self.fb.fill_rect(0, HEADER_H - 2, w, 2, gray565(20));

        // ---------- 标题
        if let Some(t) = self.text.as_mut() {
            t.draw_bold(&mut self.fb, NAV_MARGIN - 3, TITLE_BASE, 40.0, "Tags", 20);
        }

        // ---------- 排序下拉（官方：右缘贴标题右缘，右边距 30）
        let lab = "Last updated";
        let tw = self.tags_text_wb(32.0, lab);
        let lx = 1299 - tw;
        if let Some(t) = self.text.as_mut() {
            t.draw_bold(&mut self.fb, lx, 189, 32.0, lab, 20);
        }
        for i in 0..12 {
            self.fb.fill_rect(1326 + i, 172 + i, 3, 3, gray565(20));
            self.fb.fill_rect(1348 - i, 172 + i, 3, 3, gray565(20));
        }
        self.hits.push(lx - 30, 130, w - (lx - 30), 80, A_SORT);

        // ---------- tabs
        let tabs = [
            ("All".to_string(), A_TAB0),
            (format!("Pages ({})", data.pages.len()), A_TAB0 + 1),
            (
                format!("Documents ({})", data.docs.len() + data.collections.len()),
                A_TAB0 + 2,
            ),
        ];
        let mut tx = 0;
        for (i, (label, act)) in tabs.iter().enumerate() {
            let tw = self.tags_text_wb(36.0, label);
            let item_w = tw + 80;
            let selected = match self.tags.tab {
                Tab::All => i == 0,
                Tab::Pages => i == 1,
                Tab::Documents => i == 2,
            };
            if selected {
                self.fb
                    .fill_rect(tx, TABS_RULE_Y - IND_H, item_w, IND_H, gray565(20));
            }
            if let Some(t) = self.text.as_mut() {
                t.draw_bold(&mut self.fb, tx + (item_w - tw) / 2, TABS_BASE, 36.0, label, 20);
            }
            self.hits.push(tx, TABS_TOP, item_w, TABS_H, *act);
            tx += item_w;
        }
        self.fb.fill_rect(0, TABS_RULE_Y, w, 2, gray565(20));

        // ---------- 胶囊行
        self.tags_pills(w, &data.labels);
    }

    fn tags_pills(&mut self, w: i32, labels: &[String]) {
        // 官方：TagFilter 左右 30，内部 tagsContent 再窄 20、Flow padding 10
        let (x0, avail) = (40, w - 80 - 20);
        let hold = self.tags.hold.clone();
        let mut x = x0;
        let mut y = FILTER_TOP + 10;
        let mut row = 0;
        for (i, name) in labels.iter().enumerate() {
            let tw = self.tags_text_wb(28.0, name).max(50);
            let pw = tw + PILL_PAD;
            if x + pw > x0 + avail && x > x0 {
                x = x0;
                y += PILL_H + PILL_GAP;
                row += 1;
            }
            if row >= 3 {
                break;
            }
            let sel = self.tags.selected.iter().any(|t| t == name);
            self.tags_pill(x, y, pw, name, sel, hold.as_deref(), 28.0, PILL_H);
            self.tags.pills.push((name.clone(), x, y, pw, PILL_H));
            // 命中区 ≥ 80×80（官方 MouseArea 是胶囊外扩 10）
            self.hits
                .push(x - 6, y - 17, pw.max(80) + 12, 80, A_PILL0 + i as u32);
            x += pw + PILL_GAP;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn tags_pill(
        &mut self,
        x: i32,
        y: i32,
        w: i32,
        label: &str,
        selected: bool,
        hold: Option<&str>,
        size: f32,
        h: i32,
    ) {
        if hold == Some(label) {
            // 官方 `editing`：外扩 10、8px 粗黑环
            fill_capsule(&mut self.fb, x - 10, y - 10, w + 20, h + 20, 20);
            fill_capsule(&mut self.fb, x - 2, y - 2, w + 4, h + 4, 255);
        }
        if selected {
            // 选中：黑底白字
            fill_capsule(&mut self.fb, x, y, w, h, 0);
        } else {
            stroke_capsule(&mut self.fb, x, y, w, h, 2, 20);
        }
        let tw = self.tags_text_wb(size, label);
        let base = y + h / 2 + (size * 0.35) as i32;
        if let Some(t) = self.text.as_mut() {
            t.draw_bold(
                &mut self.fb,
                x + (w - tw) / 2,
                base,
                size,
                label,
                if selected { 255 } else { 20 },
            );
        }
    }

    /// 缩略图上的标签：官方 `ThumbnailTags` 竖排，最多 3 个，多的收成 `+N`。
    fn tags_thumb_pills(&mut self, x: i32, y: i32, w: i32, h: i32, tags: &[String]) {
        let n = tags.len().min(3);
        if n == 0 {
            return;
        }
        let hidden = tags.len() - n;
        let total = n as i32 * TPILL_H + (n as i32 - 1) * TPILL_GAP;
        let mut widths = Vec::with_capacity(n);
        for name in tags.iter().take(n) {
            let tw = self.tags_text_w(22.0, name).max(25);
            let cap = if hidden > 0 { w - 60 } else { w };
            widths.push((tw + 30).min(cap.max(60)));
        }
        let mut py = y + h - total;
        for i in 0..n {
            let pw = widths[i];
            stroke_capsule(&mut self.fb, x, py, pw, TPILL_H, 2, 20);
            if let Some(t) = self.text.as_mut() {
                let label = t.ellipsize(22.0, &tags[i], pw - 16);
                let tw = t.width(22.0, &label) as i32;
                t.draw(
                    &mut self.fb,
                    x + (pw - tw) / 2,
                    py + TPILL_H / 2 + 8,
                    22.0,
                    &label,
                    20,
                );
            }
            py += TPILL_H + TPILL_GAP;
        }
        if hidden > 0 {
            let cw = 50;
            let (cx, cy) = (x + w - cw, y + h - TPILL_H);
            stroke_capsule(&mut self.fb, cx, cy, cw, TPILL_H, 2, 20);
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, cx + 11, cy + 28, 22.0, &format!("+{hidden}"), 20);
            }
        }
    }

    fn tags_card(&mut self, cx: i32, cy: i32, cell: &Cell) {
        match cell {
            Cell::Doc(b) => {
                // 文档卡沿用库网格（缩略图 + 名字 + 页数），再叠标签胶囊
                let tags = self.library.tags_of(&b.uuid);
                self.draw_tile(cx, cy, b);
                self.tags_thumb_pills(cx + 15, cy + 15, COL_W - 30, TILE_H - 30, &tags);
            }
            Cell::Page(p) => {
                let th = self.tags_page_thumb(&p.uuid, p.page);
                for yy in 0..TILE_H {
                    for xx in 0..COL_W {
                        self.fb
                            .put(cx + xx, cy + yy, gray565(th[(yy * COL_W + xx) as usize]));
                    }
                }
                let edge = gray565(70);
                self.fb.fill_rect(cx, cy, COL_W, 1, edge);
                self.fb.fill_rect(cx, cy + TILE_H - 1, COL_W, 1, edge);
                self.fb.fill_rect(cx, cy, 1, TILE_H, edge);
                self.fb.fill_rect(cx + COL_W - 1, cy, 1, TILE_H, edge);
                self.tags_thumb_pills(cx + 15, cy + 15, COL_W - 30, TILE_H - 30, &p.tags);
                if let Some(t) = self.text.as_mut() {
                    let name = t.ellipsize_mid_bold(32.0, &p.name, COL_W);
                    t.draw_bold(&mut self.fb, cx + 3, cy + TILE_H + 49, 32.0, &name, 20);
                    let meta = format!("Page {}", p.page + 1);
                    t.draw(&mut self.fb, cx + 3, cy + TILE_H + 86, 26.0, &meta, 20);
                }
            }
        }
    }

    /// 集合（文件夹）行：官方 `ContentEntriesGridCollectionItem` 342×75。
    fn tags_collection_row(&mut self, cx: i32, cy: i32, b: &Book) {
        self.fb.fill_rect(cx, cy, COL_W, 75, gray565(240));
        widgets::outline_rect(&mut self.fb, cx, cy, COL_W, 75, 1, 70);
        self.fb.fill_rect(cx + 22, cy + 30, 40, 26, gray565(90));
        self.fb.fill_rect(cx + 25, cy + 23, 17, 8, gray565(90));
        if let Some(t) = self.text.as_mut() {
            let name = t.ellipsize_mid_bold(30.0, &b.name, COL_W - 110);
            t.draw_bold(&mut self.fb, cx + 78, cy + 50, 30.0, &name, 20);
        }
    }

    /// 空态：官方 `FontIcon(icon_tag, 90)` + `No tags yet` + 说明段。
    /// 有选中标签但没结果时只有说明段（标题隐藏，位置不变）。
    fn tags_empty_state(&mut self) {
        let w = self.fb.width;
        widgets::icon_tag_s(&mut self.fb, w as f32 / 2.0, 886.0, 94.0, 20);
        let sel = !self.tags.selected.is_empty();
        if !sel {
            if let Some(t) = self.text.as_mut() {
                let head = "No tags yet";
                let tw = t.width(32.0, head) as i32;
                t.draw(&mut self.fb, (w - tw) / 2, 1007, 32.0, head, 20);
            }
        }
        let lines: &[&str] = if sel {
            &[
                "You have no content with this combination of tags.",
                "Select fewer tags to find what you’re looking for.",
            ]
        } else {
            &[
                "You can tag pages, files, and folders to find them faster.",
                "All your tagged content will appear here so you can easily sort and filter",
                "notes and documents.",
            ]
        };
        if let Some(t) = self.text.as_mut() {
            for (i, line) in lines.iter().enumerate() {
                let lw = t.width(29.0, line) as i32;
                t.draw(&mut self.fb, (w - lw) / 2, 1093 + i as i32 * 55, 29.0, line, 20);
            }
        }
    }

    // ------------------------------------------------------------ 弹层

    /// 弹层（操作条 / 删除确认 / 改名）：必须画在状态栏和侧栏之上。
    pub fn tags_modals(&mut self) {
        let (w, h) = (self.fb.width, self.fb.height);

        // ---------- 长按操作条：官方 NavigatorModalActionBase，高 120，黑底白字
        if self.tags.hold.is_some() {
            let bh = 120;
            let by = h - bh;
            self.fb.fill_rect(0, by, w, bh, gray565(0));
            self.hits.push(0, by, w, bh, A_HOLD_CLOSE);
            if let Some(t) = self.text.as_mut() {
                t.draw_bold(&mut self.fb, 33, by + 76, 36.0, "Edit tag", 250);
            }
            let mut x = w - 40;
            for (label, act) in [("Delete", A_HOLD_DELETE), ("Rename", A_HOLD_RENAME)] {
                let tw = self.tags_text_wb(34.0, label);
                x -= tw;
                if let Some(t) = self.text.as_mut() {
                    t.draw_bold(&mut self.fb, x, by + 76, 34.0, label, 250);
                }
                self.hits.push(x - 30, by, tw + 60, bh, act);
                x -= 70;
            }
        }

        // ---------- 删除确认（官方 ConfirmChangeDialog）
        if let Some(tag) = self.tags.confirm.clone() {
            self.fb.dim(0, 0, w, h, gray565(140), 1, 2);
            let bw = 980;
            let bh = 420;
            let (bx, by) = ((w - bw) / 2, (h - bh) / 2);
            self.fb.fill_rect(bx, by, bw, bh, gray565(255));
            widgets::outline_rect(&mut self.fb, bx, by, bw, bh, 2, 20);
            if let Some(t) = self.text.as_mut() {
                t.draw_bold(&mut self.fb, bx + 48, by + 84, 34.0, &format!("Delete '{tag}'?"), 20);
                t.draw(
                    &mut self.fb,
                    bx + 48,
                    by + 142,
                    26.0,
                    "If you delete this tag, it’ll be removed from all tagged content.",
                    20,
                );
            }
            let (bw2, bh2) = (400, 90);
            let y2 = by + bh - 130;
            widgets::outline_rect(&mut self.fb, bx + 48, y2, bw2, bh2, 2, 30);
            widgets::outline_rect(&mut self.fb, bx + bw - 48 - bw2, y2, bw2, bh2, 2, 30);
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, bx + 78, y2 + 58, 28.0, "No, keep tag", 20);
                t.draw(
                    &mut self.fb,
                    bx + bw - 48 - bw2 + 30,
                    y2 + 58,
                    28.0,
                    "Yes, delete tag",
                    20,
                );
            }
            self.hits.push(bx + 48, y2, bw2, bh2, A_DEL_NO);
            self.hits
                .push(bx + bw - 48 - bw2, y2, bw2, bh2, A_DEL_YES);
        }

        // ---------- 改名：整屏白底 + 现有键盘（官方标题 `Rename tag`）
        if let Some((_, kb)) = self.tags.rename.clone() {
            self.fb.fill_rect(0, 0, w, h, gray565(255));
            self.fb.fill_rect(0, 0, w, 2, gray565(180));
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, NAV_MARGIN, 100, 44.0, "Rename tag", 20);
            }
            let (ix, iy, iw, ih) = (NAV_MARGIN, 140, w - 2 * NAV_MARGIN, 84);
            self.fb.fill_rect(ix, iy, iw, ih, gray565(238));
            if let Some(t) = self.text.as_mut() {
                let shown = t.ellipsize(34.0, &kb.text, iw - 40);
                t.draw(&mut self.fb, ix + 20, iy + 58, 34.0, &shown, 20);
            }
            let ky = h - kb.height();
            self.kb_hits = kb.draw(&mut self.fb, &mut self.text, 0, ky, w);
            let by = ky - 110;
            let bw = (w - 2 * NAV_MARGIN - 20) / 2;
            self.fb.fill_rect(NAV_MARGIN, by, bw, 90, gray565(210));
            self.fb.fill_rect(NAV_MARGIN + bw + 20, by, bw, 90, gray565(40));
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, NAV_MARGIN + 60, by + 60, 34.0, "Cancel", 20);
                t.draw(&mut self.fb, NAV_MARGIN + bw + 80, by + 60, 34.0, "Save", 250);
            }
            self.hits.push(NAV_MARGIN, by, bw, 90, A_REN_CANCEL);
            self.hits.push(NAV_MARGIN + bw + 20, by, bw, 90, A_REN_SAVE);
        }
    }

    // ------------------------------------------------------------ 交互

    /// 弹层（改名键盘 / 删除确认 / 长按操作条）的点击。吃掉就直接返回 true。
    pub fn tags_modal_tap(&mut self, x: f32, y: f32) -> bool {
        if self.tags.rename.is_some() {
            for (r, k) in self.kb_hits.clone() {
                let (xi, yi) = (x as i32, y as i32);
                if xi >= r.0 && xi < r.0 + r.2 && yi >= r.1 && yi < r.1 + r.3 {
                    self.tags_key(k);
                    return true;
                }
            }
            match self.hits.take(x, y) {
                Some(A_REN_CANCEL) => {
                    self.tags.rename = None;
                    self.tags.hold = None;
                    self.tags.dirty = None;
                    self.draw_library();
                }
                Some(A_REN_SAVE) => self.tags_rename_save(),
                _ => {}
            }
            return true;
        }
        if self.tags.confirm.is_some() {
            match self.hits.take(x, y) {
                Some(A_DEL_YES) => {
                    let tag = self.tags.confirm.take().unwrap_or_default();
                    if let Err(e) = self.library.delete_tag(&tag) {
                        eprintln!("[tags] 删除标签失败：{e}");
                    }
                    eprintln!("[tags] 已删除标签 {tag}");
                    self.tags.hold = None;
                    self.tags.selected.clear();
                    self.banner = Some(format!("'{tag}' was deleted"));
                    self.tags.dirty =
                        Some((0, FILTER_TOP, self.fb.width, self.fb.height - FILTER_TOP));
                    self.scroll = 0;
                    self.draw_library();
                }
                _ => {
                    self.tags.confirm = None;
                    self.tags.dirty = None;
                    self.draw_library();
                }
            }
            return true;
        }
        if self.tags.hold.is_some() {
            match self.hits.take(x, y) {
                Some(A_HOLD_RENAME) => {
                    let Some(tag) = self.tags.hold.clone() else {
                        return true;
                    };
                    let mut kb = Keyboard::new();
                    kb.text = tag.clone();
                    self.tags.confirm = None;
                    self.tags.rename = Some((tag, kb));
                    self.banner = None;
                    self.tags.dirty = None;
                    self.draw_library();
                }
                Some(A_HOLD_DELETE) => {
                    self.tags.confirm = self.tags.hold.clone();
                    self.tags.dirty = None;
                    self.draw_library();
                }
                _ => {
                    self.tags.hold = None;
                    self.tags.dirty = None;
                    self.draw_library();
                }
            }
            return true;
        }
        false
    }

    fn tags_key(&mut self, k: Key) {
        {
            let Some((_, kb)) = self.tags.rename.as_mut() else {
                return;
            };
            kb.press(k);
        }
        if k == Key::Enter {
            self.tags_rename_save();
            return;
        }
        self.draw_library();
    }

    fn tags_rename_save(&mut self) {
        let Some((old, kb)) = self.tags.rename.take() else {
            return;
        };
        let new = kb.text.trim().to_string();
        let new = if new.is_empty() { old.clone() } else { new };
        if new != old {
            match self.library.rename_tag(&old, &new) {
                Ok(n) => eprintln!("[tags] 标签改名 {old} → {new}（动了 {n} 篇）"),
                Err(e) => eprintln!("[tags] 标签改名失败：{e}"),
            }
            for t in self.tags.selected.iter_mut() {
                if *t == old {
                    *t = new.clone();
                }
            }
        }
        self.tags.hold = None;
        self.tags.rename = None;
        self.tags.dirty = None;
        self.draw_library();
    }

    /// 长按胶囊：进操作条（官方会先把其它选中清掉）。
    pub fn tags_hold(&mut self, x: f32, y: f32) -> bool {
        let (xi, yi) = (x as i32, y as i32);
        let mut best: Option<(i32, String)> = None;
        for (name, px, py, pw, ph) in &self.tags.pills {
            let (cx, cy) = (px + pw / 2, py + ph / 2);
            let (dx, dy) = ((xi - cx).abs(), (yi - cy).abs());
            if dx > pw / 2 + 20 || dy > 40 {
                continue;
            }
            let d = dx * dx + dy * dy;
            if best.as_ref().map(|(bd, _)| d < *bd).unwrap_or(true) {
                best = Some((d, name.clone()));
            }
        }
        let Some((_, name)) = best else {
            return false;
        };
        eprintln!("[tags] 长按标签 {name}");
        self.tags.selected = vec![name.clone()];
        self.tags.hold = Some(name);
        self.tags.confirm = None;
        self.tags.dirty = None;
        self.draw_library();
        true
    }

    /// Tags 视图的动作号。
    pub fn tags_action(&mut self, a: u32) {
        let (w, h) = (self.fb.width, self.fb.height);
        if self.tags.sort_open && a != A_SORT && a != A_SORT_ITEM {
            // 官方 popup 点外面就收；下拉那块得整屏刷一次才擦得干净
            self.tags.sort_open = false;
            self.tags.dirty = None;
        }
        match a {
            A_SORT => {
                self.tags.sort_open = !self.tags.sort_open;
                self.tags.dirty = None;
            }
            A_SORT_ITEM => {
                self.tags.sort_open = false;
                self.tags.dirty = Some((0, 130, w, 700));
            }
            _ if (A_TAB0..A_TAB0 + 3).contains(&a) => {
                self.tags.tab = match a - A_TAB0 {
                    0 => Tab::All,
                    1 => Tab::Pages,
                    _ => Tab::Documents,
                };
                self.scroll = 0;
                self.tags.dirty = Some((0, TABS_TOP, w, h - TABS_TOP));
            }
            _ if (A_PILL0..A_PILL0 + 80).contains(&a) => {
                let i = (a - A_PILL0) as usize;
                let labels = self.library.tag_labels();
                if let Some(name) = labels.get(i).cloned() {
                    if let Some(p) = self.tags.selected.iter().position(|t| *t == name) {
                        self.tags.selected.remove(p);
                    } else {
                        self.tags.selected.push(name);
                    }
                    self.scroll = 0;
                    self.tags.dirty = Some((0, FILTER_TOP, w, h - FILTER_TOP));
                }
            }
            _ if (A_TILE0..A_TILE0 + 100).contains(&a) => {
                // 打开文档/文件夹：`open_at` / `enter_folder` 自己会重画，别再把库糊上去
                self.tags_open_tile((a - A_TILE0) as usize);
                return;
            }
            _ => {}
        }
        self.draw_library();
    }

    fn tags_open_tile(&mut self, idx: usize) {
        let data = self.tags_data();
        if idx >= 100 {
            let i = idx - 100;
            if let Some(c) = data.collections.get(i) {
                let (uuid, name) = (c.uuid.clone(), c.name.clone());
                self.enter_folder(&uuid, &name);
            }
            return;
        }
        let cells = self.tags_cells(&data);
        match cells.get(idx) {
            Some(Cell::Doc(b)) => {
                let (uuid, page) = (b.uuid.clone(), b.last_opened_page as usize);
                self.open_at(&uuid, page);
            }
            Some(Cell::Page(p)) => {
                let (uuid, page) = (p.uuid.clone(), p.page);
                self.open_at(&uuid, page);
            }
            None => {}
        }
    }

    /// 页面缩略图（426×568，笔记本画墨迹、PDF 走 pdfium），带缓存。
    fn tags_page_thumb(&mut self, uuid: &str, page: usize) -> std::rc::Rc<Vec<u8>> {
        let key = format!("tagpg:{uuid}:{page}");
        if let Some(t) = self.thumbs.get(&key) {
            return t.clone();
        }
        let (tw, th) = (COL_W as usize, TILE_H as usize);
        let mut out = vec![255u8; tw * th];
        if let Some(b) = self.library.find(uuid) {
            if b.format == rmdev::doc::Format::Pdf {
                let path = self.library.root.join(format!("{uuid}.pdf"));
                if let Ok(pdf) = Pdf::open(path.to_str().unwrap_or("")) {
                    let asp = pdf.aspect(page).max(0.1);
                    let mut hh = th;
                    let mut ww = ((th as f32) / asp).round() as usize;
                    if ww > tw {
                        ww = tw;
                        hh = ((tw as f32) * asp).round() as usize;
                    }
                    if let Ok(g) = pdf.render(page, ww, hh) {
                        let (ox, oy) = ((tw - ww) / 2, (th - hh.min(th)) / 2);
                        for y in 0..hh.min(th) {
                            let src = y * ww;
                            let dst = (oy + y) * tw + ox;
                            out[dst..dst + ww].copy_from_slice(&g[src..src + ww]);
                        }
                    }
                }
            } else {
                let scale = tw as f32 / 1404.0;
                let p = self.library.load_page_at(uuid, page);
                let grade = rmdev::pen::pencil::DEFAULT;
                let mut c = rmdev::canvas::Canvas::new(tw, th);
                for l in p.visible_lines() {
                    c.ink_line(l, scale, (crate::PAGE_DX * scale, crate::PAGE_DY * scale), grade);
                }
                for y in 0..th {
                    for x in 0..tw {
                        out[y * tw + x] = 255 - c.dark_at(x, y);
                    }
                }
            }
        }
        let rc = std::rc::Rc::new(out);
        self.thumbs.insert(key, rc.clone());
        rc
    }

    /// 只刷变化的区域（`dirty`），没有就整屏。和库的局部重绘约定一致。
    pub fn tags_flush(&mut self) {
        let (w, h) = (self.fb.width, self.fb.height);
        match self.tags.dirty.take() {
            Some((x, y, ww, hh)) => {
                let ww = ww.min(w - x).max(1);
                let hh = hh.min(h - y).max(1);
                self.fb
                    .refresh(x, y, ww, hh, WAVEFORM_MODE_GC16, UPDATE_MODE_PARTIAL);
            }
            None => self
                .fb
                .refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL),
        }
    }
}
