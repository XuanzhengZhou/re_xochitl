//! 选区（套索 / 框选）的几何与笔迹操作。
//!
//! 规格来源（都是反编译 QML，不是猜的）：
//!
//! - `refs/qml_all/res_066a78f.qml` = `SceneSelectionHandler.qml`（映射见
//!   `res_06711d0.qml:46`）。选框描边 2px（:75-76）、四个缩放手柄 + 旋转手柄，
//!   以及**悬浮操作条**：:224 `Row`，:227 `height: 62`、:228 `width: height * 2 - 1`
//!   （= 123）、:233 `spacing: -2`（两个 2px 边框拼成一条）；里面只有两个 62×62 按钮，
//!   :243 先 copy（`icon_duplicate`）→ :259 后 cut（`icon_cut`）。
//! - `refs/qml_all/res_0666bce.qml:346` 的选区工具提示也是同一套说法：
//!   "Circle your content to rotate, move, scale, cut, or copy it. Tap to paste."
//!   —— **粘贴是点一下，不是条上的按钮**；QML 里也确实没有 paste/duplicate/delete 按钮。
//! - `res_067887b.qml:1094-1104`：copy 只写剪贴板，cut 写剪贴板再删选中项。
//!
//! 本模块只做纯几何 / 纯布局（BRIEF 要求「布局是纯函数、能单测」），
//! 绘制与状态机由调用方负责。
//!
//! 一个必须知道的坑：`doc/rm.rs` 读盘时 `Line::bounds` 是 `Default`（全 0，见
//! `parse_line` 的构造处），所以本模块**一律从采样点重算包围盒**，不吃那个字段；
//! `translate` 也会把重算结果写回 `bounds`，两者因此始终一致。

use crate::pen::{Line, RectF};

/// 悬浮操作条上的动作。
///
/// 枚举给全了（键盘快捷键 / 剪贴板流程都会用到），但**操作条上只有 [`SelAct::Copy`]
/// 和 [`SelAct::Cut`]**——QML 依据见模块头注释。要别的组合用 [`bar_layout_acts`]。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelAct {
    Cut,
    Copy,
    Paste,
    Duplicate,
    Delete,
    SelectAll,
}

/// 悬浮操作条上的一个按钮。坐标是屏幕像素（左上有原点、y 向下）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Bar {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub act: SelAct,
}

// ---------------------------------------------------------------- 多边形

/// 点到线段的距离小于这个值就算「在边上」。坐标最大 ~1872，整数运算时叉积是精确的，
/// 留容差只是为了接住浮点误差。
const EDGE_EPS: f32 = 1e-3;

/// `p` 是否落在线段 `ab` 上（含端点）。
fn on_segment(a: (f32, f32), b: (f32, f32), p: (f32, f32)) -> bool {
    let (ax, ay) = a;
    let (bx, by) = b;
    let (px, py) = p;
    let cross = (bx - ax) * (py - ay) - (by - ay) * (px - ax);
    let len = ((bx - ax) * (bx - ax) + (by - ay) * (by - ay))
        .sqrt()
        .max(1.0);
    if cross.abs() > EDGE_EPS * len {
        return false;
    }
    px >= ax.min(bx) - EDGE_EPS
        && px <= ax.max(bx) + EDGE_EPS
        && py >= ay.min(by) - EDGE_EPS
        && py <= ay.max(by) + EDGE_EPS
}

