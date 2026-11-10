//! 逐点笔宽公式——官方 `PenInputLineHandler::addPoint` 的两个巨型分支：
//!
//! * `0x5673d4` **通用路径**：工具 0–12 / 17 / 18 / 22；
//! * `0x566d20` **v2 路径**：工具 13–16 / 19–21（由 `0x5673f4` 的分派表跳过去）。
//!
//! 两条路径的差别不只在公式，还在**输入怎么来**：
//!
//! | | 通用路径 | v2 路径 |
//! |---|---|---|
//! | 压力 | `0.5·(p_cur + p_prev)`，再按位移衰减（只对 6 个工具）| **当前点原始压力** |
//! | 压力整形 | 无 | `p0 = 0.15 + 0.35·tilt` 的死区 + smoothstep（`0x566dac`）|
//! | 倾角 | `hypot(tilt_x, tilt_y)`，不夹 | `atan(hypot(tan tx, tan ty))`，夹到 1.0 |
//! | 收尾 | 统一夹 `MIN_WIDTH`（`0x5677a4`）| 无统一下限，各分支自理 |
//!
//! 所以 `Ctx::p` 的语义是"**这支笔该用的那份压力**"，由 `stroke.rs` 按路径预处理好；
//! 本文件保持纯函数，输入输出可单测、可离线重放。
//!
//! 所有常量都标了 xochitl 里的地址（`objdump -d --start-address=...` 可复核）。

use super::line::Tool;
use std::f32::consts::{FRAC_PI_2, PI, TAU};

// -------------------------------------------------------------- 官方常量（带地址）

/// 通用路径收尾统一夹的最小宽度（场景单位）。`0x5677a4` 的 `2.0` + `vmovle`。
/// **v2 路径没有这一道**——BallpointV2 在轻压时只有 1.48 宽。
pub const MIN_WIDTH: f32 = 2.0;

/// `thickness > 1` 时的厚度除数（Ballpoint / SharpPencil / BallpointV2）。
pub const THICK_DIV: f32 = 1.35; // @0x567888 / @0x5673b4

/// 位移衰减的分母：`k = 1 - d / 100`。
pub const SPEED_DAMP_DIV: f32 = 100.0; // @0x567878

/// 位移衰减的下限：`k < 0.5` 一律取 0.5。`0x567714` 的 `vcmpe s15, 0.5`。
pub const SPEED_DAMP_FLOOR: f32 = 0.5;

// ---- 通用路径：Paintbrush（`0x56781c`）-----------------------------------
const PAINT_P_GAIN: f32 = 10.0; // @0x56782c
const PAINT_P_EXP: f32 = 4.5; // @0x56781c 的 f64 4.5
const PAINT_TILT_GAIN: f32 = 5.0; // @0x567844

// ---- 通用路径：Pencil（`0x56789c`）--------------------------------------
const PENCIL_TILT_GAIN: f32 = 20.0; // @0x5678ac 的 f64 20
const PENCIL_TILT_BIAS: f32 = 0.5; // @0x5678a8 的 f64 0.5
const PENCIL_CLAMP_LO: f32 = 1.5; // @0x5678dc
const PENCIL_CLAMP_HI: f32 = 40.0; // @0x567884
const PENCIL_P_GAIN: f32 = 1.5; // @0x5678ec
const PENCIL_THICK_DIV: f32 = 1.5; // @0x5678f4

// ---- 通用路径：Marker（`0x567970`）--------------------------------------
const MARKER_MIN: f32 = 2.0; // @0x567ab0
const MARKER_MAX: f32 = 20.0; // @0x567aa0
const MARKER_BASE: f32 = 2.5; // @0x567a5c
const MARKER_V_EXP: f32 = 3.5; // @0x567a00
const MARKER_V_GAIN: f32 = 10.0; // @0x567a58
const MARKER_P_GAIN: f32 = 4.0; // @0x567a54
const MARKER_P_CAP: f32 = 4.0; // @0x567a64

// ---- 其余通用工具 --------------------------------------------------------
const HIGHLIGHTER_W: f32 = 30.0; // @0x567808
const FINELINER_W: f32 = 2.0; // @0x567814
const ERASER_W_MUL: f32 = 10.0; // @0x567900
const SHARP_W: f32 = 3.0; // @0x567928 / @0x567ac4
/// EraseSection / ZoomTool / SelectionTool 是**固定** `3 × 2.0`（`0x567780` 先写 3.0
/// 再写 2.0 覆盖掉 thickness），与 QML 传几档粗细无关。
const STATIC_W: f32 = 6.0; // @0x567780 + @0x567784

// ---- v2 路径：倾角死区（`0x566d94`）-------------------------------------
const TILT_P_OFFSET: f32 = 0.15; // @0x56704c
const TILT_P_SLOPE: f32 = 0.35; // @0x567048
const TILT_P_MAX: f32 = 0.5;
const TILT_KNEE: f32 = 1.0; // tilt 超过 1 rad 后 p0 固定 0.5

