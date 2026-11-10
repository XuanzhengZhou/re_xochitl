//! 笔记本左侧竖工具条，照官方 `Toolbar.qml` 重做（1:1）。
//!
//! 官方实测（`shots/official/tb-*.png` 逐像素量的）：
//! 条宽 **120**，格子 **120×120**，图标 40×40 居中于 x=60；底色纯白、**没有右边框**
//! （和纸面同色，全靠图标区分）。
//!
//! 槽位（y 从 0 往下）：
//! ```text
//!   0..120      收起 / 展开（圆环 + 实心点）
//!   120..240    当前笔（笔尖图标）
//!   240..360    笔画粗细 / 颜色（三条不同粗细的横线）
//!   360..480    橡皮（虚线框 + 斜放的橡皮）
//!   480..600    选区（四角方框 + 短划）
//!   600..720    撤销
//!   720..840    重做
//!   840..960    框选问 AI（闭合框 + 四角星，**第 13 格**，官方没有）
//!   960..1272   空
//!   1272..1872  页面总览 / 标签 / 图层 / 分享 / 文档设置
//! ```
//!
//! 弹出菜单：x=120..520（宽 400），**顶边对齐它所属格子的顶边**，白底 + 2px 黑边，
//! 行高 120、表头文字带 43 + 分隔线 2，激活行整行黑底反白。
//!
//! 官方是两段式点击：点一下只"选中工具"，**已经是当前工具了再点一下**才展开/收起菜单。
//! 这个状态机在 main.rs 的 `on_chrome` 里；本文件只负责 `hit()` 返回对的 `Msg`，
//! 以及按 `Chrome` 的字段把反白画对。

use crate::doc::rm::Layer;
use crate::fb::{gray565, Fb};
use crate::pen::line::Color;
use crate::pen::{pencil, Line, Tool};
use crate::ui::widgets;

pub const W: i32 = 120;
/// **竖屏**一格的边长（横屏见 [`Geom`]）。
const BTN: i32 = 120;
const ICON: i32 = 40;

/// 工具条的纵向栅格。
///
/// 竖屏是官方那套：一格 120，收起 + 上 6 格 + AI 两格，底部 5 格贴底
/// （1872 - 5*120 = 1272）。横屏时逻辑屏高只有 1404，13 格 ×120 装不下，
/// 所以格子缩成 `1404/15 = 93`（竖屏那 15 格的顺序一个不少：收起 / 6 工具 /
/// AI / 引用 / 横屏 / 底部 5），底部 5 格仍贴底。
///
/// 只有**纵向**跟着方向变；横向（条宽 120、菜单 x=120、行高 120）不受影响，
/// 所以菜单、弹出面板的横向几何一行都不用改。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geom {
    /// 一格的高度。
    pub cell: i32,
    /// 第 2 格（笔）的顶边：第 1 格是收起按钮，占 `0..cell`。
    pub top0: i32,
    /// 第 13 格（框选问 AI）的顶边。
    pub ai_y: i32,
    /// 第 15 格（横屏开关）的顶边：第 14 格是 AI 引用列表，紧挨着它。
    pub land_y: i32,
    /// 底部 5 格的第一格顶边。
    pub bottom0: i32,
}

pub fn geom() -> Geom {
    let (_, h) = crate::fb::logical();
    let cell = if crate::fb::landscape() { h / 15 } else { BTN };
    let top0 = cell;
    let ai_y = top0 + 6 * cell;
    Geom {
        cell,
        top0,
        ai_y,
        land_y: ai_y + 2 * cell,
        bottom0: h - 5 * cell,
    }
}

/// 第 13 格（框选问 AI）的顶边。
///
/// 官方是"上 7 + 下 5"=12 格，中间那一段空着。新格子就落在空段的第一格。
pub fn ai_y() -> i32 {
    geom().ai_y
}

/// 面板左边缘 = 工具条右边。
pub const MENU_X: i32 = 120;
/// 面板宽（官方 400）。
pub const MENU_W: i32 = 400;
pub const MENU_ROW: i32 = 120;
pub const MENU_SEP: i32 = 2;

/// 工具条那一列的左边缘。左利手时整列贴到屏幕右边（官方就是这么换边的）。
pub fn band_x() -> i32 {
    crate::fb::flip_x(0, W)
}

/// 弹出面板的左边缘：右利手贴工具条右边，左利手贴工具条左边（往回收）。
pub fn menu_x() -> i32 {
    crate::fb::flip_x(MENU_X, MENU_W)
}

/// 这个横坐标是不是落在工具条那一列里。
pub fn in_band(x: i32) -> bool {
    (band_x()..band_x() + W).contains(&x)
}
/// `menu_rect()` 四周多给的余量（**只影响重绘范围**，不影响命中/绘制）。
///
/// 见 `menu_rect()` 的注释：文字溢出面板时，收起菜单只刷面板本体，溢出的
/// 那几像素就永远留在纸上。留一圈余量把这类"画到框外"的东西一起擦掉。
pub const MENU_RECT_PAD: i32 = 12;
/// 右边再多留一点：标签长过面板只会往右溢（历史上漏掉的就是这一侧）。
pub const MENU_RECT_EXTRA_R: i32 = 20;
/// 表头文字带高度（下面还有 2px 分隔线）。
pub const MENU_HEAD: i32 = 43;
/// 行内图标左边缘（相对面板左边）。
pub const MENU_ICON_DX: i32 = 30;
/// 行内文字左边缘（相对面板左边）。官方 "Fineliner" 的墨迹从 x=208 起，
/// 字形左留白约 1.5px，所以笔位给 87。
pub const MENU_TEXT_DX: i32 = 87;
/// 表头文字左边缘（相对面板左边）。官方 "Stroke" 的墨迹从 x=148 起。
pub const MENU_HEAD_DX: i32 = 27;
/// 菜单正文字号。
///
/// **别再按"大写字母高 23px ÷ 0.7"算回 33。** 那个 0.7 是 Noto 的墨迹比例，
/// 换成 Maison Neue（`official_to_font` 又乘了 1.2）之后，33 渲出来的大写高
/// 是 **30px**、整体比官方宽 20~30% —— 于是 `Mechanical pencil` 那一行写到了
/// x=420（面板只有 400 宽），右边两个字母漏在面板外；收起菜单时那一小块不在
/// 重绘矩形里，就一直留在纸上（就是"收起后屏上还挂着 il"那个 bug）。
///
/// 现在按**墨迹宽度**对官方量（`shots/official/tb-pens.png` 是 1:1 截图，
/// 行高正好 120）：
///
/// | 文字 | 官方宽 | 27 号渲出来 |
/// |---|---|---|
/// | `Mechanical pencil` | 271 | 267 |
/// | `Ballpoint pen`     | 194 | 192 |
/// | `Fineliner`         | 129 | 126 |
/// | `Eraser`            |  92 |  91 |
/// | `Erase selection`   | 224 | 222 |
///
/// 偏差都在 2% 以内，大写高 24 对官方 23（差 1px）。最宽那行墨迹到 x=357，
/// 离面板右边框还有 40 多像素 —— 以后再加长标签也不会再溢出。
pub const MENU_FONT: f32 = 27.0;
/// 行文字基线（相对行顶边）。
pub const MENU_TEXT_BASE: i32 = 67;
/// 粗细那三行的文字比颜色行低 4px（官方就这样：Medium 455..477 / Black 738..760）。
pub const MENU_TEXT_BASE_THICK: i32 = 71;
/// 表头文字基线（相对表头文字带的顶边）。
pub const MENU_HEAD_BASE: i32 = 34;

/// 官方笔菜单顺序。第 9 行就是 Text（`tb-pens.png` 正好 9 行）。
pub const TOOLS: [(Tool, &str); 9] = [
    (Tool::BallpointV2, "Ballpoint pen"),
    (Tool::FinelinerV2, "Fineliner"),
    (Tool::MarkerV2, "Marker"),
    (Tool::PencilV2, "Pencil"),
    (Tool::SharpPencilV2, "Mechanical pencil"),
    (Tool::PaintbrushV2, "Paintbrush"),
    (Tool::HighlighterV2, "Highlighter"),
    (Tool::Calligraphy, "Calligraphy pen"),
    (Tool::Undefined, "Text"),
];

pub const THICKNESSES: [(f64, &str); 3] = [(1.0, "Thin"), (2.0, "Medium"), (3.0, "Thick")];

