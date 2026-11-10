//! 光栅化 + 合成。
//!
//! 官方的做法是「点序列 → 路径 → 每点一个宽度 → 多边形/四边带 → 瓦片 → 16bpp」，
//! 中间还夹着 `QRegion` 损伤判定和灰度量化。
//! 这里只保留对画质有影响的两步：**逐点宽度的胶囊（capsule）覆盖** + **单层墨密度合成**。
//!
//! 为什么用「墨密度」而不是直接往帧缓冲上盖：
//!   * 两笔重叠时不会越画越黑（官方就是单层墨）；
//!   * 抗锯齿边缘只需要一次 max，不会因为重叠变成实心；
//!   * 撤销时把密度清零重画即可，不需要保存"下面的图层"。

use crate::fb::{gray565, Fb};
use crate::pen::{pencil, Color, Line, Tool};
use crate::viewport::View;

/// 256 级灰度 → RGB565 的查表。整屏重绘是 263 万次换算，
/// 每次现算 3 组移位与掩码在这颗 ARM 上要多花十几毫秒。
static GRAY565: std::sync::LazyLock<[u16; 256]> =
    std::sync::LazyLock::new(|| std::array::from_fn(|i| gray565(i as u8)));

// ------------------------------------------------------------ 铅笔网点纹理
//
// 面板只有二值波形（DU）：中间灰要么显示不出来、要么显示成全黑。灰度波形
// （GL16）能出灰，但它要先把像素"驱到黑再回落"，所以画的时候先黑一下才变灰，
// 而且整帧慢得多——这就是"超级慢 + 先变黑再变灰"。
//
// 官方的铅笔是**黑点阵**：用黑点覆盖率模拟灰度，近看是海绵/圈圈，远看是灰线。
// 这里照做：把墨浓度 `tone` 当"该有多少比例的像素落黑"，用一张平铺的环纹阈值图
// 挑像素。于是整笔都是纯黑像素 → DU 波形直接用 → 一笔下去立刻到位。

/// 平铺边长（px）。必须是 2 的幂（取样时用 `& (TEX_N-1)`）。
///
/// 8px 太短：一条笔迹上每隔 8px 就重复一次，看得出"印网"。16px + 抖动环心，
/// 重复周期翻倍、图案也不规则，才像铅笔蹭出来的海绵。
const TEX_N: usize = 16;
/// 每边撒几个环（16×16 里 4×4 个）。
const TEX_RINGS_X: usize = 4;
/// 环半径（px）。官方那圈被吐槽偏大，这里取它的 **0.8×**。
const TEX_RING: f32 = 1.5;
/// 环带宽度（px，高斯半宽）。
const TEX_BAND: f32 = 0.85;

/// 阈值图：值越小越"先落墨"。覆盖率 ∝ 浓度，空间上聚成环。
static TEX: std::sync::LazyLock<[u8; TEX_N * TEX_N]> = std::sync::LazyLock::new(build_tex);

/// 便宜的确定性哈希（纹理用，不需要密码学强度）。
fn tex_hash(x: usize, y: usize) -> u32 {
    let mut h = (x as u32).wrapping_mul(0x9e37_79b9) ^ (y as u32).wrapping_mul(0x85eb_ca6b);
    h ^= h >> 13;
    h = h.wrapping_mul(0xc2b2_ae35);
    h ^ (h >> 16)
}

fn build_tex() -> [u8; TEX_N * TEX_N] {
    // 先撒环：环心按 4px 网格铺，但每个都抖 1.6px、半径也各不相同。
    // 不抖的话 16px 一重复，屏幕上就是一张看得见的印刷网。
    let pitch = TEX_N as f32 / TEX_RINGS_X as f32;
    let mut rings = [(0f32, 0f32, 0f32); TEX_RINGS_X * TEX_RINGS_X];
    for gy in 0..TEX_RINGS_X {
        for gx in 0..TEX_RINGS_X {
            let h = tex_hash(gx, gy);
            let j = |b: u32| ((h >> b) & 63) as f32 / 63.0 - 0.5; // -0.5..0.5
            rings[gy * TEX_RINGS_X + gx] = (
                gx as f32 * pitch + pitch * 0.5 + j(0) * 1.6,
                gy as f32 * pitch + pitch * 0.5 + j(6) * 1.6,
                TEX_RING + j(12) * 0.5,
            );
        }
    }
    let n = TEX_N as f32;
    let mut t = [0u8; TEX_N * TEX_N];
    for y in 0..TEX_N {
        for x in 0..TEX_N {
            // 4×4 超采样：这个像素有多"在环带上"（环带中心 1，离开迅速掉到 0）。
            // 每个采样点先对 16 个环取最亮，再把这些采样点求平均。
            let mut acc = 0.0f32;
            for sy in 0..4 {
                for sx in 0..4 {
                    let px = x as f32 + (sx as f32 + 0.5) * 0.25;
                    let py = y as f32 + (sy as f32 + 0.5) * 0.25;
                    let mut best = 0.0f32;
                    for &(rx, ry, rr) in rings.iter() {
                        // 平铺边界按"环面"算距离，环跨过边界也不会被切掉
                        let dx = (px - rx).abs();
                        let dx = dx.min(n - dx);
                        let dy = (py - ry).abs();
                        let dy = dy.min(n - dy);
                        let k = ((dx * dx + dy * dy).sqrt() - rr) / TEX_BAND;
                        let c = (-k * k).exp();
                        if c > best {
                            best = c;
                        }
                    }
                    acc += best;
                }
            }
            let cov = acc / 16.0;
            // 确定性噪声：把阈值铺满 0..255，也破掉规则感
            let h = (tex_hash(x + 77, y + 33) & 0xff) as f32 / 255.0;
            let v = 255.0 * (0.58 * (1.0 - cov) + 0.42 * h);
            t[y * TEX_N + x] = v.clamp(0.0, 255.0) as u8;
        }
    }
    t
}

