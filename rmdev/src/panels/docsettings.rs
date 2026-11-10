//! 文档设置面板：左侧工具条最下面那个按钮点开的下拉菜单。
//!
//! 规格来源：
//! - QML `res_067097b.qml`（DocumentSettings）：宽 495、行高 120、
//!   `subtext`、`Set to landscape / portrait view` 随状态切换、
//!   Adjust view 的二级菜单（Fit to width / Fit to height / Use current view）。
//! - 真机 3.0.4 截图：`shots/official/35-docsettings-nb.png`（笔记本）、
//!   `41-docsettings-pdf.png`（PDF）。表头高、图标/文字位置按截图实测。
//!
//! 与其它面板一致：`rows()` 是纯布局（不碰 Fb / Text，可单测），`paint()` 只照着
//! rows 画；状态变化全在 `toggle` / `click` 里。

use crate::fb::{gray565, Fb};
use crate::ui::text::Text;
use crate::ui::widgets;

/// 面板左缘 = 工具条右缘。
pub const PANEL_X: i32 = 120;
/// 底边贴"文档设置"按钮下沿（工具条最下面那个按钮，y 1752..1872）。
pub const PANEL_BOTTOM: i32 = 1872;
/// 行高（QML `Values.documentViewToolButtonSize`）。
pub const ROW: i32 = 120;

/// 面板宽（QML `foldout.width: 495`；真机 120..615）。
const PANEL_W: i32 = 495;
/// 描边（QML `Values.outlineSize`）。实测四边都是 2px 黑线。
const BORDER: i32 = 2;
/// 表头高 = 90 + 73 × 标题行数。实测：笔记本 1 行 163、PDF 3 行 309。
const HEAD_BASE: i32 = 90;
const HEAD_LINE: i32 = 73;
/// 表头标题行距 / 首行基线（实测 PDF 标题基线 1026 / 1092 / 1158）。
const TITLE_PITCH: i32 = 66;
const TITLE_TOP: i32 = 63;
/// 图标 40×40（QML `iconSizeMedium`），左边距 30（`designGridHorizontalSpacing*2`）。
const ICON: i32 = 40;
/// 表头标题的折行宽度（QML 左右各 27 边距）。
const WRAP_W: i32 = PANEL_W - 2 * 27;

/// 面板左边：右利手贴工具条右边，左利手镜像到工具条左边。
pub fn panel_x() -> i32 {
    crate::fb::flip_x(PANEL_X, PANEL_W)
}
/// 表头文字左边距（QML `toolboxFoldoutItemMargin: 27`）。
fn title_x() -> i32 {
    panel_x() + 27
}
fn icon_x() -> i32 {
    panel_x() + 30
}
/// 文字左缘 = 图标右 + 15（QML `designGridHorizontalSpacing`）。
/// 实测墨迹从 206 起，字形原点就是 205（侧边距 1~3px）。
fn text_x() -> i32 {
    icon_x() + ICON + 15
}
/// 文字可用右界（留 20px 不贴边线）。
fn text_r() -> i32 {
    panel_x() + PANEL_W - 20
}
/// 二级菜单贴在主菜单的**内侧**缘（QML `anchors.left: foldout.right`）。
/// 左利手时主菜单在工具条左边，二级菜单就往它左边铺 —— 所以这里要整个镜像，
/// 不能只加 `PANEL_W`（那会伸出屏幕）。
fn sub_x() -> i32 {
    // 右利手 = 主菜单右缘 120+495；左利手镜像到 1404-615 = 294
    crate::fb::flip_x(PANEL_X + PANEL_W, PANEL_W)
}

/// 字号按 `35-docsettings-nb.png` 实测的**落墨**反推：
/// 行文案大写字母高 22px、「Notebook settings」宽 272 → 这一档 29 落墨 22 / 275；
/// 表头标题竖笔画 5px（行内 4px）、「Quick sheets」宽 218 → 33；
/// 页数行 `TextHeaderSmall` 高 22px（宽 158，我们这一档 145）→ 30；
/// 状态小字 TextCaption 18 保持。
const FONT_BODY: f32 = 29.0;
const FONT_TITLE: f32 = 33.0;
const FONT_PAGE: f32 = 30.0;
const FONT_CAPTION: f32 = 18.0;

/// 配色照官方截图：面板是纯白底、纯黑字（原来 250/20 是"柔和一点"的自作主张）；
/// 置灰 150、反白行底 0 / 字 255。
const BG: u8 = 255;
const INK: u8 = 0;
const MUTED_INK: u8 = 150;
const INVERTED_BG: u8 = 0;
const INVERTED_INK: u8 = 255;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Notebook,
    Pdf,
    /// 电子书（EPUB/TXT/MD/FB2/MOBI/DOCX…）：官方面板是 4 行，
    /// `Ebook settings` / `Search` / `Text settings` / `Add note page`（`res_067097b.qml:133-150`）。
    Ebook,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    Width,
    Height,
    Current,
}

impl Fit {
    /// 状态字（QML `_zoomModes[*].status`）。
    pub fn status(self) -> &'static str {
        match self {
            Fit::Width => "Width",
            Fit::Height => "Height",
            Fit::Current => "Custom",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    NotebookSettings,
    PdfSettings,
    Search,
    ToggleLandscape,
    NewPage,
    /// 当前页手写转文字（C14，官方 `Convert to text`）。
    ConvertText,
    /// AI 批改这一页并写回批注（C12，自拟串 `Grade page`，官方无此功能）。
    GradePage,
    AdjustView,
    SetFit(Fit),
    /// 重排格式（EPUB/TXT/MOBI…）专用的「换一档字号」。
    TextSize,
    Back,
    /// 表头（文档名 + 页数）：不可点，点了什么也不做。
    Header,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub label: String,
    /// 第二行小字（QML `ToolButton.subtext`）：Adjust view 的当前缩放模式、表头的页数。
    pub subtext: String,
    pub inverted: bool,
    pub muted: bool,
    pub act: Act,
}

pub struct DocSettings {
    open: bool,
    /// 文档当前是竖屏（QML `documentView.portrait`），决定 orientation 行文案。
    portrait: bool,
    fit: Fit,
    /// Adjust view 的二级菜单展开中。
    submenu: bool,
    /// 缓存的文档名，只给 `rect()` 算表头高度用（`rows()`/`paint()` 用传入的 title）。
    title: String,
    /// 当前文档是不是重排格式（能改字号/行距）。
    reflow: bool,
    /// 当前字号档位（0..5 索引进 `reflow::FONT_SIZES`）。
    text_size: usize,
    /// C14：整页 OCR 的持久状态条（和 `notice` 无关，见 [`OcrStrip`]）。
    pub ocr: OcrStrip,
}

impl DocSettings {
    pub fn new() -> Self {
        DocSettings {
            open: false,
            portrait: true,
            fit: Fit::Width,
            submenu: false,
            title: String::new(),
            reflow: false,
            text_size: 2,
            ocr: OcrStrip::default(),
        }
    }

    /// 打开新文档时同步：这一本能不能改字号、当前是第几档。
    pub fn set_reflow(&mut self, on: bool, size: Option<usize>) {
        self.reflow = on;
        if let Some(s) = size {
            self.text_size = s.min(crate::doc::reflow::FONT_SIZES.len() - 1);
        }
    }

    /// 当前字号档位文案（`Text size` 行的 subtext 用）。
    pub fn text_size_label(&self) -> &'static str {
        TEXT_SIZES[self.text_size.min(TEXT_SIZES.len() - 1)]
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// 缓存文档名。`rect()` 没有 title 参数，要靠它算表头高度；
    /// 主程序打开面板 / 换文档时调一次即可。
    pub fn set_title(&mut self, title: &str) {
        self.title = title.to_string();
    }

    /// 按钮点一下：没开 → 开（总是回到主菜单）；开着 → 关。
    pub fn toggle(&mut self, _kind: Kind) {
        self.open = !self.open;
        self.submenu = false;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.submenu = false;
    }

