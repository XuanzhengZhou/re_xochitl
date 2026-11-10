//! Notebook settings 页（文档视图 → 文档设置 → `Notebook settings`）。
//!
//! 官方 `NavigatorModalSettings.qml` 在 carve 出来的 QML 里缺失（二进制里只有压缩块），
//! 规格里给的同内容参照实现是 `res_06ab865.qml`（"Create or rename a document and
//! related settings"）：黑头 `Cancel` / 标题 / `Save`，下面依次是
//! `Name`、`Notebook cover`（`Last page visited` / `First page`）、`Templates`
//! （四张模板卡：Blank / Lined medium / Grid medium / Checklist medium）。
//! 本页照这个结构做；QML 里模板是 `DocumentController.setTemplateForPage(entry,
//! entry.lastOpenedPage, templateFilename)`，即**当前页**，同时把模板名写回
//! `.pagedata`，之后 `New page` 新增的页会沿用（见 `library::add_page`）。
//!
//! `Page orientation` 一项在 QML 里只有 `landscape` 属性、没有可点控件，
//! 这里按官方模板选择器里的原文（`Portrait` / `Landscape`）补一行，
//! 落盘 `.content.orientation` / `customZoomOrientation`。
//!
//! 约定同 `docsettings`：`rows()` / `hits()` 是纯布局，`paint()` 只照着画。

use crate::fb::{gray565, Fb};
use crate::ui::keyboard::Keyboard;
use crate::ui::text::Text;
use crate::ui::widgets;

/// 黑头高（与库里其它页一致）
pub const HDR_H: i32 = 118;

/// 四张模板卡：第一项是 UI 文案（官方 `TemplateDelegate` 标题），
/// 第二项是写进 `.content` / `.pagedata` 的模板文件名。
pub const TEMPLATES: [(&str, &str); 4] = [
    ("Blank", "Blank"),
    ("Lined medium", "Lines medium"),
    ("Grid medium", "Grid medium"),
    ("Checklist medium", "Checklist medium"),
];

const FONT_HDR: f32 = 33.0;
const FONT_LABEL: f32 = 36.0;
const FONT_NAME: f32 = 40.0;
const FONT_OPTION: f32 = 34.0;
const FONT_CARD: f32 = 26.0;

/// 卡片几何：4 列，334 宽、335 步进、338 高（照 `42-new-notebook.png` 实测：
/// 卡 y 691..1028、预览 150×199 居中偏上、标题在卡内基线 y+300）。
const CARD_TOP: i32 = 975;
const CARD_W: i32 = 334;
const CARD_H: i32 = 338;
const CARD_STEP: i32 = 335;
/// 卡片里的模板预览：官方 150×199，距卡顶 41。
const PREVIEW_W: i32 = 150;
const PREVIEW_H: i32 = 199;
const PREVIEW_TOP: i32 = 41;

/// 单选行（官方 `RadioButton` 圆心 x=52、半径 20）。官方行距 60，
/// 但可点区域要 ≥80×80，这里行距放到 80（命中带 80 高、互不重叠）。
const RADIO_CX: i32 = 52;
const RADIO_R: i32 = 20;
const RADIO_ROW: i32 = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Cancel,
    Save,
    /// 点文档名 → 开/收键盘
    Name,
    /// 封面显示哪一页：0 = Last page visited，1 = First page
    Cover(usize),
    /// 竖版 / 横版
    Orientation(bool),
    /// 第几张模板卡
    Template(usize),
}

/// 这一页服务的是哪类文档：标题和封面文案按官方 QML 分开。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Notebook,
    Pdf,
    Ebook,
}

impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Kind::Notebook => "Notebook settings",
            Kind::Pdf => "PDF settings",
            Kind::Ebook => "Ebook settings",
        }
    }
    pub fn cover(self) -> &'static str {
        match self {
            Kind::Notebook => "Notebook cover",
            Kind::Pdf => "PDF cover",
            Kind::Ebook => "Ebook cover",
        }
    }
}

pub struct NbSettings {
    open: bool,
    /// 文档名输入（键盘贴底时才画）
    pub kb: Keyboard,
    kb_open: bool,
    uuid: String,
    /// 当前页序号（模板写到这一页）
    page: usize,
    cover: usize,
    template: usize,
    /// true = Portrait（官方 `.content.orientation`）
    portrait: bool,
    /// 文档类型：决定标题（Notebook/PDF/Ebook settings）和封面文案
    kind: Kind,
}

