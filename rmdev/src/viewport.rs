//! 缩放 / 平移 / 旋转的视图变换 + 手指手势识别。
//!
//! 页面逻辑坐标 = `.rm` 坐标：1404×1872，原点左上，y 向下；屏幕同尺寸。
//!
//! 变换（对应官方 `DeviceSceneTileManager` 的 scene→view）：
//!
//! ```text
//! q = R(rot) · (p − page/2)              // 先移到页面中心，再绕页中心转
//! s = q · scale + screen/2 + (tx, ty)    // 缩放，摆到屏幕中心，再平移
//! ```
//!
//! 原点取"页面中心"而不是左上角，是为了跟 QML 对齐：`tileManager.center` 是屏幕
//! 中心处的 scene 坐标，而 `defaultFitCenter.x === 0`（`refs/qml_all/res_0667e1f.qml:62`）
//! 说明 scene x=0 恰好是页面横向中心。这样 `tx/ty` 天然是"相对居中的偏移"：
//! `t = 0` 就是页面居中，`fit_*` 的平移量也是 0，夹取只是一条对称区间。
//!
//! rot 只取 90 的倍数，用 (0,±1) 的精确置换实现，不调 sin/cos ——
//! `page_to_screen` 与 `screen_to_page` 互为逆且不含三角函数舍入误差。
//!
//! 组合顺序（`Tracker` 的 `Pinch` 怎么喂给 `View`）：先 `zoom_at(factor, 旧中点)`
//! 再 `pan(dcenter)`。这与 QML `updatePinch`（`res_067f7e8.qml:103-149`）的
//! "先 moveViewBy(dcenter) 再 zoomOnPoint(新中点)"是同一个映射：两者都把旧中点
//! 底下的页面点搬到新中点，其余点的像也完全相同。每帧最后调一次 `clamp_pan()`。

/// 页面逻辑尺寸（`.rm` 坐标单位）。
pub const PAGE_W: f32 = 1404.0;
pub const PAGE_H: f32 = 1872.0;

/// 屏幕像素尺寸。数值上与页面相同，但语义不同，别混用。
pub const SCREEN_W: f32 = 1404.0;
pub const SCREEN_H: f32 = 1872.0;

/// 当前逻辑屏幕尺寸：竖屏 `(1404, 1872)`，横屏 `(1872, 1404)`。
///
/// 横屏模式（`crate::fb::set_landscape`）把整幅画面转 90°，视口也跟着从
/// "高比宽大"变成"宽比高大"。`View` 的四个字段里没有屏幕尺寸，而它又被
/// 到处按值拷来拷去（`Copy`），所以屏幕尺寸走这个全局 —— 一台机器只有一块
/// 面板，方向本来就是进程级事实。单测不碰它，默认就是竖屏。
#[inline]
pub fn screen() -> (f32, f32) {
    let (w, h) = crate::fb::logical();
    (w as f32, h as f32)
}

/// 标准页尺寸。`View` 只有四个字段（签名固定），页尺寸只能走常量；
/// 非标准页（PDF / ePub）用带 `page` 参数的那几个方法。
pub const PAGE: (f32, f32) = (PAGE_W, PAGE_H);

/// QML `updatePinch`：`scaleFromCrop > 5` 直接截断，注释写着 "max zoom is 5x"
/// （`res_067f7e8.qml:113-115`）。
pub const MAX_SCALE: f32 = 5.0;
/// QML `updateDragAndZoom`：`// min zoom is 0.5x`（`res_067f7e8.qml:208`）。
pub const MIN_SCALE: f32 = 0.5;

/// 单帧允许的缩放倍数范围。人手一帧做不到 10 倍，超出的只会是坏数据。
const MAX_FRAME_RATIO: f32 = 10.0;

/// 两帧手指距离小于这个值就算"重合"，缩放比退回 1。
const COINCIDENT: f32 = 1e-3;

/// 一帧的手指手势。翻页判定留在主程序，这里只报位移 / 缩放。
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Gesture {
    None,
    /// 单指位移（屏幕像素）。
    Drag {
        dx: f32,
        dy: f32,
    },
    /// 双指：缩放比 + 本帧中点 + 中点位移。
    Pinch {
        factor: f32,
        center: (f32, f32),
        dcenter: (f32, f32),
    },
}

/// 视图变换。`rot` 是 90° 的倍数，整页绕屏幕中心转。
///
/// 注意 `u8` 装不下 270：字段里写 `0/90/180` 就是度数，270° 写成序号 `3`
/// （两种写法都认，见 `turns()`）。要拿角度请用 `set_rot()`，别自己搬字段。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub scale: f32,
    pub rot: u8,
    pub tx: f32,
    pub ty: f32,
}

impl View {
    pub fn identity() -> View {
        View {
            scale: 1.0,
            rot: 0,
            tx: 0.0,
            ty: 0.0,
        }
    }

    pub fn page_to_screen(&self, p: (f32, f32)) -> (f32, f32) {
        let s = self.scale_ok();
        let (tx, ty) = self.t_ok();
        let (qx, qy) = rot_fwd(turns(self.rot), p.0 - PAGE_W * 0.5, p.1 - PAGE_H * 0.5);
        let (sw, sh) = screen();
        (qx * s + sw * 0.5 + tx, qy * s + sh * 0.5 + ty)
    }