    /// 纯布局：表头在最上，菜单项从上到下；二级菜单展开时追加在后面一列。
    pub fn rows(&self, kind: Kind, title: &str, page: usize, pages: usize) -> Vec<Row> {
        let lines = wrap_title(title, FONT_TITLE, WRAP_W as f32);
        let head = HEAD_BASE + HEAD_LINE * lines.len() as i32;
        let total = head + menu_h(kind, self.reflow) + BORDER;
        let mut y = PANEL_BOTTOM - total;
        let mut out = Vec::with_capacity(10);

        out.push(Row {
            x: panel_x(),
            y,
            w: PANEL_W,
            h: head,
            label: lines.join("\n"),
            subtext: format!("Page {} of {}", page, pages),
            inverted: false,
            muted: false,
            act: Act::Header,
        });
        y += head;

        let items = self.items(kind);
        let n = items.len();
        let mut adjust_y = 0;
        for (i, (act, label, subtext)) in items.into_iter().enumerate() {
            // 最后一行少 2px：面板底边压在这一行下面
            let h = if i + 1 == n { ROW - BORDER } else { ROW };
            if act == Act::AdjustView {
                adjust_y = y;
            }
            out.push(Row {
                x: panel_x(),
                y,
                w: PANEL_W,
                h,
                label,
                subtext,
                // QML `active: modelData.id === "adjustView" && zoomModeFoldout.visible`
                inverted: act == Act::AdjustView && self.submenu,
                muted: false,
                act,
            });
            y += h;
        }

        // 二级菜单：从 Adjust view 那一行的顶上飞出来（QML `anchors.topMargin = y`）
        if kind == Kind::Pdf && self.submenu {
            let sub = self.submenu_items();
            let n = sub.len();
            let mut y = adjust_y;
            for (i, (act, label)) in sub.into_iter().enumerate() {
                let h = if i + 1 == n { ROW - BORDER } else { ROW };
                out.push(Row {
                    x: sub_x(),
                    y,
                    w: PANEL_W,
                    h,
                    label: label.to_string(),
                    subtext: String::new(),
                    inverted: false,
                    muted: false,
                    act,
                });
                y += h;
            }
        }
        out
    }

    /// 面板并集（含 2px 边线）。关着返回 None。
    ///
    /// 表头高度依赖标题折行数，所以这里用的是 `set_title` 缓存的那份标题。
    pub fn rect(&self, kind: Kind) -> Option<(i32, i32, i32, i32)> {
        if !self.open {
            return None;
        }
        let lines = wrap_title(&self.title, FONT_TITLE, WRAP_W as f32).len() as i32;
        let total = HEAD_BASE + HEAD_LINE * lines + menu_h(kind, self.reflow) + BORDER;
        let (x, w) = if self.submenu && kind == Kind::Pdf {
            // 两列并集：左利手时二级菜单在主菜单左边
            (panel_x().min(sub_x()), PANEL_W * 2)
        } else {
            (panel_x(), PANEL_W)
        };
        Some((x, PANEL_BOTTOM - total, w, total))
    }

    /// 点一下。面板状态（开/关、进/出二级菜单、横竖、缩放模式）都在这里改；
    /// 返回值是"主程序要执行的动作"，只改面板状态时返回 None。
    pub fn click(&mut self, kind: Kind, x: f32, y: f32) -> Option<Act> {
        if !self.open {
            return None;
        }
        // 命中只用到矩形和 act，标题/页数随便给
        let act = self
            .rows(kind, "", 1, 1)
            .into_iter()
            .find(|r| {
                x >= r.x as f32
                    && y >= r.y as f32
                    && x < (r.x + r.w) as f32
                    && y < (r.y + r.h) as f32
            })
            .map(|r| r.act)?;
        match act {
            Act::Header => None,
            // QML：再点一次 Adjust view 会把二级菜单收起来
            Act::AdjustView => {
                self.submenu = !self.submenu;
                None
            }
            Act::SetFit(f) => {
                self.fit = f;
                self.submenu = false;
                Some(Act::SetFit(f))
            }
            Act::Back => {
                self.submenu = false;
                Some(Act::Back)
            }
            // QML：主菜单里点任何一项都会关掉菜单（`foldout.visible = id === "adjustView"`）
            _ => {
                self.submenu = false;
                if act == Act::ToggleLandscape {
                    self.portrait = !self.portrait;
                }
                self.open = false;
                Some(act)
            }
        }
    }

    /// 只照着 `rows()` 画。图标字形会看一点状态（横竖/缩放模式），布局不看。
    pub fn paint(
        &self,
        fb: &mut Fb,
        text: &mut Text,
        kind: Kind,
        title: &str,
        page: usize,
        pages: usize,
    ) {
        let rows = self.rows(kind, title, page, pages);
        let mut right = panel_x();
        for r in &rows {
            let (bg, ink) = if r.inverted {
                (INVERTED_BG, INVERTED_INK)
            } else if r.muted {
                (BG, MUTED_INK)
            } else {
                (BG, INK)
            };
            fb.fill_rect(r.x, r.y, r.w, r.h, gray565(bg));
            // 每行画上/右 2px 边线；左缘只画最左一列，免得两列接缝变成 4px
            fb.fill_rect(r.x, r.y, r.w, BORDER, gray565(INK));
            fb.fill_rect(r.x + r.w - BORDER, r.y, BORDER, r.h, gray565(INK));
            if r.x == panel_x() {
                fb.fill_rect(r.x, r.y, BORDER, r.h, gray565(INK));
            }
            right = right.max(r.x + r.w);

            if r.act == Act::Header {
                // 官方表头标题是粗体，页数行是常规体
                text.set_bold(true);
                for (i, line) in r.label.lines().enumerate() {
                    let s = text.ellipsize(FONT_TITLE, line, WRAP_W);
                    let by = r.y + TITLE_TOP + TITLE_PITCH * i as i32;
                    text.draw(fb, title_x(), by, FONT_TITLE, &s, ink);
                }
                text.set_bold(false);
                // 页数行贴着表头底部往上 31px（实测 1229+132 / 963+278）
                text.draw(fb, title_x(), r.y + r.h - 31, FONT_PAGE, &r.subtext, ink);
                continue;
            }

            // 文字列跟着**这一行**走（二级菜单那一列有自己的 x）；以前这里用
            // 固定的 `text_x()`，二级菜单的文案就压在主菜单上（右利手时也一样）。
            let tx = r.x + (text_x() - panel_x());
            let max_w = text_r() - tx;
            self.paint_icon(fb, r.x + 30, r.y + 41, r.act, ink);
            let label = text.ellipsize(FONT_BODY, &r.label, max_w);
            if r.subtext.is_empty() {
                // 官方行文案是粗体，基线 = 行顶 +73（截图 1465 - 1392）
                text.set_bold(true);
                text.draw(fb, tx, r.y + 73, FONT_BODY, &label, ink);
                text.set_bold(false);
            } else {
                // 两行：label 上移，状态字在下面（实测基线 +49 / +92）。
                // label 同样粗体、状态字常规体（41-docsettings-pdf.png 的 Adjust view / Width）。
                text.set_bold(true);
                text.draw(fb, tx, r.y + 49, FONT_BODY, &label, ink);
                text.set_bold(false);
                let sub = text.ellipsize(FONT_CAPTION, &r.subtext, max_w);
                text.draw(fb, tx, r.y + 92, FONT_CAPTION, &sub, ink);
            }
        }
        // 最后一行高 118，空出来的这 2px 就是面板底边（二级菜单开着时是两列的并集）
        let bottom = PANEL_BOTTOM - BORDER;
        let left = rows.iter().map(|r| r.x).min().unwrap_or_else(panel_x);
        fb.fill_rect(left, bottom, right - left, BORDER, gray565(INK));
    }

    /// 菜单项（QML notebookModel / pdfModel）。
    ///
    /// 笔记本：`Convert to text` / Notebook settings / Search / 横竖 / New page
    /// PDF：PDF settings / Adjust view / 横竖 / Search / Add note page
    /// —— 两边顺序不同是 QML 就这么写的（`res_067097b.qml:81-129`），真机截图也是。
    ///
    /// `Convert to text` 是 C14 加的（官方串见 `res_067dfe1.qml:174`；官方把它放在
    /// 页总览/分享菜单里，我们挪到文档设置面板）。放在**第一行**：面板是底边贴齐、
    /// 往上长的，多出来的一行在最上面时，下面四行的 y 和官方截图逐像素不变
    /// （`main.rs` 的 `docsearch` / `nbsettings` 出图都按那几个 y 点）。
    fn items(&self, kind: Kind) -> Vec<(Act, String, String)> {
        let orient = match self.portrait {
            true => "Set to landscape view",
            false => "Set to portrait view",
        };
        match kind {
            Kind::Notebook => vec![
                (
                    Act::GradePage,
                    "Grade page".to_string(),
                    String::new(),
                ),
                (
                    Act::ConvertText,
                    "Convert to text".to_string(),
                    String::new(),
                ),
                (
                    Act::NotebookSettings,
                    "Notebook settings".to_string(),
                    String::new(),
                ),
                (Act::Search, "Search".to_string(), String::new()),
                (Act::ToggleLandscape, orient.to_string(), String::new()),
                (Act::NewPage, "New page".to_string(), String::new()),
            ],
            Kind::Ebook => vec![
                (
                    Act::NotebookSettings,
                    "Ebook settings".to_string(),
                    String::new(),
                ),
                (Act::Search, "Search".to_string(), String::new()),
                (
                    Act::TextSize,
                    "Text settings".to_string(),
                    self.text_size_label().to_string(),
                ),
                (Act::NewPage, "Add note page".to_string(), String::new()),
            ],
            Kind::Pdf => vec![
                (
                    Act::GradePage,
                    "Grade page".to_string(),
                    String::new(),
                ),
                (Act::PdfSettings, "PDF settings".to_string(), String::new()),
                (
                    Act::AdjustView,
                    "Adjust view".to_string(),
                    self.fit.status().to_string(),
                ),
                (Act::ToggleLandscape, orient.to_string(), String::new()),
                (Act::Search, "Search".to_string(), String::new()),
                (Act::NewPage, "Add note page".to_string(), String::new()),
            ]
            .into_iter()
            .collect(),
        }
    }