/// 网点纹理的两种花样。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tex {
    /// 环纹（铅笔）：16×16 抖动环，近看是海绵/圈圈。
    Rings,
    /// 细网点（荧光笔）：8×8 有序抖动，1px 点，远看就是一层浅灰，
    /// 不像环纹那样聚成一坨把底下的字糊住。
    Fine,
}

/// 8×8 Bayer 矩阵（0..63）。荧光笔用：每个像素的阈值只跟坐标有关，
/// 覆盖率是多少就落多少比例的 1px 黑点。
const BAYER8: [u8; 64] = [
     0, 32,  8, 40,  2, 34, 10, 42,
    48, 16, 56, 24, 50, 18, 58, 26,
    12, 44,  4, 36, 14, 46,  6, 38,
    60, 28, 52, 20, 62, 30, 54, 22,
     3, 35, 11, 43,  1, 33,  9, 41,
    51, 19, 59, 27, 49, 17, 57, 25,
    15, 47,  7, 39, 13, 45,  5, 37,
    63, 31, 55, 23, 61, 29, 53, 21,
];

/// 取阈值图上某点的阈值（值越小越先落墨）。
#[inline]
fn texel(kind: Tex, x: usize, y: usize) -> u8 {
    match kind {
        Tex::Rings => TEX[(y & (TEX_N - 1)) * TEX_N + (x & (TEX_N - 1))],
        // 白噪声阈值：没有方向性、不会跟别的东西叠出摩尔纹，
        // 1px 的点在面板上看就是一层均匀的淡灰（有序抖动会看出斜向网纹）。
        Tex::Fine => (tex_hash(x, y) & 0xff) as u8,
    }
}

/// 墨色 → 落墨密度（0..255）。官方三种：黑 / 灰 / 白（白笔是"提亮"，
/// 常压在荧光笔上）。
///
/// 荧光笔的四个颜色是官方 QML 里写死的四个浅灰（`#d2d2d2` / `#b4b4b4` /
/// `#8c8c8c` / `#cfcfcf`），换算成"覆盖率"就是 `255 - 灰值`：45 / 75 / 115 / 48。
pub fn ink_tone(c: Color) -> u8 {
    match c {
        Color::Black | Color::Blue | Color::Red => 255,
        Color::Gray => 110,
        Color::White => 0,
        Color::HighlighterYellow => 45,
        Color::HighlighterGreen => 75,
        Color::HighlighterPink => 115,
        Color::HighlighterGray => 48,
    }
}

pub struct Canvas {
    pub w: usize,
    pub h: usize,
    /// 0 = 纸白，255 = 全黑。单层墨，取 max 而不是叠加。
    dark: Vec<u8>,
    dirty: Option<Box4>,
    /// 左侧竖工具条盖住的那条：canvas 不往这里画，免得擦掉工具条。
    pub clip_l: i32,
    /// 右侧竖工具条盖住的那条（左利手时工具条在右边）：同上，上界。
    /// 默认 `i32::MAX` = 不裁；`clip_l`/`clip_r` 同时只有一个非默认值。
    pub clip_r: i32,
    /// 底图（灰度，255=白）。空表示纯白纸。PDF 页渲染进来放这里。
    pub bg: Vec<u8>,
    /// 落墨的浓度（0..255）。黑笔 255、灰笔 ~110、白笔 0。
    /// 只影响 `ink_segment` 写入的密度，擦除不受影响。
    pub ink_tone: u8,
    /// 用哪种网点落墨：`None` = 实心（像素写成 ink_tone），`Some` = 二值
    /// （像素只写 0/255，用点的密度冒充灰度，DU 波形直接显示）。
    pub ink_tex: Option<Tex>,
    /// 截面羽化：0 = 整笔一个颜色；0.4 = 边缘浓度打到六折（倾斜铅笔的"海绵"感）。
    pub ink_fade: f32,
    /// 铅笔硬度档位（`pen::pencil` 的下标）。重绘旧笔迹时靠它把浓度还原出来；
    /// 非铅笔时 None。
    pub pencil_grade: Option<usize>,
    /// 硬边：按"覆盖过半就整格落墨"画，1-bit，和官方原版逐像素一致。
    /// 由 `feather` 反着来：羽化开就走软边，关就是硬边。
    pub ink_hard: bool,
    /// 这一笔是荧光笔。
    ///
    /// 荧光笔的"灰"是**真灰度**（官方那四档浅灰平涂），整页重绘走 GC16 能
    /// 显示出来；但它不能过 `ink_segment` 里的二值抖动 —— 那会把一片平涂灰
    /// 变成格子。实时落笔时荧光笔另有网点纹理（`ink_tex`）负责在 A2 下可见。
    pub ink_hl: bool,
    /// 边缘羽化（用户可在"粗细"菜单底部切换）。开 = 抗锯齿软边，关 = 1-bit 硬边。
    pub feather: bool,
    /// 自上次 flush 以来有没有画过"中间灰"的墨。灰色墨不能用 DU 波形推。
    dirty_gray: bool,
}