/// 一个颜色项：(存进 `.rm` 的颜色, 名字, 灰度面板上色点的灰值)。
///
/// 官方 QML（`writingSettingsMenu.qml`）里色点用的就是灰度 PNG / 纯色块，
/// 所以这里第三个字段直接是"这个颜色在 16 级灰度面板上长什么样"。
pub type ColorItem = (Color, &'static str, u8);

/// 普通笔的三色。
pub const COLORS: [ColorItem; 3] = [
    (Color::Black, "Black", 20),
    (Color::Gray, "Gray", 128),
    (Color::White, "White", 255),
];

/// 荧光笔的四种"颜色"：官方写死的四个浅灰 `#d2d2d2 / #b4b4b4 / #8c8c8c / #cfcfcf`。
/// （名字还是 Yellow/Green/Pink/Gray —— 灰度机身上它们本来就只是四档浅灰。）
pub const HIGHLIGHTER_COLORS: [ColorItem; 4] = [
    (Color::HighlighterYellow, "Yellow", 210),
    (Color::HighlighterGreen, "Green", 180),
    (Color::HighlighterPink, "Pink", 140),
    (Color::HighlighterGray, "Gray", 207),
];

/// 橡皮菜单的条目（标题，动作），顶边贴橡皮那一格。
///
/// 第 1 行以前直接复用工具条那一格的 `Msg::Eraser`，结果点它是"开/收菜单"，
/// 并不能把模式从"擦整笔"切回"刷白"—— 菜单里明明有这一行却点不动。
/// 现在单独给一个 `Msg::EraseBrush`。
pub const ERASE_ITEMS: [(&str, Msg); 4] = [
    ("Eraser", Msg::EraseBrush),
    ("Erase stroke", Msg::EraseStroke),
    ("Erase selection", Msg::EraseSection),
    ("Erase all", Msg::ClearLayer),
];

/// 上 7 格（含第 1 格"收起"）的语义。
const TOP_MSG: [Msg; 7] = [
    Msg::Hide,
    Msg::PenMenu,
    Msg::SettingsMenu,
    Msg::Eraser,
    Msg::Selection,
    Msg::Undo,
    Msg::Redo,
];
/// 底部 5 格。
const BOT_MSG: [Msg; 5] = [
    Msg::PageOverview,
    Msg::Tags,
    Msg::Layers,
    Msg::Share,
    Msg::DocSettings,
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Msg {
    /// 当前笔那格：不是笔就选中笔，已经是笔再点才开/收笔菜单
    PenMenu,
    /// 文档菜单（保留，不再占槽位）
    DocMenu,
    /// 文字工具（保留，不再占槽位；现在在笔菜单第 9 行）
    TextTool,
    /// 粗细/颜色那格
    SettingsMenu,
    /// 选第几支笔（索引进 TOOLS）
    Tool(usize),
    Thickness(f64),
    Color(Color),
    /// 铅笔硬度档位（下标进 `pen::pencil::GRADES`）
    Grade(usize),
    /// 粗细段最后一行：边缘要不要羽化
    Feather,
    Eraser,
    /// 橡皮菜单第 1 行：切回"只刷白"的普通橡皮
    EraseBrush,
    /// 橡皮菜单第 2 行：碰到就删掉整条笔迹（路径橡皮）
    EraseStroke,
    /// 橡皮菜单第 3 行：只擦选区
    EraseSection,
    /// 橡皮菜单第 3 行：清空当前图层
    ClearLayer,
    Selection,
    /// 第 13 格：框选问 AI（官方没有这一格）。点一下进工具，再点一下退出。
    AiSelect,
    /// 第 14 格：这一页的 AI 引用列表（官方也没有这一格）。
    AiRefs,
    /// 第 15 格：横屏 / 竖屏（官方也没有这一格）。
    Landscape,
    Undo,
    Redo,
    PageOverview,
    Tags,
    Layers,
    /// 图层改名（层名长按/双击）：打开 `Rename Layer` 键盘，payload 是层下标
    LayerRename(usize),
    Share,
    DocSettings,
    Back,
    DocRename,
    DocTrash,
    Hide,
    Show,
    /// 点菜单外面关掉
    Dismiss,
}

/// 设置菜单里的一段。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sec {
    /// 粗细三档
    Stroke,
    /// 颜色（普通笔三色 / 荧光笔四色）
    Color,
    /// 铅笔硬度七档
    Pencil,
}

/// 这个工具能不能选颜色（官方 `colorSettingsActive()` 的那张表）。
///
/// 铅笔/自动铅笔天生只有灰，没有颜色可选；橡皮和文字工具也没有。
pub fn tool_has_color(t: Tool) -> bool {
    matches!(
        t,
        Tool::Ballpoint
            | Tool::BallpointV2
            | Tool::Fineliner
            | Tool::FinelinerV2
            | Tool::Marker
            | Tool::MarkerV2
            | Tool::Paintbrush
            | Tool::PaintbrushV2
            | Tool::Highlighter
            | Tool::HighlighterV2
            | Tool::Calligraphy
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Menu {
    None,
    Pens,
    Settings,
    Doc,
    Layers,
    Share,
    Tags,
    Erase,
}

/// 文档菜单的条目（标题，动作）
pub const DOC_ITEMS: [(&str, Msg); 3] = [
    ("Rename", Msg::DocRename),
    ("Move to trash", Msg::DocTrash),
    ("Close", Msg::Back),
];

#[derive(Clone, Debug)]
pub struct Chrome {
    pub selected: usize,
    /// 橡皮是个模式，不占笔菜单里的位置
    pub erasing: bool,
    /// 橡皮模式：`true` = 碰到就删整条笔迹；`false` = 只把扫过的像素刷白。
    ///
    /// 两者都用 `Tool::Eraser` 落盘（`.rm` 里的工具号必须和官方一致，不能
    /// 加新值），区别只在抬笔时走哪条路：刷白会留下一条橡皮线（官方也是
    /// 这么存的，渲染时按顺序做减法）；删整条则直接从模型里移除笔迹。
    pub erase_stroke: bool,
    /// "擦选区"模式（橡皮菜单第 2 行）
    pub erase_section: bool,
    /// 选区（套索）也是个模式：和笔/橡皮互斥
    pub selecting: bool,
    /// 第 13 格"框选问 AI"的激活态（真状态在 `panels::selectai::Panel`，
    /// 这里只是给图标画反白用；切进/切出工具时由 main.rs 同步）。
    pub ask_ai: bool,
    /// 引用菜单开着（第 14 格反白）
    pub ai_refs: bool,
    pub thickness: f64,
    pub color: Color,
    /// 铅笔硬度档位（下标进 `pen::pencil::GRADES`）。只有铅笔家族用得上。
    pub pencil_grade: usize,
    /// 边缘羽化：开 = 抗锯齿的软边（默认），关 = 1-bit 硬边（官方原版的样子）。
    /// 放在"粗细"那一段的最后一行。
    pub feather: bool,
    pub visible: bool,
    pub menu: Menu,
    pub page: usize,
    pub pages: usize,
    /// main.rs 每帧写：还有没有可撤销/可重做的
    pub undo_ok: bool,
    pub redo_ok: bool,
}

impl Default for Chrome {
    fn default() -> Self {
        Chrome {
            selected: 0,
            erasing: false,
            erase_stroke: false,
            erase_section: false,
            selecting: false,
            ask_ai: false,
            ai_refs: false,
            thickness: 2.0,
            color: Color::Black,
            pencil_grade: crate::pen::pencil::DEFAULT,
            feather: true,
            visible: true,
            menu: Menu::None,
            page: 0,
            pages: 1,
            // 默认按"可撤销/可重做"画：官方截图里这两个图标一直是黑的细线，
            // 真状态由 main.rs 每帧写。
            undo_ok: true,
            redo_ok: true,
        }
    }
}

impl Chrome {
    pub fn tool(&self) -> Tool {
        if self.erasing {
            Tool::Eraser
        } else {
            TOOLS[self.selected.min(TOOLS.len() - 1)].0
        }
    }

    /// 当前菜单的顶边：对齐所属格子的顶边。
    pub fn menu_top(&self) -> i32 {
        let g = geom();
        match self.menu {
            Menu::Settings => g.top0 + g.cell,
            Menu::Erase => g.top0 + 2 * g.cell,
            _ => g.top0,
        }
    }

    /// 当前菜单占的屏幕矩形（`x, y, w, h`）。菜单收起以后要靠它把被盖住的
    /// 纸面按 partial 刷新补回来，所以调用方要在改 `menu` **之前**取。
    ///
    /// 返回的矩形比面板本身**四周各大一圈**（`MENU_RECT_PAD`）。原因是踩过的坑：
    /// `Mechanical pencil` 那行文字以前溢出面板右边 20 像素，收起菜单时只有
    /// 面板本体在重绘矩形里，那半截 "il" 就永久留在纸上了。现在字号已经校准
    /// 到放得下，这里再留一圈余量，是为了以后加长标签/换字体时**不会再漏**。
    /// 多刷一圈纸面（十几个像素）对手感和刷新时间都没有可感差别。
    ///
    /// 注意这个矩形**只用于重绘**，命中判定用的是 `MENU_X..MENU_X+MENU_W`
    /// （见 `hit`），所以放大不会把面板外的点击吃掉。
    pub fn menu_rect(&self) -> Option<(i32, i32, i32, i32)> {
        let h = self.menu_h();
        if h <= 0 {
            return None;
        }
        const P: i32 = MENU_RECT_PAD;
        Some((
            menu_x() - P,
            self.menu_top() - P,
            MENU_W + P + MENU_RECT_EXTRA_R,
            h + 2 * P,
        ))
    }

    /// 当前菜单的高度（含 2px 边框）。
    pub fn menu_h(&self) -> i32 {
        match self.menu {
            Menu::Pens => TOOLS.len() as i32 * MENU_ROW,
            Menu::Erase => ERASE_ITEMS.len() as i32 * MENU_ROW,
            Menu::Doc => DOC_ITEMS.len() as i32 * MENU_ROW,
            Menu::Settings => {
                // 走一遍各段：每段 = 表头 43+2 + 行数*120 + 分隔 2
                let mut h = MENU_SEP;
                for (_, rows) in self.settings_sections() {
                    h += MENU_HEAD + MENU_SEP + rows * MENU_ROW + MENU_SEP;
                }
                h
            }
            _ => 0,
        }
    }

    /// 当前选中档位的下标（设置菜单里"哪一行是反白的"）。
    pub fn thickness_idx(&self) -> usize {
        THICKNESSES
            .iter()
            .position(|(w, _)| (*w - self.thickness).abs() < 0.01)
            .unwrap_or(0)
    }

    pub fn color_idx(&self) -> usize {
        self.colors()
            .iter()
            .position(|(c, _, _)| *c == self.color)
            .unwrap_or(0)
    }

    // ---- 设置菜单的分段几何 ------------------------------------------------
    // 官方的 `writingSettingsMenu.qml` 是按工具给不同段的：
    //   * 荧光笔：**没有 Stroke**（粗细写死 30），只有 4 个颜色；
    //   * 铅笔/自动铅笔：只有 Stroke（我们额外加了一段硬度档）；
    //   * 圆珠笔/针管笔/马克笔/画笔/书法：Stroke + Color。
    // 段数和行数都不固定，所以位置一律"走一遍"算，不再硬编码两段。

    /// 当前工具下设置菜单有哪几段（自上而下）。
    pub fn settings_sections(&self) -> Vec<(Sec, i32)> {
        let t = self.tool();
        let mut v = Vec::new();
        if !t.is_highlighter() {
            // 三档粗细；实心墨的笔再加一行"羽化"开关（官方没有这一行，是我们加的）。
            // 铅笔是网点画的、文字工具根本不落墨，给了开关也没效果，所以不给。
            let extra = if self.has_feather(t) { 1 } else { 0 };
            v.push((Sec::Stroke, THICKNESSES.len() as i32 + extra));
        }
        if t.is_pencil() {
            v.push((Sec::Pencil, pencil::GRADES.len() as i32));
        } else if tool_has_color(t) {
            v.push((Sec::Color, self.colors().len() as i32));
        }
        v
    }

    /// 这支笔吃不吃"羽化"开关：实心墨的才吃（铅笔走网点、文字不落墨）。
    pub fn has_feather(&self, t: Tool) -> bool {
        !t.is_pencil() && !t.is_highlighter() && t != Tool::Undefined
    }

    /// 当前工具能选的颜色表（荧光笔有自己那套）。
    pub fn colors(&self) -> &'static [ColorItem] {
        if self.tool().is_highlighter() {
            &HIGHLIGHTER_COLORS
        } else {
            &COLORS
        }
    }

    /// 颜色段是哪一种（普通色 or 铅笔硬度）——main.rs 局部刷新时要按这个取行。
    pub fn color_sec(&self) -> Sec {
        if self.tool().is_pencil() {
            Sec::Pencil
        } else {
            Sec::Color
        }
    }

    /// 某一段第一行相对菜单顶边的 y。没这一段就返回 None。
    fn section_y(&self, want: Sec) -> Option<i32> {
        let mut y = MENU_SEP;
        for (sec, rows) in self.settings_sections() {
            if sec == want {
                return Some(y + MENU_HEAD + MENU_SEP);
            }
            y += MENU_HEAD + MENU_SEP + rows * MENU_ROW + MENU_SEP;
        }
        None
    }

    /// 设置菜单里某一行的屏幕矩形。
    ///
    /// 改粗细/颜色时菜单本身不动，只有"旧的反白行"和"新的反白行"变了；
    /// 整块菜单 400×1080 的 GC16 面板时间约 47ms，两行只有 96k 像素 ≈ 10ms。
    pub fn settings_row(&self, sec: Sec, idx: usize) -> Option<(i32, i32, i32, i32)> {
        if self.menu != Menu::Settings {
            return None;
        }
        let rows = self
            .settings_sections()
            .into_iter()
            .find(|(s, _)| *s == sec)
            .map(|(_, n)| n)?;
        if idx as i32 >= rows {
            return None;
        }
        let y = self.section_y(sec)?;
        let x = menu_x();
        Some((x, self.menu_top() + y + idx as i32 * MENU_ROW, MENU_W, MENU_ROW))
    }

    pub fn hit(&self, x: f32, y: f32) -> Option<Msg> {
        let (xi, yi) = (x as i32, y as i32);
        let g = geom();
        if !self.visible {
            // 收起态：整个屏幕只剩左上角那一格，点它展开
            return if in_band(xi) && (0..g.cell).contains(&yi) {
                Some(Msg::Show)
            } else {
                None
            };
        }
        // 图层面板/分享/标签面板的命中由 App 算（它们比 MENU_W 宽得多），这里别抢
        match self.menu {
            Menu::Layers | Menu::Share | Menu::Tags => {
                if in_band(xi) {
                    return None;
                }
            }
            Menu::None => {}
            _ => {
                let mx = menu_x();
                if (mx..mx + MENU_W).contains(&xi) {
                    return self.hit_menu(yi);
                }
                // 落在菜单外：点纸面关掉菜单。
                // 但**工具条那一列不算"空白"**：官方开着笔菜单时点橡皮会直接切过去
                // （`ToolButton` 自己把别的 foldout 关掉），所以那一列继续往下走槽位表。
                if !in_band(xi) {
                    return Some(Msg::Dismiss);
                }
            }
        }
        // 没有菜单时，工具条以外都是纸面 —— 必须返回 None，否则笔一落下就被吞掉
        if !in_band(xi) || yi < 0 {
            return None;
        }
        if yi < g.top0 {
            return Some(Msg::Hide);
        }
        if yi < g.top0 + 6 * g.cell {
            return Some(TOP_MSG[((yi - g.top0) / g.cell) as usize + 1]);
        }
        // 第 13 格：框选问 AI（空段里的第一格）
        if (g.ai_y..g.ai_y + g.cell).contains(&yi) {
            return Some(Msg::AiSelect);
        }
        // 第 14 格：这一页的 AI 引用列表
        if (g.ai_y + g.cell..g.ai_y + 2 * g.cell).contains(&yi) {
            return Some(Msg::AiRefs);
        }
        // 第 15 格：横屏开关（再点一次回竖屏）
        if (g.land_y..g.land_y + g.cell).contains(&yi) {
            return Some(Msg::Landscape);
        }
        if (g.bottom0..g.bottom0 + 5 * g.cell).contains(&yi) {
            return Some(BOT_MSG[((yi - g.bottom0) / g.cell) as usize]);
        }
        None
    }

    fn hit_menu(&self, y: i32) -> Option<Msg> {
        let r = y - self.menu_top();
        match self.menu {
            // 图层面板/分享面板的命中由 App 算，这里不参与
            Menu::Layers | Menu::Share | Menu::Tags => None,
            Menu::Pens => {
                if (0..TOOLS.len() as i32 * MENU_ROW).contains(&r) {
                    Some(Msg::Tool((r / MENU_ROW) as usize))
                } else {
                    Some(Msg::Dismiss)
                }
            }
            Menu::Erase => {
                if (0..ERASE_ITEMS.len() as i32 * MENU_ROW).contains(&r) {
                    Some(ERASE_ITEMS[(r / MENU_ROW) as usize].1)
                } else {
                    Some(Msg::Dismiss)
                }
            }
            Menu::Settings => {
                // 顶边 2 起，逐段往下走
                let mut y = MENU_SEP;
                for (sec, rows) in self.settings_sections() {
                    let r0 = y + MENU_HEAD + MENU_SEP;
                    let r1 = r0 + rows * MENU_ROW;
                    if (r0..r1).contains(&r) {
                        let i = ((r - r0) / MENU_ROW) as usize;
                        return Some(match sec {
                            Sec::Stroke if i < THICKNESSES.len() => {
                                Msg::Thickness(THICKNESSES[i].0)
                            }
                            Sec::Stroke => Msg::Feather,
                            Sec::Color => Msg::Color(self.colors()[i].0),
                            Sec::Pencil => Msg::Grade(i),
                        });
                    }
                    y = r1 + MENU_SEP;
                }
                Some(Msg::Dismiss)
            }
            Menu::Doc => {
                if (0..DOC_ITEMS.len() as i32 * MENU_ROW).contains(&r) {
                    Some(DOC_ITEMS[(r / MENU_ROW) as usize].1)
                } else {
                    Some(Msg::Dismiss)
                }
            }
            Menu::None => None,
        }
    }

    /// 这一格现在是不是"激活"状态（整格黑底反白）。
    fn active(&self, idx: i32) -> bool {
        let text_tool = self.selected == TOOLS.len() - 1;
        match idx {
            // 当前笔：橡皮/选区/文字模式下不亮；开着笔菜单时也亮
            1 => (!self.erasing && !self.selecting && !text_tool) || self.menu == Menu::Pens,
            2 => self.menu == Menu::Settings,
            3 => self.erasing || self.menu == Menu::Erase,
            4 => self.selecting,
            // 第 13 格：框选问 AI 的工具态
            12 => self.ask_ai,
            // 第 14 格：引用菜单开着
            13 => self.ai_refs,
            // 第 15 格：横屏模式（状态在 `fb::landscape()`，不在工具条里）
            14 => crate::fb::landscape(),
            _ => false,
        }
    }

    /// 只往帧缓冲里画，不推面板。调用方负责一次性 refresh。
    pub fn draw_in(&self, fb: &mut Fb, text: &mut Option<crate::ui::text::Text>) {
        if std::env::var("RM_DEBUG").is_ok() {
            eprintln!(
                "[chrome] draw_in visible={} menu={:?} sel={}",
                self.visible, self.menu, self.selected
            );
        }
        // 左利手：整列挪到屏幕右边（`band_x()`），图标还是按 `ox` 摆在列里
        let bx = band_x();
        let ox = bx + W / 2 - ICON / 2;
        let g = geom();
        // 收起态：整屏只剩左上角"圆环 + 实心点"，其余什么都不画
        if !self.visible {
            draw_ring(fb, ox, g.cell / 2 - ICON / 2, 20, 255, false);
            return;
        }
        let t0 = std::time::Instant::now();
        fb.fill_rect(bx, 0, W, fb.height, gray565(255));
        let t_fill = t0.elapsed();

        // 第 1 格：收起
        draw_ring(fb, ox, g.cell / 2 - ICON / 2 - 1, 20, 255, true);
        // 上 6 格（笔 / 粗细 / 橡皮 / 选区 / 撤销 / 重做）
        for i in 1..=6 {
            let y = g.top0 + (i - 1) * g.cell;
            let on = self.active(i);
            if on {
                fb.fill_rect(bx, y, W, g.cell, gray565(0));
            }
            let (fg, bg) = if on { (255u8, 0u8) } else { (0u8, 255u8) };
            // 撤销/重做没得用时画灰
            let fg = match i {
                5 if !self.undo_ok => 150,
                6 if !self.redo_ok => 150,
                _ => fg,
            };
            let oy = y + g.cell / 2 - ICON / 2;
            match i {
                1 => draw_nib(fb, ox, oy, fg, bg),
                2 => draw_thickness(fb, ox, oy, fg, bg),
                3 => draw_eraser(fb, ox, oy, fg, bg, true),
                4 => draw_selection(fb, ox, oy, fg, bg),
                5 => draw_undo(fb, ox, oy, fg, bg, false),
                _ => draw_undo(fb, ox, oy, fg, bg, true),
            }
        }
        // 第 13 格：框选问 AI（官方 12 格之外的那一格，落在中间空段的第一格）
        {
            let y = g.ai_y;
            let on = self.active(12);
            if on {
                fb.fill_rect(bx, y, W, g.cell, gray565(0));
            }
            let (fg, bg) = if on { (255u8, 0u8) } else { (0u8, 255u8) };
            crate::panels::selectai::draw_icon(fb, ox, y + g.cell / 2 - ICON / 2, fg, bg);
        }
        // 第 14 格：这一页的 AI 引用列表（方框 + 指向它的箭头）
        {
            let y = g.ai_y + g.cell;
            let on = self.active(13);
            if on {
                fb.fill_rect(bx, y, W, g.cell, gray565(0));
            }
            let (fg, bg) = if on { (255u8, 0u8) } else { (0u8, 255u8) };
            draw_ai_refs(fb, ox, y + g.cell / 2 - ICON / 2, fg, bg);
        }
        // 第 15 格：横屏开关（当前是横屏就反白）
        {
            let y = g.land_y;
            let on = crate::fb::landscape();
            if on {
                fb.fill_rect(bx, y, W, g.cell, gray565(0));
            }
            let (fg, bg) = if on { (255u8, 0u8) } else { (0u8, 255u8) };
            draw_landscape(fb, ox, y + g.cell / 2 - ICON / 2, fg, bg);
        }
        // 底部 5 格
        for i in 0..5 {
            let y = g.bottom0 + i * g.cell;
            let oy = y + g.cell / 2 - ICON / 2;
            match i {
                0 => draw_overview(fb, ox, oy, 0, 255),
                1 => draw_tag(fb, ox, oy, 0, 255),
                2 => draw_layers(fb, ox, oy, 0, 255),
                3 => draw_share(fb, ox, oy, 0, 255),
                _ => draw_docsettings(fb, ox, oy, 0, 255),
            }
        }

        let t_slots = t0.elapsed();
        if self.menu != Menu::None {
            self.draw_menu(fb, text);
        }
        if std::env::var("RM_TIMING").is_ok() {
            eprintln!(
                "[chrome] draw_in 底色={}us 格子={}us 菜单={}us",
                t_fill.as_micros(),
                (t_slots - t_fill).as_micros(),
                (t0.elapsed() - t_slots).as_micros()
            );
        }
    }

    /// 画完顺手推上去。
    pub fn draw(&self, fb: &mut Fb, text: &mut Option<crate::ui::text::Text>) {
        self.draw_in(fb, text);
        if !self.visible {
            return;
        }
        fb.refresh(
            band_x(),
            0,
            W,
            fb.height,
            crate::fb::WAVEFORM_MODE_GC16,
            crate::fb::UPDATE_MODE_PARTIAL,
        );
    }

    fn draw_menu(&self, fb: &mut Fb, text: &mut Option<crate::ui::text::Text>) {
        // 图层面板/分享面板要读状态，由 App 画
        if matches!(
            self.menu,
            Menu::Layers | Menu::Share | Menu::Tags | Menu::None
        ) {
            return;
        }
        let x = menu_x();
        let top = self.menu_top();
        let h = self.menu_h();
        fb.fill_rect(x, top, MENU_W, h, gray565(255));
        widgets::outline_rect(fb, x, top, MENU_W, h, MENU_SEP, 0);
        match self.menu {
            Menu::Pens => {
                for (i, (_, name)) in TOOLS.iter().enumerate() {
                    let y = top + i as i32 * MENU_ROW;
                    let on = i == self.selected && !self.erasing && !self.selecting;
                    if on {
                        fb.fill_rect(x, y, MENU_W, MENU_ROW, gray565(0));
                    }
                    let (fg, bg) = if on { (255u8, 0u8) } else { (0u8, 255u8) };
                    draw_pen_icon(fb, i, x + MENU_ICON_DX, y + 40, fg, bg);
                    if let Some(t) = text.as_mut() {
                        t.draw(fb, x + MENU_TEXT_DX, y + MENU_TEXT_BASE, MENU_FONT, name, fg);
                    }
                    if !on {
                        sep_line(fb, x, y + MENU_ROW - MENU_SEP);
                    }
                }
            }
            Menu::Erase => {
                for (i, (name, _)) in ERASE_ITEMS.iter().enumerate() {
                    let y = top + i as i32 * MENU_ROW;
                    let on = match i {
                        // 普通橡皮（刷白）：橡皮模式、既不是"擦整笔"也不是"擦选区"
                        0 => self.erasing && !self.erase_section && !self.erase_stroke,
                        1 => self.erasing && self.erase_stroke,
                        2 => self.erasing && self.erase_section,
                        _ => false,
                    };
                    if on {
                        fb.fill_rect(x, y, MENU_W, MENU_ROW, gray565(0));
                    }
                    let (fg, bg) = if on { (255u8, 0u8) } else { (0u8, 255u8) };
                    draw_erase_icon(fb, i, x + MENU_ICON_DX, y + 40, fg, bg);
                    if let Some(t) = text.as_mut() {
                        t.draw(fb, x + MENU_TEXT_DX, y + MENU_TEXT_BASE, MENU_FONT, name, fg);
                    }
                    if !on {
                        sep_line(fb, x, y + MENU_ROW - MENU_SEP);
                    }
                }
            }
            Menu::Settings => {
                // 逐段画：段数和行数都跟着当前工具走（见 `settings_sections`）
                let secs = self.settings_sections();
                let last = secs.len().saturating_sub(1);
                let mut y = top + MENU_SEP;
                for (si, (sec, rows)) in secs.iter().enumerate() {
                    let head = match sec {
                        Sec::Stroke => "Stroke",
                        Sec::Color => "Color",
                        Sec::Pencil => "Pencil",
                    };
                    if let Some(t) = text.as_mut() {
                        t.draw(fb, x + MENU_HEAD_DX, y + MENU_HEAD_BASE, MENU_FONT, head, 0);
                    }
                    y += MENU_HEAD;
                    sep_line(fb, x, y);
                    y += MENU_SEP;
                    for i in 0..*rows as usize {
                        let ry = y + i as i32 * MENU_ROW;
                        match sec {
                            Sec::Stroke if i >= THICKNESSES.len() => {
                                // 羽化开关：左边一个方框（开=实心带勾），右边写 On/Off
                                draw_check(fb, x + MENU_ICON_DX - 4, ry + 44, self.feather);
                                if let Some(t) = text.as_mut() {
                                    t.draw(
                                        fb,
                                        x + MENU_TEXT_DX,
                                        ry + MENU_TEXT_BASE_THICK,
                                        MENU_FONT,
                                        "Feathered edge",
                                        0,
                                    );
                                    let state = if self.feather { "On" } else { "Off" };
                                    // 右对齐到面板内边距。以前是写死的 `MENU_W - 74`，
                                    // 标签一长（"Feathered edge"）就和状态粘成
                                    // "Feathered edgeOn"；按实际宽度算落点，
                                    // 状态永远贴着右边框，标签再长也撞不上。
                                    let sw = t.width(MENU_FONT, state);
                                    t.draw(
                                        fb,
                                        x + MENU_W - MENU_SEP - 16 - sw as i32,
                                        ry + MENU_TEXT_BASE_THICK,
                                        MENU_FONT,
                                        state,
                                        0,
                                    );
                                }
                                if i + 1 < *rows as usize {
                                    sep_line(fb, x, ry + MENU_ROW - MENU_SEP);
                                }
                            }
                            Sec::Stroke => {
                                let (w, name) = THICKNESSES[i];
                                let on = (w - self.thickness).abs() < 0.01;
                                if on {
                                    fb.fill_rect(x, ry, MENU_W, MENU_ROW, gray565(0));
                                }
                                let (fg, bg) = if on { (255u8, 0u8) } else { (0u8, 255u8) };
                                draw_stroke_sample(fb, x + MENU_ICON_DX, ry + 40, i, fg, bg);
                                if let Some(t) = text.as_mut() {
                                    t.draw(
                                        fb,
                                        x + MENU_TEXT_DX,
                                        ry + MENU_TEXT_BASE_THICK,
                                        MENU_FONT,
                                        name,
                                        fg,
                                    );
                                }
                            }
                            Sec::Color => {
                                let (col, name, gray) = self.colors()[i];
                                let on = col == self.color;
                                if on {
                                    fb.fill_rect(x, ry, MENU_W, MENU_ROW, gray565(0));
                                }
                                let (fg, bg) = if on { (255u8, 0u8) } else { (0u8, 255u8) };
                                if self.tool().is_highlighter() {
                                    // 官方荧光笔是纯色圆（没有描边圆环），
                                    // "Gray·Overlapping" 右半边再压一层淡黑
                                    draw_hl_dot(fb, x + MENU_ICON_DX - 6, ry + 32, gray);
                                    if let Some(t) = text.as_mut() {
                                        if name == "Gray" {
                                            t.draw(
                                                fb,
                                                x + MENU_TEXT_DX,
                                                ry + MENU_TEXT_BASE - 14,
                                                MENU_FONT,
                                                name,
                                                fg,
                                            );
                                            t.draw(
                                                fb,
                                                x + MENU_TEXT_DX,
                                                ry + MENU_TEXT_BASE + 26,
                                                24.0,
                                                "Overlapping",
                                                fg,
                                            );
                                        } else {
                                            t.draw(
                                                fb,
                                                x + MENU_TEXT_DX,
                                                ry + MENU_TEXT_BASE,
                                                MENU_FONT,
                                                name,
                                                fg,
                                            );
                                        }
                                    }
                                } else {
                                    draw_color_dot(fb, x + MENU_ICON_DX - 6, ry + 32, gray, fg, bg);
                                    if let Some(t) = text.as_mut() {
                                        t.draw(
                                            fb,
                                            x + MENU_TEXT_DX,
                                            ry + MENU_TEXT_BASE,
                                            MENU_FONT,
                                            name,
                                            fg,
                                        );
                                    }
                                }
                            }
                            Sec::Pencil => {
                                let g = &pencil::GRADES[i];
                                let on = i == self.pencil_grade;
                                if on {
                                    fb.fill_rect(x, ry, MENU_W, MENU_ROW, gray565(0));
                                }
                                let (fg, bg) = if on { (255u8, 0u8) } else { (0u8, 255u8) };
                                draw_grade_dot(fb, x + MENU_ICON_DX - 6, ry + 32, g.tone, fg, bg);
                                if let Some(t) = text.as_mut() {
                                    t.draw(
                                        fb,
                                        x + MENU_TEXT_DX,
                                        ry + MENU_TEXT_BASE,
                                        MENU_FONT,
                                        g.name,
                                        fg,
                                    );
                                }
                            }
                        }
                    }
                    y += rows * MENU_ROW;
                    // 段之间的分隔线；最后一段不画（菜单边框已经在那儿了）
                    if si != last {
                        sep_line(fb, x, y);
                        y += MENU_SEP;
                    }
                }
                let _ = last;
            }
            Menu::Doc => {
                for (i, (name, _)) in DOC_ITEMS.iter().enumerate() {
                    let y = top + i as i32 * MENU_ROW;
                    if let Some(t) = text.as_mut() {
                        t.draw(fb, x + MENU_TEXT_DX, y + MENU_TEXT_BASE, MENU_FONT, name, 0);
                    }
                    sep_line(fb, x, y + MENU_ROW - MENU_SEP);
                }
            }
            _ => {}
        }
    }
}

/// 羽化开关的方框：开 = 实心 + 白勾，关 = 空心方框。
///
/// 尺寸按菜单行高推的（行 120，图标框 40）：30px 的方框在左边，和粗细样张对齐。
fn draw_check(fb: &mut Fb, ox: i32, oy: i32, on: bool) {
    const S: i32 = 30;
    if on {
        fb.fill_rect(ox, oy, S, S, gray565(0));
        let tick = widgets::thick(&[(7.5, 16.0), (13.0, 21.5), (23.0, 8.5)], 3.6);
        widgets::shade_at(widgets::sid("check_on"), fb, ox, oy, S, S, 255, 0, |x, y| {
            widgets::in_poly(x, y, &tick)
        });
    } else {
        fb.fill_rect(ox, oy, S, 2, gray565(0));
        fb.fill_rect(ox, oy + S - 2, S, 2, gray565(0));
        fb.fill_rect(ox, oy, 2, S, gray565(0));
        fb.fill_rect(ox + S - 2, oy, 2, S, gray565(0));
    }
}

/// 面板里的一条横分隔线（不压左右边框）。
fn sep_line(fb: &mut Fb, x: i32, y: i32) {
    fb.fill_rect(x + MENU_SEP, y, MENU_W - 2 * MENU_SEP, MENU_SEP, gray565(0));
}

// ---------------------------------------------------------------- 图标
//
// 全部按 `shots/official/` 的像素量：坐标是 40×40 图标框内的局部坐标，
// 用 `widgets::shade` 按覆盖率画（直边还是硬的，圆弧/斜边有和官方一样的灰阶过渡）。

/// 收起/展开：圆环 + 实心点。展开态点在环内左上，收起态点在右下（官方两态只差这一点）。
fn draw_ring(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8, expanded: bool) {
    let (dx, dy) = if expanded { (14.5, 14.0) } else { (24.5, 24.0) };
    let id = widgets::sid(if expanded { "ring_open" } else { "ring_closed" });
    widgets::shade_at(id, fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        widgets::in_ring(x, y, 20.0, 20.0, 20.0, 4.0) || widgets::in_disc(x, y, dx, dy, 5.0)
    });
}

/// 笔尖（工具条第 2 格 = 菜单里的 Ballpoint pen）。
fn draw_nib(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8) {
    let left = vec![
        widgets::thick(&[(10.5, 7.5), (7.5, 24.0)], 4.0),
        widgets::thick(&[(7.5, 23.0), (5.5, 28.5), (5.5, 40.5)], 4.0),
    ];
    let legs = [left.clone(), mirror_x(&left)].concat();
    widgets::shade_at(widgets::sid("draw_nib"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        // 笔舌孔（顶端那个小圆环）
        (widgets::in_disc(x, y, 19.5, 6.5, 6.5) && !widgets::in_disc(x, y, 19.5, 6.5, 2.2))
            || widgets::in_rect(x, y, 10.0, 8.0, 20.0, 4.0)
            || widgets::in_rect(x, y, 6.5, 19.8, 27.0, 4.2)
            || legs.iter().any(|p| widgets::in_poly(x, y, p))
    });
}

/// 三条横线：粗细 3 / 6 / 10，官方是 50% 棋盘抖动（看起来是灰的）。
fn draw_thickness(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8) {
    widgets::shade_at(widgets::sid("draw_thickness"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        let dither = ((x.floor() as i32 + y.floor() as i32) & 1) == 1;
        dither
            && ((1.0..4.0).contains(&y) || (13.0..19.0).contains(&y) || (29.0..39.0).contains(&y))
    });
}

/// 橡皮：45° 斜放的圆角矩形 + 一条分隔线 + 实心的一头。
/// `frame` 打开时再加官方工具条那格外面的虚线方框。
fn draw_eraser(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8, frame: bool) {
    // 长边 x+y=33.5 / 51.5，短边 x-y=9.5 / -31，分隔线 x-y=-9.5（都是实测的）
    let body = vec![
        widgets::thick(&[(22.0, 12.0), (3.0, 31.0)], 4.3),
        widgets::thick(&[(30.0, 21.0), (11.0, 40.0)], 4.3),
        widgets::thick(&[(22.0, 12.0), (30.0, 21.0)], 4.3),
        widgets::thick(&[(3.0, 31.0), (11.0, 40.0)], 4.3),
        widgets::thick(&[(12.5, 21.5), (21.5, 30.5)], 4.3),
    ];
    // 底面那头是实心的（官方 y>=35 一片黑）
    let cap = [(5.5, 34.8), (20.0, 34.8), (15.5, 39.0), (8.0, 39.0)];
    let id = widgets::sid(if frame { "eraser_frame" } else { "eraser_plain" });
    widgets::shade_at(id, fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        body.iter().any(|p| widgets::in_poly(x, y, p))
            || widgets::in_poly(x, y, &cap)
            || (frame && dashed_frame(x, y))
    });
}