    /// 二级菜单（QML `_zoomModes` / `zoomModeFoldout`）。
    /// 官方只有三项，返回主菜单靠再点一次 Adjust view；这里多加一行 Back，
    /// 因为 Act::Back 得有个能点到的行。
    fn submenu_items(&self) -> Vec<(Act, &'static str)> {
        vec![
            (Act::SetFit(Fit::Width), "Fit to width"),
            (Act::SetFit(Fit::Height), "Fit to height"),
            (Act::SetFit(Fit::Current), "Use current view"),
            (Act::Back, "Back"),
        ]
    }

    fn paint_icon(&self, fb: &mut Fb, x: i32, y: i32, act: Act, tone: u8) {
        match act {
            Act::NotebookSettings | Act::PdfSettings => icon_gear(fb, x, y, tone),
            Act::Search => icon_magnifier(fb, x, y, tone),
            Act::NewPage => page_plus(fb, x, y, tone),
            Act::ToggleLandscape => orientation_icon(fb, x, y, self.portrait, tone),
            Act::AdjustView => fit_icon(fb, x, y, self.fit, tone),
            Act::SetFit(f) => fit_icon(fb, x, y, f, tone),
            Act::TextSize => icon_textsize(fb, x, y, tone),
            // 官方 `Icons.icon_a`：一个字母 A（C14）
            Act::ConvertText => icon_letter_a(fb, x, y, tone),
            Act::GradePage => icon_check_box(fb, x, y, tone),
            Act::Back => widgets::back_arrow(fb, x + 8, y + 20, 14, tone),
            Act::Header => {}
        }
    }
}

impl Default for DocSettings {
    fn default() -> Self {
        DocSettings::new()
    }
}

/// 菜单区高度（不含表头、不含底部 2px 边线）。
fn menu_h(kind: Kind, reflow: bool) -> i32 {
    let _ = reflow;
    let n = match kind {
        // 5 = Convert to text / Notebook settings / Search / 横竖 / New page；
        // C12 的 `Grade page` 是第 6 行（插在最上面，面板往上长 120）。
        Kind::Notebook => 6,
        Kind::Ebook => 4,
        // C12 的 `Grade page` 也给了 PDF（批改本来就是"在书上做题再判对错"）
        Kind::Pdf => 6,
    };
    ROW * (n - 1) + ROW - BORDER
}

/// 字号图标：三条长度递减的横线。「字号」在 QML 里是字体图标，这里用几何形状占位。
fn icon_textsize(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    use crate::fb::gray565;
    fb.fill_rect(x + 10, y + 18, 26, 3, gray565(tone));
    fb.fill_rect(x + 10, y + 27, 19, 3, gray565(tone));
    fb.fill_rect(x + 10, y + 36, 12, 3, gray565(tone));
}

/// 「转文字」图标（官方 `Icons.icon_a` 是个字母 A 的字形；这里是几何 A）。
///
/// 两条斜边 + 一道横杠，线宽 4，占满 40×40 那一格（和别的图标一样左边距 30）。
fn icon_letter_a(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    widgets::line(fb, x + 7, y + 38, x + 20, y + 2, 4, tone);
    widgets::line(fb, x + 20, y + 2, x + 33, y + 38, 4, tone);
    widgets::line(fb, x + 12, y + 26, x + 28, y + 26, 4, tone);
}

/// 「批改」图标：一个方框里一个勾（自拟，官方没有 AI 批改这个功能）。
fn icon_check_box(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let (w, h) = (34, 30);
    for t in 0..2 {
        fb.fill_rect(x, y + t, w, 1, gray565(tone));
        fb.fill_rect(x, y + h - 1 - t, w, 1, gray565(tone));
        fb.fill_rect(x + t, y, 1, h, gray565(tone));
        fb.fill_rect(x + w - 1 - t, y, 1, h, gray565(tone));
    }
    // 勾：两段粗线
    let pts = [(x + 7, y + 16), (x + 14, y + 23), (x + 27, y + 8)];
    for seg in pts.windows(2) {
        let ((x0, y0), (x1, y1)) = (seg[0], seg[1]);
        let n = ((x1 - x0).abs().max((y1 - y0).abs())).max(1);
        for i in 0..=n {
            let fx = x0 + (x1 - x0) * i / n;
            let fy = y0 + (y1 - y0) * i / n;
            fb.fill_rect(fx, fy, 3, 3, gray565(tone));
        }
    }
}

/// 字号档位的显示名（和 `reflow::FONT_SIZES` 一一对应）。
pub const TEXT_SIZES: [&str; 5] = ["Small", "Medium", "Large", "X-Large", "XX-Large"];

// ---------------------------------------------------------------- 图标

/// Fit 图标：宽度 = 左右两列点 + 横向双箭头；高度 = 上下两行点 + 竖向双箭头；
/// 当前视图 = 一个方框。官方是字体图标，这里用几何形状占位。
fn fit_icon(fb: &mut Fb, x: i32, y: i32, fit: Fit, tone: u8) {
    let c = gray565(tone);
    match fit {
        Fit::Width => {
            dots_col(fb, x + 4, y + 5, tone);
            dots_col(fb, x + 32, y + 5, tone);
            for i in 0..8 {
                let dy = 7 - i as i32;
                fb.fill_rect(x + 9 + i, y + 20 - dy, 2, 2 * dy, c);
                fb.fill_rect(x + 31 - i, y + 20 - dy, 2, 2 * dy, c);
            }
            fb.fill_rect(x + 14, y + 17, 12, 6, c);
        }
        Fit::Height => {
            dots_row(fb, x + 5, y + 4, tone);
            dots_row(fb, x + 5, y + 32, tone);
            for i in 0..8 {
                let dx = 7 - i as i32;
                fb.fill_rect(x + 20 - dx, y + 9 + i, 2 * dx, 2, c);
                fb.fill_rect(x + 20 - dx, y + 31 - i, 2 * dx, 2, c);
            }
            fb.fill_rect(x + 17, y + 14, 6, 12, c);
        }
        Fit::Current => {
            widgets::outline_rect(fb, x + 8, y + 8, 24, 24, BORDER, tone);
            fb.fill_rect(x + 15, y + 15, 10, 10, c);
        }
    }
}

/// 齿轮（官方 `Icons.icon_cog`，40×40）：细圆环 + 8 个圆齿。
///
/// 截图：环外半径 15.5、环宽 ~3.2，中间孔很大；8 个齿是半径 4 的圆，
/// 圆心落在半径 16 的圆周上，齿顶正好压在 0/39 两行上（整格 40×40 都满）。
/// 工具条/设置页那个 `widgets::icon_gear` 是实心圆盘子（孔只有 8），和这格的
/// 官方图形差得明显，这里单独画。
fn icon_gear(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    widgets::shade_at(widgets::sid("ds_gear"), fb, x, y, 40, 40, tone, 255, |px, py| {
        if widgets::in_ring(px, py, 20.0, 20.0, 15.5, 3.2) {
            return true;
        }
        // 齿是 8 个半径 4 的圆（截图顶齿宽度 4/6/8 逐行变宽，是圆的截面）
        (0..8).any(|k| {
            let a = k as f32 * std::f32::consts::TAU / 8.0;
            widgets::in_disc(px, py, 20.0 + a.cos() * 16.0, 20.0 + a.sin() * 16.0, 4.0)
        })
    });
}

/// 放大镜（官方 `Icons.icon_lens`，40×40）：镜片圆心 (23.5,16)、外半径 16、环宽 4，
/// 手柄从镜片左下沿拉到左下角，线宽 ~4.6（截图 x0..3 / y37..39 那个尖）。
fn icon_magnifier(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let handle = widgets::thick(&[(12.5, 26.5), (1.0, 39.0)], 4.6);
    widgets::shade_at(widgets::sid("ds_magnifier"), fb, x, y, 40, 40, tone, 255, |px, py| {
        widgets::in_ring(px, py, 23.5, 16.0, 16.0, 4.0)
            || widgets::in_poly(px, py, &handle)
    });
}

/// 横竖切换图标（官方 `icon_set_landscape_view` / `icon_set_portrait_view`）。
///
/// 截图里竖屏态画的是「横放的长方形 + 左上方一条点线框」：长方形 x0..39、
/// y14..39（4px 描边）；点线框的顶边 4 个「十字点」（y0..3，x 0/8/16/24）+
/// 左右各一个点（y8..11）——就是被长方形盖住大半的竖版页面轮廓。
/// 横屏态的图标官方没截到，把同一张图顺时针转 90°（目标页面变竖版）。
fn orientation_icon(fb: &mut Fb, x: i32, y: i32, portrait: bool, tone: u8) {
    let id = widgets::sid(if portrait {
        "ds_orient_landscape"
    } else {
        "ds_orient_portrait"
    });
    widgets::shade_at(id, fb, x, y, 40, 40, tone, 255, |px, py| {
        // `portrait` = 现在是竖屏 → 目标横版，就是截图里那张图；
        // 横屏态把同一张图转 90°（采样点先逆变换回原坐标再判）。
        let (bx, by) = if portrait {
            (px, py)
        } else {
            (py, 39.0 - px)
        };
        // 横放的长方形（4px 描边）
        if widgets::in_frame(bx, by, 0.0, 14.0, 40.0, 26.0, 4.0) {
            return true;
        }
        // 上面那条点线框：4 个十字点 + 左右各 1 个
        const DOTS: [(f32, f32); 6] = [
            (0.0, 0.0),
            (8.0, 0.0),
            (16.0, 0.0),
            (24.0, 0.0),
            (0.0, 8.0),
            (24.0, 8.0),
        ];
        DOTS.iter().any(|&(dx, dy)| {
            widgets::in_rect(bx, by, dx + 1.0, dy, 2.0, 4.0)
                || widgets::in_rect(bx, by, dx, dy + 1.0, 4.0, 2.0)
        })
    });
}

/// 加页（官方 `Icons.icon_page_add`）：4px 描边的页面框，右下角挖掉一块放加号。
///
/// 截图像素：上边 0..3 通栏、左边 0..3 通高；右边只画到 y19（下面让给加号竖杠）；
/// 下边只画到 x19；加号横杠 x20..39 / y28..31，竖杠 x28..31 / y20..39。
fn page_plus(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    fb.fill_rect(x, y, 40, 4, c);
    fb.fill_rect(x, y, 4, 40, c);
    fb.fill_rect(x + 36, y, 4, 20, c);
    fb.fill_rect(x, y + 36, 20, 4, c);
    fb.fill_rect(x + 20, y + 28, 20, 4, c);
    fb.fill_rect(x + 28, y + 20, 4, 20, c);
}

fn dots_col(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    for i in 0..5 {
        fb.fill_rect(x, y + i * 8, 4, 4, c);
    }
}

fn dots_row(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    for i in 0..5 {
        fb.fill_rect(x + i * 8, y, 4, 4, c);
    }
}

// ---------------------------------------------------------------- C14 状态条（整页 OCR）

// 点 `Convert to text` 之后整页 OCR 实测要 **115 s**，而原来的反馈只有一条
// 4 s 的 toast（`ocr::PROGRESS` + `notice`）—— 4 秒后屏幕上一点提示都没有，
// 用户只会以为卡死，然后去点第二次。
//
// 这里另起一份**独立**状态（不动 `notice` 的语义：它还是 4 s、还在库里那条
// 通知条上）：转换进行中在屏幕底部固定位置显示一行持久的
// `Converting to text` + 三个点轮流实心（灰度面板，不用颜色），
// 结束/失败换成结果，4 s 后自己消失。

/// 转换中的文案（官方进度串 `Converting to text`，`res_066b1b0.qml:45`）。
pub const OCR_RUNNING: &str = "Converting to text";
/// 成功（和 `ocr::DONE` 同一条官方串）。
pub const OCR_DONE: &str = "Text added";
/// 失败。具体原因（没配对、断网、服务器 5xx）还是走 `notice` 那条 toast，
/// 状态条只给一句"没成"，免得在底部塞一整段英文报错。
pub const OCR_FAILED: &str = "Conversion failed";

/// 结果停留多久（官方 `Values.popupTimeout` = 4 s）。
pub const OCR_DONE_MS: f64 = 4_000.0;
/// 三个点轮流实心的步长（600 ms × 3 = 一圈 1.8 s）。
pub const OCR_DOT_MS: f64 = 600.0;
/// 状态条高。
pub const OCR_H: i32 = 76;
/// 状态条宽（`Converting to text` 28 号 ≈ 364 px + 三个点 + 左右内边距，
/// 实测还余 ~40 px：真机字体宽度略有差别时也不会被截成 `Converting to tex…`）。
const OCR_W: i32 = 520;
/// 左右内边距。
const OCR_PAD: i32 = 24;
/// 三个点的间距（含点的直径 12）。
const OCR_DOT_PITCH: i32 = 20;

/// 状态条在屏幕上的位置：**右下角**，底边离屏幕底 16 px。
///
/// 为什么不贴正中：页脚那行 `Page 3 of 41` 是居中的（基线 1846、右缘 ~810），
/// 这条在 1404 宽的屏上从 x = 836 起，横向完全错开；纵向压着页脚带，
/// 纸上通常也是空白。左下角是工具条（0..120），也不能去。
pub fn ocr_strip_rect(w: i32) -> (i32, i32, i32, i32) {
    let x = (w - 48 - OCR_W).max(panel_x() + 24);
    (x, PANEL_BOTTOM - 16 - OCR_H, OCR_W, OCR_H)
}

/// 转换状态条。`state == None` = 不显示。
///
/// 纯状态机：时间由调用方注入（主循环传 `fb::now_ms()`），单测直接给数，
/// 不用 sleep、也不碰帧缓冲。
#[derive(Clone, Debug, Default)]
pub struct OcrStrip {
    state: Option<Stage>,
    /// 这一趟的目标页 + 开始前文字层有几行：结束时靠"行数变多了吗"判成败。
    target: Option<(String, usize, usize)>,
    /// 上一次重画时的动画步（同一帧里不重复刷屏）。
    shown_step: Option<usize>,
}

#[derive(Clone, Copy, Debug)]
enum Stage {
    Running { start_ms: f64 },
    Done { text: &'static str, at_ms: f64 },
}

impl OcrStrip {
    /// 一趟转换开始。已经在跑就不叠加（返回 `false`，起点/动画相位都不动）。
    pub fn start(&mut self, now_ms: f64, doc: &str, page: usize, before: usize) -> bool {
        if self.running() {
            return false;
        }
        self.state = Some(Stage::Running { start_ms: now_ms });
        self.target = Some((doc.to_string(), page, before));
        self.shown_step = Some(Self::step_from(0.0));
        true
    }