impl Canvas {
    /// 按工具/颜色/压力决定这一笔怎么落墨——实时画和重绘旧笔迹都走这里，
    /// 两边才不会跑偏。
    ///
    /// 铅笔：环纹网点 + 边缘羽化 + 硬度档位（浓度随压力变）；
    /// 荧光笔：细网点平涂（它是浅灰墨，DU 显示不出来）；
    /// 书法：普通实心笔画，但**不抗锯齿**（原版就是 1-bit 硬边，见 `ink_hard`）；
    /// 其余：实心，浓度就是颜色。
    pub fn set_ink(&mut self, tool: Tool, color: Color, grade: usize, pressure: f32) {
        self.ink_hard = !self.feather;
        self.ink_hl = tool.is_highlighter();
        if tool.is_pencil() {
            self.ink_tex = Some(Tex::Rings);
            self.ink_fade = 0.42;
            self.pencil_grade = Some(grade);
            self.ink_tone = pencil::grade(grade).tone_at(pressure);
        } else {
            self.ink_tex = if tool.is_highlighter() {
                Some(Tex::Fine)
            } else {
                None
            };
            self.ink_fade = 0.0;
            self.pencil_grade = None;
            self.ink_tone = ink_tone(color);
        }
    }

    /// 恢复成"实心黑墨"（重绘循环收尾时用）。
    pub fn solid_ink(&mut self) {
        self.ink_tex = None;
        self.ink_hl = false;
        self.ink_fade = 0.0;
        self.pencil_grade = None;
        self.ink_hard = false;
        self.ink_tone = 255;
    }
}

#[derive(Clone, Copy, Debug)]
struct Box4 {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl Box4 {
    fn empty() -> Box4 {
        Box4 {
            x0: i32::MAX,
            y0: i32::MAX,
            x1: i32::MIN,
            y1: i32::MIN,
        }
    }
    fn is_empty(&self) -> bool {
        self.x1 < self.x0 || self.y1 < self.y0
    }
    fn add(&mut self, o: Box4) {
        if o.is_empty() {
            return;
        }
        self.x0 = self.x0.min(o.x0);
        self.y0 = self.y0.min(o.y0);
        self.x1 = self.x1.max(o.x1);
        self.y1 = self.y1.max(o.y1);
    }
    fn merge(&mut self, x: i32, y: i32) {
        self.x0 = self.x0.min(x);
        self.y0 = self.y0.min(y);
        self.x1 = self.x1.max(x);
        self.y1 = self.y1.max(y);
    }
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Canvas {
        Canvas {
            w,
            h,
            dark: vec![0u8; w * h],
            dirty: None,
            clip_l: 0,
            clip_r: i32::MAX,
            bg: Vec::new(),
            ink_tone: 255,
            ink_tex: None,
            ink_fade: 0.0,
            pencil_grade: None,
            ink_hard: false,
            ink_hl: false,
            feather: true,
            dirty_gray: false,
        }
    }

    /// 换底图。传 None 恢复白纸。
    pub fn set_bg(&mut self, bg: Option<Vec<u8>>) {
        self.bg = bg.unwrap_or_default();
    }

    /// 取某点的墨密度（缩略图用）。
    #[inline]
    /// 整块墨迹缓冲。页面光栅缓存直接存它——重绘时"全部笔迹重画一遍"
    /// 在设备上要几百毫秒，内容没变就没必要再算一次。
    pub fn dark_buf(&self) -> &[u8] {
        &self.dark
    }

    /// 把整块墨迹换回来（尺寸不符返回 false，调用方按缓存失效处理）。
    pub fn set_dark_buf(&mut self, buf: &[u8]) -> bool {
        if buf.len() != self.dark.len() {
            return false;
        }
        self.dark.copy_from_slice(buf);
        self.dirty = None;
        true
    }

    pub fn dark_at(&self, x: usize, y: usize) -> u8 {
        if x >= self.w || y >= self.h {
            0
        } else {
            self.dark[y * self.w + x]
        }
    }

    pub fn clear(&mut self) {
        self.dark.fill(0);
        self.dirty_gray = false;
        self.dirty = Some(Box4 {
            x0: 0,
            y0: 0,
            x1: self.w as i32 - 1,
            y1: self.h as i32 - 1,
        });
    }