/// 射线法：点是否在多边形内（`poly` 是闭合折线，不重复首点）。
///
/// **落在边上或顶点上算在内部**（选取时宁可多圈一点，也免得贴着边框的笔迹选不中）；
/// 少于 3 个点没有面积，一律 `false`。
pub fn point_in_poly(poly: &[(f32, f32)], p: (f32, f32)) -> bool {
    if poly.len() < 3 {
        return false;
    }
    for i in 0..poly.len() {
        if on_segment(poly[i], poly[(i + 1) % poly.len()], p) {
            return true;
        }
    }
    let (px, py) = p;
    let mut inside = false;
    for i in 0..poly.len() {
        let (x1, y1) = poly[i];
        let (x2, y2) = poly[(i + 1) % poly.len()];
        // 半开区间：顶点只算一次，避免「穿过顶点」被数成两次
        if (y1 > py) != (y2 > py) {
            let x = x1 + (py - y1) * (x2 - x1) / (y2 - y1);
            if x > px {
                inside = !inside;
            }
        }
    }
    inside
}

/// 多边形的包围盒。空多边形返回 `None`。
pub fn poly_bbox(poly: &[(f32, f32)]) -> Option<RectF> {
    let (mut x0, mut y0) = *poly.first()?;
    let (mut x1, mut y1) = (x0, y0);
    for &(x, y) in &poly[1..] {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    Some(RectF {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
}

// ---------------------------------------------------------------- 笔迹包围盒

/// 一笔的几何包围盒：从采样点重算，每条按**半笔宽**外扩。
///
/// 不用 `Line::bounds` 的原因见模块头（读盘后是默认值）；另外 `RectF::include`
/// 在零尺寸时会重置原点，拿它累加会丢掉首点，所以这里自己算 min/max。
fn geom(l: &Line) -> Option<RectF> {
    let first = l.points.first()?;
    let hw = first.width * 0.5;
    let (mut x0, mut y0) = (first.x - hw, first.y - hw);
    let (mut x1, mut y1) = (first.x + hw, first.y + hw);
    for p in &l.points[1..] {
        let hw = p.width * 0.5;
        x0 = x0.min(p.x - hw);
        y0 = y0.min(p.y - hw);
        x1 = x1.max(p.x + hw);
        y1 = y1.max(p.y + hw);
    }
    Some(RectF {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
}

fn union(a: RectF, b: RectF) -> RectF {
    let x0 = a.x.min(b.x);
    let y0 = a.y.min(b.y);
    let x1 = (a.x + a.w).max(b.x + b.w);
    let y1 = (a.y + a.h).max(b.y + b.h);
    RectF {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    }
}

fn overlap_area(a: RectF, b: RectF) -> f32 {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.w).min(b.x + b.w);
    let y1 = (a.y + a.h).min(b.y + b.h);
    if x1 <= x0 || y1 <= y0 {
        0.0
    } else {
        (x1 - x0) * (y1 - y0)
    }
}

fn area(r: RectF) -> f32 {
    r.w.max(0.0) * r.h.max(0.0)
}

/// 「实质重叠」的门槛：选区至少盖住笔迹包围盒的一半。
///
/// 用笔迹自己的面积做分母，而不是两边的 `min`——拿真 `.rm` 试过：小选区落在一笔
/// 大范围涂鸦的 bbox 里时，`min` 会让那条笔迹整条被选中（bbox 罩住选区 ≠ 有墨）。
const SUBSTANTIAL: f32 = 0.5;

// ---------------------------------------------------------------- 选中

/// 套索选中：整条笔迹的采样点**多数**落在多边形内，或者笔迹 bbox 与选区有实质重叠
/// （选区盖住 bbox 一半以上，且真有点落在多边形里——防「大笔迹 bbox 罩住小选区」误选）。
///
/// 空输入 / 退化多边形返回空。
pub fn lasso_select(lines: &[Line], poly: &[(f32, f32)]) -> Vec<usize> {
    if lines.is_empty() || poly.len() < 3 {
        return Vec::new();
    }
    let Some(pb) = poly_bbox(poly) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let Some(lb) = geom(l) else { continue };
        let hit = l
            .points
            .iter()
            .filter(|p| point_in_poly(poly, (p.x, p.y)))
            .count();
        let majority = hit * 2 > l.points.len();
        let covered = overlap_area(pb, lb) >= SUBSTANTIAL * area(lb);
        if majority || (hit > 0 && covered) {
            out.push(i);
        }
    }
    out
}

/// 框选：笔迹 bbox **完全**落在矩形内（含边界）才算选中——和套索的宽松规则相反，
/// 框选是「框住整条笔迹」的语义。
pub fn rect_select(lines: &[Line], r: RectF) -> Vec<usize> {
    if r.w <= 0.0 || r.h <= 0.0 {
        return Vec::new();
    }
    let (rx1, ry1) = (r.x + r.w, r.y + r.h);
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            geom(l)
                .is_some_and(|b| b.x >= r.x && b.y >= r.y && b.x + b.w <= rx1 && b.y + b.h <= ry1)
        })
        .map(|(i, _)| i)
        .collect()
}

/// 「擦选区」用的相交判定：笔迹 bbox 与矩形有**正面积**重叠就返回。
///
/// 和 [`rect_select`] 的"完全框住"相反——橡皮的语义是"框扫过的整条删掉"，
/// 拖框压住半条笔迹也该擦。只蹭到一条边（重叠面积为 0）不算，免得
/// 一次点击（零尺寸框）把贴着边框的笔迹全删了。
pub fn rect_hit(lines: &[Line], r: RectF) -> Vec<usize> {
    if r.w <= 0.0 || r.h <= 0.0 {
        return Vec::new();
    }
    let (rx1, ry1) = (r.x + r.w, r.y + r.h);
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            geom(l).is_some_and(|b| b.x < rx1 && b.x + b.w > r.x && b.y < ry1 && b.y + b.h > r.y)
        })
        .map(|(i, _)| i)
        .collect()
}