    /// 一趟转换结束：`after` = 目标页现在文字层有几行。
    ///
    /// 返回 `Some(成功?)`；没在跑（没显示过"转换中"）返回 `None` ——
    /// 一次没看见开始的操作不该弹结果。
    pub fn finish(&mut self, now_ms: f64, after: usize) -> Option<bool> {
        if !matches!(self.state, Some(Stage::Running { .. })) {
            return None;
        }
        let (_, _, before) = self.target.take()?;
        let ok = after > before;
        self.state = Some(Stage::Done {
            text: if ok { OCR_DONE } else { OCR_FAILED },
            at_ms: now_ms,
        });
        Some(ok)
    }

    /// 这一帧要画什么：`(文案, Some(第几个点实心))`；`None` = 不画。
    pub fn view(&self, now_ms: f64) -> Option<(&'static str, Option<usize>)> {
        match self.state.as_ref()? {
            Stage::Running { start_ms } => {
                Some((OCR_RUNNING, Some(Self::step_from(now_ms - start_ms))))
            }
            Stage::Done { text, at_ms } => {
                if now_ms - at_ms >= OCR_DONE_MS {
                    None
                } else {
                    Some((text, None))
                }
            }
        }
    }

    /// 每帧一次（主循环 `tick` 里）：动画步进、或者结果到期该消失 → `true`
    /// （调用方把 [`ocr_strip_rect`] 那一块重画 + 局部刷新）。
    pub fn tick(&mut self, now_ms: f64) -> bool {
        match self.state.as_ref() {
            Some(Stage::Running { start_ms }) => {
                let step = Self::step_from(now_ms - start_ms);
                if self.shown_step == Some(step) {
                    return false;
                }
                self.shown_step = Some(step);
                true
            }
            Some(Stage::Done { at_ms, .. }) => {
                if now_ms - at_ms < OCR_DONE_MS {
                    return false;
                }
                self.state = None;
                true
            }
            None => false,
        }
    }