/// 虚线方框：4×3 的小方块，横边 5 个、竖边 4 个（实测 x/y 都是隔 8）。
fn dashed_frame(x: f32, y: f32) -> bool {
    let mut on = false;
    for k in 0..5 {
        let p = 4.0 + k as f32 * 8.0;
        on |= widgets::in_rect(x, y, p, 0.0, 4.0, 3.0);
        on |= widgets::in_rect(x, y, p, 32.0, 4.0, 3.0);
    }
    for k in 0..4 {
        let p = 7.4 + k as f32 * 8.0;
        on |= widgets::in_rect(x, y, 4.0, p, 4.0, 4.2);
        on |= widgets::in_rect(x, y, 36.0, p, 4.0, 4.2);
    }
    on
}

/// 选区：四角 12×12 方框（线宽 4、中间挖空）+ 每边两个 4×4 短划。
/// 第 14 格：这一页的 AI 引用**列表** —— 三行条目 + 一颗 sparkle。
///
/// 旧画法（虚线框 + 一条从框外扎进左上角的箭头 + 两撇"箭头毛"）在 40×40
/// 里全是 1.6px 的细线和断点，墨屏上糊成一团，用户原话"看起来很怪异"。
/// 现在改成实心的三行 + 和 AI 工具那格同一颗四角星（缩小版），"这是 AI 的
/// 一份清单"，结构和框选工具那格（整框 + 大星）区分得开。
fn draw_ai_refs(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8) {
    widgets::shade_at(widgets::sid("draw_ai_refs"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        // 三行条目：x 3..26，高 3，行距 10（整组 8.5..31.5，纵向居中）
        let rows = (0..3).any(|i| widgets::in_rect(x, y, 3.0, 8.5 + i as f32 * 10.0, 23.0, 3.0));
        // 四角星：和 `selectai::draw_icon` 同一颗 astroid，半径缩到 6.4
        let (dx, dy) = ((x - 32.5).abs(), (y - 20.0).abs());
        let star = (dx / 6.4).powf(0.5) + (dy / 6.4).powf(0.5) <= 1.0;
        rows || star
    });
}