/// 选出这些笔迹的并集包围盒。空选集 / 全是越界下标返回 `None`。
pub fn bounds_of(lines: &[Line], idx: &[usize]) -> Option<RectF> {
    let mut out: Option<RectF> = None;
    for &i in idx {
        let Some(b) = lines.get(i).and_then(geom) else {
            continue;
        };
        out = Some(match out {
            None => b,
            Some(a) => union(a, b),
        });
    }
    out
}

// ---------------------------------------------------------------- 移动 / 搬走 / 放回

/// 平移若干条笔迹（`idx` 可以乱序、重复、越界），**`points` 和 `bounds` 一起更新**。
///
/// 重复的下标只搬一次——`idx` 是从选中集合来的，语义是「这些笔迹各移动 dx/dy」。
pub fn translate(lines: &mut [Line], idx: &[usize], dx: f32, dy: f32) {
    let mut v: Vec<usize> = idx.iter().copied().filter(|&i| i < lines.len()).collect();
    v.sort_unstable();
    v.dedup();
    for i in v {
        let l = &mut lines[i];
        if l.points.is_empty() {
            continue;
        }
        for p in &mut l.points {
            p.x += dx;
            p.y += dy;
        }
        l.bounds = geom(l).unwrap_or_default();
    }
}

/// 从页里摘走若干条笔迹，返回摘走的那些（**按原下标升序**，方便调用方记账）。
///
/// 下标先排序去重再**降序删**，避免每删一次都从头找一遍（那就是 O(n²)）。
pub fn take(lines: &mut Vec<Line>, idx: &[usize]) -> Vec<Line> {
    let mut v: Vec<usize> = idx.iter().copied().filter(|&i| i < lines.len()).collect();
    v.sort_unstable();
    v.dedup();
    let mut out = Vec::with_capacity(v.len());
    for &i in v.iter().rev() {
        out.push(lines.remove(i));
    }
    out.reverse();
    out
}

/// 放回去（追加到末尾；顺序会变，页内位置不变，因为坐标是绝对的）。
pub fn put_back(lines: &mut Vec<Line>, sel: Vec<Line>) {
    lines.extend(sel);
}

/// 点是否在矩形内（**右/下边界开区间**，做命中测试时相邻矩形不会抢同一像素）。
pub fn inside(b: RectF, p: (f32, f32)) -> bool {
    p.0 >= b.x && p.1 >= b.y && p.0 < b.x + b.w && p.1 < b.y + b.h
}