    /// 正在转换。
    pub fn running(&self) -> bool {
        matches!(self.state, Some(Stage::Running { .. }))
    }

    /// 这一趟的目标页 `(doc, page)`：主程序靠它数"开始前 / 结束后"的文字行数。
    pub fn target(&self) -> Option<(&str, usize)> {
        self.target.as_ref().map(|(d, p, _)| (d.as_str(), *p))
    }

    /// 第几个点实心：0/1/2 轮转，负数（时钟被打回）当 0。
    fn step_from(dt_ms: f64) -> usize {
        (dt_ms.max(0.0) / OCR_DOT_MS) as usize % 3
    }
}

/// 画状态条（**只画像素、不刷屏**：刷新交给调用方的 `repaint_rects`）。
///
/// 视觉：白底 + 2 px 黑框 + 黑字。底部那条 toast 是黑底白字，两者一眼就能
/// 分开（那是"通知"，这是"状态"），也不会让纸面上的墨迹透出来。
pub fn paint_ocr_strip(
    fb: &mut Fb,
    text: &mut Text,
    view: (&'static str, Option<usize>),
    w: i32,
) {
    let (label, dots) = view;
    let (x, y, bw, bh) = ocr_strip_rect(w);
    fb.fill_rect(x, y, bw, bh, gray565(BG));
    widgets::outline_rect(fb, x, y, bw, bh, BORDER, INK);

    const SIZE: f32 = 28.0;
    // 右边留给三个点（3 格 × 20 px）+ 和文字之间的 16 px
    let max_w = bw - 2 * OCR_PAD - 3 * OCR_DOT_PITCH - 16;
    let shown = text.ellipsize(SIZE, label, max_w);
    text.draw(fb, x + OCR_PAD, y + 50, SIZE, &shown, INK);

    let Some(active) = dots else { return };
    let tw = text.width(SIZE, &shown) as i32;
    let cy = y + 39;
    let mut cx = x + OCR_PAD + tw + 16 + 6;
    for i in 0..3 {
        if i == active {
            // 实心：正在走的那一格
            widgets::disc(fb, cx, cy, 6, INK);
        } else {
            // 空心：灰度面板上不用颜色区分，只靠实心/空心
            widgets::ring(fb, cx, cy, 6, 2, INK);
        }
        cx += OCR_DOT_PITCH;
    }
}

// ---------------------------------------------------------------- 文字折行

/// 表头标题按面板宽度折行，最多 3 行，超出补 `…`
/// （QML `maximumLineCount: 3` + `ElideRight`）。
///
/// `rows()` 是纯布局，拿不到字体；这里用 Noto Sans 的字宽表（单位 1/1000 em）
/// 估算，中文按全角 1em。paint 时还会按真实字形再 elide 一次兜底。
///
/// 两个和"官方看起来一样"有关的细节：
/// * 标题是**粗体**，而 `ADV` 表是常规体的字宽，粗体实测约为它的 0.78——
///   不补这个系数，折出来的行会比实际能放下的短一截（PDF 那个长标题会从
///   `Comparison Oracles-` 缩成 `Comparison`）。
/// * Qt 允许在**连字符**处断行（`res_067097b.qml` 的 `elide: ElideRight` +
///   默认 `Text.WordWrap`）。所以先把单词按 `-` 拆成 token，再逐 token 贪心填行；
///   跟在连字符后面的 token 直接接上、不加空格。
fn wrap_title(title: &str, size: f32, max_w: f32) -> Vec<String> {
    let max_w = max_w / TITLE_BOLD_W;
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut truncated = false;
    'outer: for word in title.split_whitespace() {
        for tok in hyphen_tokens(word) {
            let cand = if cur.is_empty() {
                tok.clone()
            } else if cur.ends_with('-') {
                format!("{cur}{tok}")
            } else {
                format!("{cur} {tok}")
            };
            if text_w(&cand, size) <= max_w {
                cur = cand;
                continue;
            }
            if !cur.is_empty() {
                lines.push(std::mem::take(&mut cur));
            }
            if text_w(&tok, size) <= max_w {
                cur = tok;
            } else {
                // 单个 token 就放不下一行（中文标题没有空格）：按字符硬折
                for ch in tok.chars() {
                    let mut next = cur.clone();
                    next.push(ch);
                    if text_w(&next, size) > max_w && !cur.is_empty() {
                        lines.push(std::mem::take(&mut cur));
                    }
                    cur.push(ch);
                }
            }
            if lines.len() >= 3 {
                truncated = true;
                break 'outer;
            }
        }
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    if lines.len() > 3 {
        lines.truncate(3);
        truncated = true;
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    if truncated {
        let last = lines.last_mut().expect("刚补过空行");
        while !last.is_empty() && text_w(last, size) + text_w("…", size) > max_w {
            last.pop();
        }
        last.push('…');
    }
    lines
}

/// 粗体标题的实际宽度 / 常规体字宽表（同一 em）。实测 `NotoSans-Bold.ttf`
/// 在 `size=33` 时是字宽表的 0.775~0.791 倍。
const TITLE_BOLD_W: f32 = 0.78;

/// 按连字符把单词拆成可断行的 token（连字符留在前一个 token 上）：
/// `Oracles-based` → `["Oracles-", "based"]`。没有连字符就原样一个 token。
fn hyphen_tokens(word: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in word.chars() {
        cur.push(ch);
        if ch == '-' {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// 估算一行文字的像素宽。字号参数是"官方 QML 字号"，
/// 实际 em = size × 1.4（同 `ui::text` 的换算）。
fn text_w(s: &str, size: f32) -> f32 {
    let em = size * 1.4;
    s.chars().map(|c| adv(c) * em / 1000.0).sum()
}

/// 单个字符的宽度（Noto Sans SC，1/1000 em）。ASCII 来自字体度量表，
/// 全角按 1000，其余按半角 500 兜底。
fn adv(c: char) -> f32 {
    let i = c as u32;
    if (32..127).contains(&i) {
        ADV[(i - 32) as usize] as f32
    } else if i >= 0x2e80 {
        1000.0
    } else {
        500.0
    }
}

/// `NotoSansSC.otf` 里 ASCII 32..126 的 advance（1/1000 em，直接量出来的）。
#[rustfmt::skip]
const ADV: [u16; 95] = [
    224, 323, 474, 555, 555, 921, 680, 278, 338, 338, 467,
    555, 278, 347, 278, 392, 555, 555, 555, 555, 555, 555,
    555, 555, 555, 555, 278, 278, 555, 555, 555, 474, 946,
    608, 657, 638, 688, 589, 552, 689, 728, 293, 535, 646,
    543, 812, 723, 742, 633, 742, 635, 596, 599, 721, 575,
    878, 573, 531, 603, 338, 392, 338, 555, 559, 606, 563,
    618, 510, 620, 554, 325, 564, 607, 275, 275, 552, 284,
    926, 610, 606, 620, 620, 388, 468, 377, 607, 521, 802,
    498, 521, 475, 338, 270, 338, 555,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// 官方笔记本截图里的文档（单行标题）。
    const NB_TITLE: &str = "Quick sheets";
    /// 官方 PDF 截图里的长标题（折 3 行）。
    const PDF_TITLE: &str =
        "Function-free Optimization via Comparison Oracles-based Derivative-Free Algorithms";

    fn nb(d: &DocSettings) -> Vec<Row> {
        d.rows(Kind::Notebook, NB_TITLE, 3, 3)
    }
    fn eb(d: &DocSettings) -> Vec<Row> {
        d.rows(Kind::Ebook, "EPUB sample", 1, 3)
    }
    fn pdf(d: &DocSettings) -> Vec<Row> {
        d.rows(Kind::Pdf, PDF_TITLE, 4, 41)
    }
    fn find(rows: &[Row], act: Act) -> Row {
        rows.iter().find(|r| r.act == act).expect("缺少这一行").clone()
    }
    fn mid(r: &Row) -> (f32, f32) {
        ((r.x + r.w / 2) as f32, (r.y + r.h / 2) as f32)
    }
    fn opened(kind: Kind) -> DocSettings {
        let mut d = DocSettings::new();
        d.set_title(match kind {
            Kind::Notebook => NB_TITLE,
            Kind::Ebook => "EPUB sample",
            Kind::Pdf => PDF_TITLE,
        });
        d.toggle(kind);
        d
    }

    /// 电子书面板：官方 `res_067097b.qml:133-150` 就 4 行，
    /// 而且**没有** Adjust view / 方向切换（那是 PDF 专有）。
    #[test]
    fn ebook_menu_is_the_four_official_rows() {
        let d = opened(Kind::Ebook);
        let rows = eb(&d);
        let body: Vec<&Row> = rows.iter().filter(|r| r.act != Act::Header).collect();
        let acts: Vec<Act> = body.iter().map(|r| r.act).collect();
        assert_eq!(
            acts,
            vec![
                Act::NotebookSettings,
                Act::Search,
                Act::TextSize,
                Act::NewPage
            ]
        );
        let labels: Vec<&str> = body.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["Ebook settings", "Search", "Text settings", "Add note page"]
        );
        assert!(!rows.iter().any(|r| matches!(r.act, Act::AdjustView)));
    }

    /// 笔记本菜单：表头 + 6 项。前四项是官方截图顺序；`Grade page`（C12）和
    /// `Convert to text`（C14）都插在**最上面**，所以官方那四行的 y 不变。
    #[test]
    fn notebook_menu_order() {
        let d = opened(Kind::Notebook);
        let rows = nb(&d);
        let acts: Vec<Act> = rows.iter().map(|r| r.act).collect();
        assert_eq!(
            acts,
            vec![
                Act::Header,
                Act::GradePage,
                Act::ConvertText,
                Act::NotebookSettings,
                Act::Search,
                Act::ToggleLandscape,
                Act::NewPage,
            ]
        );
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            vec![
                "Quick sheets",
                "Grade page",
                "Convert to text",
                "Notebook settings",
                "Search",
                "Set to landscape view",
                "New page",
            ]
        );
        assert_eq!(rows[0].subtext, "Page 3 of 3");
    }

    /// C14 入口：官方串 `Convert to text`（`res_067dfe1.qml:174`），
    /// 点一下返回 `Act::ConvertText`，面板收起来（和别的菜单行一样）。
    #[test]
    fn convert_to_text_row_is_official_label() {
        let mut d = opened(Kind::Notebook);
        let row = find(&nb(&d), Act::ConvertText);
        assert_eq!(row.label, "Convert to text");
        assert_eq!((row.x, row.w, row.h), (panel_x(), 495, 120));
        let (x, y) = mid(&row);
        assert_eq!(d.click(Kind::Notebook, x, y), Some(Act::ConvertText));
        assert!(!d.is_open());
        // PDF / 电子书面板没有这一行（`.rm` 文字层只给笔记本）
        assert!(!pdf(&opened(Kind::Pdf))
            .iter()
            .any(|r| r.act == Act::ConvertText));
        assert!(!eb(&opened(Kind::Ebook))
            .iter()
            .any(|r| r.act == Act::ConvertText));
    }

    /// PDF 菜单：表头 + 5 项，Adjust view 在第二行，带状态小字。
    #[test]
    fn pdf_menu_order() {
        let d = opened(Kind::Pdf);
        let rows = pdf(&d);
        let acts: Vec<Act> = rows.iter().map(|r| r.act).collect();
        assert_eq!(
            acts,
            vec![
                Act::Header,
                Act::GradePage,
                Act::PdfSettings,
                Act::AdjustView,
                Act::ToggleLandscape,
                Act::Search,
                Act::NewPage,
            ]
        );
        // 表头那一行存的是**折行后**的标题（用 \n 连起来），所以这里只看条目行
        let labels: Vec<&str> = rows
            .iter()
            .filter(|r| r.act != Act::Header)
            .map(|r| r.label.as_str())
            .collect();
        assert_eq!(
            labels,
            vec![
                "Grade page",
                "PDF settings",
                "Adjust view",
                "Set to landscape view",
                "Search",
                "Add note page",
            ]
        );
        assert_eq!(find(&rows, Act::AdjustView).subtext, "Width");
    }

    /// 6e：`PdfSettings` 的行为从"弹 toast"改成"打开 Notebook settings 那个 sheet"，
    /// PDF 面板自己的行一项都不能变——除 C12 插在最上面的 `Grade page` 之外，
    /// 第一个官方行仍然是 `PDF settings`。
    #[test]
    fn pdf_settings_row_is_unchanged() {
        let mut d = opened(Kind::Pdf);
        let rows = pdf(&d);
        let first = rows
            .iter()
            .find(|r| r.act != Act::Header && r.act != Act::GradePage)
            .expect("少条目行");
        assert_eq!(first.act, Act::PdfSettings);
        assert_eq!(first.label, "PDF settings");
        assert_eq!(first.h, 120);
        // 点它必须还是这个动作（main.rs 拿它去开 sheet，不是 toast）
        assert_eq!(d.click(Kind::Pdf, mid(first).0, mid(first).1), Some(Act::PdfSettings));
    }

    /// 真机实测几何：表头 163（1 行标题）/ 309（3 行标题），行高 120、末行 118，
    /// 宽 495，图标 x=150、文字 x=205。
    ///
    /// 笔记本多出来的 `Convert to text`（C14）在最上面：面板整体上沿抬高一行，
    /// 官方截图里那四行（`Notebook settings` 起）的 y 仍然逐像素对得上。
    #[test]
    fn measured_geometry() {
        let d = opened(Kind::Notebook);
        let rows = nb(&d);
        let hs: Vec<i32> = rows.iter().map(|r| r.h).collect();
        assert_eq!(hs, vec![163, 120, 120, 120, 120, 120, 118]);
        // 面板底边贴齐 1870、往上长：C12 的 `Grade page` 插在最上面，
        // 所以官方那四行的 y（1392/1512/1632/1752）一个都没动。
        assert_eq!(rows[0].y, 989);
        assert_eq!(rows[1].y, 1152); // Grade page（C12）
        assert_eq!(rows[2].y, 1272); // Convert to text
        assert_eq!(rows[3].y, 1392); // Notebook settings（官方截图 1392）
        assert_eq!(rows[4].y, 1512); // Search
        assert_eq!(rows[5].y, 1632); // 横竖
        assert_eq!(rows[6].y, 1752); // New page
        assert_eq!(rows[6].y + rows[6].h, 1870);
        assert_eq!(d.rect(Kind::Notebook), Some((120, 989, 495, 883)));
        for r in rows.iter().skip(1) {
            assert_eq!((r.x, r.w), (panel_x(), 495));
        }
        assert_eq!(icon_x(), 150);
        assert_eq!(text_x(), 205);

        let d = opened(Kind::Pdf);
        let rows = pdf(&d);
        let hs: Vec<i32> = rows.iter().map(|r| r.h).collect();
        assert_eq!(hs, vec![309, 120, 120, 120, 120, 120, 118]);
        // C12 的 `Grade page` 也插在 PDF 菜单最上面 → 面板再往上长 120
        assert_eq!(rows[0].y, 843);
        assert_eq!(rows[1].y, 1152); // Grade page
        assert_eq!(rows[2].y, 1272); // PDF settings（官方 y 不变）
        assert_eq!(d.rect(Kind::Pdf), Some((120, 843, 495, 1029)));
        // 长标题折成 3 行
        assert_eq!(rows[0].label.lines().count(), 3);
    }

    /// 横竖文案：点了之后切到 "Set to portrait view"，再点回来。
    #[test]
    fn landscape_label_switches() {
        let mut d = opened(Kind::Notebook);
        let row = find(&nb(&d), Act::ToggleLandscape);
        assert_eq!(row.label, "Set to landscape view");
        let (x, y) = mid(&row);
        assert_eq!(d.click(Kind::Notebook, x, y), Some(Act::ToggleLandscape));
        // 主菜单里点一项就关面板（QML `foldout.visible = id === "adjustView"`）
        assert!(!d.is_open());
        d.toggle(Kind::Notebook);
        assert_eq!(
            find(&nb(&d), Act::ToggleLandscape).label,
            "Set to portrait view"
        );
        let row = find(&nb(&d), Act::ToggleLandscape);
        let (x, y) = mid(&row);
        assert_eq!(d.click(Kind::Notebook, x, y), Some(Act::ToggleLandscape));
        d.toggle(Kind::Notebook);
        assert_eq!(
            find(&nb(&d), Act::ToggleLandscape).label,
            "Set to landscape view"
        );
    }

    /// 二级菜单：点 Adjust view 展开（反白），点 Back 回主菜单，面板仍开着。
    #[test]
    fn submenu_enter_and_back() {
        let mut d = opened(Kind::Pdf);
        let adj = find(&pdf(&d), Act::AdjustView);
        let (x, y) = mid(&adj);
        assert_eq!(d.click(Kind::Pdf, x, y), None); // 只改面板状态
        let rows = pdf(&d);
        // 主菜单 7 行 + 二级 4 行
        assert_eq!(rows.len(), 11);
        assert!(find(&rows, Act::AdjustView).inverted);
        let fits: Vec<(Act, String)> = rows
            .iter()
            .filter(|r| matches!(r.act, Act::SetFit(_)))
            .map(|r| (r.act, r.label.clone()))
            .collect();
        assert_eq!(
            fits,
            vec![
                (Act::SetFit(Fit::Width), "Fit to width".to_string()),
                (Act::SetFit(Fit::Height), "Fit to height".to_string()),
                (Act::SetFit(Fit::Current), "Use current view".to_string()),
            ]
        );
        // 二级菜单贴着主菜单右缘，从 Adjust view 那一行的顶上开始；
        // 三个选项各 120，Back 排在它们下面（第 4 格）
        let first = rows
            .iter()
            .find(|r| matches!(r.act, Act::SetFit(_)))
            .unwrap();
        assert_eq!((first.x, first.y, first.w), (sub_x(), adj.y, 495));
        let back = find(&rows, Act::Back);
        assert_eq!((back.x, back.y, back.w), (sub_x(), adj.y + 3 * 120, 495));
        let (x, y) = mid(&back);
        assert_eq!(d.click(Kind::Pdf, x, y), Some(Act::Back));
        let rows = pdf(&d);
        assert_eq!(rows.len(), 7); // 主菜单（表头 + Grade page + 5 项）
        assert!(!find(&rows, Act::AdjustView).inverted);
        assert!(d.is_open());
    }

    /// 选一个缩放模式：回主菜单、状态字跟着变。
    #[test]
    fn submenu_pick_fit() {
        let mut d = opened(Kind::Pdf);
        let adj = find(&pdf(&d), Act::AdjustView);
        let (x, y) = mid(&adj);
        d.click(Kind::Pdf, x, y);
        let h = find(&pdf(&d), Act::SetFit(Fit::Height));
        let (x, y) = mid(&h);
        assert_eq!(d.click(Kind::Pdf, x, y), Some(Act::SetFit(Fit::Height)));
        let rows = pdf(&d);
        assert_eq!(rows.len(), 7); // 回到主菜单（表头 + Grade page + 5 项）
        assert_eq!(find(&rows, Act::AdjustView).subtext, "Height");
        assert!(d.is_open());
        // 再点一次 Adjust view 能再展开，且状态字跟着模式走
        let adj = find(&rows, Act::AdjustView);
        let (x, y) = mid(&adj);
        d.click(Kind::Pdf, x, y);
        assert!(find(&pdf(&d), Act::AdjustView).inverted);
        let c = find(&pdf(&d), Act::SetFit(Fit::Current));
        let (x, y) = mid(&c);
        assert_eq!(d.click(Kind::Pdf, x, y), Some(Act::SetFit(Fit::Current)));
        assert_eq!(find(&pdf(&d), Act::AdjustView).subtext, "Custom");
    }

    /// 关着时 rect() 是 None。
    #[test]
    fn rect_is_none_when_closed() {
        let d = DocSettings::new();
        assert_eq!(d.rect(Kind::Notebook), None);
        assert_eq!(d.rect(Kind::Pdf), None);
        assert!(!d.is_open());
    }

    /// 点在面板外：返回 None，且状态一点没动。
    #[test]
    fn click_outside_keeps_state() {
        let mut d = opened(Kind::Notebook);
        let before = nb(&d);
        let rect = d.rect(Kind::Notebook);
        // 工具条上（面板左）、面板右边、屏上方
        assert_eq!(d.click(Kind::Notebook, 119.0, 1400.0), None);
        assert_eq!(d.click(Kind::Notebook, 616.0, 1400.0), None);
        assert_eq!(d.click(Kind::Notebook, 300.0, 100.0), None);
        // 表头也不吃点击（表头现在是 989..1152；下面一行是 Grade page，别点到它）
        assert_eq!(d.click(Kind::Notebook, 300.0, 1050.0), None);
        assert!(d.is_open());
        assert_eq!(nb(&d), before);
        assert_eq!(d.rect(Kind::Notebook), rect);
    }

    /// 右边界不算命中；左边界算。
    #[test]
    fn right_edge_is_not_a_hit() {
        let row = find(&nb(&opened(Kind::Notebook)), Act::NotebookSettings);
        let y = (row.y + 60) as f32;
        let mut d = opened(Kind::Notebook);
        assert_eq!(d.click(Kind::Notebook, 614.9, y), Some(Act::NotebookSettings));
        let mut d = opened(Kind::Notebook);
        assert_eq!(d.click(Kind::Notebook, 615.0, y), None);
        assert!(d.is_open());
        assert_eq!(d.click(Kind::Notebook, 120.0, y), Some(Act::NotebookSettings));
        // 最后一行到 1870 为止，下面 2px 是底边，不算命中
        let mut d = opened(Kind::Notebook);
        assert_eq!(d.click(Kind::Notebook, 300.0, 1870.0), None);
        assert_eq!(d.click(Kind::Notebook, 300.0, 1869.9), Some(Act::NewPage));
        assert!(!d.is_open());
    }

    /// 所有 Row 互不重叠，且都在屏内（含二级菜单展开）。
    #[test]
    fn rows_tile_the_panel() {
        for kind in [Kind::Notebook, Kind::Pdf] {
            for submenu in [false, true] {
                let mut d = opened(kind);
                if submenu {
                    let adj = find(&pdf(&d), Act::AdjustView);
                    let (x, y) = mid(&adj);
                    d.click(Kind::Pdf, x, y);
                }
                let rows = match kind {
                    Kind::Notebook => nb(&d),
                    Kind::Ebook => eb(&d),
                    Kind::Pdf => pdf(&d),
                };
                for r in &rows {
                    assert!(r.x >= 0 && r.y >= 0);
                    assert!(r.x + r.w <= 1404 && r.y + r.h <= 1872);
                    assert_eq!(r.w, 495);
                }
                for (i, a) in rows.iter().enumerate() {
                    for b in rows.iter().skip(i + 1) {
                        let overlap = a.x < b.x + b.w
                            && b.x < a.x + a.w
                            && a.y < b.y + b.h
                            && b.y < a.y + a.h;
                        assert!(!overlap, "{:?} 和 {:?} 叠了", a.act, b.act);
                    }
                }
            }
        }
    }

    /// toggle 往返：开 → 关 → 再开总是回主菜单。
    #[test]
    fn toggle_roundtrip() {
        let mut d = DocSettings::new();
        d.toggle(Kind::Pdf);
        assert!(d.is_open());
        assert!(d.rect(Kind::Pdf).is_some());
        // 先展开二级菜单
        let adj = find(&pdf(&d), Act::AdjustView);
        let (x, y) = mid(&adj);
        d.click(Kind::Pdf, x, y);
        // 主菜单 7 行（表头 + Grade page + 5 项）+ 二级 4 项 = 11
        assert_eq!(pdf(&d).len(), 11);
        d.toggle(Kind::Pdf);
        assert!(!d.is_open());
        assert_eq!(d.rect(Kind::Pdf), None);
        d.toggle(Kind::Pdf);
        assert!(d.is_open());
        assert_eq!(pdf(&d).len(), 7); // 再开是主菜单（表头 + Grade page + 5 项）
        d.close();
        assert!(!d.is_open());
    }

    /// 只要标题够长就会截断成 3 行（末尾有省略号），表头随之变高。
    #[test]
    fn long_title_is_clipped_to_three_lines() {
        let mut d = DocSettings::new();
        let long = "A ".repeat(200);
        let rows = d.rows(Kind::Notebook, &long, 1, 2);
        assert_eq!(rows[0].label.lines().count(), 3);
        assert!(rows[0].label.ends_with('…'));
        assert_eq!(rows[0].h, HEAD_BASE + 3 * HEAD_LINE);
        // 空标题也占一行
        let rows = d.rows(Kind::Pdf, "", 1, 1);
        assert_eq!(rows[0].h, 163);
        d.set_title(&long);
        d.toggle(Kind::Notebook);
        let (_, y, _, h) = d.rect(Kind::Notebook).unwrap();
        // 3 行标题 309 + 6 行菜单（5×120+118=718）+ 2px 底边
        assert_eq!(h, 163 - 90 + 90 + 2 * 73 + 718 + 2);
        assert_eq!(y, 1872 - h);
    }

    /// 中文长标题没有空格，按字符硬折。
    #[test]
    fn cjk_title_wraps_by_char() {
        let rows = DocSettings::new().rows(
            Kind::Notebook,
            "基于比较预言机的无导数优化方法研究报告",
            1,
            1,
        );
        let n = rows[0].label.lines().count();
        assert!((2..=3).contains(&n), "中文应当折行，实际 {} 行", n);
    }

    /// Fit 的状态字照 QML `_zoomModes[*].status`。
    #[test]
    fn fit_status_text() {
        assert_eq!(Fit::Width.status(), "Width");
        assert_eq!(Fit::Height.status(), "Height");
        assert_eq!(Fit::Current.status(), "Custom");
    }

    // ---- C14 状态条：进行中 → 成功 → 4 s 后消失 --------------------------

    /// 一趟成功的转换：进行中（带点动画）→ `Text added` → 4 s 后消失。
    #[test]
    fn ocr_strip_runs_then_succeeds_then_expires() {
        let mut s = OcrStrip::default();
        assert!(!s.running());
        assert_eq!(s.view(0.0), None, "没开始就什么都不画");

        assert!(s.start(0.0, "doc-a", 0, 3), "第一趟该是新起的");
        assert!(s.running());
        assert_eq!(s.target(), Some(("doc-a", 0)));
        assert_eq!(s.view(0.0), Some((OCR_RUNNING, Some(0))));

        // 600 ms 换下一个点（要重画），同一帧里不重复重画
        assert!(!s.tick(OCR_DOT_MS - 1.0));
        assert!(s.tick(OCR_DOT_MS), "动画步进要重画");
        assert_eq!(s.view(OCR_DOT_MS).unwrap().1, Some(1));
        assert_eq!(s.view(2.0 * OCR_DOT_MS).unwrap().1, Some(2));
        assert_eq!(s.view(3.0 * OCR_DOT_MS).unwrap().1, Some(0), "三个点轮转");
        assert!(!s.tick(OCR_DOT_MS + 10.0), "同一格不该反复刷屏");

        // 结束：文字层 3 行 → 5 行 = 成功
        assert_eq!(s.finish(100_000.0, 5), Some(true));
        assert!(!s.running());
        assert_eq!(s.target(), None);
        assert_eq!(s.view(100_000.0), Some((OCR_DONE, None)));

        // 4 s 之内一直挂着，到点才消失
        assert_eq!(
            s.view(100_000.0 + OCR_DONE_MS - 1.0),
            Some((OCR_DONE, None))
        );
        assert!(!s.tick(100_000.0 + OCR_DONE_MS - 1.0));
        assert!(s.tick(100_000.0 + OCR_DONE_MS), "到期要重画一次（擦掉）");
        assert_eq!(s.view(100_000.0 + OCR_DONE_MS), None);
        // 消失之后再 tick 不该再要重画
        assert!(!s.tick(200_000.0));
    }

    /// 失败：文字层一行都没多，换成 `Conversion failed`，同样 4 s 后消失。
    #[test]
    fn ocr_strip_failure_shows_failed() {
        let mut s = OcrStrip::default();
        s.start(0.0, "doc-a", 2, 4);
        assert_eq!(s.finish(1_000.0, 4), Some(false), "行数没变就是失败");
        assert_eq!(s.view(1_000.0), Some((OCR_FAILED, None)));
        assert!(s.tick(1_000.0 + OCR_DONE_MS));
        assert_eq!(s.view(1_000.0 + OCR_DONE_MS), None);
        // 空文字层也算失败（服务器回了空串）
        let mut s = OcrStrip::default();
        s.start(0.0, "doc-a", 0, 0);
        assert_eq!(s.finish(50.0, 0), Some(false));
        assert_eq!(s.view(50.0), Some((OCR_FAILED, None)));
    }

    /// 重复点 `Convert to text`：只有一条状态条，起点和动画相位都不动。
    #[test]
    fn ocr_strip_does_not_stack_on_repeated_clicks() {
        let mut s = OcrStrip::default();
        assert!(s.start(0.0, "doc-a", 0, 1));
        assert!(!s.start(5_000.0, "doc-a", 0, 1), "同一趟不该再开一条");
        assert!(s.running());
        // 起点没被重置：5 s / 600 ms = 8，8 % 3 = 2
        assert_eq!(s.view(5_000.0).unwrap().1, Some(2), "动画相位不能被点回去");
        // 连换一本也不叠加（单飞任务在 D5 那边已经挡住了）
        assert!(!s.start(6_000.0, "doc-b", 9, 0));
        assert_eq!(s.target(), Some(("doc-a", 0)));
        // 结束之后再点一次是全新一趟
        s.finish(7_000.0, 2);
        assert!(s.start(8_000.0, "doc-b", 9, 0));
        assert_eq!(s.target(), Some(("doc-b", 9)));
        assert_eq!(s.view(8_000.0), Some((OCR_RUNNING, Some(0))));
    }

    /// 没显示过"转换中"就不该弹结果（比如 gate 直接挡了、任务没起来）。
    #[test]
    fn ocr_strip_stays_quiet_without_a_start() {
        let mut s = OcrStrip::default();
        assert_eq!(s.finish(0.0, 99), None);
        assert_eq!(s.view(0.0), None);
        assert!(!s.tick(0.0));
    }

    /// 状态条在屏幕内，而且横向不和居中的页码打架。
    #[test]
    fn ocr_strip_rect_keeps_out_of_the_page_number() {
        for w in [1404, 1872] {
            let (x, y, bw, bh) = ocr_strip_rect(w);
            assert!(x >= panel_x(), "别压住左边工具条：x={x}");
            assert!(x + bw <= w, "别出屏：{x}+{bw} > {w}");
            assert!(y >= 0 && y + bh <= PANEL_BOTTOM, "底边要留在屏内：{y}+{bh}");
            // 页码居中，右缘大约在 w/2 + 100；这条从 w/2 + 100 往右才开始
            assert!(x >= w / 2 + 100, "和居中的页码打架：x={x}, w={w}");
        }
    }
    // === C17-SCRATCH begin ===
    #[test]
    fn c17_scratch_click_vs_paint() {
        let kind = Kind::Pdf;
        let title = PDF_TITLE;
        let d0 = opened(kind);
        let painted = d0.rows(kind, title, 1, 3);
        eprintln!("-- 画的空间（paint）:");
        for r in &painted { eprintln!("   y={} h={} {:?}", r.y, r.h, r.act); }
        eprintln!("-- 命中的空间（click 内部用 \"\"）:");
        for r in d0.rows(kind, "", 1, 1) { eprintln!("   y={} h={} {:?}", r.y, r.h, r.act); }
        for r in &painted {
            let mut d = opened(kind);
            let (cx, cy) = mid(r);
            let got = d.click(kind, cx, cy);
            match got {
                Some(a) if a == r.act => {}
                None if r.act == Act::Header || r.act == Act::AdjustView => {}
                other => eprintln!("!!! 画在 y={} ({:?}) 中心 {:?} → 命中 {:?}", r.y, r.act, (cx, cy), other),
            }
        }
    }    #[test]
    fn c17_scratch_rows_click_space() {
        let mut d = opened(Kind::Notebook);
        d.set_title(NB_TITLE);
        let painted = d.rows(Kind::Notebook, NB_TITLE, 1, 3);
        let clicky = d.rows(Kind::Notebook, "", 1, 1);
        eprintln!("wrap_title(empty) = {:?}", wrap_title("", FONT_TITLE, WRAP_W as f32));
        eprintln!("wrap_title(nb) = {:?}", wrap_title(NB_TITLE, FONT_TITLE, WRAP_W as f32));
        for r in &painted { eprintln!("画: y={} h={} {:?}", r.y, r.h, r.act); }
        eprintln!("---");
        for r in &clicky { eprintln!("命中空间: y={} h={} {:?}", r.y, r.h, r.act); }
    }

}