// ---- v2 路径：PencilV2（`0x566fa4`）------------------------------------
const PENCIL_EMA_NEW: f32 = 0.2; // @0x5673bc
const PENCIL_EMA_OLD: f32 = 0.8; // @0x5673c0
const PENCIL_TILT_LO: f32 = 0.523_599; // 30° @0x567060
const PENCIL_TILT_RANGE: f32 = 0.436_332; // 25° @0x567068（55° @0x567064 = LO + RANGE）
const PENCIL_TILT_BONUS: f32 = 15.0; // @0x567058 附近的 f32 15
const PENCIL_V2_BASE: f32 = 2.0; // @0x566fe4

// ---- v2 路径：SharpPencilV2（`0x566f3c`）-------------------------------
const SHARP_V2_W: f32 = 4.0; // @0x566f60
const SHARP_V2_P_OFFSET: f32 = 0.7; // @0x56705c
const SHARP_V2_P_GAIN: f32 = 0.3; // @0x567058

// ---- v2 路径：Calligraphy（`0x567074`）----------------------------------
const CALLI_W_EMA_NEW: f32 = 0.25; // @0x5671d0
const CALLI_W_EMA_OLD: f32 = 0.75; // @0x5671e8
const CALLI_P_MUL: f32 = 4.5; // @0x5670d8
const CALLI_W_BASE: f32 = 1.0; // @0x567144
const CALLI_ANGLE: f32 = 1.047_198; // 60° @0x5673a0
const CALLI_MIN_W: f32 = 1.5; // @0x7c764c
/// 笔尖相对屏幕的固定朝向 40°（`0x7c7644`）。首点没有前一段方向时官方改用 170°（`0x7c7648`），
/// 这里没复刻那个首点特例：差值只影响一笔的第一个点，且会把笔尖方向翻到反面。
const CALLI_NIB_OFFSET: f32 = 0.698_131_7;

/// 输入上下文。凡是被公式读到的量都放这儿，公式本身保持无副作用。
#[derive(Clone, Copy, Debug)]
pub struct Ctx {
    /// 压力，**语义随工具的路径而变**（`stroke.rs` 负责预处理）：
    /// * 通用路径：`min(0.5·(p_cur + p_prev), 1)`，吃到位移衰减的工具再乘 `k`；
    /// * v2 路径：**当前点的原始压力**（官方 `0x566da4` 直接读 `[in+8]`，不做均值）。
    pub p: f32,
    /// 倾角幅值（v2 口径）：`atan(sqrt(tan²tx + tan²ty))`，公式内部再夹到 1.0。
    pub tilt: f32,
    pub tilt_x: f32,
    pub tilt_y: f32,
    /// 笔画方向（弧度，`[0,2π)`）：相邻两段的平均（官方 `0x567560`）。
    pub dir: f32,
    /// 当前段的瞬时方向（弧度，`[0,2π)`）：`atan2(y-y_prev, x-x_prev)`。
    /// 通用路径的垂距项（`0x565250`）用的是这一段，不是平均方向。
    pub seg_dir: f32,
    /// 视图旋转（弧度）。官方那边传的是**度**（0/90/180/270，`[in]` 之外的 +152 字段）。
    /// 只有 Calligraphy 用。
    pub rotation: f32,
    /// 上一点存进 Line 的压力（PencilV2 的 EMA 用）。
    pub prev_pressure: f32,
    /// 上一点存进 Line 的宽度（Calligraphy 的 EMA 用）。
    pub prev_width: f32,
    /// 这一笔之前有没有已经存下的点。官方用 `points.size() != 0` 判（`0x566fd4`）。
    /// 没有前一点时 PencilV2 不做 EMA、Calligraphy 不混合上一宽度。
    pub has_prev: bool,
}

impl Default for Ctx {
    fn default() -> Self {
        Ctx {
            p: 0.0,
            tilt: 0.0,
            tilt_x: 0.0,
            tilt_y: 0.0,
            dir: 0.0,
            seg_dir: 0.0,
            rotation: 0.0,
            prev_pressure: 0.0,
            prev_width: 0.0,
            has_prev: false,
        }
    }
}

/// 一个点的产出：描边宽度 + **要存回 Line 的 pressure**（部分工具会改写它）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Width {
    pub w: f32,
    pub pressure: f32,
}

/// v2 路径的压力重映射（`0x566dac`–`0x566de4`）：随倾角抬高的死区 + smoothstep。
///
/// 斜着拿笔时同样的物理压力被认为更轻——这是官方对"笔尖接触面积"的补偿。
/// 注意官方先把 `tilt` 夹到 1.0（`0x566d9c` 的 `vmovgt s20, s15`）再算 `p0`。
pub fn v2_remap(p: f32, tilt: f32) -> f32 {
    let tilt = tilt.min(TILT_KNEE);
    let p0 = if tilt <= TILT_KNEE {
        TILT_P_OFFSET + TILT_P_SLOPE * tilt
    } else {
        TILT_P_MAX
    };
    let q = ((p - p0) / (1.0 - p0)).clamp(0.0, 1.0);
    smoothstep(q)
}