    pub fn screen_to_page(&self, s: (f32, f32)) -> (f32, f32) {
        let sc = self.scale_ok();
        let (tx, ty) = self.t_ok();
        let (sw, sh) = screen();
        let (qx, qy) = rot_inv(
            turns(self.rot),
            (s.0 - sw * 0.5 - tx) / sc,
            (s.1 - sh * 0.5 - ty) / sc,
        );
        (qx + PAGE_W * 0.5, qy + PAGE_H * 0.5)
    }

    /// 以屏幕点 `anchor_screen` 为锚缩放：锚点底下的内容保持不动。
    /// `factor` 是倍数，与 QML `zoomOnPoint` 语义一致（`updatePinch` 里
    /// `newScale = pinchScale / lastScale`，`res_067f7e8.qml:105`）。
    pub fn zoom_at(&mut self, factor: f32, anchor_screen: (f32, f32)) {
        if !factor.is_finite() || factor <= 0.0 {
            return;
        }
        let old = self.scale_ok();
        // QML 的上下限；到顶了就把倍数吃掉，但不许漂移。
        let target = (old * factor).clamp(MIN_SCALE, MAX_SCALE);
        if !target.is_finite() || target <= 0.0 {
            return;
        }
        // 直接反解平移量：让 anchor 在**新** scale 下仍映射到 anchor，锚点误差为 0。
        // （不用增量公式，免得来回缩放累积漂移。）
        let p = self.screen_to_page(anchor_screen);
        let (qx, qy) = rot_fwd(turns(self.rot), p.0 - PAGE_W * 0.5, p.1 - PAGE_H * 0.5);
        self.scale = target;
        let (sw, sh) = screen();
        self.tx = anchor_screen.0 - sw * 0.5 - qx * target;
        self.ty = anchor_screen.1 - sh * 0.5 - qy * target;
    }

    /// 屏幕像素平移：内容跟着手指走。
    pub fn pan(&mut self, dx: f32, dy: f32) {
        if !dx.is_finite() || !dy.is_finite() {
            return;
        }
        self.tx += dx;
        self.ty += dy;
    }

    /// 标准页（1404×1872）的夹取。
    pub fn clamp_pan(&mut self) {
        self.clamp_pan_of(PAGE)
    }

    /// 夹取：页面比屏幕大时不许露白（QML `updatePinch` 把视口夹进内容矩形，
    /// `res_067f7e8.qml:128-142`）；比屏幕小时不再让人拖着跑，固定居中
    /// （对应 `defaultFitCenter` 的"装得下就摆正中"，`res_0667e1f.qml:62`）。
    pub fn clamp_pan_of(&mut self, page: (f32, f32)) {
        let (bw, bh) = self.box_on_screen(page);
        let (tx, ty) = self.t_ok();
        let (sw, sh) = screen();
        self.tx = clamp_axis(tx, bw, sw);
        self.ty = clamp_axis(ty, bh, sh);
    }

    /// 旋转：整页作为当前屏幕内容绕**屏幕中心**刚性旋转。
    /// 屏幕点 `q` 的新位置是 `screen/2 + R(q − screen/2)`，而页面中心在
    /// `screen/2 + t`，所以等价于 `t ← R·t`。四次 90° 回来分毫不差。
    ///
    /// 参数写法见 `turns()`：`0/90/180` 与 `1/2/3` 都认，其它值就近取到 90 的
    /// 倍数（45→90，44→0），`turns(rot)` 永远是 0..3。
    pub fn set_rot(&mut self, deg: u8) {
        let want = turns(deg);
        let delta = (want + 4 - turns(self.rot)) % 4;
        let (tx, ty) = self.t_ok();
        let (nx, ny) = rot_fwd(delta, tx, ty);
        self.rot = rot_field(want);
        self.tx = nx;
        self.ty = ny;
    }

    /// 页面在屏幕上的外框 `(x, y, w, h)`（标准页）。
    pub fn screen_rect_of_page(&self) -> (f32, f32, f32, f32) {
        self.screen_rect_of(PAGE)
    }

    /// 同上，页面尺寸由调用方给。90/270 时宽高在屏幕上对调。
    pub fn screen_rect_of(&self, page: (f32, f32)) -> (f32, f32, f32, f32) {
        let (bw, bh) = self.box_on_screen(page);
        let (tx, ty) = self.t_ok();
        let (sw, sh) = screen();
        (
            sw * 0.5 + tx - bw * 0.5,
            sh * 0.5 + ty - bh * 0.5,
            bw,
            bh,
        )
    }

    /// 页面贴宽：宽度正好等于屏幕宽（QML `FitToWidth = viewportSize.width /
    /// contentRect.width`，`refs/qml_all/res_0667e1f.qml:52`），另一轴装不下就
    /// 居中溢出。`screen` 传真实视口（1404×1872）——变换本身固定在屏幕坐标系里，
    /// 传别的值只影响算出来的 scale，平移量仍按"页面居中"算。
    pub fn fit_width(screen: (f32, f32), page: (f32, f32)) -> View {
        fit(screen, page, false)
    }

    /// 页面贴高（`FitToHeight = viewportSize.height / contentRect.height`，
    /// `res_0667e1f.qml:50`）。注：没有"Use current view"的变体——那是把当前
    /// scale 记成一个自定义 fit（`res_067887b.qml:1922-1930`），由主程序存。
    pub fn fit_height(screen: (f32, f32), page: (f32, f32)) -> View {
        fit(screen, page, true)
    }