/// 第 15 格：横屏开关。一张**横着放的纸**（和文档设置那格竖纸区分），
/// 里面两行内容 —— 形状本身就是"页面方向"，不需要再加需要辨认的小箭头。
fn draw_landscape(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8) {
    widgets::shade_at(widgets::sid("draw_landscape"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        // 纸：34.8 × 21.2，边框 2.6，纵向居中（9.4..30.6）
        let frame = widgets::in_rect(x, y, 2.6, 9.4, 34.8, 2.6)
            || widgets::in_rect(x, y, 2.6, 28.0, 34.8, 2.6)
            || widgets::in_rect(x, y, 2.6, 9.4, 2.6, 21.2)
            || widgets::in_rect(x, y, 34.8, 9.4, 2.6, 21.2);
        // 里面两行：一行长、一行短，都靠左
        let lines = widgets::in_rect(x, y, 8.0, 15.0, 17.0, 2.4)
            || widgets::in_rect(x, y, 8.0, 21.0, 11.0, 2.4);
        frame || lines
    });
}

fn draw_selection(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8) {
    widgets::shade_at(widgets::sid("draw_selection"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        let corner = |cx: f32, cy: f32| widgets::in_frame(x, y, cx, cy, 12.0, 12.0, 4.0);
        corner(0.0, 0.0)
            || corner(28.0, 0.0)
            || corner(0.0, 28.0)
            || corner(28.0, 28.0)
            || [
                (14.0, 4.0),
                (22.0, 4.0),
                (14.0, 32.0),
                (22.0, 32.0),
                (4.0, 14.0),
                (4.0, 22.0),
                (32.0, 14.0),
                (32.0, 22.0),
            ]
            .iter()
            .any(|&(dx, dy)| widgets::in_rect(x, y, dx, dy, 4.0, 4.0))
    });
}

/// 撤销 / 重做：官方是 1px 的抖动细线（50% 棋盘），重做是撤销的左右镜像。
fn draw_undo(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8, mirror: bool) {
    // 可用 → 实心 2.4px；不可用 → 官方画的是"抖动的一像素细线"，看着是灰的。
    // 之前两种情况都用抖动细线，结果"能不能撤销"根本看不出来。
    let solid = fg < 100;
    let w = if solid { 2.4 } else { 1.4 };
    let mut ps = vec![
        widgets::thick(&[(10.5, 6.5), (2.0, 15.5)], w),
        widgets::thick(&[(2.0, 15.5), (10.5, 23.5)], w),
        widgets::thick(
            &[(33.0, 15.0), (38.0, 21.0), (36.5, 26.0), (33.0, 30.5), (30.0, 32.0)],
            w,
        ),
    ];
    if mirror {
        ps = mirror_x(&ps);
    }
    let bar_h = w + 0.8;
    let id = widgets::sid(if mirror { "redo" } else { "undo" });
    widgets::shade_at(id, fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        if !solid && ((x.floor() as i32 + y.floor() as i32) & 1) != 1 {
            return false;
        }
        widgets::in_rect(x, y, 2.0, 15.5 - bar_h * 0.5, 32.0, bar_h)
            || ps.iter().any(|p| widgets::in_poly(x, y, p))
    });
}

/// 页面总览：2×2 四个 18×18 方框（线宽 4、缝 4）。
fn draw_overview(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8) {
    widgets::shade_at(widgets::sid("draw_overview"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        [0.0, 22.0].into_iter().any(|dy| {
            [0.0, 22.0]
                .into_iter()
                .any(|dx| widgets::in_frame(x, y, dx, dy, 18.0, 18.0, 4.0))
        })
    });
}

/// 标签：斜 45° 的吊牌（右上角是直角）+ 右上一个小圆孔。
///
/// 用「外多边形挖内多边形」而不是把折线加粗：五边形的锐角上 `thick` 的斜接会自己
/// 交叉，填出来是乱的（这条踩过坑）。
fn draw_tag(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8) {
    let outer = [
        (19.5, 0.0),
        (39.5, 0.0),
        (39.5, 19.5),
        (19.5, 39.5),
        (-0.5, 19.5),
    ];
    // 内框 = 外框各边往内缩 4px（45° 边的法向缩进换算成 x+y / x-y 的常数）
    let inner = [
        (20.8, 4.2),
        (36.0, 4.2),
        (36.0, 17.8),
        (19.5, 34.6),
        (5.4, 19.5),
    ];
    widgets::shade_at(widgets::sid("draw_tag"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        (widgets::in_poly(x, y, &outer) && !widgets::in_poly(x, y, &inner))
            || widgets::in_disc(x, y, 27.5, 11.5, 3.0)
    });
}