/// `3q² - 2q³`（`0x566dd0` 的三个常量 3 / 2）。
#[inline]
fn smoothstep(q: f32) -> f32 {
    (3.0 - 2.0 * q) * q * q
}

/// 倾角幅值，输入是两根轴的正切（v2 路径口径）。
pub fn tilt_magnitude(tilt_x: f32, tilt_y: f32) -> f32 {
    (tilt_x.tan().powi(2) + tilt_y.tan().powi(2)).sqrt().atan()
}

/// 通用路径的倾角：直接 `hypot(tilt_x, tilt_y)`（`0x567594` 的 `vsqrt d12`），
/// **不取 tan、也不夹到 1.0**——这是和 v2 路径最容易搞混的一处。
#[inline]
fn raw_tilt(c: &Ctx) -> f32 {
    (c.tilt_x * c.tilt_x + c.tilt_y * c.tilt_y).sqrt()
}

/// 通用路径里 Pencil / Marker 的 `v`（`0x565250` 那一次调用）。
///
/// 官方把 `C = 当前点 + 单位倾角向量` 投到线段（上一点 → 当前点）上算垂距；
/// 因为 `|C - 当前点| == 1`，结果就是 `|sin(笔画方向 − 倾角方向)|` ∈ [0,1]。
/// 直觉：笔杆倒的方向与运笔方向垂直时最大（像用铅笔侧锋蹭），顺纹走时接近 0。
#[inline]
fn tilt_perp(c: &Ctx) -> f32 {
    let tilt_dir = c.tilt_y.atan2(c.tilt_x); // @0x567570 的 atan2（原始比值，不取 sin）
    (c.seg_dir - tilt_dir).sin().abs()
}

#[inline]
fn thick_eff(t: f32) -> f32 {
    if t > 1.0 {
        t / THICK_DIV
    } else {
        t
    }
}

/// 把角度折进 `[0, 2π)`（官方 `0x567490`–`0x5674c8` 的循环）。
#[inline]
fn norm_tau(mut a: f32) -> f32 {
    while a < 0.0 {
        a += TAU;
    }
    while a >= TAU {
        a -= TAU;
    }
    a
}