    /// 字段是 pub 的，别人可以直接塞 NaN 进来；变换前统一兜底，
    /// 保证 `page_to_screen` / `screen_to_page` 在任何字段值下仍互逆。
    fn scale_ok(&self) -> f32 {
        if self.scale.is_finite() && self.scale > 0.0 {
            self.scale
        } else {
            1.0
        }
    }

    fn t_ok(&self) -> (f32, f32) {
        (
            if self.tx.is_finite() { self.tx } else { 0.0 },
            if self.ty.is_finite() { self.ty } else { 0.0 },
        )
    }

    /// 页面在屏幕上的轴对齐尺寸。
    fn box_on_screen(&self, page: (f32, f32)) -> (f32, f32) {
        let s = self.scale_ok();
        let pw = pos_or(page.0, PAGE_W);
        let ph = pos_or(page.1, PAGE_H);
        if turns(self.rot) % 2 == 1 {
            (ph * s, pw * s)
        } else {
            (pw * s, ph * s)
        }
    }
}

impl Default for View {
    fn default() -> Self {
        View::identity()
    }
}

/// 手指手势识别：把每帧的触摸点换算成增量。
///
/// 不做激活阈值 / 超时判定（QML 的 `distanceThreshold`、`timeout` 属于
/// `TouchAreaPinchFilter`，`res_067887b.qml:1294-1341`）——这里每帧给原始增量，
/// 阈值和翻页判定留给主程序。
#[derive(Clone, Debug, Default)]
pub struct Tracker {
    /// 上一帧的前两根手指。
    prev: [(i64, f32, f32); 2],
    /// 上一帧手指数（0/1/2）。0 表示上一帧没手指。
    n: usize,
}

impl Tracker {
    pub fn new() -> Self {
        Tracker::default()
    }

    /// 每次触摸帧调用。`fingers` 是当前仍按着的点 `[(id, x, y)]`（屏幕坐标），
    /// 顺序即使用顺序（只用前两根）。
    pub fn update(&mut self, fingers: &[(i64, f32, f32)]) -> Gesture {
        let mut cur = [(0i64, 0.0f32, 0.0f32); 2];
        let mut n = 0;
        for f in fingers {
            if n == 2 {
                break;
            }
            // 坐标坏了就当这根手指不在，别让它污染基准
            if !f.1.is_finite() || !f.2.is_finite() {
                continue;
            }
            cur[n] = *f;
            n += 1;
        }

        let prev_n = self.n;
        let g = if n == 0 || n != prev_n {
            // 手指数变化（含 0）：这一帧只重取基准，绝不能产生跳变
            Gesture::None
        } else if n == 1 {
            Gesture::Drag {
                dx: cur[0].1 - self.prev[0].1,
                dy: cur[0].2 - self.prev[0].2,
            }
        } else {
            pinch(&self.prev, &cur)
        };

        self.prev = cur;
        self.n = n;
        g
    }

    pub fn reset(&mut self) {
        *self = Tracker::default();
    }

    pub fn fingers(&self) -> usize {
        self.n
    }
}

fn pinch(prev: &[(i64, f32, f32); 2], cur: &[(i64, f32, f32); 2]) -> Gesture {
    let dp = dist(prev[0], prev[1]);
    let dc = dist(cur[0], cur[1]);
    // 两指重合时 dp == 0，倍数没有定义：退回 1（不缩），绝不产出 Inf/NaN。
    // 单帧超过 10 倍一律当坏数据。
    let factor = if dp > COINCIDENT {
        let f = dc / dp;
        if f.is_finite() {
            f.clamp(1.0 / MAX_FRAME_RATIO, MAX_FRAME_RATIO)
        } else {
            1.0
        }
    } else {
        1.0
    };
    let c = mid(cur[0], cur[1]);
    let pc = mid(prev[0], prev[1]);
    Gesture::Pinch {
        factor,
        center: c,
        dcenter: (c.0 - pc.0, c.1 - pc.1),
    }
}

fn dist(a: (i64, f32, f32), b: (i64, f32, f32)) -> f32 {
    let dx = b.1 - a.1;
    let dy = b.2 - a.2;
    (dx * dx + dy * dy).sqrt()
}

fn mid(a: (i64, f32, f32), b: (i64, f32, f32)) -> (f32, f32) {
    ((a.1 + b.1) * 0.5, (a.2 + b.2) * 0.5)
}

/// 字段 `rot: u8` 与 270° 的相处方式：u8 装不下 270，而契约又要求
/// {0,90,180,270}，这里只能两种写法都认 ——
///
/// - `0..=3`：当"90° 的倍数序号"（0=0°，1=90°，2=180°，3=270°）
/// - 其它：按度数就近取到 90 的倍数（90→90°，180→180°，194→180°，255→270°）
///
/// 两种读法在 0 上重合，而 1/2/3 不是 90 的倍数，所以不会撞车：
/// 整个 u8 空间都有确定含义，且 270° 可达（写成 3）。
fn turns(rot: u8) -> u8 {
    if rot <= 3 {
        rot
    } else {
        (((rot as u32 + 45) / 90 * 90) % 360 / 90) as u8
    }
}