// ---------------------------------------------------------------- 悬浮操作条

const SCREEN_W: i32 = 1404;
const SCREEN_H: i32 = 1872;
/// 左工具条宽度（`chrome::W`，官方 120），操作条不能压上去。
const TOOLBAR_W: i32 = crate::chrome::W;
/// `SceneSelectionHandler.qml:227` 的 `height: 62`，也是每个按钮的边长（:237、:253
/// 的 `width: parent.height`）。
const BTN: i32 = 62;
/// 条与选框的间距 = `Values.iconSizeMedium`（`res_06ba1de.qml:146` 的 40，
/// 见 `SceneSelectionHandler.qml:330` 的 `const gap`）。
const BAR_GAP: i32 = 40;
/// 下方留白 = `pageLabel.height` = `Values.navigatorStatusbarHeight`
/// （`res_06ba1de.qml:52` 的 72；`res_067887b.qml:1085` 传的 `toolsMargin`）。
const TOOLS_MARGIN: i32 = 72;

/// 浮点 → 屏幕像素，越界夹紧。`f32 as i32` 是饱和转换，NaN 变 0，不会 panic。
fn px(v: f32, lo: i32, hi: i32) -> i32 {
    if hi < lo {
        return lo;
    }
    let i = if v.is_finite() { v.round() as i32 } else { lo };
    i.clamp(lo, hi)
}

/// 悬浮操作条。**只有 copy 和 cut**（QML 依据见模块头），顺序也是 QML 的顺序：
/// 左边 copy、右边 cut，每个 62×62。
///
/// 几何照 `SceneSelectionHandler.qml:328-348` 的 `updateToolsGeometry()`：
/// 竖直方向贴在选框下方 `gap = 40` 处，放不下（超过 `H - 72`）就翻到上方 `40` 处；
/// 水平方向以选框中心对齐，撞到左工具条或右屏边就贴边。
/// 有两处跟 QML 不同，都是为了「不出屏」：QML 翻到上方时不夹 y（选框贴顶会画到负坐标），
/// 这里夹到 `0`；负间距 `-2` 在 QML 里是两个边框叠成一条，这里按 0 间距排格子，
/// 所以条宽是 `n × 62`（两个按钮 124，QML 声明的是 123，差的 1px 就是叠掉的边框）。
///
/// `bounds` 是屏幕坐标（调用方已经加过页面偏移）。空选区不出条——和 QML 的
/// `open()` 里 `sceneRect.width == 0 || height == 0 → return`（:296-298）一致。
pub fn bar_layout(bounds: RectF) -> Vec<Bar> {
    bar_layout_acts(bounds, &[SelAct::Copy, SelAct::Cut])
}

/// [`bar_layout`] 的通用版：按任意动作序列排条（格子大小 / 间距 / 夹紧规则完全一样）。
pub fn bar_layout_acts(bounds: RectF, acts: &[SelAct]) -> Vec<Bar> {
    if acts.is_empty() || bounds.w <= 0.0 || bounds.h <= 0.0 {
        return Vec::new();
    }
    // 最多排满一屏，保证每个格子都在屏内
    let n = (acts.len() as i32).min(SCREEN_W / BTN) as usize;
    let w = n as i32 * BTN;

    // 竖直：优先后下方，压到底部留白就翻到上方
    let mut y = bounds.y + bounds.h + BAR_GAP as f32;
    if y + BTN as f32 > (SCREEN_H - TOOLS_MARGIN) as f32 {
        y = bounds.y - (BAR_GAP + BTN) as f32;
    }
    let y = px(y, 0, SCREEN_H - BTN);

    // 水平：以选框中心对齐，左右贴边夹紧（左边不压工具条）
    let cx = bounds.x + (bounds.w * 0.5).round();
    let x = px(
        cx - w as f32 * 0.5,
        TOOLBAR_W.min(SCREEN_W - w),
        SCREEN_W - w,
    );

    acts[..n]
        .iter()
        .enumerate()
        .map(|(i, &act)| Bar {
            x: x + i as i32 * BTN,
            y,
            w: BTN,
            h: BTN,
            act,
        })
        .collect()
}