/// 图层：一个菱形描边 + 下面两道"人字形"叠层。
///
/// 菱形拆成四条**独立**线段再加粗：闭合折线走 `thick` 时角上的斜接会交叉，
/// 填出来中间是空的。
fn draw_layers(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8) {
    // 菱形 = 外框挖内框（描边 5：外顶点 -1.7 / 内顶点 4.6 都是按 11.5×19.5 的解算出来的）
    let outer = [(19.5, -1.0), (39.5, 11.5), (19.5, 23.0), (0.0, 11.5)];
    let inner = [(19.5, 3.8), (30.8, 11.5), (19.5, 20.0), (8.6, 11.5)];
    // 下面两道人字形：45° 的臂，顶点在 (19.5,30.5) / (19.5,38.5)
    let arms = vec![
        widgets::thick(&[(1.5, 18.5), (19.5, 30.5), (37.5, 18.5)], 4.0),
        widgets::thick(&[(1.5, 26.5), (19.5, 38.5), (37.5, 26.5)], 4.0),
    ];
    widgets::shade_at(widgets::sid("draw_layers"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        (widgets::in_poly(x, y, &outer) && !widgets::in_poly(x, y, &inner))
            || arms.iter().any(|p| widgets::in_poly(x, y, p))
    });
}

/// 分享：左下角一个开口方框 + 右上穿出的箭头（折角箭头 = 横竖两根）。
fn draw_share(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8) {
    let shaft = widgets::thick(&[(21.5, 18.5), (34.5, 4.0)], 5.0);
    widgets::shade_at(widgets::sid("draw_share"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        widgets::in_rect(x, y, 0.0, 8.0, 12.0, 4.0)
            || widgets::in_rect(x, y, 0.0, 8.0, 4.0, 32.0)
            || widgets::in_rect(x, y, 0.0, 36.0, 32.0, 4.0)
            || widgets::in_rect(x, y, 28.0, 28.0, 4.0, 12.0)
            || widgets::in_rect(x, y, 20.0, 0.0, 20.0, 4.0)
            || widgets::in_rect(x, y, 36.0, 0.0, 4.0, 20.0)
            || widgets::in_poly(x, y, &shaft)
    });
}

/// 文档设置：一页纸 + 左边缘 4 道短横线（书脊）。
fn draw_docsettings(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8) {
    widgets::shade_at(widgets::sid("draw_docsettings"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        widgets::in_frame(x, y, 4.0, 0.0, 36.0, 39.4, 4.0)
            || [7.5, 17.5, 27.5, 35.5]
                .into_iter()
                .any(|ty| widgets::in_rect(x, y, 0.0, ty, 10.0, 4.0))
    });
}

// ---------------------------------------------------------------- 笔菜单的 9 个图标

fn draw_pen_icon(fb: &mut Fb, i: usize, ox: i32, oy: i32, fg: u8, bg: u8) {
    match i {
        0 => draw_nib(fb, ox, oy, fg, bg),
        1 => draw_fineliner(fb, ox, oy, fg, bg),
        2 => draw_marker(fb, ox, oy, fg, bg),
        3 => draw_pencil(fb, ox, oy, fg, bg),
        4 => draw_mech(fb, ox, oy, fg, bg),
        5 => draw_brush(fb, ox, oy, fg, bg),
        6 => draw_highlighter(fb, ox, oy, fg, bg),
        7 => draw_calligraphy(fb, ox, oy, fg, bg),
        _ => draw_texticon(fb, ox, oy, fg, bg),
    }
}

/// Fineliner：细长针管 + 上下两根横杠。
fn draw_fineliner(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8) {
    widgets::shade_at(widgets::sid("draw_fineliner"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        widgets::in_rect(x, y, 18.0, 0.0, 4.0, 12.0)
            || widgets::in_rect(x, y, 10.0, 12.0, 20.0, 4.0)
            || widgets::in_rect(x, y, 10.0, 16.0, 4.0, 4.0)
            || widgets::in_rect(x, y, 26.0, 16.0, 4.0, 4.0)
            || widgets::in_rect(x, y, 4.0, 20.0, 32.0, 4.0)
            || widgets::in_rect(x, y, 4.0, 24.0, 4.0, 16.0)
            || widgets::in_rect(x, y, 32.0, 24.0, 4.0, 16.0)
    });
}

/// Marker：宽而平的锥形笔尖 + 梯形的第二圈。
fn draw_marker(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8) {
    let tip = [(19.0, -0.5), (21.0, -0.5), (26.0, 11.5), (13.5, 11.5)];
    let bar2 = [(9.5, 19.7), (30.5, 19.7), (34.5, 24.0), (5.5, 24.0)];
    let left = vec![widgets::thick(&[(7.0, 23.5), (5.0, 27.0), (5.0, 40.5)], 4.0)];
    let legs = [left.clone(), mirror_x(&left)].concat();
    widgets::shade_at(widgets::sid("draw_marker"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        widgets::in_poly(x, y, &tip)
            || widgets::in_rect(x, y, 10.0, 12.0, 20.0, 4.0)
            || widgets::in_rect(x, y, 10.0, 16.0, 4.0, 4.0)
            || widgets::in_rect(x, y, 26.0, 16.0, 4.0, 4.0)
            || widgets::in_poly(x, y, &bar2)
            || legs.iter().any(|p| widgets::in_poly(x, y, p))
    });
}

/// Pencil：实心木质圆锥 + 中间的铅芯尖 + 三道笔杆。
fn draw_pencil(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8) {
    let cone = [(19.0, -0.5), (20.0, -0.5), (28.0, 12.0), (11.0, 12.0)];
    let lead = [(19.5, 11.0), (23.5, 12.2), (19.5, 13.6), (15.5, 12.2)];
    let left = vec![widgets::thick(&[(12.5, 12.0), (5.0, 19.5)], 4.5)];
    let edges = [left.clone(), mirror_x(&left)].concat();
    widgets::shade_at(widgets::sid("draw_pencil"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        widgets::in_poly(x, y, &cone)
            || widgets::in_poly(x, y, &lead)
            || edges.iter().any(|p| widgets::in_poly(x, y, p))
            || widgets::in_rect(x, y, 4.0, 20.0, 32.0, 4.0)
            || widgets::in_rect(x, y, 4.0, 24.0, 4.0, 16.0)
            || widgets::in_rect(x, y, 18.0, 24.0, 4.0, 16.0)
            || widgets::in_rect(x, y, 32.0, 24.0, 4.0, 16.0)
    });
}

/// Mechanical pencil：细长针管 + 带夹子的方领 + 下面张开的 V 形。
fn draw_mech(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8) {
    let collar = [(14.0, 16.0), (26.0, 16.0), (27.5, 20.0), (12.5, 20.0)];
    let left = vec![widgets::thick(&[(14.0, 19.5), (6.5, 30.0)], 4.6)];
    let v = [left.clone(), mirror_x(&left)].concat();
    widgets::shade_at(widgets::sid("draw_mech"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        widgets::in_rect(x, y, 18.0, 0.0, 4.0, 6.0)
            || widgets::in_rect(x, y, 14.0, 6.0, 12.0, 4.0)
            || widgets::in_rect(x, y, 14.0, 10.0, 4.0, 6.0)
            || widgets::in_rect(x, y, 22.0, 10.0, 4.0, 6.0)
            || widgets::in_poly(x, y, &collar)
            || v.iter().any(|p| widgets::in_poly(x, y, p))
            || widgets::in_rect(x, y, 4.0, 30.0, 32.0, 4.0)
            || widgets::in_rect(x, y, 4.0, 34.0, 4.0, 6.0)
            || widgets::in_rect(x, y, 32.0, 34.0, 4.0, 6.0)
    });
}

/// Paintbrush：水滴形的刷毛 + 细颈 + 箍 + 笔杆。
fn draw_brush(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8) {
    let blob = [
        (24.2, 0.0),
        (25.6, 3.0),
        (26.6, 8.0),
        (28.6, 11.0),
        (29.6, 15.0),
        (29.6, 19.0),
        (28.4, 22.0),
        (27.4, 25.8),
        (24.5, 25.8),
        (22.0, 25.8),
        (19.5, 25.8),
        (16.0, 25.8),
        (13.5, 25.8),
        (12.4, 23.0),
        (10.4, 20.0),
        (10.2, 16.0),
        (10.4, 12.0),
        (11.6, 8.0),
        (13.6, 4.0),
        (17.0, 1.4),
        (20.5, 0.2),
    ];
    let left = vec![widgets::thick(&[(13.5, 25.8), (12.0, 32.0)], 4.0)];
    let neck = [left.clone(), mirror_x(&left)].concat();
    widgets::shade_at(widgets::sid("draw_brush"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        widgets::in_poly(x, y, &blob)
            || neck.iter().any(|p| widgets::in_poly(x, y, p))
            || widgets::in_rect(x, y, 10.0, 32.0, 20.0, 4.0)
            || widgets::in_rect(x, y, 10.0, 36.0, 4.0, 4.0)
            || widgets::in_rect(x, y, 26.0, 36.0, 4.0, 4.0)
    });
}

/// Highlighter：凿形笔尖 + 方箍 + 里面一个 U 形（笔身凹口）。
fn draw_highlighter(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8) {
    let tip = [(14.0, -0.5), (15.4, -0.5), (28.5, 15.5), (10.5, 15.5)];
    widgets::shade_at(widgets::sid("draw_highlighter"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        widgets::in_poly(x, y, &tip)
            || widgets::in_rect(x, y, 10.0, 15.5, 4.0, 4.0)
            || widgets::in_rect(x, y, 26.0, 15.5, 4.0, 4.0)
            || widgets::in_rect(x, y, 4.0, 20.0, 32.0, 4.0)
            || widgets::in_rect(x, y, 4.0, 24.0, 4.0, 6.0)
            || widgets::in_rect(x, y, 32.0, 24.0, 4.0, 6.0)
            || widgets::in_rect(x, y, 12.0, 30.0, 16.0, 4.0)
            || widgets::in_rect(x, y, 12.0, 34.0, 4.0, 6.0)
            || widgets::in_rect(x, y, 24.0, 34.0, 4.0, 6.0)
    });
}

/// Calligraphy pen：两片向外张开的笔身 + 实心的斜笔尖 + 分叉的尖端。
fn draw_calligraphy(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8) {
    let left = [
        (16.9, -0.5),
        (18.6, -0.5),
        (18.6, 13.0),
        (16.0, 24.5),
        (9.8, 24.5),
        (13.2, 10.0),
    ];
    let right: Vec<(f32, f32)> = left.iter().map(|&(x, y)| (39.0 - x, y)).collect();
    let nib = [
        (10.5, 23.8),
        (28.5, 23.8),
        (26.5, 29.0),
        (22.0, 34.6),
        (17.0, 34.6),
        (13.0, 29.0),
    ];
    let l = vec![widgets::thick(&[(13.0, 33.5), (12.0, 40.0)], 4.0)];
    let legs = [l.clone(), mirror_x(&l)].concat();
    widgets::shade_at(widgets::sid("draw_calligraphy"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        widgets::in_poly(x, y, &left)
            || widgets::in_poly(x, y, &right)
            || widgets::in_poly(x, y, &nib)
            || legs.iter().any(|p| widgets::in_poly(x, y, p))
    });
}

/// Text：衬线体大写 T。
fn draw_texticon(fb: &mut Fb, ox: i32, oy: i32, fg: u8, bg: u8) {
    let foot = [(17.0, 33.5), (22.0, 33.5), (27.0, 37.5), (12.0, 37.5)];
    widgets::shade_at(widgets::sid("draw_texticon"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        widgets::in_rect(x, y, 4.0, 2.0, 32.0, 4.0)
            || widgets::in_rect(x, y, 17.0, 2.0, 6.0, 32.0)
            || widgets::in_poly(x, y, &[(4.0, 5.0), (8.0, 5.0), (7.0, 11.0), (4.0, 11.0)])
            || widgets::in_poly(x, y, &[(35.0, 5.0), (31.0, 5.0), (32.0, 11.0), (35.0, 11.0)])
            || widgets::in_poly(x, y, &foot)
    });
}

// ---------------------------------------------------------------- 橡皮菜单 / 粗细菜单的小图标

fn draw_erase_icon(fb: &mut Fb, i: usize, ox: i32, oy: i32, fg: u8, bg: u8) {
    match i {
        // 普通橡皮 / 擦整笔：斜放的橡皮
        0 | 1 => draw_eraser(fb, ox, oy, fg, bg, false),
        // 擦选区：虚线框 + 橡皮（= 官方工具条那一格）
        2 => draw_eraser(fb, ox, oy, fg, bg, true),
        // 全清：纸 + 叉
        _ => {
            let x1 = widgets::thick(&[(13.0, 15.0), (27.0, 31.0)], 4.0);
            let x2 = widgets::thick(&[(27.0, 15.0), (13.0, 31.0)], 4.0);
            widgets::shade_at(widgets::sid("draw_erase_icon"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
                widgets::in_frame(x, y, 3.0, 3.0, 34.0, 34.0, 4.0)
                    || widgets::in_poly(x, y, &x1)
                    || widgets::in_poly(x, y, &x2)
            });
        }
    }
}