/// 把序号写回字段：能写成度数就写度数，只有 270° 没有 u8 表示，写序号 3。
fn rot_field(t: u8) -> u8 {
    match t % 4 {
        1 => 90,
        2 => 180,
        3 => 3,
        _ => 0,
    }
}

/// 屏幕空间绕中心旋转 `t` 个 90°（y 向下，正角是视觉顺时针）。
/// 没有三角函数，纯精确置换，逆变换因此也是精确的。
fn rot_fwd(t: u8, x: f32, y: f32) -> (f32, f32) {
    match t % 4 {
        1 => (-y, x),
        2 => (-x, -y),
        3 => (y, -x),
        _ => (x, y),
    }
}

fn rot_inv(t: u8, x: f32, y: f32) -> (f32, f32) {
    match t % 4 {
        1 => (y, -x),
        2 => (-x, -y),
        3 => (-y, x),
        _ => (x, y),
    }
}

/// `t` 是相对"页面居中"的偏移，所以居中就是 `t = 0`；页面比屏幕大时
/// 允许的滑动量是 `±(box − screen)/2`（再多就露白）。
fn clamp_axis(t: f32, extent: f32, screen: f32) -> f32 {
    if extent <= screen {
        0.0
    } else {
        let m = (extent - screen) * 0.5;
        t.clamp(-m, m)
    }
}

fn fit(screen: (f32, f32), page: (f32, f32), by_height: bool) -> View {
    let (dw, dh) = crate::viewport::screen();
    let sw = pos_or(screen.0, dw);
    let sh = pos_or(screen.1, dh);
    let pw = pos_or(page.0, PAGE_W);
    let ph = pos_or(page.1, PAGE_H);
    // 这里**不**夹到 [MIN_SCALE, MAX_SCALE]：那是手势的缩放范围（QML 的
    // `updateDragAndZoom` 才夹），fit 的职责是让页面正好贴边，page 比屏幕大
    // 得多时 scale 就是要小于 0.5（QML `defaultFitScaleFactor` 同样不夹）。
    let raw = if by_height { sh / ph } else { sw / pw };
    let scale = if raw.is_finite() && raw > 0.0 {
        raw
    } else {
        1.0
    };
    // 贴合轴上页面宽（高）正好等于屏幕，而这个坐标系的原点本来就是页面中心，
    // 所以"贴边"和"另一轴居中"的平移量都是 0 —— 与 QML `defaultFitCenter`
    // 把装得下的那轴摆正中的做法一致（`res_0667e1f.qml:56-64`）。
    View {
        scale,
        rot: 0,
        tx: 0.0,
        ty: 0.0,
    }
}