    /// 把某块矩形的墨量清零（不改背景）。撤销/橡皮之后要局部重涂时用。
    pub fn clear_rect(&mut self, r: (i32, i32, i32, i32)) {
        let x0 = r.0.max(0) as usize;
        let y0 = r.1.max(0) as usize;
        let x1 = ((r.0 + r.2) as usize).min(self.w);
        let y1 = ((r.1 + r.3) as usize).min(self.h);
        for y in y0..y1 {
            let row = y * self.w;
            for x in x0..x1 {
                self.dark[row + x] = 0;
            }
        }
    }

    /// 全屏重绘之后调用：帧缓冲里已经是最新的了，攒着的脏区再刷一次纯属浪费，
    /// 而且 `flush` 会**按 canvas 重写整屏像素**，把盖在纸面上的 chrome 浮层
    /// （收起态左上角那个圆环、右上角 ×、页码）一起抹掉。
    pub fn clear_dirty(&mut self) {
        self.dirty = None;
        self.dirty_gray = false;
    }

    /// 攒着的脏区里有灰色墨吗（决定用 DU 还是 GL16 波形）。
    pub fn dirty_gray(&self) -> bool {
        self.dirty_gray
    }

    /// 一段带锥度的笔画：两端半径 `ar` → `br`。
    ///
    /// 覆盖函数用「到线段的距离 + 沿段线性插值的半径」，等价于
    /// 官方用 `QPainterPathStroker` 生成轮廓再填充，但天然带圆头圆角。
    /// 抗锯齿是 1 px 的解析羽化（`r + 0.5 - d`），不是超采样——
    /// 面板只有 16 级灰，超采样纯属浪费算力。
    pub fn ink_segment(&mut self, ax: f32, ay: f32, ar: f32, bx: f32, by: f32, br: f32) {
        self.ink_capsule(ax, ay, ar, bx, by, br);
    }

    /// 一段带锥度的圆头笔画（普通笔全是这一支）。
    pub fn ink_capsule(&mut self, ax: f32, ay: f32, ar: f32, bx: f32, by: f32, br: f32) {
        // 网点纹理落的是纯黑像素（0/255），DU 波形能直接显示，不算"灰墨"。
        if self.ink_tone < 250 && self.ink_tex.is_none() {
            self.dirty_gray = true;
        }
        let rmax = ar.max(br).max(0.5);
        let pad = rmax + 1.5;
        let x0 = ((ax.min(bx) - pad).floor() as i32).max(0);
        let y0 = ((ay.min(by) - pad).floor() as i32).max(0);
        let x1 = ((ax.max(bx) + pad).ceil() as i32).min(self.w as i32 - 1);
        let y1 = ((ay.max(by) + pad).ceil() as i32).min(self.h as i32 - 1);
        if x1 < x0 || y1 < y0 {
            return;
        }

        let dx = bx - ax;
        let dy = by - ay;
        let len2 = dx * dx + dy * dy;
        let inv_len2 = if len2 > 1e-6 { 1.0 / len2 } else { 0.0 };
        let dr = br - ar;

        let mut touched = Box4::empty();
        for y in y0..=y1 {
            let fy = y as f32 + 0.5;
            let row = y as usize * self.w;
            for x in x0..=x1 {
                let fx = x as f32 + 0.5;
                let px = fx - ax;
                let py = fy - ay;
                let t = ((px * dx + py * dy) * inv_len2).clamp(0.0, 1.0);
                let cx = ax + dx * t;
                let cy = ay + dy * t;
                let d = ((fx - cx) * (fx - cx) + (fy - cy) * (fy - cy)).sqrt();
                let r = ar + dr * t;
                let cov = (r + 0.5 - d).clamp(0.0, 1.0);
                if cov <= 0.0 {
                    continue;
                }
                let i = row + x as usize;
                if let Some(kind) = self.ink_tex {
                    // 浓度 = 覆盖率 × 截面羽化 × 硬度档位的墨量；和阈值图比一下，
                    // 过了就落一个**纯黑**像素 —— 灰度是"点够多"看起来的灰。
                    let f = if r > 0.51 {
                        // 倾斜的铅笔截面不是一种颜色：中线浓、两侧淡
                        let q = (d / r).min(1.0);
                        cov * (1.0 - self.ink_fade * q * q)
                    } else {
                        cov * (1.0 - self.ink_fade)
                    } * self.ink_tone as f32;
                    let m = texel(kind, x as usize, y as usize);
                    if f > m as f32 {
                        self.dark[i] = 255;
                        touched.merge(x, y);
                    }
                    continue;
                }
                let c = if self.ink_hard {
                    if cov > 0.5 { self.ink_tone } else { 0 }
                } else {
                    (cov * self.ink_tone as f32 + 0.5) as u8
                };
                // **有序抖动**：面板的二值波形（A2）对「目标为中间灰」的像素
                // 一个驱动码都不给，中间灰会被整片跳过。
                //
                // 铅笔看不出来是因为它走网点（`ink_tex`，直接落 255）；圆珠笔
                // /钢笔这类"实心 + 抗锯齿"的笔画边缘全是 60~200 的灰，A2 下
                // 只有 `cov` 接近 1 的那几个核心像素变黑 —— 屏上就是"一条线
                // 变成了一串点"。这正是圆珠笔看起来不正常、铅笔正常的原因。
                //
                // 这里按 8×8 Bayer 阈值把浓度折成 0/255：视觉上是实线，图案
                // 只跟坐标有关，重绘/滚动都一致，也不会像白噪声那样在 e-ink
                // 上闪。表和网点纹理共用 `BAYER8`，乘 4 摊到 0..252。
                let c = if !self.ink_hl && c > 0 && c < 255 {
                    let t = BAYER8[(y as usize & 7) * 8 + (x as usize & 7)] as u32 * 4;
                    if c as u32 > t {
                        255
                    } else {
                        0
                    }
                } else {
                    c
                };
                if c > self.dark[i] {
                    self.dark[i] = c;
                    touched.merge(x, y);
                }
            }
        }

        if !touched.is_empty() {
            // 刷新区域要比墨迹本身大一圈：面板的刷新是按 8 像素块走的。
            let padded = Box4 {
                x0: (touched.x0 - 4).max(0),
                y0: (touched.y0 - 4).max(0),
                x1: (touched.x1 + 4).min(self.w as i32 - 1),
                y1: (touched.y1 + 4).min(self.h as i32 - 1),
            };
            match &mut self.dirty {
                Some(d) => d.add(padded),
                None => self.dirty = Some(padded),
            }
        }
    }