/// 官方 `addPoint` 的宽度分支。返回的宽度**已经是最终描边宽度**（含厚度与下限夹取）。
pub fn width(tool: Tool, c: &Ctx, thickness: f32) -> Width {
    let t = thickness;

    match tool {
        // ================= 通用路径（0x5673d4）================================
        // 画笔：压力^4.5 主导（10 倍）+ 倾角^4 补一点点（5 倍）。
        // 注意第二项是 **tilt**（`0x567830` 读 d12）而不是速度——速度只影响压力衰减。
        Tool::Paintbrush | Tool::PaintbrushV2 => {
            let mut w =
                PAINT_P_GAIN * c.p.powf(PAINT_P_EXP) + PAINT_TILT_GAIN * raw_tilt(c).powi(4); // 4 次方 @0x567834
            if w < MIN_WIDTH {
                w = MIN_WIDTH;
            }
            // 老工具（tool ≤ 4 / 7）先把基准宽取整，再乘厚度（`0x567794` 的 vcvtls）。
            if tool == Tool::Paintbrush {
                w = w.trunc();
            }
            Width {
                w: (w * t).max(MIN_WIDTH),
                pressure: c.p,
            }
        }
        // 铅笔：`20·(tilt^4 - 0.5)·v^3.5` 夹 [1.5, 40]，再 + 1.5p，取整后乘 t/1.5。
        // 平放（tilt < 0.84 rad）时括号为负，整笔就是最细的 1.5 + 1.5p——官方这支笔
        // 的粗细分两段，靠 tilt 的 4 次方过零。
        Tool::Pencil => {
            let x = (PENCIL_TILT_GAIN * (raw_tilt(c).powi(4) - PENCIL_TILT_BIAS)
                * tilt_perp(c).powf(3.5))
            .clamp(PENCIL_CLAMP_LO, PENCIL_CLAMP_HI);
            let base = (x + c.p * PENCIL_P_GAIN).trunc();
            Width {
                w: (base * (t / PENCIL_THICK_DIV)).max(MIN_WIDTH),
                pressure: c.p,
            }
        }
        // 圆珠笔（v1）：压力先过一道"整数台阶"，再乘厚度。
        Tool::Ballpoint => {
            let mut v = c.p * 3.0;
            if v >= 2.0 {
                v = v.trunc();
            }
            Width {
                w: (v * thick_eff(t)).max(MIN_WIDTH),
                pressure: c.p,
            }
        }
        // 马克笔（v1）：`10·v^3.5·max(|tx|,|ty|)^6 + 2.5 + min(4p², 4)`，夹 [2,20]。
        // `0x567a24`–`0x567a38`：倾角取**两根轴绝对值的较大者**再夹到 1，不是 hypot。
        Tool::Marker => {
            let tmax = c.tilt_x.abs().max(c.tilt_y.abs()).min(1.0);
            let v = tilt_perp(c);
            let p_term = (c.p * c.p * MARKER_P_GAIN).min(MARKER_P_CAP).max(0.0);
            let base = (MARKER_BASE
                + MARKER_V_GAIN * v.powf(MARKER_V_EXP) * tmax.powi(6) // 6 次方 @0x567a44
                + p_term)
                .clamp(MARKER_MIN, MARKER_MAX)
                .trunc();
            Width {
                w: (base * t).max(MIN_WIDTH),
                pressure: c.p,
            }
        }
        // 签字笔：恒定粗细——这就是用户最常用的那支笔。
        Tool::Fineliner | Tool::FinelinerV2 => Width {
            w: (FINELINER_W * t).max(MIN_WIDTH),
            pressure: c.p,
        },
        // 荧光笔：**永远 30**——与压力/速度/倾角/厚度都无关（`0x567808` 把 30 和
        // 固定厚度 1.0 直接写死，QML 那边也把 Stroke 菜单整段隐藏）。
        Tool::Highlighter | Tool::HighlighterV2 => Width {
            w: HIGHLIGHTER_W,
            pressure: c.p,
        },
        // 橡皮：**10·t²**，不是 10·t。实测 t=2.4 → 57.6、t=1.2 → 14.4（官方 .rm 原始值）。
        Tool::Eraser => Width {
            w: (ERASER_W_MUL * t * t).max(MIN_WIDTH),
            pressure: c.p,
        },
        Tool::SharpPencil => Width {
            w: (SHARP_W * thick_eff(t)).max(MIN_WIDTH),
            pressure: c.p,
        },
        // EraseSection / ZoomTool / SelectionTool：固定 6.0（3 × 写死的 2.0）。
        Tool::EraseSection | Tool::ZoomTool | Tool::SelectionTool => Width {
            w: STATIC_W,
            pressure: c.p,
        },
        // 清屏不产生点（`0x567758` 直接 return），走到这里说明调用方写错了。
        Tool::ClearPage => Width {
            w: MIN_WIDTH,
            pressure: c.p,
        },
        // 官方 default 是直接 return（不产生点），这里用最小宽度占位。
        Tool::Undefined => Width {
            w: (MIN_WIDTH * t).max(MIN_WIDTH),
            pressure: c.p,
        },

        // ================= v2 路径（0x566d20）=================================
        // 自动铅笔：宽度只有 `4 × 厚度`，不吃压力（QML 也不给它倾角/压力 UI）。
        // 官方 `0x566f50` 的厚度除法挂在 `pv > 0` 上——轻压（还没过死区）时用 4t，
        // 过了死区变 4t/1.35。这条看起来像官方的笔误，但实测前按汇编照抄。
        Tool::SharpPencilV2 => {
            let pv = v2_remap(c.p, c.tilt);
            let te = if pv > 0.0 { t / THICK_DIV } else { t };
            // 存回的压力再走一次 smoothstep 压到 [0.7, 1]（`0x566f7c`）。
            let p_store = SHARP_V2_P_OFFSET + SHARP_V2_P_GAIN * smoothstep(pv.min(1.0));
            Width {
                w: te * SHARP_V2_W,
                pressure: p_store,
            }
        }
        // 铅笔 v2：压力先过倾角死区重映射，然后 0.2/0.8 的 EMA（用**上一点存下的
        // 压力**，不是原始压力），再加 2.0 底宽和 30°→55° 的倾角加成（最高 15）。
        Tool::PencilV2 => {
            let tilt = c.tilt.min(TILT_KNEE);
            let pv = v2_remap(c.p, tilt);
            let ema = if c.has_prev {
                PENCIL_EMA_NEW * pv + PENCIL_EMA_OLD * c.prev_pressure
            } else {
                // 官方 `0x566fd4`：points 为空时直接跳过 EMA。
                pv
            };
            // 30° 以下 0、55° 以上 1；25° 正好是 55°-30°，所以 clamp 的上界就是 1。
            let a = ((tilt - PENCIL_TILT_LO) / PENCIL_TILT_RANGE).clamp(0.0, 1.0);
            Width {
                w: (ema + PENCIL_V2_BASE + a * a * PENCIL_TILT_BONUS) * t,
                pressure: ema,
            }
        }
        Tool::BallpointV2 => {
            let pv = v2_remap(c.p, c.tilt);
            Width {
                w: (pv * 3.0).max(MIN_WIDTH) * thick_eff(t),
                pressure: pv,
            }
        }
        // 马克笔 v2：`2.5 + 10·(0.5 + 0.5pv)·tilt²`，再乘厚度。倾角夹 1.0。
        Tool::MarkerV2 => {
            let tilt = c.tilt.min(TILT_KNEE);
            let pv = v2_remap(c.p, tilt);
            Width {
                w: (MARKER_BASE
                    + MARKER_V_GAIN * (pv * 0.5 + 0.5) * tilt * tilt)
                    * t,
                pressure: pv,
            }
        }
        Tool::SolidPen | Tool::ReservedPen => Width {
            w: MIN_WIDTH * t, // 0x566ed4：2·thickness，无下限夹取
            pressure: v2_remap(c.p, c.tilt),
        },
        // 书法：宽度只取决于**笔画方向与笔尖方向的夹角**，压力只是一个 4.5 倍的
        // 调制项（`pv == 0` 时仍然有 1.0·t 的底宽）。
        //
        // 这个"笔尖方向"只进公式，**不进渲染**：原版画出来是普通的变宽圆头笔画
        // （宽度=直径）。拿官方 .rm 里的 1216 个实测点回放，我们的公式 MAE 0.08px；
        // 渲染成扁笔尖四边形反而对不上（IoU 0.68 vs 圆头 0.75）。
        //   笔尖方向 = `π/2 - ((40° + 视图旋转))`（`0x5670d0`/`0x5670fc`/`0x7c7644`）
        //   夹角超过 60° → 最细；夹角 0 → 最粗。
        Tool::Calligraphy => {
            let tilt = c.tilt.min(TILT_KNEE);
            let pv = v2_remap(c.p, tilt);
            // 官方先把两根轴的倾角取 sin 再 atan2（`0x5670ac` / `0x5670bc`）。
            let tilt_dir = norm_tau(c.tilt_y.sin().atan2(c.tilt_x.sin()));
            let nib = FRAC_PI_2 - CALLI_NIB_OFFSET - c.rotation;
            let mut d = norm_tau(c.dir) - tilt_dir;
            if d < 0.0 {
                d += TAU;
            }
            d -= nib;
            if d < 0.0 {
                d += TAU;
            }
            if d > PI {
                d -= TAU;
            }
            let f = ((CALLI_ANGLE - d.abs()) / CALLI_ANGLE).max(0.0);
            let mut w = (CALLI_W_BASE + f * pv * CALLI_P_MUL) * t;
            if w < CALLI_MIN_W {
                w = CALLI_MIN_W;
            }
            if c.has_prev {
                w = w * CALLI_W_EMA_NEW + c.prev_width * CALLI_W_EMA_OLD;
            }
            Width { w, pressure: pv }
        }
    }
}