fn pos_or(v: f32, fallback: f32) -> f32 {
    if v.is_finite() && v > 0.0 {
        v
    } else {
        fallback
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // 四种旋转。270° 在 u8 里写成序号 3（见 `turns()`）。
    const ROTS: [u8; 4] = [0, 90, 180, 3];

    fn close(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    fn pt_close(a: (f32, f32), b: (f32, f32), eps: f32) -> bool {
        close(a.0, b.0, eps) && close(a.1, b.1, eps)
    }

    #[test]
    fn api_shape_matches_the_brief() {
        // 签名一字不改：类型对不上这里直接编译不过
        let _: fn() -> View = View::identity;
        let _: fn(&View, (f32, f32)) -> (f32, f32) = View::page_to_screen;
        let _: fn(&View, (f32, f32)) -> (f32, f32) = View::screen_to_page;
        let _: fn(&mut View, f32, (f32, f32)) = View::zoom_at;
        let _: fn(&mut View, f32, f32) = View::pan;
        let _: fn(&mut View) = View::clamp_pan;
        let _: fn(&mut View, u8) = View::set_rot;
        let _: fn((f32, f32), (f32, f32)) -> View = View::fit_width;
        let _: fn((f32, f32), (f32, f32)) -> View = View::fit_height;
        let _: fn(&View) -> (f32, f32, f32, f32) = View::screen_rect_of_page;
        let _: fn() -> Tracker = Tracker::new;
        let _: fn(&mut Tracker, &[(i64, f32, f32)]) -> Gesture = Tracker::update;
        let _: fn(&mut Tracker) = Tracker::reset;
        let _: fn(&Tracker) -> usize = Tracker::fingers;
    }

    #[test]
    fn identity_maps_page_onto_screen() {
        let v = View::identity();
        // 页面与屏幕同尺寸且都居中，所以恒等变换就是恒等映射
        assert_eq!(v.page_to_screen((0.0, 0.0)), (0.0, 0.0));
        assert_eq!(v.page_to_screen((PAGE_W, PAGE_H)), (SCREEN_W, SCREEN_H));
        assert_eq!(v.page_to_screen((703.0, 936.0)), (703.0, 936.0));
    }

    #[test]
    fn roundtrip_over_every_rot_scale_and_pan() {
        let ps = [
            (0.0f32, 0.0f32),
            (1.0, 1.0),
            (PAGE_W, PAGE_H),
            (PAGE_W * 0.5, PAGE_H * 0.25),
            (1403.5, 1871.5),
        ];
        for &rot in &ROTS {
            for &scale in &[MIN_SCALE, 0.75, 1.0, 2.0, MAX_SCALE] {
                for &(tx, ty) in &[(0.0f32, 0.0f32), (137.0, -61.0), (-500.0, 900.0)] {
                    let v = View { scale, rot, tx, ty };
                    for &p in &ps {
                        let s = v.page_to_screen(p);
                        let back = v.screen_to_page(s);
                        assert!(
                            pt_close(back, p, 0.05),
                            "rot={rot} scale={scale} t=({tx},{ty}) p={p:?} -> {s:?} -> {back:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn zoom_at_keeps_the_anchor_fixed() {
        for &rot in &ROTS {
            for &factor in &[1.25f32, 2.0, 0.5, 0.9, 1.0] {
                for &anchor in &[(0.0f32, 0.0f32), (702.0, 936.0), (200.0, 1700.0)] {
                    let mut v = View {
                        scale: 1.0,
                        rot,
                        tx: 40.0,
                        ty: -30.0,
                    };
                    let under = v.screen_to_page(anchor);
                    v.zoom_at(factor, anchor);
                    let now = v.page_to_screen(under);
                    assert!(
                        pt_close(now, anchor, 0.5),
                        "rot={rot} f={factor} anchor={anchor:?} now={now:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn zoom_at_is_multiplicative_and_clamped() {
        let mut v = View::identity();
        v.zoom_at(2.0, (702.0, 936.0));
        assert!(close(v.scale, 2.0, 1e-6));
        v.zoom_at(0.5, (702.0, 936.0));
        assert!(close(v.scale, 1.0, 1e-6));

        // QML 的上下限：0.5 / 5
        v.zoom_at(1000.0, (0.0, 0.0));
        assert_eq!(v.scale, MAX_SCALE);
        v.zoom_at(1e-6, (0.0, 0.0));
        assert_eq!(v.scale, MIN_SCALE);

        // 坏倍数：不动、不炸、不写坏字段
        for bad in [f32::NAN, f32::INFINITY, -1.0, 0.0] {
            let before = v;
            v.zoom_at(bad, (100.0, 100.0));
            assert_eq!(v, before);
        }
    }

    #[test]
    fn fit_width_touches_both_side_edges() {
        let page = (1000.0f32, 2000.0f32);
        let v = View::fit_width((SCREEN_W, SCREEN_H), page);
        assert!(close(v.scale, SCREEN_W / page.0, 1e-6));
        assert_eq!((v.tx, v.ty), (0.0, 0.0));
        let (x, y, w, h) = v.screen_rect_of(page);
        assert!(close(x, 0.0, 0.02), "左边没贴边 x={x}");
        assert!(close(x + w, SCREEN_W, 0.02), "右边没贴边 right={}", x + w);
        // 页面比屏幕高 → 竖直居中溢出
        assert!(close(w, SCREEN_W, 0.02));
        assert!(close(h, page.1 * v.scale, 0.05));
        assert!(close(y + h * 0.5, SCREEN_H * 0.5, 0.05));
    }

    #[test]
    fn fit_height_touches_both_horizontal_edges() {
        let page = (2000.0f32, 1000.0f32);
        let v = View::fit_height((SCREEN_W, SCREEN_H), page);
        assert!(close(v.scale, SCREEN_H / page.1, 1e-6));
        assert_eq!((v.tx, v.ty), (0.0, 0.0));
        let (x, y, w, h) = v.screen_rect_of(page);
        assert!(close(y, 0.0, 0.02), "上边没贴边 y={y}");
        assert!(close(y + h, SCREEN_H, 0.02), "下边没贴边 bottom={}", y + h);
        assert!(close(w, page.0 * v.scale, 0.05));
        assert!(close(x + w * 0.5, SCREEN_W * 0.5, 0.05));
    }

    #[test]
    fn fit_can_go_below_the_gesture_limit() {
        // 3000×4000 的页在 1404×1872 屏上 fit 是 0.468 < MIN_SCALE：
        // fit 不夹手势范围，否则就不"贴边"了
        let page = (3000.0f32, 4000.0f32);
        let v = View::fit_width((SCREEN_W, SCREEN_H), page);
        assert!(v.scale < MIN_SCALE, "scale={}", v.scale);
        let (x, _, w, _) = v.screen_rect_of(page);
        assert!(close(x, 0.0, 0.02) && close(w, SCREEN_W, 0.02));
        // 手势缩放仍然被夹在 [0.5, 5]
        let mut z = v;
        z.zoom_at(1.0, (702.0, 936.0));
        assert_eq!(z.scale, MIN_SCALE);
    }

    #[test]
    fn fit_of_the_real_page_is_scale_one() {
        assert_eq!(
            View::fit_width((SCREEN_W, SCREEN_H), PAGE),
            View::identity()
        );
        assert_eq!(
            View::fit_height((SCREEN_W, SCREEN_H), PAGE),
            View::identity()
        );
    }

    #[test]
    fn clamp_pan_zoomed_in_has_no_blank_area() {
        // 拖到右上角外：只剩左边 / 下边贴边，一点空白都不许露
        let mut v = View {
            scale: 3.0,
            rot: 0,
            tx: 5000.0,
            ty: -5000.0,
        };
        v.clamp_pan();
        let (x, y, w, h) = v.screen_rect_of_page();
        assert!(
            close(x, 0.0, 1e-3) && close(y + h, SCREEN_H, 1e-3),
            "{x} {y} {w} {h}"
        );
        // 反向拖到底：换成右边 / 上边贴边
        v.pan(-1e6, 1e6);
        v.clamp_pan();
        let (x, y, w, h) = v.screen_rect_of_page();
        assert!(
            close(x + w, SCREEN_W, 1e-3) && close(y, 0.0, 1e-3),
            "{x} {y} {w} {h}"
        );
        // 中间任意位置都盖满屏幕（含 90° 旋转后的宽高对调）
        for &(tx, ty) in &[(-100.0f32, 50.0f32), (300.0, -900.0), (0.0, 0.0)] {
            let mut v = View {
                scale: 2.0,
                rot: 90,
                tx,
                ty,
            };
            v.clamp_pan();
            let (x, y, w, h) = v.screen_rect_of_page();
            assert!(
                x <= 1e-3 && y <= 1e-3 && x + w >= SCREEN_W - 1e-3 && y + h >= SCREEN_H - 1e-3,
                "({tx},{ty}) -> {x} {y} {w} {h}"
            );
        }
    }

    #[test]
    fn clamp_pan_zoomed_out_centers_the_page() {
        let mut v = View {
            scale: 0.5,
            rot: 0,
            tx: 300.0,
            ty: -200.0,
        };
        v.clamp_pan();
        // 页面装得下 → 固定居中，拖不动
        assert_eq!((v.tx, v.ty), (0.0, 0.0));
        let (x, y, w, h) = v.screen_rect_of_page();
        assert!(pt_close(
            (x + w * 0.5, y + h * 0.5),
            (SCREEN_W * 0.5, SCREEN_H * 0.5),
            1e-3
        ));
        // 90° 也一样
        let mut v = View {
            scale: 0.4,
            rot: 90,
            tx: -900.0,
            ty: 900.0,
        };
        v.clamp_pan();
        assert_eq!((v.tx, v.ty), (0.0, 0.0));
    }

    #[test]
    fn clamp_pan_leaves_the_fit_alone() {
        for &page in &[(1000.0f32, 2000.0f32), (2000.0, 1000.0), PAGE] {
            for v0 in [
                View::fit_width((SCREEN_W, SCREEN_H), page),
                View::fit_height((SCREEN_W, SCREEN_H), page),
            ] {
                let mut v = v0;
                v.clamp_pan_of(page);
                assert!(
                    close(v.tx, v0.tx, 1e-3) && close(v.ty, v0.ty, 1e-3),
                    "fit 被 clamp 改了：{v0:?} -> {v:?}"
                );
            }
        }
    }

    #[test]
    fn set_rot_four_times_returns_home() {
        let start = View {
            scale: 1.7,
            rot: 0,
            tx: 123.0,
            ty: -45.0,
        };
        let mut v = start;
        // 0° → 90° → 180° → 270° → 0°。270 写成序号 3，理由见 `turns()`。
        for (deg, want) in [(90u8, 90u8), (180, 180), (3, 3), (0, 0)] {
            v.set_rot(deg);
            assert_eq!(v.rot, want, "set_rot({deg})");
            assert_eq!(v.scale, start.scale);
        }
        // 90° 置换是精确的：回来必须分毫不差
        assert_eq!(v, start);
        assert_eq!((v.tx, v.ty), (start.tx, start.ty));
    }

    #[test]
    fn set_rot_swaps_the_screen_box_and_normalizes_degrees() {
        let mut v = View::identity();
        let (_, _, w0, h0) = v.screen_rect_of_page();
        v.set_rot(90);
        let (x, y, w, h) = v.screen_rect_of_page();
        assert!(close(w, h0, 1e-3) && close(h, w0, 1e-3), "90° 宽高没对调");
        // t = 0 时页面中心不动
        assert!(pt_close(
            (x + w * 0.5, y + h * 0.5),
            (SCREEN_W * 0.5, SCREEN_H * 0.5),
            1e-3
        ));

        // 就近取整 + u8 编码：270° 只能落在序号 3 上
        for (deg, want) in [
            (0u8, 0u8),
            (44, 0),
            (45, 90),
            (90, 90),
            (180, 180),
            (194, 180),
            (255, 3),
            (2, 180),
        ] {
            let mut v = View::identity();
            v.set_rot(deg);
            assert_eq!(v.rot, want, "set_rot({deg})");
        }
        // 3 = 270°：宽高同样对调，但和 90° 差一个 180°
        let mut a = View::identity();
        a.set_rot(90);
        let mut b = View::identity();
        b.set_rot(3);
        let pa = a.page_to_screen((0.0, 0.0));
        let pb = b.page_to_screen((0.0, 0.0));
        assert!(
            pt_close(pb, (SCREEN_W - pa.0, SCREEN_H - pa.1), 1e-3),
            "270° 应该是 90° 再转 180°：{pa:?} vs {pb:?}"
        );
    }

    #[test]
    fn set_rot_rotates_content_about_the_screen_center() {
        let mut v = View {
            scale: 1.3,
            rot: 0,
            tx: 0.0,
            ty: 120.0,
        };
        let p = (100.0f32, 200.0f32);
        let q = v.page_to_screen(p);
        v.set_rot(90);
        let q2 = v.page_to_screen(p);
        // 屏幕点绕中心转 90°：(x,y) → 中心 + (−(y−cy), (x−cx))
        let expect = (
            SCREEN_W * 0.5 - (q.1 - SCREEN_H * 0.5),
            SCREEN_H * 0.5 + (q.0 - SCREEN_W * 0.5),
        );
        assert!(
            pt_close(q2, expect, 0.02),
            "{q:?} -> {q2:?} want {expect:?}"
        );
    }

    #[test]
    fn tracker_single_finger_drag() {
        let mut t = Tracker::new();
        assert_eq!(t.update(&[(7, 100.0, 200.0)]), Gesture::None);
        assert_eq!(t.fingers(), 1);
        assert_eq!(
            t.update(&[(7, 130.0, 180.0)]),
            Gesture::Drag {
                dx: 30.0,
                dy: -20.0
            }
        );
        // 不动也得给 0 位移（主程序靠它推进状态机）
        assert_eq!(
            t.update(&[(7, 130.0, 180.0)]),
            Gesture::Drag { dx: 0.0, dy: 0.0 }
        );
        assert_eq!(t.fingers(), 1);
    }

    #[test]
    fn tracker_two_finger_pinch_factor() {
        let mut t = Tracker::new();
        assert_eq!(
            t.update(&[(1, 400.0, 400.0), (2, 800.0, 400.0)]),
            Gesture::None
        );
        match t.update(&[(1, 300.0, 400.0), (2, 900.0, 400.0)]) {
            Gesture::Pinch {
                factor,
                center,
                dcenter,
            } => {
                // 距离 400 → 600
                assert!(close(factor, 1.5, 1e-6), "factor={factor}");
                assert!(pt_close(center, (600.0, 400.0), 1e-4));
                assert!(pt_close(dcenter, (0.0, 0.0), 1e-4));
            }
            g => panic!("{g:?}"),
        }
        // 两指往里收 → 缩小
        match t.update(&[(1, 450.0, 400.0), (2, 750.0, 400.0)]) {
            Gesture::Pinch { factor, .. } => assert!(close(factor, 300.0 / 600.0, 1e-6)),
            g => panic!("{g:?}"),
        }
    }

    #[test]
    fn tracker_two_finger_pan() {
        let mut t = Tracker::new();
        assert_eq!(
            t.update(&[(1, 400.0, 400.0), (2, 800.0, 400.0)]),
            Gesture::None
        );
        match t.update(&[(1, 420.0, 450.0), (2, 820.0, 450.0)]) {
            Gesture::Pinch {
                factor,
                center,
                dcenter,
            } => {
                // 距离不变 → 只平移
                assert!(close(factor, 1.0, 1e-6));
                assert!(pt_close(center, (620.0, 450.0), 1e-4));
                assert!(pt_close(dcenter, (20.0, 50.0), 1e-4));
            }
            g => panic!("{g:?}"),
        }
    }

    #[test]
    fn tracker_finger_count_change_never_jumps() {
        let mut t = Tracker::new();
        assert_eq!(t.update(&[(1, 100.0, 100.0)]), Gesture::None);
        assert_eq!(
            t.update(&[(1, 110.0, 130.0)]),
            Gesture::Drag { dx: 10.0, dy: 30.0 }
        );
        // 第二根手指落下：这一帧必须 None，不能拿单指位移或凭空缩放去跳一下
        assert_eq!(
            t.update(&[(1, 110.0, 130.0), (2, 900.0, 900.0)]),
            Gesture::None
        );
        assert_eq!(t.fingers(), 2);
        match t.update(&[(1, 110.0, 130.0), (2, 900.0, 920.0)]) {
            Gesture::Pinch {
                factor,
                center,
                dcenter,
            } => {
                // 距离 1103.18（dx=790,dy=770）→ 1117.23（dx=790,dy=790）
                let d0 = (790.0f32 * 790.0 + 770.0 * 770.0).sqrt();
                let d1 = (790.0f32 * 790.0 + 790.0 * 790.0).sqrt();
                assert!(close(factor, d1 / d0, 1e-6), "factor={factor}");
                assert!(pt_close(center, (505.0, 525.0), 1e-4));
                assert!(pt_close(dcenter, (0.0, 10.0), 1e-4));
            }
            g => panic!("{g:?}"),
        }
        // 抬走一根：同样 None，且不残留双指状态
        assert_eq!(t.update(&[(1, 120.0, 120.0)]), Gesture::None);
        assert_eq!(t.fingers(), 1);
        assert_eq!(
            t.update(&[(1, 125.0, 118.0)]),
            Gesture::Drag { dx: 5.0, dy: -2.0 }
        );
    }

    #[test]
    fn tracker_coincident_fingers_never_explode() {
        let mut t = Tracker::new();
        // 两指重合着落下
        assert_eq!(
            t.update(&[(1, 500.0, 500.0), (2, 500.0, 500.0)]),
            Gesture::None
        );
        for f in [
            // 距离一直是 0
            [(1, 500.0, 500.0), (2, 500.0, 500.0)],
            // 从 0 突然分开（上一帧距离 0 → 倍数无定义）
            [(1, 300.0, 500.0), (2, 700.0, 500.0)],
            // 分开后瞬间又重合（本帧距离 0 → 倍数 0，要夹住）
            [(1, 500.0, 500.0), (2, 500.0, 500.0)],
            // 亚像素距离，dc/dp 也不许溢出
            [(1, 500.0, 500.0), (2, 500.0 + 1e-6, 500.0)],
        ] {
            match t.update(&f) {
                Gesture::Pinch {
                    factor,
                    center,
                    dcenter,
                } => {
                    assert!(factor.is_finite() && factor > 0.0, "factor={factor}");
                    assert!((1.0 / MAX_FRAME_RATIO..=MAX_FRAME_RATIO).contains(&factor));
                    assert!(center.0.is_finite() && center.1.is_finite());
                    assert!(dcenter.0.is_finite() && dcenter.1.is_finite());
                }
                g => panic!("{g:?}"),
            }
        }
    }

    #[test]
    fn tracker_reset_is_clean() {
        let mut t = Tracker::new();
        t.update(&[(1, 10.0, 10.0), (2, 90.0, 90.0)]);
        t.reset();
        assert_eq!(t.fingers(), 0);
        // reset 后第一帧只取基准
        assert_eq!(t.update(&[(1, 500.0, 500.0)]), Gesture::None);
        assert_eq!(t.fingers(), 1);
        assert_eq!(
            t.update(&[(1, 501.0, 502.0)]),
            Gesture::Drag { dx: 1.0, dy: 2.0 }
        );
        // 空帧一直 None
        t.reset();
        for _ in 0..3 {
            assert_eq!(t.update(&[]), Gesture::None);
            assert_eq!(t.fingers(), 0);
        }
    }

    #[test]
    fn tracker_handles_bad_input() {
        let mut t = Tracker::new();
        assert_eq!(t.update(&[(1, f32::NAN, 0.0)]), Gesture::None);
        assert_eq!(t.fingers(), 0);
        assert_eq!(t.update(&[(1, 0.0, f32::INFINITY)]), Gesture::None);
        // 坏手指被丢掉，好的照常
        assert_eq!(
            t.update(&[(1, 10.0, 10.0), (2, f32::NAN, 10.0)]),
            Gesture::None
        );
        assert_eq!(t.fingers(), 1);
        assert_eq!(
            t.update(&[(1, 15.0, 10.0), (2, f32::NAN, 10.0)]),
            Gesture::Drag { dx: 5.0, dy: 0.0 }
        );
        // 极端量级：平方溢出成 inf，也不许 panic / 给出非有限倍数
        let mut t = Tracker::new();
        t.update(&[(1, 3.0e38, 0.0), (2, -3.0e38, 0.0)]);
        match t.update(&[(1, 3.0e38, 0.0), (2, -3.0e38, 1.0e38)]) {
            Gesture::Pinch { factor, .. } => assert!(factor.is_finite() && factor > 0.0),
            g => panic!("{g:?}"),
        }
        // 超过两根只认前两根
        let mut t = Tracker::new();
        t.update(&[(1, 0.0, 0.0), (2, 100.0, 0.0), (3, 200.0, 0.0)]);
        assert_eq!(t.fingers(), 2);
        assert!(matches!(
            t.update(&[(1, 0.0, 0.0), (2, 200.0, 0.0), (3, 300.0, 0.0)]),
            Gesture::Pinch { .. }
        ));
    }

    #[test]
    fn view_survives_garbage_fields() {
        let mut v = View {
            scale: f32::NAN,
            rot: 200,
            tx: f32::INFINITY,
            ty: f32::NEG_INFINITY,
        };
        let s = v.page_to_screen((10.0, 10.0));
        assert!(s.0.is_finite() && s.1.is_finite());
        assert!(pt_close(v.screen_to_page(s), (10.0, 10.0), 0.05));
        v.zoom_at(2.0, (700.0, 900.0));
        v.pan(10.0, 10.0);
        v.clamp_pan();
        v.set_rot(90);
        let r = v.screen_rect_of_page();
        assert!([r.0, r.1, r.2, r.3].iter().all(|x| x.is_finite()), "{r:?}");
        // 页面 / 屏幕尺寸给 0 或坏值也不许除出 Inf
        let f = View::fit_width((0.0, 0.0), (0.0, 0.0));
        assert!(f.scale.is_finite() && f.scale > 0.0);
        // 页面大到乘出 inf：clamp 的边界也是 inf，不能 panic
        let mut big = View {
            scale: 5.0,
            rot: 0,
            tx: 1e30,
            ty: 1e30,
        };
        big.clamp_pan_of((3.0e38, 3.0e38));
        big.set_rot(1);
    }

    #[test]
    fn pinch_composes_to_keep_the_old_midpoint_under_the_new_one() {
        let mut t = Tracker::new();
        let mut v = View {
            scale: 1.5,
            rot: 0,
            tx: -20.0,
            ty: 30.0,
        };
        assert_eq!(
            t.update(&[(1, 400.0, 800.0), (2, 800.0, 800.0)]),
            Gesture::None
        );
        let old_mid = (600.0f32, 800.0f32);
        let under = v.screen_to_page(old_mid);
        match t.update(&[(1, 350.0, 760.0), (2, 950.0, 840.0)]) {
            Gesture::Pinch {
                factor,
                center,
                dcenter,
                ..
            } => {
                v.zoom_at(factor, old_mid);
                v.pan(dcenter.0, dcenter.1);
                // 文档里承诺的组合顺序：旧中点底下的页面点落到新中点
                assert!(pt_close(v.page_to_screen(under), center, 0.5));
            }
            g => panic!("{g:?}"),
        }
    }
}