    pub fn ink_disc(&mut self, x: f32, y: f32, r: f32) {
        self.ink_segment(x, y, r, x, y, r);
    }

    /// 擦除一段。与落墨对称：软边按覆盖度扣掉墨量，中心直接清零。
    ///
    /// 官方把橡皮也存成"线"（tool 6 / 8），渲染时必须**按顺序**对已有笔画做减法，
    /// 否则你以前擦掉的东西会重新冒出来。
    pub fn erase_segment(&mut self, ax: f32, ay: f32, ar: f32, bx: f32, by: f32, br: f32) {
        let rmax = ar.max(br).max(0.5);
        let pad = rmax + 1.5;
        let x0 = ((ax.min(bx) - pad).floor() as i32).max(0);
        let y0 = ((ay.min(by) - pad).floor() as i32).max(0);
        let x1 = ((ax.max(bx) + pad).ceil() as i32).min(self.w as i32 - 1);
        let y1 = ((ay.max(by) + pad).ceil() as i32).min(self.h as i32 - 1);
        if x1 < x0 || y1 < y0 {
            return;
        }
        let dx = bx - ax;
        let dy = by - ay;
        let len2 = dx * dx + dy * dy;
        let inv_len2 = if len2 > 1e-6 { 1.0 / len2 } else { 0.0 };
        let dr = br - ar;

        let mut touched = Box4::empty();
        // 被擦的像素里有多少是**中间灰**。二值波形（A2）只能把 0/255 翻面，
        // 「推白 10 帧」从平涂灰推不回纸白 —— 擦荧光笔就会留一道灰。
        // 黑笔主体是 255 不算，只有它抗锯齿的边缘是灰，几十个像素，不会
        // 误触发；荧光笔平涂一大片 200 上下，立刻超阈值，改走 GL16。
        let mut gray_px = 0usize;
        for y in y0..=y1 {
            let fy = y as f32 + 0.5;
            let row = y as usize * self.w;
            for x in x0..=x1 {
                let fx = x as f32 + 0.5;
                let px = fx - ax;
                let py = fy - ay;
                let t = ((px * dx + py * dy) * inv_len2).clamp(0.0, 1.0);
                let cx = ax + dx * t;
                let cy = ay + dy * t;
                let d = ((fx - cx) * (fx - cx) + (fy - cy) * (fy - cy)).sqrt();
                let r = ar + dr * t;
                let cov = (r + 0.5 - d).clamp(0.0, 1.0);
                if cov <= 0.0 {
                    continue;
                }
                let i = row + x as usize;
                let old = self.dark[i];
                if old == 0 {
                    continue;
                }
                if (32..=224).contains(&old) {
                    gray_px += 1;
                }
                // **硬擦**，不是按覆盖率线性衰减。
                //
                // 线性衰减会留下 `old * (1-cov)` 的中间灰：圆珠笔是抖动出来的
                // 纯黑点阵（255），点阵边缘的 `cov` 只有 0.5~0.9，于是被擦成
                // 25~128 的灰。数据层看它已经擦过了（退出重进是白的），但
                // 二值波形（A2）对中间灰一帧都不驱 —— 屏上就是擦不掉的
                // 残影点。够得着就直接归零，够不着的保持原样。
                let new = if cov >= 0.5 { 0 } else { old };
                if new != old {
                    self.dark[i] = new;
                    touched.merge(x, y);
                }
            }
        }
        if gray_px > 64 {
            self.dirty_gray = true;
        }
        self.touch(touched);
    }