/// 命中操作条。后画的在上层，所以从后往前找；边界规则同 [`inside`]。
pub fn bar_hit(bars: &[Bar], x: f32, y: f32) -> Option<SelAct> {
    bars.iter()
        .rev()
        .find(|b| {
            inside(
                RectF {
                    x: b.x as f32,
                    y: b.y as f32,
                    w: b.w as f32,
                    h: b.h as f32,
                },
                (x, y),
            )
        })
        .map(|b| b.act)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pen::{Color, Point, Tool};

    /// 造一笔：笔宽固定 2.0（半宽 1），方便算包围盒。
    fn line(pts: &[(f32, f32)]) -> Line {
        let mut l = Line::new(Tool::BallpointV2, Color::Black, 1.0);
        for &(x, y) in pts {
            l.push(Point {
                x,
                y,
                width: 2.0,
                ..Default::default()
            });
        }
        l
    }

    const SQUARE: [(f32, f32); 4] = [(0.0, 0.0), (100.0, 0.0), (100.0, 100.0), (0.0, 100.0)];

    #[test]
    fn point_in_poly_concave() {
        // L 形：右下角 (60,60)-(100,100) 挖掉
        let poly = [
            (0.0, 0.0),
            (100.0, 0.0),
            (100.0, 60.0),
            (60.0, 60.0),
            (60.0, 100.0),
            (0.0, 100.0),
        ];
        assert!(point_in_poly(&poly, (30.0, 30.0)));
        assert!(point_in_poly(&poly, (90.0, 30.0)));
        assert!(point_in_poly(&poly, (30.0, 90.0)));
        // 缺口里是外面
        assert!(!point_in_poly(&poly, (80.0, 80.0)));
        assert!(!point_in_poly(&poly, (120.0, 30.0)));
    }

    #[test]
    fn point_in_poly_on_edge_counts_as_inside() {
        assert!(point_in_poly(&SQUARE, (0.0, 50.0)));
        assert!(point_in_poly(&SQUARE, (50.0, 0.0)));
        assert!(point_in_poly(&SQUARE, (100.0, 50.0)));
        assert!(point_in_poly(&SQUARE, (50.0, 100.0)));
        assert!(point_in_poly(&SQUARE, (0.0, 0.0))); // 顶点
        assert!(!point_in_poly(&SQUARE, (-0.5, 50.0)));
    }

    #[test]
    fn point_in_poly_degenerate_is_false() {
        assert!(!point_in_poly(&[], (0.0, 0.0)));
        assert!(!point_in_poly(&[(0.0, 0.0)], (0.0, 0.0)));
        assert!(!point_in_poly(&[(0.0, 0.0), (10.0, 10.0)], (5.0, 5.0)));
    }

    #[test]
    fn poly_bbox_of_empty_is_none() {
        assert_eq!(poly_bbox(&[]), None);
        assert_eq!(
            poly_bbox(&SQUARE),
            Some(RectF {
                x: 0.0,
                y: 0.0,
                w: 100.0,
                h: 100.0,
            })
        );
        assert_eq!(
            poly_bbox(&[(-5.0, 3.0), (7.0, 9.0), (2.0, -4.0)]),
            Some(RectF {
                x: -5.0,
                y: -4.0,
                w: 12.0,
                h: 13.0,
            })
        );
    }

    #[test]
    fn lasso_selects_inside_and_skips_outside() {
        let lines = vec![
            line(&[(10.0, 10.0), (40.0, 40.0)]),             // 全在框里
            line(&[(200.0, 200.0), (260.0, 260.0)]),         // 远在外面
            line(&[(10.0, 10.0), (10.0, 10.0)]),             // 单点，在框里
            Line::new(Tool::BallpointV2, Color::Black, 1.0), // 没有点
        ];
        assert_eq!(lasso_select(&lines, &SQUARE), vec![0, 2]);
    }

    #[test]
    fn lasso_empty_inputs() {
        assert!(lasso_select(&[], &SQUARE).is_empty());
        assert!(lasso_select(&[line(&[(10.0, 10.0)])], &[]).is_empty());
        assert!(lasso_select(&[line(&[(10.0, 10.0)])], &[(0.0, 0.0), (10.0, 10.0)]).is_empty());
    }

    #[test]
    fn lasso_bbox_overlap_fallback() {
        // 两个点：一个在框里、一个探出去 → 不算多数；但包围盒 64% 被选区盖住 → 选中
        let half_in = line(&[(10.0, 10.0), (150.0, 50.0)]);
        // 只擦到右上角，包围盒几乎没被盖住 → 两条规则都不选
        let clip = line(&[(95.0, 95.0), (300.0, 300.0)]);
        assert_eq!(lasso_select(&[half_in, clip], &SQUARE), vec![0]);

        // 凹多边形：bbox 全在选区 bbox 里、但点全在缺口里 → 不能选
        let l_shape = [
            (0.0, 0.0),
            (100.0, 0.0),
            (100.0, 60.0),
            (60.0, 60.0),
            (60.0, 100.0),
            (0.0, 100.0),
        ];
        let in_notch = line(&[(70.0, 70.0), (90.0, 90.0)]);
        assert!(lasso_select(&[in_notch], &l_shape).is_empty());
    }

    #[test]
    fn rect_select_boundary() {
        let lines = vec![
            line(&[(10.0, 10.0), (20.0, 20.0)]),  // bbox 9..21
            line(&[(50.0, 50.0), (150.0, 50.0)]), // 探出右边
        ];
        let all = RectF {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
        };
        assert_eq!(rect_select(&lines, all), vec![0]);
        // 恰好贴着边界算选中
        let exact = RectF {
            x: 9.0,
            y: 9.0,
            w: 12.0,
            h: 12.0,
        };
        assert_eq!(rect_select(&lines, exact), vec![0]);
        // 边界差 0.5px 就不算
        let tight = RectF {
            x: 9.5,
            y: 9.0,
            w: 11.5,
            h: 12.0,
        };
        assert!(rect_select(&lines, tight).is_empty());
        // 空矩形什么都选不中
        let empty = RectF {
            x: 0.0,
            y: 0.0,
            w: 0.0,
            h: 100.0,
        };
        assert!(rect_select(&lines, empty).is_empty());
        assert!(rect_select(&[], all).is_empty());
    }

    /// 6d：擦选区的"相交"判定。压住半条的也要选（和 `rect_select` 的
    /// "完全框住"相反），框外的不选、只贴边（重叠面积为 0）的也不选。
    #[test]
    fn rect_hit_intersects_partial_strokes() {
        let lines = vec![
            line(&[(10.0, 10.0), (20.0, 20.0)]),      // 0 整条在框里（bbox 9..21）
            line(&[(50.0, 50.0), (150.0, 60.0)]),     // 1 右边探出框
            line(&[(200.0, 200.0), (260.0, 260.0)]),  // 2 完全在框外
            line(&[(95.0, 40.0), (300.0, 40.0)]),     // 3 只有一头在框里
            line(&[(0.0, -1.0), (0.0, -1.0)]),        // 4 bbox y=-2..0：贴上边界外
            Line::new(Tool::BallpointV2, Color::Black, 1.0), // 5 没有点
        ];
        let all = RectF {
            x: 0.0,
            y: 0.0,
            w: 100.0,
            h: 100.0,
        };
        assert_eq!(rect_hit(&lines, all), vec![0, 1, 3]);
        // 对照：框选（完全框住）只认 0 —— 擦除要的是相交
        assert_eq!(rect_select(&lines, all), vec![0]);
        // 只压住 2 号笔迹的一角也算（bbox 199..261）
        let corner = RectF {
            x: 210.0,
            y: 210.0,
            w: 20.0,
            h: 20.0,
        };
        assert_eq!(rect_hit(&lines, corner), vec![2]);
        // 零尺寸框（点一下没拖）/ 空输入：什么都不删
        assert!(rect_hit(
            &lines,
            RectF {
                x: 10.0,
                y: 10.0,
                w: 0.0,
                h: 0.0
            }
        )
        .is_empty());
        assert!(rect_hit(&[], all).is_empty());
    }

    #[test]
    fn bounds_of_union_and_empty() {
        let lines = vec![
            line(&[(10.0, 10.0), (20.0, 20.0)]), // bbox 9..21
            line(&[(50.0, 50.0), (60.0, 60.0)]), // bbox 49..61
        ];
        assert_eq!(bounds_of(&lines, &[]), None);
        assert_eq!(bounds_of(&lines, &[7, 9]), None); // 全越界
        assert_eq!(bounds_of(&[], &[0]), None);
        assert_eq!(
            bounds_of(&lines, &[1]),
            Some(RectF {
                x: 49.0,
                y: 49.0,
                w: 12.0,
                h: 12.0,
            })
        );
        assert_eq!(
            bounds_of(&lines, &[0, 1]),
            Some(RectF {
                x: 9.0,
                y: 9.0,
                w: 52.0,
                h: 52.0,
            })
        );
    }

    #[test]
    fn translate_moves_points_and_bounds() {
        let mut lines = vec![line(&[(10.0, 10.0), (20.0, 20.0)]), line(&[(50.0, 50.0)])];
        translate(&mut lines, &[1, 0, 0], 5.0, -3.0);
        assert_eq!(lines[0].points[0].x, 15.0);
        assert_eq!(lines[0].points[1].y, 17.0);
        // 平移不改大小，bounds 跟着走
        assert_eq!(
            lines[0].bounds,
            RectF {
                x: 14.0,
                y: 6.0,
                w: 12.0,
                h: 12.0,
            }
        );
        assert_eq!(
            lines[1].bounds,
            RectF {
                x: 54.0,
                y: 46.0,
                w: 2.0,
                h: 2.0,
            }
        );
        // 越界下标忽略，不 panic
        translate(&mut lines, &[9], 1.0, 1.0);
        translate(&mut [], &[0], 1.0, 1.0);
    }

    #[test]
    fn take_unsorted_and_duplicate_indices() {
        let mut lines = vec![
            line(&[(0.0, 0.0)]),
            line(&[(10.0, 0.0)]),
            line(&[(20.0, 0.0)]),
            line(&[(30.0, 0.0)]),
        ];
        let out = take(&mut lines, &[2, 0, 2, 9]);
        assert_eq!(out.len(), 2);
        // 按原下标升序返回
        assert_eq!(out[0].points[0].x, 0.0);
        assert_eq!(out[1].points[0].x, 20.0);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].points[0].x, 10.0);
        assert_eq!(lines[1].points[0].x, 30.0);

        assert!(take(&mut lines, &[]).is_empty());
        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn take_then_put_back_restores() {
        let mut lines = vec![
            line(&[(10.0, 10.0), (20.0, 20.0)]),
            line(&[(50.0, 50.0), (60.0, 60.0)]),
            line(&[(90.0, 90.0)]),
        ];
        let before = bounds_of(&lines, &[0, 1, 2]).unwrap();
        let sel = take(&mut lines, &[1]);
        assert_eq!(lines.len(), 2);
        put_back(&mut lines, sel);
        assert_eq!(lines.len(), 3);
        assert_eq!(bounds_of(&lines, &[0, 1, 2]), Some(before));
    }

    #[test]
    fn inside_is_half_open() {
        let r = RectF {
            x: 10.0,
            y: 20.0,
            w: 30.0,
            h: 40.0,
        };
        assert!(inside(r, (10.0, 20.0))); // 左上闭
        assert!(inside(r, (39.9, 59.9)));
        assert!(!inside(r, (40.0, 30.0))); // 右边开
        assert!(!inside(r, (20.0, 60.0))); // 下边开
        assert!(!inside(r, (9.9, 30.0)));
        assert!(!inside(r, (20.0, 19.9)));
        assert!(!inside(
            RectF {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 0.0
            },
            (0.0, 0.0)
        ));
    }

    #[test]
    fn bar_layout_qml_metrics() {
        let bars = bar_layout(RectF {
            x: 400.0,
            y: 600.0,
            w: 200.0,
            h: 100.0,
        });
        assert_eq!(bars.len(), 2);
        // QML 顺序：先 copy（icon_duplicate）后 cut（icon_cut）
        assert_eq!(bars[0].act, SelAct::Copy);
        assert_eq!(bars[1].act, SelAct::Cut);
        for b in &bars {
            assert_eq!((b.w, b.h), (62, 62));
        }
        // 两个格子首尾相接、不重叠
        assert_eq!(bars[0].x + bars[0].w, bars[1].x);
        assert_eq!(bars[0].y, bars[1].y);
        // 竖直：选框下方 40px
        assert_eq!(bars[0].y, 740);
        // 水平：以选框中心 500 对齐，500 - 124/2
        assert_eq!(bars[0].x, 438);
    }

    #[test]
    fn bar_layout_stays_on_screen() {
        let cases = [
            RectF {
                x: -900.0,
                y: -900.0,
                w: 3000.0,
                h: 3000.0,
            }, // 比屏还大
            RectF {
                x: 0.0,
                y: 0.0,
                w: 4.0,
                h: 4.0,
            }, // 左上角
            RectF {
                x: 1300.0,
                y: 1800.0,
                w: 100.0,
                h: 70.0,
            }, // 右下角，得翻到上方
            RectF {
                x: 2000.0,
                y: 500.0,
                w: 50.0,
                h: 50.0,
            }, // 完全在屏外
        ];
        for b in cases {
            let bars = bar_layout(b);
            assert_eq!(bars.len(), 2, "{b:?}");
            for bar in &bars {
                assert!(bar.x >= 0 && bar.y >= 0, "{bar:?}");
                assert!(bar.x + bar.w <= 1404 && bar.y + bar.h <= 1872, "{bar:?}");
            }
            assert!(bars[0].x + bars[0].w <= bars[1].x, "重叠 {bars:?}");
        }
        // 空选区不出条
        assert!(bar_layout(RectF {
            x: 10.0,
            y: 10.0,
            w: 0.0,
            h: 0.0
        })
        .is_empty());
    }

    #[test]
    fn bar_hit_edges() {
        let bars = bar_layout(RectF {
            x: 100.0,
            y: 100.0,
            w: 200.0,
            h: 100.0,
        });
        let a = bars[0];
        let b = bars[1];
        assert_eq!(bar_hit(&bars, 169.0, 271.0), Some(SelAct::Copy));
        assert_eq!(bar_hit(&bars, b.x as f32, 271.0), Some(SelAct::Cut)); // 左边界算
        assert_eq!(bar_hit(&bars, (b.x + b.w) as f32, 271.0), None); // 右边界不算
        assert_eq!(bar_hit(&bars, a.x as f32 - 0.1, 271.0), None);
        assert_eq!(bar_hit(&bars, 169.0, a.y as f32), Some(SelAct::Copy)); // 上边界算
        assert_eq!(bar_hit(&bars, 169.0, (a.y + a.h) as f32), None); // 下边界不算
        assert!(bar_hit(&[], 0.0, 0.0).is_none());
    }
}