/// 铅笔类工具的"零压基准宽度"：硬度档位把围绕它的波动按比例收窄（`pencil::Grade::var`）。
///
/// 取的是各分支在**没压力、笔杆竖着**时的宽度，也就是公式里那些常数项——
/// 收窄波动不该改变"这支笔有多粗"，只改变"同一支笔在不同倾角/压力下差多少"。
pub fn pencil_nominal(tool: Tool, thickness: f32) -> f32 {
    let t = thickness;
    match tool {
        // 通用路径：`(1.5 + 1.5p) · t/1.5`
        Tool::Pencil => (PENCIL_CLAMP_LO * (t / PENCIL_THICK_DIV)).max(MIN_WIDTH),
        // v2 路径：`(ema + 2 + 15a²) · t`
        Tool::PencilV2 => PENCIL_V2_BASE * t,
        // 自动铅笔本来就恒定 4t（只有过倾角死区时 4t→4t/1.35 那一跳）
        Tool::SharpPencil => (SHARP_W * thick_eff(t)).max(MIN_WIDTH),
        Tool::SharpPencilV2 => SHARP_V2_W * t,
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(p: f32) -> Ctx {
        Ctx {
            p,
            prev_pressure: p,
            prev_width: 4.0,
            ..Ctx::default()
        }
    }

    fn w(tool: Tool, c: &Ctx, t: f32) -> f32 {
        width(tool, c, t).w
    }

    /// `p0 = 0.15`（倾角 0）时，让 pv 恰好落在中段的压力。
    fn tilted(p: f32, tilt: f32) -> Ctx {
        Ctx {
            p,
            tilt,
            ..Ctx::default()
        }
    }

    // ---------------------------------------------------------------- 荧光笔

    #[test]
    fn highlighter_is_always_30() {
        // `0x567808`：s15 = 30.0，厚度写死 1.0，唯一一个不吃任何输入的笔。
        for tool in [Tool::Highlighter, Tool::HighlighterV2] {
            for p in [0.0, 0.1, 0.5, 0.9, 1.0] {
                for tilt in [0.0, 0.4, 1.2] {
                    for t in [1.0, 2.0, 3.0, 0.7] {
                        let c = Ctx {
                            p,
                            tilt,
                            tilt_x: tilt,
                            tilt_y: tilt,
                            seg_dir: 1.0,
                            ..Ctx::default()
                        };
                        assert_eq!(w(tool, &c, t), 30.0, "{tool:?} p={p} tilt={tilt} t={t}");
                    }
                }
            }
        }
    }

    // ---------------------------------------------------------------- 铅笔

    #[test]
    fn pencil_v2_pressure_makes_it_wider() {
        // 倾角 0 时 pv = smoothstep((p - 0.15) / 0.85)，w = (pv + 2)·t
        let lo = w(Tool::PencilV2, &tilted(0.1, 0.0), 1.0);
        let hi = w(Tool::PencilV2, &tilted(0.9, 0.0), 1.0);
        assert!((lo - 2.0).abs() < 1e-5, "轻压应该是底宽 2，得到 {lo}");
        assert!(hi > lo * 1.4, "0.9 压力应该明显比 0.1 宽：{hi} vs {lo}");
        assert!((hi - 2.95).abs() < 0.05, "p=0.9 时约 2.95，得到 {hi}");
        // 单调
        let mut prev = -1.0;
        for i in 0..=100 {
            let v = w(Tool::PencilV2, &tilted(i as f32 / 100.0, 0.0), 1.0);
            assert!(v >= prev - 1e-6, "{v} < {prev}");
            prev = v;
        }
    }

    #[test]
    fn pencil_v2_tilt_bonus_is_30_to_55_degrees() {
        // a = clamp((tilt - 30°) / 25°)，30° 以下无加成，55° 以上加满 15
        let flat = w(Tool::PencilV2, &tilted(1.0, 0.3), 1.0);
        let knee = w(Tool::PencilV2, &tilted(1.0, 0.523_599), 1.0);
        let steep = w(Tool::PencilV2, &tilted(1.0, 0.959_931), 1.0);
        let over = w(Tool::PencilV2, &tilted(1.0, 1.4), 1.0);
        assert!((flat - knee).abs() < 1e-4, "30° 是拐点：{flat} vs {knee}");
        assert!(steep > knee + 10.0, "55° 应该多出 ~15 的加成：{steep} vs {knee}");
        assert!(
            (over - steep).abs() < 1e-4,
            "tilt 超过 1.0 被夹住（0x566d9c）：{over} vs {steep}"
        );
    }

    #[test]
    fn pencil_v2_ema_carries_previous_stored_pressure() {
        // 首点（没有前一点）不做 EMA，直接用 pv（0x566fd4）
        let first = width(Tool::PencilV2, &ctx(1.0), 1.0);
        assert!((first.pressure - 1.0).abs() < 1e-6, "{}", first.pressure);

        // 有前一点：0.2·pv + 0.8·prev
        let mut c = ctx(1.0);
        c.has_prev = true;
        c.prev_pressure = 0.0;
        let second = width(Tool::PencilV2, &c, 1.0);
        assert!((second.pressure - 0.2).abs() < 1e-6, "{}", second.pressure);

        // EMA 会拖后腿：同样的新压力，上一笔越轻这一笔越细
        let mut light = ctx(0.5);
        light.has_prev = true;
        light.prev_pressure = 0.0;
        let mut heavy = ctx(0.5);
        heavy.has_prev = true;
        heavy.prev_pressure = 0.9;
        assert!(w(Tool::PencilV2, &light, 2.0) < w(Tool::PencilV2, &heavy, 2.0));
    }

    #[test]
    fn generic_pencil_needs_tilt_and_perpendicular_stroke() {
        // 通用路径的 Pencil：tilt^4 过零 + |sin(倾角,笔画)|^3.5
        let flat_w = w(Tool::Pencil, &tilted(1.0, 0.0), 2.0);
        let mut tilted_along = tilted(1.0, 1.2);
        tilted_along.tilt_x = 1.2;
        tilted_along.seg_dir = 0.0; // 顺着倾角方向走 → v=0
        let mut tilted_across = tilted(1.0, 1.2);
        tilted_across.tilt_x = 1.2;
        tilted_across.seg_dir = FRAC_PI_2; // 垂直于倾角 → v=1
        assert!(flat_w <= w(Tool::Pencil, &tilted_along, 2.0));
        assert!(
            w(Tool::Pencil, &tilted_across, 2.0) > w(Tool::Pencil, &tilted_along, 2.0) + 1.0,
            "侧锋应该比顺纹宽"
        );
    }

    // ---------------------------------------------------------------- 自动铅笔

    #[test]
    fn sharp_pencil_v2_is_pressure_independent() {
        // 过了死区（0.15）以后宽度恒为 4·t/1.35，与压力无关
        let a = w(Tool::SharpPencilV2, &tilted(0.3, 0.0), 2.0);
        let b = w(Tool::SharpPencilV2, &tilted(1.0, 0.0), 2.0);
        assert!((a - b).abs() < 1e-6);
        assert!((a - 4.0 * 2.0 / 1.35).abs() < 1e-4, "{a}");

        // 官方在 pv==0（还没过死区）时仍用 4t——`0x566f50` 的 vdivgt 挂在 pv>0 上。
        let touch = w(Tool::SharpPencilV2, &tilted(0.0, 0.0), 2.0);
        assert!((touch - 8.0).abs() < 1e-4, "{touch}");
    }

    #[test]
    fn sharp_pencil_v2_stores_squashed_pressure() {
        // 0.7 + 0.3·smoothstep(pv)：压满 → 1.0，轻压 → 0.7
        let light = width(Tool::SharpPencilV2, &tilted(0.2, 0.0), 2.0).pressure;
        let full = width(Tool::SharpPencilV2, &tilted(1.0, 0.0), 2.0).pressure;
        assert!(light >= 0.7 - 1e-6 && light < 0.75, "{light}");
        assert!((full - 1.0).abs() < 1e-6, "{full}");
    }

    // ---------------------------------------------------------------- 毛笔

    #[test]
    fn paintbrush_is_very_pressure_sensitive() {
        let thin = w(Tool::PaintbrushV2, &tilted(0.3, 0.0), 1.0);
        let mid = w(Tool::PaintbrushV2, &tilted(0.7, 0.0), 1.0);
        let fat = w(Tool::PaintbrushV2, &tilted(1.0, 0.0), 1.0);
        assert!((thin - 2.0).abs() < 1e-4, "轻压落在 2.0 的地板上：{thin}");
        assert!(mid > thin, "{mid} vs {thin}");
        assert!(fat > mid * 2.0, "p^4.5 的尾巴很陡：{fat} vs {mid}");
        assert!((fat - 10.0).abs() < 1e-3, "p=1 时应为 10：{fat}");
        // 不乘厚度 1.35 的除数，直接线性乘厚度
        assert!((w(Tool::PaintbrushV2, &tilted(1.0, 0.0), 3.0) - 30.0).abs() < 1e-3);
    }

    #[test]
    fn paintbrush_tilt_term_is_not_speed() {
        // 第二项是 5·tilt^4（0x567830 读的是 hypot(tilt_x,tilt_y)），不是速度
        let mut flat = tilted(0.1, 0.0);
        flat.seg_dir = 2.0;
        let mut propped = tilted(0.1, 0.0);
        propped.tilt_x = 1.0;
        propped.tilt_y = 0.0;
        propped.seg_dir = 2.0;
        assert!((w(Tool::PaintbrushV2, &flat, 1.0) - 2.0).abs() < 1e-4);
        assert!(
            (w(Tool::PaintbrushV2, &propped, 1.0) - 5.0).abs() < 1e-3,
            "tilt=1 应该补上 5·1^4"
        );
    }

    // ---------------------------------------------------------------- 书法

    #[test]
    fn calligraphy_depends_on_direction_not_pressure() {
        // 笔尖在 50°（π/2 - 40°）：顺着笔尖走最宽，垂直/反向走最窄
        let nib = FRAC_PI_2 - CALLI_NIB_OFFSET;
        let mut along = tilted(0.6, 0.0);
        along.dir = norm_tau(nib);
        let mut across = tilted(0.6, 0.0);
        across.dir = norm_tau(nib + FRAC_PI_2);
        let wide = w(Tool::Calligraphy, &along, 2.0);
        let thin = w(Tool::Calligraphy, &across, 2.0);
        assert!(wide > thin + 2.0, "顺着笔尖 {wide} 应该明显宽于横切 {thin}");
        assert!((thin - 2.0).abs() < 1e-4, "横切只剩 1.0·t=2：{thin}");

        // 两个相反方向宽度不同
        let mut back = tilted(0.6, 0.0);
        back.dir = norm_tau(nib + PI);
        assert!(w(Tool::Calligraphy, &back, 2.0) < wide);
    }

    #[test]
    fn calligraphy_pressure_modulates_but_does_not_dominate() {
        let nib = FRAC_PI_2 - CALLI_NIB_OFFSET;
        let mut c = tilted(1.0, 0.0);
        c.dir = norm_tau(nib);
        let full = w(Tool::Calligraphy, &c, 2.0);
        let light = {
            let mut l = c;
            l.p = 0.0;
            w(Tool::Calligraphy, &l, 2.0)
        };
        assert!(full > light);
        assert!((light - 2.0).abs() < 1e-4, "没压力也有 1.0·t=2 底宽：{light}");
    }

    #[test]
    fn calligraphy_ema_damps_jumps() {
        let nib = FRAC_PI_2 - CALLI_NIB_OFFSET;
        let mut c = tilted(1.0, 0.0);
        c.dir = norm_tau(nib);
        c.has_prev = true;
        c.prev_width = 3.0;
        let blended = w(Tool::Calligraphy, &c, 2.0);
        let fresh = w(Tool::Calligraphy, &Ctx { has_prev: false, ..c }, 2.0);
        assert!(blended < fresh, "0.25/0.75 的 EMA 要被上一宽度拉住");
        assert!(blended > 2.0);
    }

    // ---------------------------------------------------------------- 公共

    #[test]
    fn fineliner_is_constant_width() {
        // 用户最常用的那支：2.0 × thickness，与压力无关
        for p in [0.0, 0.3, 0.7, 1.0] {
            assert_eq!(w(Tool::Fineliner, &ctx(p), 1.0), 2.0);
            assert_eq!(w(Tool::Fineliner, &ctx(p), 2.0), 4.0);
            assert_eq!(w(Tool::FinelinerV2, &ctx(p), 3.0), 6.0);
        }
    }

    #[test]
    fn eraser_is_thickness_squared() {
        // 实测锚点：官方 .rm 里 t=2.4 → 57.6，t=1.2 → 14.4
        assert!((w(Tool::Eraser, &ctx(0.5), 2.4) - 57.6).abs() < 1e-3);
        assert!((w(Tool::Eraser, &ctx(0.5), 1.2) - 14.4).abs() < 1e-3);
        // EraseSection 是固定 3×2.0（官方把 thickness 覆盖成 2.0）
        assert!((w(Tool::EraseSection, &ctx(0.5), 2.0) - 6.0).abs() < 1e-4);
        assert!((w(Tool::EraseSection, &ctx(0.5), 1.0) - 6.0).abs() < 1e-4);
    }

    #[test]
    fn eraser_and_sharp_pencil() {
        assert_eq!(w(Tool::Eraser, &ctx(0.5), 2.0), 40.0);
        // t>1 走 /1.35
        assert!((w(Tool::SharpPencil, &ctx(0.5), 2.0) - 3.0 * 2.0 / 1.35).abs() < 1e-4);
        assert!((w(Tool::SharpPencil, &ctx(0.5), 1.0) - 3.0).abs() < 1e-4);
    }

    #[test]
    fn ballpoint_quantizes_pressure() {
        // p=0.5 → v=1.5 < 2，不量化
        assert!((w(Tool::Ballpoint, &ctx(0.5), 2.0) - 1.5 * 2.0 / 1.35).abs() < 1e-4);
        // p=0.9 → v=2.7 → trunc 2
        assert!((w(Tool::Ballpoint, &ctx(0.9), 2.0) - 2.0 * 2.0 / 1.35).abs() < 1e-4);
    }

    #[test]
    fn v2_remap_shape() {
        // 倾角 0 → p0 = 0.15；压力低于死区直接 0
        assert_eq!(v2_remap(0.10, 0.0), 0.0);
        assert!((v2_remap(1.0, 0.0) - 1.0).abs() < 1e-6);
        // 倾角抬到 1 rad → p0 = 0.5
        assert!((v2_remap(0.5, 1.0)).abs() < 1e-6);
        // 单调
        let mut prev = -1.0;
        for i in 0..=100 {
            let v = v2_remap(i as f32 / 100.0, 0.4);
            assert!(v >= prev - 1e-6);
            prev = v;
        }
    }

    #[test]
    fn min_width_applies_to_generic_path_only() {
        // 通用路径统一夹 2.0；v2 路径没有这道（BallpointV2 轻压能到 1.48）
        for t in Tool::ALL {
            if t == Tool::ClearPage || t.is_v2() {
                continue;
            }
            let v = w(t, &ctx(0.0), 1.0);
            assert!(v >= MIN_WIDTH, "{t:?} -> {v}");
        }
        let light = w(Tool::BallpointV2, &tilted(0.0, 0.0), 2.0);
        assert!((light - 2.0 * 2.0 / 1.35).abs() < 1e-4);
        // v2 路径没有通用路径那道 2.0 的收尾夹取：厚度 < 1 时能掉到 2 以下
        let thin = w(Tool::BallpointV2, &tilted(0.0, 0.0), 0.8);
        assert!(thin < MIN_WIDTH, "v2 路径不该被 2.0 夹：{thin}");
        assert!((thin - 1.6).abs() < 1e-4);
    }

    #[test]
    fn ballpoint_v2_matches_measured_rm() {
        // MEASURED.md §1.4：tool 15 / thickness 2.0 的 .rm 里 width ∈ [3.0, 4.5]
        let mut lo = f32::MAX;
        let mut hi = f32::MIN;
        for i in 0..=100 {
            let v = w(Tool::BallpointV2, &tilted(i as f32 / 100.0, 0.0), 2.0);
            lo = lo.min(v);
            hi = hi.max(v);
        }
        assert!((lo - 2.0 * 2.0 / 1.35).abs() < 1e-3);
        assert!((hi - 3.0 * 2.0 / 1.35).abs() < 1e-3);
        assert!(lo >= 2.9 && hi <= 4.5);
    }

    #[test]
    fn marker_v2_uses_tilt() {
        let low = w(Tool::MarkerV2, &tilted(0.0, 0.0), 2.0);
        assert!((low - 2.5 * 2.0).abs() < 1e-4, "{low}");
        let propped = tilted(0.5, 1.0);
        let high = w(Tool::MarkerV2, &propped, 2.0);
        assert!(high > low, "{high} vs {low}");
        assert!(high <= (2.5 + 10.0) * 2.0 + 1e-3);
    }
}