    /// 把一小块并进脏区（带面板对齐余量）。
    fn touch(&mut self, t: Box4) {
        if t.is_empty() {
            return;
        }
        let padded = Box4 {
            x0: (t.x0 - 4).max(0),
            y0: (t.y0 - 4).max(0),
            x1: (t.x1 + 4).min(self.w as i32 - 1),
            y1: (t.y1 + 4).min(self.h as i32 - 1),
        };
        match &mut self.dirty {
            Some(d) => d.add(padded),
            None => self.dirty = Some(padded),
        }
    }

    /// 整笔重画（打开/撤销/收起工具条走这条）。
    ///
    /// `off` 是「场景坐标 → 视图坐标」的平移：官方页面 **x 居中于 0、y 自上而下**，
    /// 所以 x 要补半个页宽。（这是把官方 `.rm` 画对的关键，少这个平移整页会左移 702 px。）
    ///
    /// 橡皮/清屏/选区这些"不产生墨"的工具直接跳过——它们将来要走擦除路径。
    pub fn ink_line(&mut self, line: &Line, pixel_scale: f32, off: (f32, f32), grade: usize) {
        // 坐标也要乘缩放（缩略图就是靠这个把整页塞进小画布）；
        // 全尺寸时 pixel_scale = 1，所以之前的代码看不出毛病。
        let s = pixel_scale;
        let r = |w: f32| (w * 0.5 * s).max(0.5);
        // 橡皮与清屏是"减法"，必须按 CRDT 顺序对已画的墨生效
        if matches!(line.tool, Tool::Eraser | Tool::EraseSection) {
            let (dx, dy) = off;
            let pts = line.points.as_slice();
            for i in 1..pts.len() {
                let a = &pts[i - 1];
                let b = &pts[i];
                self.erase_segment(
                    a.x * s + dx,
                    a.y * s + dy,
                    r(a.width),
                    b.x * s + dx,
                    b.y * s + dy,
                    r(b.width),
                );
            }
            if let [p] = pts {
                self.erase_segment(
                    p.x * s + dx,
                    p.y * s + dy,
                    r(p.width),
                    p.x * s + dx,
                    p.y * s + dy,
                    r(p.width),
                );
            }
            return;
        }
        if matches!(line.tool, Tool::ClearPage | Tool::SelectionTool | Tool::ZoomTool) {
            return;
        }
        let (dx, dy) = off;
        // 缩略图（s < 1）走灰度而不是网点：8px 的网点缩到 200px 宽的缩略图上只剩噪点。
        //
        // 荧光笔还有一条：**重绘走真灰度**。原版存的就是一层平涂浅灰（官方 QML 的
        // #d2d2d2 那四档），而整页重绘本来就是 GC16，画真灰度不要额外代价，
        // 和原版逐像素一致；只有"边画边显"那几十毫秒用细网点，因为灰度波形
        // 会把新像素先驱到黑再回落（就是铅笔那次"先黑后灰"的毛病）。
        let tex = s >= 0.999 && !line.tool.is_highlighter();
        match line.points.as_slice() {
            [] => {}
            [p] => {
                self.set_ink(line.tool, line.color, grade, p.pressure);
                if !tex {
                    self.ink_tex = None;
                }
                self.ink_disc(p.x * s + dx, p.y * s + dy, r(p.width));
            }
            pts => {
                for i in 1..pts.len() {
                    let a = &pts[i - 1];
                    let b = &pts[i];
                    // 每个点各自算浓度：铅笔的浓淡跟着笔画里的压力走，
                    // 所以重开一页看到的深浅和当时画的一样。
                    self.set_ink(line.tool, line.color, grade, b.pressure);
                    if !tex {
                        self.ink_tex = None;
                    }
                    self.ink_segment(
                        a.x * s + dx,
                        a.y * s + dy,
                        r(a.width),
                        b.x * s + dx,
                        b.y * s + dy,
                        r(b.width),
                    );
                }
            }
        }
    }

    /// 把脏区域合成为 8 位灰度写进帧缓冲，然后推一次面板刷新。
    /// 推送攒下的脏区，返回 (x,y,w,h)；没有脏区就返回 None。
    pub fn flush(&mut self, fb: &mut Fb, waveform: u32) -> Option<(i32, i32, i32, i32)> {
        // **消费一次就清掉**。以前只有整屏重绘（`clear_dirty`）才会清，
        // 于是荧光笔画过一笔之后，后面每一次刷新都被判成"有灰墨"、
        // 全部走 GL16（46 帧 / 540 ms），一直拖到某次整屏重绘为止。
        self.dirty_gray = false;
        let d = match self.dirty.take() {
            Some(d) if !d.is_empty() => d,
            _ => return None,
        };
        let x0 = d.x0.max(self.clip_l);
        let x1 = (d.x1 + 1).min(self.clip_r);
        if x0 >= x1 {
            return None;
        }
        self.blit_1to1(fb, x0, d.y0.max(0), x1, d.y1 + 1);
        // 刷新矩形必须和"实际画过的范围"一致：从裁剪后的 x0 起算。
        // 否则 DU 波形会去重刷左侧工具条那块**没变**的静态图标（圆环、撤销箭头），
        // 墨水屏上的表现就是图标缺一块 / 看起来变透明了。
        fb.refresh(
            x0,
            d.y0,
            x1 - x0,
            d.y1 - d.y0 + 1,
            waveform,
            crate::fb::UPDATE_MODE_PARTIAL,
        );
        Some((x0, d.y0, x1 - x0, d.y1 - d.y0 + 1))
    }

