//! 官方墨迹数据模型。
//!
//! 依据：`out/modules/M1-pen-input.md` §2.4/§2.5、`M2-rasterization.md` §2.2-§2.4、
//! `INK.md` §1。所有布局都与 `.rm` v6 磁盘格式一一对应，不要随手改字段顺序。

/// 一笔里的一个点，**24 字节 = 6 × f32**。
///
/// 内存布局与磁盘布局一致（`rmscene/scene_stream.py` 交叉验证过），
/// 唯一的差别只在**语义缩放**：磁盘上 speed/width 是 `4×`、pressure 是 `255×`、
/// dir 是 `255·θ/2π`。这里存的是内存语义值（即已归一化的那一份）。
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    /// 场景坐标，合法域 ±65000
    pub x: f32,
    pub y: f32,
    /// 语义 = 4 × 磁盘值；定义见 `stroke.rs`（位移 × 缩放，不是位移/时间）
    pub speed: f32,
    /// 弧度，官方落在 [0, 2π) 但**不做 unwrap**
    pub dir: f32,
    /// 逐点笔宽（场景单位）；渲染时再乘视图缩放
    pub width: f32,
    /// 归一化 0..1，**不是 0..255**
    pub pressure: f32,
}

impl Point {
    /// 官方 `linesfile.cpp::debugLines` 的合法域校验（`0x002d42b0`）。
    /// 写入路径可以拿它做断言，读取路径可以拿它做告警。
    pub fn is_valid(&self) -> bool {
        self.x.abs() <= 65_000.0
            && self.y.abs() <= 65_000.0
            && self.speed < 100_000.0
            && self.dir.abs() < std::f32::consts::TAU
            && self.width < 1000.0
            && self.pressure >= -0.1
            && self.pressure <= 1.1
    }
}

/// `Line::Tool`，**23 个值，数值必须与官方一致**——`.rm` 里存的就是这个 u8。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum Tool {
    Paintbrush = 0,
    Pencil = 1,
    Ballpoint = 2,
    Marker = 3,
    Fineliner = 4,
    Highlighter = 5,
    Eraser = 6,
    SharpPencil = 7,
    EraseSection = 8,
    ClearPage = 9,
    ZoomTool = 10,
    SelectionTool = 11,
    PaintbrushV2 = 12,
    SharpPencilV2 = 13,
    PencilV2 = 14,
    BallpointV2 = 15,
    MarkerV2 = 16,
    FinelinerV2 = 17,
    HighlighterV2 = 18,
    SolidPen = 19,
    ReservedPen = 20,
    Calligraphy = 21,
    #[default]
    Undefined = 22,
}

impl Tool {
    pub const ALL: [Tool; 23] = [
        Tool::Paintbrush,
        Tool::Pencil,
        Tool::Ballpoint,
        Tool::Marker,
        Tool::Fineliner,
        Tool::Highlighter,
        Tool::Eraser,
        Tool::SharpPencil,
        Tool::EraseSection,
        Tool::ClearPage,
        Tool::ZoomTool,
        Tool::SelectionTool,
        Tool::PaintbrushV2,
        Tool::SharpPencilV2,
        Tool::PencilV2,
        Tool::BallpointV2,
        Tool::MarkerV2,
        Tool::FinelinerV2,
        Tool::HighlighterV2,
        Tool::SolidPen,
        Tool::ReservedPen,
        Tool::Calligraphy,
        Tool::Undefined,
    ];

    pub fn from_u8(v: u8) -> Tool {
        *Tool::ALL.get(v as usize).unwrap_or(&Tool::Undefined)
    }

    #[inline]
    pub fn is_v2(self) -> bool {
        matches!(
            self,
            Tool::SharpPencilV2
                | Tool::PencilV2
                | Tool::BallpointV2
                | Tool::MarkerV2
                | Tool::SolidPen
                | Tool::ReservedPen
                | Tool::Calligraphy
        )
    }

    /// 荧光笔：半透明色带。灰墨在 DU 波形上显示不出来，所以也走网点。
    #[inline]
    pub fn is_highlighter(self) -> bool {
        matches!(self, Tool::Highlighter | Tool::HighlighterV2)
    }

    /// 铅笔家族（含自动铅笔）：灰墨 + 网点纹理，各有硬度档位。
    ///
    /// 官方这四支都是"固定灰、实心"的；我们给它们加了 2H…6B 的硬度档，
    /// 档案见 `pencil.rs`，纹理见 `canvas.rs` 的 `TEX`。
    #[inline]
    pub fn is_pencil(self) -> bool {
        matches!(
            self,
            Tool::Pencil | Tool::PencilV2 | Tool::SharpPencil | Tool::SharpPencilV2
        )
    }