impl NbSettings {
    pub fn new() -> NbSettings {
        NbSettings {
            open: false,
            kb: Keyboard::new(),
            kb_open: false,
            uuid: String::new(),
            page: 0,
            cover: 0,
            template: 0,
            portrait: true,
            kind: Kind::Notebook,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn kb_open(&self) -> bool {
        self.kb_open
    }

    pub fn uuid(&self) -> &str {
        &self.uuid
    }

    pub fn page(&self) -> usize {
        self.page
    }

    pub fn portfolio(&self) -> bool {
        self.portrait
    }

    /// 打开设置页。`template` 是当前页模板在 [`TEMPLATES`] 里的下标；
    /// 认不出来的模板按 Blank 显示（官方也是选中第一张）。`cover` 是
    /// 「封面显示哪一页」的当前值（0 = Last page visited，1 = First page）。
    #[allow(clippy::too_many_arguments)]
    pub fn show(
        &mut self,
        uuid: &str,
        name: &str,
        page: usize,
        template: &str,
        portrait: bool,
        cover: usize,
    ) {
        self.open = true;
        self.kb_open = false;
        // 默认按笔记本；PDF/电子书由调用方随后 `set_kind(..)`（`show` 不带 kind 参数，
        // 这样老调用点一个都不用改）
        self.kind = Kind::Notebook;
        self.uuid = uuid.to_string();
        self.page = page;
        self.cover = cover.min(1);
        self.portrait = portrait;
        self.template = TEMPLATES
            .iter()
            .position(|(_, f)| *f == template)
            .unwrap_or(0);
        self.kb.text = name.to_string();
        self.kb.layer = crate::ui::keyboard::Layer::Lower;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.kb_open = false;
    }

    /// 点文档名：开/收键盘。
    pub fn toggle_keyboard(&mut self) {
        self.kb_open = !self.kb_open;
    }

    pub fn set_cover(&mut self, i: usize) {
        self.cover = i.min(1);
    }

    /// 当前文档是不是 PDF。只影响封面那行标题（官方 `res_06ab865.qml:371-380`
    /// 按 `entry.fileType` 选 `PDF cover` / `Notebook cover`）。
    pub fn set_kind(&mut self, kind: Kind) {
        self.kind = kind;
    }

    pub fn kind(&self) -> Kind {
        self.kind
    }

    pub fn set_pdf(&mut self, pdf: bool) {
        self.kind = if pdf { Kind::Pdf } else { Kind::Notebook };
    }

    pub fn is_pdf(&self) -> bool {
        self.kind == Kind::Pdf
    }

    pub fn set_orientation(&mut self, portrait: bool) {
        self.portrait = portrait;
    }

    pub fn set_template(&mut self, i: usize) {
        self.template = i.min(TEMPLATES.len() - 1);
    }

    pub fn template(&self) -> usize {
        self.template
    }

    /// 文档名校验（官方 `documentNameIsValid`：trim 后非空）。
    pub fn name_valid(&self) -> bool {
        !self.kb.text.trim().is_empty()
    }

    /// `Save` 时要落盘的几项（名字沿用了 `library.rename`，其余三个是新增 API）。
    pub fn save_state(&self) -> (String, usize, usize, bool) {
        let name = self.kb.text.trim().to_string();
        (name, self.page, self.cover, self.portrait)
    }

    /// 顶栏按钮 + 各行的可点区域。所有命中区都 ≥ 80×80。
    pub fn hits(&self, w: i32, h: i32) -> Vec<((i32, i32, i32, i32), Act)> {
        let mut v = Vec::with_capacity(12);
        v.push(((0, 0, 240, HDR_H), Act::Cancel));
        v.push(((w - 240, 0, 240, HDR_H), Act::Save));
        // 文档名（含下划线那一行）
        v.push(((0, 170, w, 150), Act::Name));
        // 封面两个单选：命中带 80 高，互不重叠
        for i in 0..2 {
            let cy = cover_cy(i);
            v.push(((20, cy - 40, 520, 80), Act::Cover(i)));
        }
        // 方向两个单选
        for i in 0..2 {
            let cy = orient_cy(i);
            v.push(((20, cy - 40, 520, 80), Act::Orientation(i == 0)));
        }
        // 模板四张卡（标题在卡内，命中盖整张卡）
        for i in 0..TEMPLATES.len() {
            let tx = 33 + i as i32 * CARD_STEP;
            v.push(((tx, CARD_TOP - 10, CARD_W, CARD_H + 20), Act::Template(i)));
        }
        // 键盘盖住的区域不算（调用方先查键盘）
        v.retain(|(r, _)| r.1 + r.3 <= h);
        v
    }

    /// 命中测试（键盘由调用方先处理）。
    pub fn hit(&self, x: f32, y: f32, w: i32, h: i32) -> Option<Act> {
        self.hits(w, h)
            .into_iter()
            .find(|(r, _)| {
                x >= r.0 as f32
                    && y >= r.1 as f32
                    && x < (r.0 + r.2) as f32
                    && y < (r.1 + r.3) as f32
            })
            .map(|(_, a)| a)
    }

    /// 文档名那一行的局部刷新范围（打字时只刷它，不整屏）。
    pub fn name_rect(w: i32) -> (i32, i32, i32, i32) {
        (0, 170, w, 150)
    }

    /// 画整页。调用方保证打开状态；`fb` 会被整块刷白。
    pub fn paint(&self, fb: &mut Fb, text: &mut Text, w: i32, h: i32) {
        fb.fill_rect(0, 0, w, h, gray565(255));

        // ---------- 黑头：Cancel | Notebook settings | Save
        fb.fill_rect(0, 0, w, HDR_H, gray565(0));
        text.draw_bold(fb, 32, 74, FONT_HDR, "Cancel", 255);
        let title = self.kind.title();
        let tw = text.width_bold(FONT_HDR, title);
        text.draw_bold(fb, (w as f32 / 2.0 - tw / 2.0) as i32, 74, FONT_HDR, title, 255);
        let save = "Save";
        let sw = text.width_bold(FONT_HDR, save);
        let tone = if self.name_valid() { 255 } else { 130 };
        text.draw_bold(fb, w - 32 - sw as i32, 74, FONT_HDR, save, tone);

        // ---------- Name
        text.draw_bold(fb, 33, 206, FONT_LABEL, "Name", 20);
        let name = self.kb.text.clone();
        let shown = text.ellipsize(FONT_NAME, &name, w - 100);
        text.draw(fb, 37, 292, FONT_NAME, &shown, 20);
        if self.kb_open {
            // 聚焦态：文字后面一根光标
            let tw = text.width(FONT_NAME, &shown) as i32;
            fb.fill_rect(37 + tw + 3, 258, 2, 40, gray565(20));
        }
        // 输入框下面 2px 黑线（官方截图 y 308..309）
        fb.fill_rect(33, 308, w - 66, 2, gray565(0));

        // ---------- Notebook cover / PDF cover
        text.draw_bold(fb, 33, 400, FONT_LABEL, self.kind.cover(), 20);
        for (i, label) in ["Last page visited", "First page"].iter().enumerate() {
            self.radio(fb, text, cover_cy(i), *label, self.cover == i);
        }

        // ---------- Page orientation
        text.draw_bold(fb, 33, 660, FONT_LABEL, "Page orientation", 20);
        for (i, label) in ["Portrait", "Landscape"].iter().enumerate() {
            self.radio(fb, text, orient_cy(i), *label, self.portrait == (i == 0));
        }

        // ---------- Templates
        text.draw_bold(fb, 33, 930, FONT_LABEL, "Templates", 20);
        for (i, (label, _)) in TEMPLATES.iter().enumerate() {
            let tx = 33 + i as i32 * CARD_STEP;
            let sel = self.template == i;
            if sel {
                // 选中 = 整张卡反白（官方截图的 Blank 就是黑卡白线稿）
                fb.fill_rect(tx, CARD_TOP, CARD_W, CARD_H, gray565(0));
            }
            template_preview(fb, tx, CARD_TOP, i, if sel { 255 } else { 20 });
            let lw = text.width_bold(FONT_CARD, label);
            text.draw_bold(
                fb,
                tx + (CARD_W - lw as i32) / 2,
                CARD_TOP + 300,
                FONT_CARD,
                label,
                if sel { 255 } else { 20 },
            );
        }
    }

    fn radio(&self, fb: &mut Fb, text: &mut Text, cy: i32, label: &str, on: bool) {
        widgets::ring(fb, RADIO_CX, cy, RADIO_R, 4, 20);
        if on {
            widgets::disc(fb, RADIO_CX, cy, 9, 20);
        }
        text.draw(fb, 82, cy + 12, FONT_OPTION, label, 20);
    }
}

impl Default for NbSettings {
    fn default() -> Self {
        NbSettings::new()
    }
}

fn cover_cy(i: usize) -> i32 {
    465 + i as i32 * RADIO_ROW
}

/// 封面那行的标题。官方 QML（`res_06ab865.qml:369-380`）：
/// `Document.Pdf` → `PDF cover`，`Document.Ebook` → `Ebook cover`，
/// 其余（含新建笔记本）→ `Notebook cover`。我们只有 PDF / 笔记本两档。
pub fn cover_title(pdf: bool) -> &'static str {
    if pdf {
        Kind::Pdf.cover()
    } else {
        Kind::Notebook.cover()
    }
}

fn orient_cy(i: usize) -> i32 {
    725 + i as i32 * RADIO_ROW
}

/// 模板卡里的黑白线稿预览（照 `42-new-notebook.png`：150×199、距卡顶 41）。
fn template_preview(fb: &mut Fb, x: i32, y: i32, kind: usize, fg: u8) {
    let c = gray565(fg);
    let ix = x + (CARD_W - PREVIEW_W) / 2;
    let iy = y + PREVIEW_TOP;
    widgets::outline_rect(fb, ix, iy, PREVIEW_W, PREVIEW_H, 3, fg);
    match kind {
        // Lined medium：8 条横线
        1 => {
            for k in 1..8 {
                let ly = iy + PREVIEW_H * k / 8;
                fb.fill_rect(ix + 3, ly, PREVIEW_W - 6, 2, c);
            }
        }
        // Grid medium：横线 + 竖线
        2 => {
            for k in 1..8 {
                let ly = iy + PREVIEW_H * k / 8;
                fb.fill_rect(ix + 3, ly, PREVIEW_W - 6, 2, c);
                let lx = ix + PREVIEW_W * k / 8;
                fb.fill_rect(lx, iy + 3, 2, PREVIEW_H - 6, c);
            }
        }
        // Checklist medium：小方框 + 横线
        3 => {
            for k in 0..4 {
                let ly = iy + 20 + k * 38;
                widgets::outline_rect(fb, ix + 8, ly, 18, 18, 2, fg);
                fb.fill_rect(ix + 34, ly + 8, PREVIEW_W - 46, 2, c);
            }
        }
        // Blank：只有页框
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opened() -> NbSettings {
        let mut s = NbSettings::new();
        s.show("uuid-1", "Quick sheets", 2, "Lines medium", true, 0);
        s
    }

    /// 打开后状态来自当前文档：名字、页号、当前页模板、方向。
    #[test]
    fn show_copies_document_state() {
        let s = opened();
        assert!(s.is_open());
        assert!(!s.kb_open(), "进设置页先不挡模板，点名字才升键盘");
        assert_eq!(s.kb.text, "Quick sheets");
        assert_eq!(s.uuid(), "uuid-1");
        assert_eq!(s.page(), 2);
        assert_eq!(s.template(), 1); // Lines medium
        assert!(s.portfolio());
        // 认不出的模板退回 Blank
        let mut s = NbSettings::new();
        s.show("u", "n", 0, "Weird template", false, 1);
        assert_eq!(s.template(), 0);
        assert!(!s.portfolio());
        assert_eq!(s.hit(300.0, cover_cy(1) as f32, 1404, 1872), Some(Act::Cover(1)));
    }

    /// 四张模板卡：文案和落盘文件名都照官方。
    #[test]
    fn templates_are_official() {
        let labels: Vec<&str> = TEMPLATES.iter().map(|(l, _)| *l).collect();
        assert_eq!(
            labels,
            vec!["Blank", "Lined medium", "Grid medium", "Checklist medium"]
        );
        let files: Vec<&str> = TEMPLATES.iter().map(|(_, f)| *f).collect();
        assert_eq!(
            files,
            vec!["Blank", "Lines medium", "Grid medium", "Checklist medium"]
        );
    }

    /// 点模板卡 / 单选 / 名子都有命中，且命中区 ≥ 80×80。
    #[test]
    fn hit_areas_are_big_enough() {
        let s = opened();
        let hs = s.hits(1404, 1872);
        assert!(hs.len() >= 2 + 1 + 2 + 2 + 4);
        for (r, act) in &hs {
            assert!(r.2 >= 80 && r.3 >= 80, "{act:?} 命中区 {r:?} 太小");
            assert!(r.0 >= 0 && r.0 + r.2 <= 1404);
            assert!(r.1 >= 0 && r.1 + r.3 <= 1872);
        }
    }

    /// 点名字 → 开/收键盘；空名字 `Save` 不可用；`save_state` 给 trim 过的名字。
    #[test]
    fn name_editing_and_save_state() {
        let mut s = opened();
        let name = s.hit(400.0, 260.0, 1404, 1872);
        assert_eq!(name, Some(Act::Name));
        assert!(s.name_valid());
        s.toggle_keyboard();
        assert!(s.kb_open());
        s.toggle_keyboard();
        assert!(!s.kb_open());
        s.kb.text = "   ".into();
        assert!(!s.name_valid());
        s.kb.text = "  New name  ".into();
        assert!(s.name_valid());
        let (name, page, cover, portrait) = s.save_state();
        assert_eq!(name, "New name");
        assert_eq!(page, 2);
        assert_eq!(cover, 0);
        assert!(portrait);
    }

    /// 单选 / 模板在点中后改变状态（绘制由 `paint` 读状态）。
    #[test]
    fn clicking_options_updates_state() {
        let mut s = opened();
        let cy = cover_cy(1);
        assert_eq!(s.hit(300.0, cy as f32, 1404, 1872), Some(Act::Cover(1)));
        s.set_cover(1);
        let cy = orient_cy(1);
        assert_eq!(
            s.hit(300.0, cy as f32, 1404, 1872),
            Some(Act::Orientation(false))
        );
        s.set_orientation(false);
        assert!(!s.portfolio());
        // 第 4 张卡（Checklist medium）
        let tx = 33 + 3 * CARD_STEP;
        assert_eq!(
            s.hit((tx + 100) as f32, (CARD_TOP + 100) as f32, 1404, 1872),
            Some(Act::Template(3))
        );
        s.set_template(3);
        assert_eq!(s.template(), 3);
        // 越界夹取
        s.set_template(9);
        assert_eq!(s.template(), 3);
    }

    /// 顶栏 Cancel / Save 各自命中，键盘贴底时被盖住的行不再命中
    /// （否则手指点键盘会误触到下面的模板卡）。
    #[test]
    fn header_hits_and_keyboard_occlusion() {
        let s = opened();
        assert_eq!(s.hit(100.0, 60.0, 1404, 1872), Some(Act::Cancel));
        assert_eq!(s.hit(1350.0, 60.0, 1404, 1872), Some(Act::Save));
        // 键盘 601 高：y=1300 以下是键盘，模板卡那行（y≥965）本来就在
        // 键盘上沿之外——这里只验证调用方给的可见高度会裁掉卡片命中区。
        let hs = s.hits(1404, 900);
        assert!(hs.iter().all(|(r, _)| r.1 + r.3 <= 900));
        assert!(hs.iter().any(|(_, a)| *a == Act::Name));
        assert!(!hs.iter().any(|(_, a)| matches!(a, Act::Template(3))));
    }

    /// 关闭后状态清掉，重复打开不残留上一次的键盘。
    #[test]
    fn close_resets() {
        let mut s = opened();
        s.toggle_keyboard();
        assert!(s.kb_open());
        s.close();
        assert!(!s.is_open() && !s.kb_open());
    }

    /// 6e：PDF 和笔记本共用这个 sheet，只有封面标题不一样（官方 QML 原文）；
    /// `show()` 默认按笔记本，PDF 由 `set_pdf(true)` 显式指定。
    #[test]
    fn cover_title_follows_document_kind() {
        assert_eq!(cover_title(false), "Notebook cover");
        assert_eq!(cover_title(true), "PDF cover");
        assert_eq!(Kind::Ebook.title(), "Ebook settings");
        assert_eq!(Kind::Ebook.cover(), "Ebook cover");
        let mut s = opened();
        assert!(!s.is_pdf(), "默认是笔记本");
        s.set_pdf(true);
        assert!(s.is_pdf());
        // 重新打开一份笔记本不能继承上一份的 PDF 状态
        s.show("uuid-2", "Notes", 0, "Blank", true, 0);
        assert!(!s.is_pdf());
        // PDF 下命中区 / 行不变（只是文案变）
        s.set_pdf(true);
        assert_eq!(s.hits(1404, 1872).len(), opened().hits(1404, 1872).len());
        assert_eq!(s.hit(300.0, cover_cy(0) as f32, 1404, 1872), Some(Act::Cover(0)));
    }
}