    pub fn flush_all(&mut self, fb: &mut Fb, waveform: u32) {
        self.dirty = Some(Box4 {
            x0: 0,
            y0: 0,
            x1: self.w as i32 - 1,
            y1: self.h as i32 - 1,
        });
        self.flush(fb, waveform);
    }

    /// 把密度缓冲整块刷到帧缓冲（不刷新面板），用于全屏重绘。
    pub fn paint_all(&mut self, fb: &mut Fb) {
        let x0 = self.clip_l.max(0);
        let x1 = (self.w as i32).min(self.clip_r);
        self.blit_1to1(fb, x0, 0, x1, self.h as i32);
    }

    /// 1:1 贴一张：屏幕像素 == 画布像素，**不做逆变换**。
    ///
    /// 以前每个像素都算一遍 `screen_to_page`（4 次浮点乘加）+ `sample`（三次
    /// 边界判断）+ `put`（又来一次边界判断）。实测 49 万像素要 40ms，而整屏
    /// 重绘 263 万像素要 100ms+ —— 翻页/开菜单的"慢"有一半是这里。
    ///
    /// 1:1 时 `sample((x+0.5, y+0.5))` 截断后就是 `(x, y)`，所以可以按行整段算，
    /// 再用 `Fb::row_mut` 拿一段连续 u16 写下去。结果逐像素等价。
    fn blit_1to1(&self, fb: &mut Fb, x0: i32, y0: i32, x1: i32, y1: i32) {
        let x0 = x0.max(0);
        let y0 = y0.max(0);
        let x1 = x1.min(self.w as i32).min(fb.width);
        let y1 = y1.min(self.h as i32).min(fb.height);
        if x1 <= x0 || y1 <= y0 {
            return;
        }
        let has_bg = !self.bg.is_empty();
        let n = (x1 - x0) as usize;
        for y in y0..y1 {
            let row = y as usize * self.w;
            let base_off = row + x0 as usize;
            let Some(dst) = fb.row_mut(y, x0, n as i32) else {
                // 面板不是 16bpp（理论上不会）：退回逐像素
                for x in x0..x1 {
                    let i = base_off + (x - x0) as usize;
                    let base = if has_bg { self.bg[i] } else { 255 };
                    let g = (base as u32 * (255 - self.dark[i] as u32) / 255) as u8;
                    fb.put(x, y, gray565(g));
                }
                continue;
            };
            // 用切片 + zip 写：`self.dark[i]` 这种"下标 + 边界检查"的写法
            // 编译器向量化不了，263 万像素要 86ms（实测 33ns/px）；
            // 换成切片 zip 之后每像素只剩一次查表一次存。
            let lut: &[u16; 256] = &GRAY565;
            let src = &self.dark[base_off..base_off + n];
            if has_bg {
                let bgs = &self.bg[base_off..base_off + n];
                for ((d, ink), base) in dst.iter_mut().zip(src.iter()).zip(bgs.iter()) {
                    let g = (*base as u32 * (255 - *ink as u32) / 255) as u8;
                    *d = lut[g as usize];
                }
            } else if src.iter().all(|&v| v == 0) {
                // 空白行（笔记本里绝大多数行都是）：直接 memcpy 级填充。
                // 查表是"gather"，向量化不了；这一条让空白页的整屏重绘
                    // 从 40ms 掉到 3ms 以下。
                dst.fill(lut[255]);
            } else {
                // 纯白底：结果只跟墨量有关，省掉一次背景读取
                for (d, ink) in dst.iter_mut().zip(src.iter()) {
                    *d = lut[(255 - *ink) as usize];
                }
            }
        }
    }

    // ---------------------------------------------------------------- 缩放

    /// 画布坐标就是**页面坐标**（PAGE_DX/PAGE_DY 已经在落笔时加进
    /// `ink_line` 的 offset 里了），所以 `View` 的 page 空间和这里一一对应。
    ///
    /// 取样一个页面坐标点。页面外面一律当白纸 —— 缩小时四周要露白，
    /// 不能残留上一帧的墨。
    fn sample(&self, p: (f32, f32)) -> u8 {
        if p.0 < 0.0 || p.1 < 0.0 {
            return 255;
        }
        let (x, y) = (p.0 as i32, p.1 as i32);
        if x >= self.w as i32 || y >= self.h as i32 {
            return 255;
        }
        let i = y as usize * self.w + x as usize;
        let base = if self.bg.is_empty() { 255 } else { self.bg[i] };
        (base as u32 * (255 - self.dark[i] as u32) / 255) as u8
    }