    /// 走 §4.4(b) 位移衰减的工具（`0x73e840` 那张 23 字节表的逐项值）。
    #[inline]
    pub fn has_speed_damping(self) -> bool {
        matches!(
            self,
            Tool::Paintbrush
                | Tool::Pencil
                | Tool::Marker
                | Tool::PaintbrushV2
                | Tool::BallpointV2
                | Tool::MarkerV2
        )
    }

    /// `Line::width()`（`FUN_005659d8`）里的工具系数：命中测试/光标/橡皮用它，
    /// 描边用的是逐点 `width`。两者不是一回事，见 `REWRITE-SPEC.md` §6 冲突 1。
    pub fn line_width_factor(self) -> f64 {
        match self {
            Tool::Paintbrush | Tool::Pencil => 50.0,
            Tool::Ballpoint => 2.0,
            Tool::Marker => 20.0,
            Tool::Fineliner => 3.0,
            Tool::Highlighter => 30.0,
            Tool::Eraser => 10.0,
            Tool::SharpPencil => 3.0,
            Tool::EraseSection => 3.0,
            Tool::ClearPage => 1.0,
            Tool::ZoomTool => 10.0,
            Tool::SelectionTool => 3.0,
            Tool::PaintbrushV2 => 15.0,
            Tool::SharpPencilV2 => 4.0,
            Tool::PencilV2 => 18.0,
            Tool::BallpointV2 => 2.0,
            Tool::MarkerV2 => 20.0,
            Tool::FinelinerV2 => 3.0,
            Tool::HighlighterV2 => 30.0,
            Tool::SolidPen | Tool::ReservedPen => 2.0,
            Tool::Calligraphy => 6.0,
            Tool::Undefined => 1.0,
        }
    }
}

/// `Line::Color`，8 个正常值 + HighlighterGray。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[repr(u8)]
pub enum Color {
    #[default]
    Black = 0,
    Gray = 1,
    White = 2,
    HighlighterYellow = 3,
    HighlighterGreen = 4,
    HighlighterPink = 5,
    Blue = 6,
    Red = 7,
    HighlighterGray = 8,
}

impl Color {
    pub const ALL: [Color; 9] = [
        Color::Black,
        Color::Gray,
        Color::White,
        Color::HighlighterYellow,
        Color::HighlighterGreen,
        Color::HighlighterPink,
        Color::Blue,
        Color::Red,
        Color::HighlighterGray,
    ];

    pub fn from_u8(v: u8) -> Color {
        *Color::ALL.get(v as usize).unwrap_or(&Color::Black)
    }

    /// 是不是荧光笔那四种颜色（它们的浓淡跟普通笔不是一个量级）。
    #[inline]
    pub fn is_highlighter(self) -> bool {
        matches!(
            self,
            Color::HighlighterYellow
                | Color::HighlighterGreen
                | Color::HighlighterPink
                | Color::HighlighterGray
        )
    }

    /// 8 位灰度（rM2 面板只有 16 级灰，这里先给 8 位，扫描输出时再 `>> 4`）。
    /// 高亮三色在灰度屏上都趋于浅灰，与官方表现一致。
    pub fn gray8(self) -> u8 {
        match self {
            Color::Black => 0,
            Color::Gray => 90,
            Color::White => 255,
            Color::HighlighterYellow => 205,
            Color::HighlighterGreen => 195,
            Color::HighlighterPink => 200,
            Color::Blue => 70,
            Color::Red => 110,
            Color::HighlighterGray => 180,
        }
    }
}

/// 数字化仪的一个采样（官方 `InputPoint`，20 字节）。
///
/// 已经过 §4.1 的归一化：x/y 是**屏幕像素**，pressure 是 0..1，tilt 是弧度。
/// 注意 x 来自 `ABS_Y`、y 来自 `ABS_X` 且取反——这是 rM2 的传感器朝向决定的。
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct InputPoint {
    pub x: f32,
    pub y: f32,
    pub pressure: f32,
    pub tilt_x: f32,
    pub tilt_y: f32,
    /// 笔尾（`BTN_TOOL_RUBBER`）——官方把笔翻过来就是橡皮，不看当前选的工具
    pub rubber: bool,
}