/// 粗细菜单左边的笔画样本：一根 45° 斜的笔画，两头细、中间粗。
/// 官方三档实测（水平截面）：Thin 6px、Medium 8px、Thick 17px。
fn draw_stroke_sample(fb: &mut Fb, ox: i32, oy: i32, i: usize, fg: u8, bg: u8) {
    let (ax, ay, bx, by, wmax) = [
        (35.5f32, 0.0f32, 2.0f32, 38.0f32, 4.2f32),
        (34.0, 0.0, 5.5, 38.0, 5.7),
        (32.5, 0.0, 4.5, 38.5, 12.0),
    ][i.min(2)];
    widgets::shade_at(widgets::sid("draw_stroke_sample"), fb, ox, oy, ICON, ICON, fg, bg, |x, y| {
        let (dx, dy) = (bx - ax, by - ay);
        let l2 = dx * dx + dy * dy;
        let t = (((x - ax) * dx + (y - ay) * dy) / l2).clamp(0.0, 1.0);
        widgets::dist_seg(x, y, ax, ay, bx, by) <= wmax * 0.5 * taper(t)
    });
}

/// 笔画沿长度方向的收缩：两头细、中间最粗（官方样本就是这个手感）。
fn taper(t: f32) -> f32 {
    let s = (t * std::f32::consts::PI).sin();
    0.18 + 0.82 * s.sqrt()
}

/// 颜色菜单左边的圆点：外径 40 的描边圆 + 内部的色块。
///
/// 官方色点中心在 (166.5, 行顶边+54.5)：比 40×40 的图标框往左上各出去几像素，
/// 所以这里单独给它一个 44×44 的框，不然会被框边切掉一块。
///
/// `gray` 是这个颜色在灰度面板上的样子（官方色点 PNG 本身就是灰度的）。
fn draw_color_dot(fb: &mut Fb, ox: i32, oy: i32, gray: u8, fg: u8, bg: u8) {
    let (cx, cy) = (22.5f32, 22.5f32);
    widgets::shade_at(widgets::sid("color_dot_fill"), fb, ox, oy, 44, 44, gray, bg, |x, y| {
        widgets::in_disc(x, y, cx, cy, 16.0)
    });
    widgets::shade_at(widgets::sid("color_dot_ring"), fb, ox, oy, 44, 44, fg, bg, |x, y| {
        widgets::in_ring(x, y, cx, cy, 20.0, 4.0)
    });
}

/// 荧光笔的色点：官方是一枚**纯色实心圆**（没有描边圆环）。
///
/// 最后那档 "Gray" 的右半边会再压一层 20% 淡黑——官方用它表示"这一档是叠加变深的"。
fn draw_hl_dot(fb: &mut Fb, ox: i32, oy: i32, gray: u8) {
    let (cx, cy) = (ox + 22, oy + 22);
    widgets::disc(fb, cx, cy, 16, gray);
    // 右半边（只覆盖圆内）：gray * 0.8
    let dark = (gray as u16 * 4 / 5) as u8;
    for dy in -16..=16 {
        for dx in 0..=16 {
            if dx * dx + dy * dy <= 16 * 16 {
                fb.put(cx + dx, cy + dy, gray565(dark));
            }
        }
    }
}

/// 硬度档位的圆点：内部就是**面板上会看到的灰度**（覆盖率越高越黑）。
///
/// 不用 `shade_at`：它的缓存键含 tone，7 个档位各算一次 4×4 超采样要 8ms，
/// 而这里要的就是一个实心圆，`widgets::disc` 直接落像素（几十微秒）。
fn draw_grade_dot(fb: &mut Fb, ox: i32, oy: i32, tone: u8, fg: u8, bg: u8) {
    let (cx, cy) = (ox + 22, oy + 22);
    widgets::disc(fb, cx, cy, 16, 255 - tone);
    widgets::shade_at(widgets::sid("color_dot_ring"), fb, ox, oy, 44, 44, fg, bg, |x, y| {
        widgets::in_ring(x, y, 22.5, 22.5, 20.0, 4.0)
    });
}

// ---------------------------------------------------------------- 图标预热

/// 启动时把所有图标变体先光栅化一遍（在第一帧刷新之前调用）。
///
/// 一张 40×40 的图标 = 1600 像素 × 4×4 超采样 + 多边形命中，在这颗 ARM 上
/// 要 3~5ms。12 格工具条第一次画是 55ms、9 行笔菜单第一次开再加 48ms ——
/// 用户看到的就是"第一次点工具条/第一次开菜单要顿半秒"。
///
/// 变体集合是有限的（普通 / 反白 / 不可用三套色），所以在第一次刷屏之前
/// 全部算进缓存，之后每次重绘只剩一次贴图（实测整条工具条 1~2ms）。
///
/// 会往帧缓冲左上角乱画，但**必须在第一帧 refresh 之前**调，否则会闪一下。
pub fn prewarm(fb: &mut Fb) {
    let (ox, oy) = (0, 0);
    for (fg, bg) in [(0u8, 255u8), (255, 0), (150, 255)] {
        draw_ring(fb, ox, oy, fg, bg, true);
        draw_ring(fb, ox, oy, fg, bg, false);
        draw_nib(fb, ox, oy, fg, bg);
        draw_thickness(fb, ox, oy, fg, bg);
        draw_eraser(fb, ox, oy, fg, bg, true);
        draw_eraser(fb, ox, oy, fg, bg, false);
        draw_selection(fb, ox, oy, fg, bg);
        draw_undo(fb, ox, oy, fg, bg, false);
        draw_undo(fb, ox, oy, fg, bg, true);
        draw_overview(fb, ox, oy, fg, bg);
        draw_tag(fb, ox, oy, fg, bg);
        draw_layers(fb, ox, oy, fg, bg);
        draw_share(fb, ox, oy, fg, bg);
        draw_docsettings(fb, ox, oy, fg, bg);
        draw_texticon(fb, ox, oy, fg, bg);
        for i in 0..TOOLS.len() {
            draw_pen_icon(fb, i, ox, oy, fg, bg);
        }
        for i in 0..ERASE_ITEMS.len() {
            draw_erase_icon(fb, i, ox, oy, fg, bg);
        }
        for i in 0..THICKNESSES.len() {
            draw_stroke_sample(fb, ox, oy, i, fg, bg);
        }
        for (_, _, gray) in COLORS.iter().chain(HIGHLIGHTER_COLORS.iter()) {
            draw_color_dot(fb, ox, oy, *gray, fg, bg);
        }
    }
}

// ---------------------------------------------------------------- 撤销栈

/// 撤销栈里的一步。
///
/// 为什么不是原来的 `Vec<(usize, Line)>`：一次「剪切 7 笔」必须一次撤销掉。也不能简化成
/// `One(usize, Line) / Many(Vec<(usize, Line)>)` 两种——「新落一笔/粘贴」和「剪切」的
/// 撤销方向相反（前者要删掉刚插进来的，后者要按原位插回去），得能区分插入与删除。
///
/// 放在 chrome.rs 是因为 Undo/Redo 本来就是工具条的两个按钮；而且本文件在 lib 里，
/// `apply`/`revert` 可以完全脱离帧缓冲单测（`main.rs` 是 bin，测试不算进 `--lib`）。
#[derive(Clone, Debug)]
pub enum Op {
    /// 在某层第 `index` 笔之前插入了这些笔迹：新落的一笔、粘贴都是这个。
    /// 撤销 = 把这一段删掉；重做 = 再插回去。
    Add {
        layer: usize,
        index: usize,
        lines: Vec<Line>,
    },
    /// 从某层摘掉了这些笔迹（剪切）。`items` 按**原下标升序**——
    /// 撤销要按原下标一笔笔插回去，顺序和位置都不能变。
    Remove {
        layer: usize,
        items: Vec<(usize, Line)>,
    },
    /// 拖动：某层 `idx` 这些笔迹整体平移了 `(dx, dy)`。
    Move {
        layer: usize,
        idx: Vec<usize>,
        dx: f32,
        dy: f32,
    },
}

impl Op {
    /// 只给日志用
    pub fn label(&self) -> &'static str {
        match self {
            Op::Add { .. } => "add",
            Op::Remove { .. } => "remove",
            Op::Move { .. } => "move",
        }
    }

    /// 重做。下标对不上就返回 `false` 并且**一笔都不动**，宁可不做也不能删错笔迹。
    pub fn apply(&self, layers: &mut [Layer]) -> bool {
        match self {
            Op::Add {
                layer,
                index,
                lines,
            } => {
                let Some(l) = layers.get_mut(*layer) else {
                    return false;
                };
                let at = (*index).min(l.lines.len());
                let tail = l.lines.split_off(at);
                l.lines.extend(lines.iter().cloned());
                l.lines.extend(tail);
                true
            }
            Op::Remove { layer, items } => {
                let Some(l) = layers.get_mut(*layer) else {
                    return false;
                };
                if items.iter().any(|(i, _)| *i >= l.lines.len()) {
                    return false;
                }
                // 升序记录 → 倒着删，前面的下标才不会漂
                for &(i, _) in items.iter().rev() {
                    l.lines.remove(i);
                }
                true
            }
            Op::Move {
                layer,
                idx,
                dx,
                dy,
            } => {
                let Some(l) = layers.get_mut(*layer) else {
                    return false;
                };
                crate::select::translate(&mut l.lines, idx, *dx, *dy);
                true
            }
        }
    }

    /// 撤销。和 [`Op::apply`] 严格互逆：`revert` 之后再 `apply` 回到同一状态。
    pub fn revert(&self, layers: &mut [Layer]) -> bool {
        match self {
            Op::Add {
                layer,
                index,
                lines,
            } => {
                let Some(l) = layers.get_mut(*layer) else {
                    return false;
                };
                // 这里不夹紧：夹紧等于删掉别人的笔迹
                let at = *index;
                if at > l.lines.len() || at + lines.len() > l.lines.len() {
                    return false;
                }
                l.lines.drain(at..at + lines.len());
                true
            }
            Op::Remove { layer, items } => {
                let Some(l) = layers.get_mut(*layer) else {
                    return false;
                };
                for (i, line) in items {
                    let at = (*i).min(l.lines.len());
                    l.lines.insert(at, line.clone());
                }
                true
            }
            Op::Move {
                layer,
                idx,
                dx,
                dy,
            } => {
                let Some(l) = layers.get_mut(*layer) else {
                    return false;
                };
                crate::select::translate(&mut l.lines, idx, -*dx, -*dy);
                true
            }
        }
    }
}

/// 左右镜像（局部坐标 40 宽，轴在 19.5）。
fn mirror_x(ps: &[Vec<(f32, f32)>]) -> Vec<Vec<(f32, f32)>> {
    ps.iter()
        .map(|p| p.iter().map(|&(x, y)| (39.0 - x, y)).collect())
        .collect()
}

// ---------------------------------------------------------------- 交互小件（P13）
//
// 这一段的文案全部是官方 QML 原文（注释里给了文件和行号），布局/命中都是纯函数，
// 单测只需要喂数字。`main.rs` 只负责调它们和刷屏。

/// 剪贴板提示条的高度。官方 `Values.navigatorStatusbarHeight` = 72
/// （`res_06ba1de.qml:52`，提示条 `notifierBackground` 用的就是它）。
pub const TOAST_H: i32 = 72;

/// 复制之后的提示条文案。官方 `res_067887b.qml:2119`
/// （`Clipboard.onItemsChanged` 里赋给 `clipboardText`）。**整句照抄**，
/// 包括后面那句 "Tap any page with Marker to paste." —— 它就是"点一下粘贴"的说明。
pub const CLIP_COPIED: &str = "Copied to clipboard. Tap any page with Marker to paste.";

/// 提示条右边的按钮（官方 `res_067887b.qml:1751` `IconButton { title: ... }`）。
pub const CLIP_CLEAR: &str = "Clear clipboard";

/// 剪贴板提示条该不该显示：剪贴板里有东西、而且人还在选区（套索）模式。
///
/// 官方是 `clipboardNotificationVisible && !selectionHandler.visible`
/// （`res_067887b.qml:1679/1722`），而 `clipboardNotificationVisible` 只在
/// 复制时置真、切走选区工具就置假 —— 所以等价于"有内容 + 还在选区模式"。
/// 点了 `Clear clipboard` 之后 `has_clip` 就是 false，这一条跟着消失。
pub fn clip_toast_visible(has_clip: bool, selecting: bool) -> bool {
    has_clip && selecting
}

/// 提示条整条贴底的矩形（`x, y, w, h`）。
pub fn clip_toast_rect(w: i32, h: i32) -> (i32, i32, i32, i32) {
    (0, h - TOAST_H, w, TOAST_H)
}

/// `Clear clipboard` 命中区的宽度（从屏幕右边量）。
///
/// 官方那一串从右边排：1px 竖线 + `IconButton`（40px 图标 + 文字 + 两个 gutter）。
/// "Clear clipboard" 在 28 号字下约 200px，加图标和边距取 340 —— 手指点得到，
/// 也不会压到左边那句提示。
const CLIP_CLEAR_W: i32 = 340;

/// `Clear clipboard` 的命中区：提示条右端那一块，整条高。
pub fn clip_clear_rect(w: i32, h: i32) -> (i32, i32, i32, i32) {
    let (_, y, _, th) = clip_toast_rect(w, h);
    (w - CLIP_CLEAR_W, y, CLIP_CLEAR_W, th)
}