    /// 缩小时做 2×2 盒式平均，不然细笔画会闪成断点。
    fn sample_filtered(&self, p: (f32, f32), scale: f32) -> u8 {
        if scale >= 0.99 {
            return self.sample(p);
        }
        let q = 1.0 / scale.max(0.01);
        let mut sum = 0u32;
        for (dx, dy) in [(0.0, 0.0), (q, 0.0), (0.0, q), (q, q)] {
            sum += self.sample((p.0 + dx, p.1 + dy)) as u32;
        }
        (sum / 4) as u8
    }

    /// 按视图变换把画布贴到屏幕上的一个矩形。**不做面板刷新**，调用方自己
    /// 决定刷新区域（全屏重绘 / 落笔时的增量区域）。
    pub fn paint_view_rect(&self, fb: &mut Fb, v: &View, rect: (i32, i32, i32, i32)) {
        let (rx, ry, rw, rh) = rect;
        let x0 = rx.max(self.clip_l).max(0);
        let x1 = (rx + rw).min(fb.width).min(self.clip_r);
        let y0 = ry.max(0);
        let y1 = (ry + rh).min(fb.height);
        if v.scale == 1.0 && v.rot == 0 && v.tx == 0.0 && v.ty == 0.0 {
            self.blit_1to1(fb, x0, y0, x1, y1);
            return;
        }
        for y in y0..y1 {
            for x in x0..x1 {
                // +0.5：取像素中心，否则整数倍缩放会整体偏半个像素
                let p = v.screen_to_page((x as f32 + 0.5, y as f32 + 0.5));
                fb.put(x, y, gray565(self.sample_filtered(p, v.scale)));
            }
        }
    }

    /// 整屏按视图变换重画。
    pub fn paint_view(&self, fb: &mut Fb, v: &View) {
        self.paint_view_rect(fb, v, (0, 0, fb.width, fb.height));
    }

    /// 页面的一块（页面坐标）映射到屏幕后覆盖的矩形，用于增量落笔。
    pub fn screen_rect_of(&self, v: &View, page_rect: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
        let (x, y, w, h) = page_rect;
        let corners = [
            (x as f32, y as f32),
            (x as f32 + w as f32, y as f32),
            (x as f32, y as f32 + h as f32),
            (x as f32 + w as f32, y as f32 + h as f32),
        ];
        let mut sx0 = f32::MAX;
        let mut sy0 = f32::MAX;
        let mut sx1 = f32::MIN;
        let mut sy1 = f32::MIN;
        for c in corners {
            let s = v.page_to_screen(c);
            sx0 = sx0.min(s.0);
            sy0 = sy0.min(s.1);
            sx1 = sx1.max(s.0);
            sy1 = sy1.max(s.1);
        }
        let x0 = sx0.floor() as i32 - 2; // 留 2px 余量给抗锯齿与取整误差
        let y0 = sy0.floor() as i32 - 2;
        (x0, y0, (sx1.ceil() as i32 + 2) - x0, (sy1.ceil() as i32 + 2) - y0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::viewport::View;

    fn canvas_with(dark: &[(usize, usize)], w: usize, h: usize) -> Canvas {
        let mut c = Canvas::new(w, h);
        for &(x, y) in dark {
            c.dark[y * w + x] = 255;
        }
        c
    }

    #[test]
    fn sample_identity_and_outside() {
        let c = canvas_with(&[(10, 20)], 100, 100);
        assert_eq!(c.sample((10.0, 20.0)), 0, "实心黑点");
        assert_eq!(c.sample((11.0, 20.0)), 255, "旁边是白纸");
        assert_eq!(c.sample((-1.0, 5.0)), 255, "页面外一律白");
        assert_eq!(c.sample((999.0, 5.0)), 255);
    }

    #[test]
    fn identity_view_samples_same_pixel() {
        let c = canvas_with(&[(10, 20)], 100, 100);
        let v = View::identity();
        let (pw, _) = (100.0f32, 100.0f32);
        // identity 的 page 空间就是画布坐标：屏幕点 = 页面点
        let p = v.screen_to_page((10.5, 20.5));
        assert!((p.0 - 10.5).abs() < 0.01 && (p.1 - 20.5).abs() < 0.01);
        assert_eq!(c.sample(p), 0);
        let _ = pw;
    }

    #[test]
    fn screen_rect_of_grows_when_zoomed() {
        let c = Canvas::new(1404, 1872);
        let v = View::identity();
        let r = c.screen_rect_of(&v, (100, 100, 10, 10));
        assert!(r.2 >= 14 && r.3 >= 14, "含余量后不小于原矩形: {r:?}");
        let mut z = View::identity();
        z.scale = 2.0;
        let rz = c.screen_rect_of(&z, (100, 100, 10, 10));
        assert!(rz.2 > r.2 * 2 - 8, "放大后覆盖的屏幕区域变大: {rz:?}");
    }
}