impl InputPoint {
    #[inline]
    pub fn dist(&self, o: &InputPoint) -> f32 {
        ((self.x - o.x).powi(2) + (self.y - o.y).powi(2)).sqrt()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RectF {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl RectF {
    pub fn from_point(p: &Point) -> RectF {
        RectF {
            x: p.x,
            y: p.y,
            w: 0.0,
            h: 0.0,
        }
    }

    /// 把一个点并进矩形。
    ///
    /// **不能有「w==0 就重置原点」那种分支。** 这里以前有一份：
    ///
    /// ```ignore
    /// if self.w == 0.0 && self.h == 0.0 { self.x = x; self.y = y; }
    /// ```
    ///
    /// 而 `Line::push` 对每个采样点调**两次** `include`（左上角、右下角）。
    /// 第一次 `w==0` 成立 → 重置原点，算完 `w` 还是 0；第二次又成立 → 又
    /// 重置。于是无论一笔有多少个点，`Line.bounds` 的 `w`/`h` **永远停在 0**。
    ///
    /// 后果不只是包围盒不准：橡皮「擦整笔」靠 `line_bbox` 求范围、局部重绘
    /// 靠它算重刷矩形、选区靠它判相交 —— 全部退化成一个点。表现就是
    /// 「荧光笔擦不掉」「擦完屏上还留着」。
    pub fn include(&mut self, x: f32, y: f32) {
        let x0 = self.x.min(x);
        let y0 = self.y.min(y);
        let x1 = (self.x + self.w).max(x);
        let y1 = (self.y + self.h).max(y);
        self.x = x0;
        self.y = y0;
        self.w = x1 - x0;
        self.h = y1 - y0;
    }
}

/// 一笔。官方是 `0x40` 字节的 `Q_GADGET`，比这里多几个 Qt 的冗余字段。
#[derive(Clone, Debug, Default)]
pub struct Line {
    pub tool: Tool,
    pub color: Color,
    /// QML 的 `lineThickness`（Thin=1 / Medium=2 / Thick=3，或橡皮的 sqrt(t²/scale)）
    pub thickness: f64,
    pub points: Vec<Point>,
    pub bounds: RectF,
}

impl Line {
    pub fn new(tool: Tool, color: Color, thickness: f64) -> Line {
        Line {
            tool,
            color,
            thickness,
            points: Vec::with_capacity(256),
            bounds: RectF::default(),
        }
    }

    /// `Line::width(scale)` = `scale · thickness · tool_factor + 2.0`。
    /// 用途是命中测试（橡皮擦到哪、光标多粗），**不是描边宽度**。
    pub fn hit_width(&self, scale: f32) -> f32 {
        let w = self.thickness * self.tool.line_width_factor();
        if self.tool == Tool::Eraser {
            // 0x5659d8 的橡皮分支是 w = w * (float)(w * 10.0) = 10·w²
            (scale as f64 * (10.0 * w * w)) as f32 + 2.0
        } else {
            (scale as f64 * w) as f32 + 2.0
        }
    }

    pub fn push(&mut self, p: Point) {
        if self.points.is_empty() {
            self.bounds = RectF::from_point(&p);
        } else {
            let w = p.width * 0.5;
            self.bounds.include(p.x - w, p.y - w);
            self.bounds.include(p.x + w, p.y + w);
        }
        self.points.push(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_is_24_bytes() {
        assert_eq!(std::mem::size_of::<Point>(), 24);
    }

    #[test]
    fn tool_discriminants_match_official() {
        for (i, t) in Tool::ALL.iter().enumerate() {
            assert_eq!(*t as u8, i as u8, "{t:?}");
            assert_eq!(Tool::from_u8(i as u8), *t);
        }
    }

    #[test]
    fn speed_damping_table() {
        // 0x73e840 那张表：1 的位置只有 0,1,3,12,15,16
        let expect = [0, 1, 3, 12, 15, 16];
        for (i, t) in Tool::ALL.iter().enumerate() {
            let want = expect.contains(&i);
            assert_eq!(t.has_speed_damping(), want, "{t:?}");
        }
    }

    #[test]
    fn valid_domain() {
        let p = Point {
            x: 10.0,
            y: -10.0,
            speed: 100.0,
            dir: 3.0,
            width: 4.0,
            pressure: 0.5,
        };
        assert!(p.is_valid());
        let bad = Point { width: 1001.0, ..p };
        assert!(!bad.is_valid());
    }
}