/// 分享面板第 `row` 行的文案。官方：屏幕共享那行在
/// `res_067e8cc.qml:103`（`!ScreenShare.serverActive ? Strings.screenShareString : "Stop sharing your screen"`），
/// 另两个串来自 `frag_0081_006bd97e.qml:8/10`。
pub fn share_label(row: usize, screen_share_on: bool) -> &'static str {
    match (row, screen_share_on) {
        (0, true) => "Stop sharing your screen",
        (0, false) => "Present with Screen Share",
        (1, _) => "Send by email",
        _ => "Convert notes to text",
    }
}

/// 排序下拉的按钮文字。官方 `res_06a7343.qml:18/71`：
/// `text: dropdown.isOpen ? expandedText : closedText`，`expandedText: qsTr("Sort by")`。
/// 收起时还是当前的排序名。
pub fn sort_button_label(open: bool, closed: &'static str) -> &'static str {
    if open {
        "Sort by"
    } else {
        closed
    }
}

/// 按词折行，语义是官方 `ToolButton` 的 `Text.WrapAtWordBoundaryOrAnywhere`
/// （`res_064c683.qml:87`、`res_067e8cc.qml:175` 的 `nameLineHeigt: 1.2`）。
///
/// 宽度由调用方给的 `measure` 量（我们只有 `Text` 会量像素），所以这个函数
/// 本身是纯的：同样的输入 + 同样的量法，结果一定一样，单测喂个假量法就行。
/// 单个词比 `max_w` 还宽时独占一行（真机没有这种词，不为此加断词逻辑）。
pub fn wrap_words<F>(text: &str, max_w: f32, measure: &mut F) -> Vec<String>
where
    F: FnMut(&str) -> f32,
{
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in text.split_whitespace() {
        let cand = if cur.is_empty() {
            word.to_string()
        } else {
            format!("{cur} {word}")
        };
        if !cur.is_empty() && measure(&cand) > max_w {
            out.push(cur);
            cur = word.to_string();
        } else {
            cur = cand;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    if out.is_empty() {
        // 空串也要给一行，调用方不用特判
        out.push(String::new());
    }
    out
}

/// 标签面板最后一行的文字。官方 `res_067497c.qml:349`
/// （`IconButton { icon: icon_plus_circle; title: qsTr("Create new tag") }`）。
pub const TAG_CREATE_NEW: &str = "Create new tag";

/// 标签面板的高度：`n` 个标签行 + 最后一行 `Create new tag`，行高 `row`。
///
/// 面板是"底边贴着标签按钮往上长"的（`TAG_BTN_BOTTOM`），所以高度定了位置就定了。
pub fn tags_panel_height(n_tags: usize, row: i32) -> i32 {
    (n_tags as i32 + 1) * row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pen::{Color, Point, Tool};

    /// 造一笔：笔宽固定 2.0，点的 x 就是它的"编号"，方便断言顺序。
    fn line(x: f32) -> Line {
        let mut l = Line::new(Tool::BallpointV2, Color::Black, 2.0);
        l.push(Point {
            x,
            y: 0.0,
            width: 2.0,
            ..Default::default()
        });
        l
    }

    /// 一层里放 n 笔，第 i 笔的 x = i*10
    fn layer_with(n: usize) -> Layer {
        let mut l = Layer::default();
        for i in 0..n {
            l.lines.push(line(i as f32 * 10.0));
        }
        l
    }

    fn xs(l: &Layer) -> Vec<f32> {
        l.lines.iter().map(|l| l.points[0].x).collect()
    }

    #[test]
    fn add_op_roundtrip() {
        let mut layers = vec![layer_with(2)];
        let op = Op::Add {
            layer: 0,
            index: 1,
            lines: vec![line(99.0), line(98.0)],
        };
        assert!(op.apply(&mut layers));
        assert_eq!(xs(&layers[0]), vec![0.0, 99.0, 98.0, 10.0]);
        assert!(op.revert(&mut layers));
        assert_eq!(xs(&layers[0]), vec![0.0, 10.0]);
        // 已经撤销过了，再撤一次不能顺手删掉别人的笔
        assert!(!op.revert(&mut layers));
        assert_eq!(layers[0].lines.len(), 2);
        // 粘贴到末尾（index == len）也要能来回
        let op = Op::Add {
            layer: 0,
            index: 2,
            lines: vec![line(7.0)],
        };
        assert!(op.apply(&mut layers));
        assert_eq!(xs(&layers[0]), vec![0.0, 10.0, 7.0]);
        assert!(op.revert(&mut layers));
        assert_eq!(xs(&layers[0]), vec![0.0, 10.0]);
    }

    #[test]
    fn remove_op_restores_index_and_order() {
        let mut layers = vec![layer_with(5)];
        let items: Vec<(usize, Line)> = [1usize, 3]
            .iter()
            .map(|&i| (i, layers[0].lines[i].clone()))
            .collect();
        let op = Op::Remove { layer: 0, items };
        assert!(op.apply(&mut layers));
        assert_eq!(xs(&layers[0]), vec![0.0, 20.0, 40.0]);
        // 撤销：剪掉的笔回原位、顺序不变
        assert!(op.revert(&mut layers));
        assert_eq!(xs(&layers[0]), vec![0.0, 10.0, 20.0, 30.0, 40.0]);
        // 重做：再删掉同样两条
        assert!(op.apply(&mut layers));
        assert_eq!(xs(&layers[0]), vec![0.0, 20.0, 40.0]);
    }

    #[test]
    fn op_refuses_stale_indices() {
        let mut layers = vec![layer_with(2)];
        // 笔迹下标越界
        let op = Op::Remove {
            layer: 0,
            items: vec![(5, line(0.0))],
        };
        assert!(!op.apply(&mut layers));
        assert_eq!(layers[0].lines.len(), 2);
        // 图层下标越界
        let op = Op::Remove {
            layer: 9,
            items: vec![(0, line(0.0))],
        };
        assert!(!op.apply(&mut layers));
        assert!(!op.revert(&mut layers));
        assert_eq!(layers.len(), 1);
    }

    #[test]
    fn move_op_roundtrip() {
        let mut layers = vec![layer_with(3)];
        let before = xs(&layers[0]);
        let op = Op::Move {
            layer: 0,
            idx: vec![0, 2],
            dx: 7.0,
            dy: -4.0,
        };
        assert!(op.apply(&mut layers));
        assert_eq!(layers[0].lines[0].points[0].x, 7.0);
        assert_eq!(layers[0].lines[2].points[0].y, -4.0);
        // 没选中的那笔不动
        assert_eq!(layers[0].lines[1].points[0].x, 10.0);
        // bounds 跟着走（select::translate 会重算）
        assert_eq!(layers[0].lines[0].bounds.x, 6.0);
        assert!(op.revert(&mut layers));
        assert_eq!(xs(&layers[0]), before);
        assert_eq!(layers[0].lines[0].bounds.x, -1.0);
    }

    #[test]
    fn slots_are_in_official_order() {
        let c = Chrome::default();
        // 逐格：收起 / 笔 / 粗细 / 橡皮 / 选区 / 撤销 / 重做
        assert_eq!(c.hit(60.0, 60.0), Some(Msg::Hide));
        assert_eq!(c.hit(60.0, 180.0), Some(Msg::PenMenu));
        assert_eq!(c.hit(60.0, 300.0), Some(Msg::SettingsMenu));
        assert_eq!(c.hit(60.0, 420.0), Some(Msg::Eraser));
        assert_eq!(c.hit(60.0, 540.0), Some(Msg::Selection));
        assert_eq!(c.hit(60.0, 660.0), Some(Msg::Undo));
        assert_eq!(c.hit(60.0, 780.0), Some(Msg::Redo));
        // 第 13 格：框选问 AI（官方 12 格之外新加，落在 840..960 的空段里）
        assert_eq!(c.hit(60.0, 900.0), Some(Msg::AiSelect));
        // 新格子只占 120 px：上下邻格一个像素都不许被它抢走
        assert_eq!(c.hit(60.0, 839.0), Some(Msg::Redo), "重做那格还是重做");
        assert_eq!(c.hit(60.0, 840.0), Some(Msg::AiSelect), "新格子顶边");
        assert_eq!(c.hit(60.0, 959.0), Some(Msg::AiSelect), "新格子底边前一像素");
        // 第 14 格：整页 AI 引用列表（960..1080）
        assert_eq!(c.hit(60.0, 960.0), Some(Msg::AiRefs), "引用列表格顶边");
        assert_eq!(c.hit(60.0, 1079.0), Some(Msg::AiRefs), "引用列表格底边前一像素");
        // 第 15 格：横屏开关（1080..1200）
        assert_eq!(c.hit(60.0, 1080.0), Some(Msg::Landscape), "横屏格顶边");
        assert_eq!(c.hit(60.0, 1199.0), Some(Msg::Landscape), "横屏格底边前一像素");
        // 1200..1272 仍是空的
        assert_eq!(c.hit(60.0, 1200.0), None);
        assert_eq!(c.hit(60.0, 1271.0), None);
        // 新格子在 120 宽的条里，条右边不算
        assert_eq!(c.hit(120.0, 900.0), None);
        // 下列 5 格
        assert_eq!(c.hit(60.0, 1300.0), Some(Msg::PageOverview));
        assert_eq!(c.hit(60.0, 1450.0), Some(Msg::Tags));
        assert_eq!(c.hit(60.0, 1570.0), Some(Msg::Layers));
        assert_eq!(c.hit(60.0, 1690.0), Some(Msg::Share));
        assert_eq!(c.hit(60.0, 1800.0), Some(Msg::DocSettings));
        // 条右边是纸面，不能被当成按钮吃掉
        assert_eq!(c.hit(200.0, 400.0), None);
        assert_eq!(c.hit(120.0, 400.0), None);
    }

    /// 第 13 格真的画出了东西，而且激活时整格反白（和别格同一套语义）。
    /// 第 14 / 15 格的图标：三行条目 + sparkle ／ 一张横纸。
    ///
    /// 用户报"AI 引用那个图案看起来很怪异"——旧画法是 1.6px 的虚线框加一条
    /// 从框外扎进来的箭头，40×40 里全是断点。这两个断言钉住新画法的结构：
    /// 行的位置有墨、行与行之间是白的、星心有墨；横纸的上下边有墨、纸外是白的。
    #[test]
    fn ai_refs_and_landscape_icons_draw() {
        let mut fb = Fb::open_mem();
        let mut text = None;
        let c = Chrome::default();
        c.draw_in(&mut fb, &mut text);
        let g = geom();
        let ox = W / 2 - ICON / 2;
        let oy = g.ai_y + g.cell + g.cell / 2 - ICON / 2;
        // 三行条目：局部 (3..26, 8.5..11.5 / 18.5..21.5 / 28.5..31.5)
        assert!(fb.get(ox + 6, oy + 10) < 128, "第一行条目要黑");
        assert!(fb.get(ox + 6, oy + 15) > 200, "行与行之间要白");
        assert!(fb.get(ox + 6, oy + 29) < 128, "第三行条目要黑");
        // sparkle：局部中心 (32.5, 20)
        assert!(fb.get(ox + 32, oy + 20) < 128, "星心要黑");
        assert!(fb.get(ox + 39, oy + 2) > 200, "右上角是白的");
        // 第 15 格：横纸（局部边框 y 9.4..30.6、x 2.6..37.4）
        let oy = g.land_y + g.cell / 2 - ICON / 2;
        assert!(fb.get(ox + 10, oy + 10) < 128, "纸的上边要黑");
        assert!(fb.get(ox + 10, oy + 29) < 128, "纸的下边要黑");
        assert!(fb.get(ox + 10, oy + 4) > 200, "纸外面是白的");
    }

    /// 横屏把格子压成 `高度/15`：竖屏那 15 格的顺序不变，底部 5 格仍贴底。
    #[test]
    fn geom_in_portrait_keeps_official_grid() {
        let g = geom();
        assert_eq!(g.cell, 120);
        assert_eq!(g.top0, 120);
        assert_eq!(g.ai_y, 840);
        assert_eq!(g.land_y, 1080);
        assert_eq!(g.bottom0, 1872 - 600);
        // 第 15 格不能盖住底部那 5 格
        assert!(g.land_y + g.cell <= g.bottom0);
    }

    #[test]
    fn ai_cell_draws_and_inverts_when_active() {
        let mut c = Chrome::default();
        let mut fb = Fb::open_mem();
        let mut text = None;
        c.draw_in(&mut fb, &mut text);
        let mut ink = 0;
        for y in ai_y()..ai_y() + 120 {
            for x in 0..W {
                if fb.get(x, y) < 128 {
                    ink += 1;
                }
            }
        }
        assert!(ink > 120, "第 13 格要有图标（{ink} px）");
        assert!(ink < 4000, "别糊成一整块黑（{ink} px）");
        // 图标居中的那一块：框线在 x≈41 起，星心在正中间
        assert!(fb.get(43, ai_y() + 60) < 128, "左边的框线要黑");
        assert!(fb.get(60, ai_y() + 60) < 128, "星心要黑");
        // 空格子处是白的（说明只占了 120 高，没画到邻格）
        assert!(fb.get(60, ai_y() - 10) > 200, "上一格（重做）底边还是白的");
        assert!(fb.get(60, ai_y() + 130) > 200, "下一格是空的");

        // 激活：整格黑底 + 图标反白
        c.ask_ai = true;
        c.draw_in(&mut fb, &mut text);
        assert!(fb.get(10, ai_y() + 10) < 40, "激活态整格黑底");
        assert!(fb.get(43, ai_y() + 60) > 200, "激活后图标反白");
    }

    #[test]
    fn collapsed_only_has_the_corner_cell() {
        let mut c = Chrome::default();
        c.visible = false;
        // 收起后只剩左上角一格能点回展开
        assert_eq!(c.hit(60.0, 60.0), Some(Msg::Show));
        assert_eq!(c.hit(119.0, 119.0), Some(Msg::Show));
        assert_eq!(c.hit(60.0, 180.0), None);
        assert_eq!(c.hit(200.0, 60.0), None);
        // 屏幕中线那个"把手"已经废掉了
        assert_eq!(c.hit(10.0, 936.0), None);
    }

    #[test]
    fn menu_hit_uses_panel_top_of_its_own_cell() {
        let mut c = Chrome::default();
        // 笔菜单：面板 120..520，行高 120，顶边对齐笔那一格
        c.menu = Menu::Pens;
        assert_eq!(c.hit(200.0, 130.0), Some(Msg::Tool(0)));
        assert_eq!(c.hit(200.0, 250.0), Some(Msg::Tool(1)));
        assert_eq!(c.hit(200.0, 1190.0), Some(Msg::Tool(8)));
        assert_eq!(c.hit(200.0, 1210.0), Some(Msg::Dismiss));
        // 面板右边是纸面：菜单开着也算"点空白"
        assert_eq!(c.hit(600.0, 300.0), Some(Msg::Dismiss));
        // 粗细菜单：表头 45 之后才是三档，再隔一个表头是三色
        c.menu = Menu::Settings;
        assert_eq!(c.hit(200.0, 250.0), Some(Msg::Dismiss));
        assert_eq!(c.hit(200.0, 300.0), Some(Msg::Thickness(1.0)));
        assert_eq!(c.hit(200.0, 420.0), Some(Msg::Thickness(2.0)));
        assert_eq!(c.hit(200.0, 540.0), Some(Msg::Thickness(3.0)));
        // 粗细段最后一行是羽化开关（我们自己加的第 4 行）
        assert_eq!(c.hit(200.0, 660.0), Some(Msg::Feather));
        assert_eq!(c.hit(200.0, 820.0), Some(Msg::Color(Color::Black)));
        assert_eq!(c.hit(200.0, 940.0), Some(Msg::Color(Color::Gray)));
        assert_eq!(c.hit(200.0, 1060.0), Some(Msg::Color(Color::White)));
        assert_eq!(c.hit(200.0, 1180.0), Some(Msg::Dismiss));
        // 橡皮菜单：4 行（刷白 / 擦整笔 / 擦选区 / 清层），顶边贴橡皮那一格
        c.menu = Menu::Erase;
        assert_eq!(c.hit(200.0, 370.0), Some(Msg::EraseBrush));
        assert_eq!(c.hit(200.0, 490.0), Some(Msg::EraseStroke));
        assert_eq!(c.hit(200.0, 610.0), Some(Msg::EraseSection));
        assert_eq!(c.hit(200.0, 730.0), Some(Msg::ClearLayer));
        assert_eq!(c.hit(200.0, 850.0), Some(Msg::Dismiss));
    }

    #[test]
    #[test]
    fn feather_row_only_for_solid_pens() {
        let mut c = Chrome::default();
        c.menu = Menu::Settings;
        // 圆珠笔/细头笔/书法笔：粗细三档下面多一行羽化
        for tool in [Tool::BallpointV2, Tool::FinelinerV2, Tool::Calligraphy] {
            let i = TOOLS.iter().position(|(t, _)| *t == tool).unwrap();
            c.selected = i;
            assert!(c.has_feather(c.tool()), "{tool:?} 应该有羽化开关");
            let rows = c.settings_sections()[0].1;
            assert_eq!(rows, THICKNESSES.len() as i32 + 1);
            assert_eq!(c.settings_row(Sec::Stroke, THICKNESSES.len()).map(|r| r.3), Some(120));
        }
        // 铅笔（网点）和文字工具不给：给了也看不出效果
        for tool in [Tool::PencilV2, Tool::SharpPencilV2, Tool::Undefined] {
            let i = TOOLS.iter().position(|(t, _)| *t == tool).unwrap();
            c.selected = i;
            assert!(!c.has_feather(c.tool()), "{tool:?} 不该有羽化开关");
        }
    }

    fn tool_is_text_only_in_the_menu() {
        let mut c = Chrome::default();
        assert_eq!(c.tool(), Tool::BallpointV2);
        c.selected = TOOLS.len() - 1;
        assert_eq!(c.tool(), Tool::Undefined);
        c.erasing = true;
        assert_eq!(c.tool(), Tool::Eraser);
    }
}

#[cfg(test)]
mod label_fit {
    use super::*;

    /// 菜单标签必须画得进面板。
    ///
    /// 踩过的坑：`Mechanical pencil` 在 `MENU_FONT = 33` 时墨迹写到 x=420
    /// （面板只有 400 宽），右边两个字母漏在面板外；收起菜单时只有面板本体
    /// 在重绘矩形里，那半截 "il" 就永久留在纸上。字号已经按官方的墨迹宽度
    /// 重新标定过（见 `MENU_FONT` 的注释），这条测试把"放得下"钉住。
    ///
    /// 量的是**推进宽度**（比墨迹还宽 1~2px），所以判据偏严。没有字体文件的
    /// 环境直接跳过 —— 真机和开发机都有。
    #[test]
    fn every_menu_label_fits_the_panel() {
        let Some(mut t) = crate::ui::text::Text::load() else {
            eprintln!("没有字体，跳过");
            return;
        };
        // 右边至少留 8px：面板边框 2px + 人眼看得到的空隙
        let limit = (MENU_W - 8 - MENU_TEXT_DX) as f32;
        let mut worst = (0.0f32, "");
        for name in TOOLS
            .iter()
            .map(|(_, n)| *n)
            .chain(ERASE_ITEMS.iter().map(|(n, _)| *n))
            .chain(DOC_ITEMS.iter().map(|(n, _)| *n))
        {
            let w = t.width(MENU_FONT, name);
            if w > worst.0 {
                worst = (w, name);
            }
            assert!(
                w <= limit,
                "{name:?} 宽 {w:.0}px，面板只剩 {limit:.0}px —— 会溢出到纸面上"
            );
        }
        eprintln!("最宽的标签是 {:?}（{:.0}px / 上限 {limit:.0}px）", worst.1, worst.0);
    }

    /// 收起菜单的重绘矩形要**盖住面板外那一圈**。
    ///
    /// 以前 `menu_rect()` 正好等于面板本体，任何画到框外的东西（溢出的文字、
    /// 将来可能加的阴影）都会留在纸上。现在四周各多给一圈。
    #[test]
    fn menu_rect_covers_a_margin_around_the_panel() {
        let mut c = Chrome::default();
        c.menu = Menu::Pens;
        let (x, y, w, h) = c.menu_rect().unwrap();
        assert!(x < MENU_X, "左边没余量：x={x}");
        assert!(y <= c.menu_top(), "上边没余量：y={y}");
        assert!(
            x + w >= MENU_X + MENU_W + MENU_RECT_EXTRA_R,
            "右边余量不够：{} < {}",
            x + w,
            MENU_X + MENU_W + MENU_RECT_EXTRA_R
        );
        assert!(y + h >= c.menu_top() + c.menu_h(), "下边没余量");
    }

    // ---- P13：交互小件的纯函数 --------------------------------------------

    /// 剪贴板提示条：文案是官方原文，贴底 72 高，只在"有内容 + 选区模式"时出现。
    #[test]
    fn clip_toast_text_and_visibility() {
        assert_eq!(
            CLIP_COPIED,
            "Copied to clipboard. Tap any page with Marker to paste."
        );
        assert_eq!(CLIP_CLEAR, "Clear clipboard");
        assert!(clip_toast_visible(true, true));
        assert!(!clip_toast_visible(true, false), "切走选区工具就不该再提示");
        assert!(!clip_toast_visible(false, true), "清空剪贴板后提示要消失");
        let (x, y, w, h) = clip_toast_rect(1404, 1872);
        assert_eq!((x, y, w, h), (0, 1872 - 72, 1404, 72), "提示条贴底整条宽");
    }

    /// `Clear clipboard` 的命中区在提示条右端、整条高，且不和左边的提示文字重叠。
    #[test]
    fn clip_clear_hit_area_is_at_the_right_end() {
        let (w, h) = (1404, 1872);
        let (x, y, cw, ch) = clip_clear_rect(w, h);
        let (_, ty, _, th) = clip_toast_rect(w, h);
        assert_eq!((y, ch), (ty, th), "命中区跟提示条同高");
        assert_eq!(x + cw, w, "命中区贴右边");
        assert!(cw >= 240, "太小了手指点不中：{cw}");
        // 左边的提示文字（按 28 号字约 780px 宽）不能压在按钮上
        assert!(x > 700, "命中区吃掉了提示文字：x={x}");
    }

    /// 官方那三个分享串在 416 宽的面板里折行之后，每一行都画得下。
    ///
    /// `Present with Screen Share` 整串 28 号字有 413px，面板文字区只有
    /// 416-90-8 = 318px —— 不折行就会糊到面板外（`main.rs` 的分享面板用的是
    /// `wrap_words`）。没有字体文件的环境跳过。
    #[test]
    fn share_labels_fit_after_wrapping() {
        let Some(mut t) = crate::ui::text::Text::load() else {
            eprintln!("没有字体，跳过");
            return;
        };
        const PANEL_W: i32 = 416; // main.rs `SHARE_PANEL_W`
        const TEXT_DX: i32 = 90; // 分享行文字起点
        let max_w = (PANEL_W - TEXT_DX - 8) as f32;
        for row in 0..3 {
            let label = share_label(row, false);
            let lines = wrap_words(label, max_w, &mut |s| t.width(28.0, s));
            assert!(lines.len() <= 2, "{label:?} 折成了 {} 行", lines.len());
            for ln in &lines {
                let w = t.width(28.0, ln);
                assert!(w <= max_w + 0.5, "{ln:?} 宽 {w:.0}px，只有 {max_w:.0}px");
            }
        }
    }

    /// 分享面板三个串全是官方原文。
    #[test]
    fn share_panel_labels_are_official() {
        assert_eq!(share_label(0, false), "Present with Screen Share");
        assert_eq!(share_label(0, true), "Stop sharing your screen");
        assert_eq!(share_label(1, false), "Send by email");
        assert_eq!(share_label(2, false), "Convert notes to text");
        assert_eq!(share_label(2, true), "Convert notes to text");
        // 越界也不能 panic（面板行数将来可能变）
        assert_eq!(share_label(9, false), "Convert notes to text");
    }

    /// 排序按钮：展开时是 `Sort by`，收起时是当前排序名。
    #[test]
    fn sort_button_says_sort_by_while_expanded() {
        assert_eq!(sort_button_label(true, "Last updated"), "Sort by");
        assert_eq!(sort_button_label(false, "Last updated"), "Last updated");
        assert_eq!(sort_button_label(true, "Date removed"), "Sort by");
        assert_eq!(sort_button_label(false, "Page count"), "Page count");
    }

    /// 长标题按词折行（官方 `Text.WrapAtWordBoundaryOrAnywhere`）。
    /// 假量法：每个字符 10px，这样结果和字体无关，纯测折行逻辑。
    #[test]
    fn long_labels_wrap_at_word_boundaries() {
        let mut measure = |s: &str| s.chars().count() as f32 * 10.0;
        assert_eq!(wrap_words("Sort by", 200.0, &mut measure), vec!["Sort by"]);
        assert_eq!(
            wrap_words("Present with Screen Share", 200.0, &mut measure),
            vec!["Present with Screen", "Share"]
        );
        assert_eq!(
            wrap_words("Present with Screen Share", 400.0, &mut measure),
            vec!["Present with Screen Share"],
            "放得下就不折"
        );
        // 比整行还宽的单词独占一行，不会被切断
        assert_eq!(
            wrap_words("abcdefghij klm", 50.0, &mut measure),
            vec!["abcdefghij", "klm"]
        );
        assert_eq!(wrap_words("", 100.0, &mut measure), vec![""]);
        assert_eq!(wrap_words("   ", 100.0, &mut measure), vec![""]);
    }

    /// 标签面板：最后一行是 `Create new tag`，高度 = (n+1) 行。
    #[test]
    fn tag_panel_has_the_create_row() {
        assert_eq!(TAG_CREATE_NEW, "Create new tag");
        assert_eq!(tags_panel_height(0, 120), 120, "没有标签时只有新建那一行");
        assert_eq!(tags_panel_height(3, 120), 480);
        assert_eq!(tags_panel_height(2, 80), 240);
    }
}

