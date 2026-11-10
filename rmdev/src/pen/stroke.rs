//! 笔迹累积器：采样流 → `Point` 流。
//!
//! 对应官方 `PenInputLineHandler::handlePenPosition`（`0x16fbe4`）与 `handlePenMove`
//! （`0x17011c`）加两个巨型 `addPoint`（`0x5673d4` / `0x566d20`）。
//!
//! 官方的三条硬规则，照抄不改：
//!   1. **落笔点本身不进 Line**——它只是第一个点的 `prev`，所以"点一下"得到 0 个点；
//!   2. `speed` 是**位移 × 缩放**，没有 dt 参与（`dt` 传进来了但没用）；
//!   3. `pressure` 的口径分两条路径：通用路径是「相邻两点均值 × 位移衰减」（只对 6 个工具），
//!      v2 路径（`0x566d20`）只读**当前点原始压力**。见 `width.rs` 顶部那张表。

use super::line::{Color, InputPoint, Line, Point, Tool};
use super::width::{self, Ctx, SPEED_DAMP_DIV, SPEED_DAMP_FLOOR};

/// 一笔的构建器。生命周期 = 一次 pen-down .. pen-up。
#[derive(Clone, Debug)]
pub struct StrokeBuilder {
    tool: Tool,
    color: Color,
    thickness: f32,
    /// 渲染器的场景缩放（官方 `painter + 0x98`）
    scale: f32,
    rotation: f32,
    /// 铅笔硬度 → 粗细变化系数（1 = 官方原样，越小越"硬"）。见 `pencil::Grade::var`。
    hard_var: f32,

    line: Line,
    prev: Option<InputPoint>,
    prevprev: Option<InputPoint>,
    /// 落笔时的原始位置，只为让第一段不出现空隙（不写进 Line）
    down: Option<InputPoint>,
}

impl StrokeBuilder {
    pub fn new(tool: Tool, color: Color, thickness: f32, scale: f32) -> StrokeBuilder {
        StrokeBuilder {
            tool,
            color,
            thickness,
            scale,
            rotation: 0.0,
            hard_var: 1.0,
            line: Line::new(tool, color, thickness as f64),
            prev: None,
            prevprev: None,
            down: None,
        }
    }

    /// 硬度档位对粗细的影响（1 = 官方原样，见 `pencil::Grade::var`）。
    pub fn set_hard_var(&mut self, v: f32) {
        self.hard_var = v;
    }

    /// 落笔。只记录基准点，**不产生 Point**。
    pub fn begin(&mut self, ip: InputPoint) {
        self.line = Line::new(self.tool, self.color, self.thickness as f64);
        self.prev = Some(ip);
        self.prevprev = None;
        self.down = Some(ip);
    }

    /// 落笔后的每个采样（= 每个 SYN_REPORT）产生一个 Point。
    pub fn add(&mut self, ip: InputPoint) -> Point {
        let prev = self.prev.unwrap_or(ip);

        // §4.3 速度：两段长度求平均 × 缩放。没有 dt。
        // 第一个 move 只有一段，直接用它，不要拿 0 去凑平均（会把首点速度砍半）。
        let d1 = ip.dist(&prev);
        let d = match self.prevprev {
            Some(pp) => 0.5 * (d1 + prev.dist(&pp)),
            None => d1,
        };
        let speed = d * self.scale;

        // 压力口径分两条路径（`width.rs` 顶部有对照表）：
        //   通用路径：min(相邻两点均值, 1)，吃位移衰减的工具再乘 k；
        //   v2 路径：当前点原始压力（官方 `0x566da4` 直接读 `[in+8]`，不做均值也不衰减）。
        let v2_path = self.tool.is_v2();
        let mut p = if v2_path {
            ip.pressure
        } else {
            (0.5 * (ip.pressure + prev.pressure)).min(1.0)
        };

        // §4.4(b) 位移衰减：画得越快越细/越淡，>50 单位直接打五折
        if !v2_path && self.tool.has_speed_damping() {
            let k = 1.0 - d / SPEED_DAMP_DIV;
            if k < 1.0 {
                p *= if k < SPEED_DAMP_FLOOR {
                    SPEED_DAMP_FLOOR
                } else {
                    k
                };
            }
        }

        // §4.2 方向：两段平均。官方在 0/2π 处不 unwrap，会在 +x 轴附近跳变；
        // 这里用向量平均消掉那个毛刺（dir 只是渲染提示，不影响 .rm 的可读性）。
        let dir = mean_direction(&ip, &prev, self.prevprev.as_ref());
        // 当前段的瞬时方向：通用路径的垂距项（`0x565250`）用的是这一段，不是平均方向。
        let seg_dir = norm_tau((ip.y - prev.y).atan2(ip.x - prev.x));

        let tilt = width::tilt_magnitude(ip.tilt_x, ip.tilt_y);
        let last = self.line.points.last().copied().unwrap_or_default();
        let ctx = Ctx {
            p,
            tilt,
            tilt_x: ip.tilt_x,
            tilt_y: ip.tilt_y,
            dir,
            seg_dir,
            rotation: self.rotation,
            prev_pressure: last.pressure,
            prev_width: last.width,
            has_prev: !self.line.points.is_empty(),
        };
        let mut w = width::width(self.tool, &ctx, self.thickness);
        // 硬度档位：把宽度波动按比例收窄到"零压基准宽"附近。
        // 硬铅（2H）几乎不变粗，软铅（6B）保留官方的全部起伏。
        if self.hard_var != 1.0 && self.tool.is_pencil() {
            let nom = width::pencil_nominal(self.tool, self.thickness);
            w.w = nom + (w.w - nom) * self.hard_var;
        }

        let point = Point {
            x: ip.x,
            y: ip.y,
            speed,
            dir,
            width: w.w,
            pressure: w.pressure,
        };
        debug_assert!(point.is_valid(), "点超出官方合法域: {point:?}");

        self.line.push(point);
        self.prevprev = Some(prev);
        self.prev = Some(ip);
        point
    }

    pub fn line(&self) -> &Line {
        &self.line
    }

    /// 收笔成线。
    ///
    /// **落笔点要补进去**：`begin()` 只记住了位置，`add()` 才 push 点，所以原来
    /// 存下来的笔迹是从"第一个 move"开始的 —— 落笔到第一个采样之间的那一小段
    /// 只存在于画布上。后果有两个：翻页重绘之后笔画头部会短一截；撤销时按模型
    /// 包围盒清屏，那一小段清不掉（屏幕上留下一个圆头残影）。
    ///
    /// 这里补一个和首点同宽、同压力的点，等于官方"落笔处一个圆头"的效果。
    pub fn into_line(mut self) -> Line {
        if let Some(down) = self.down {
            let first = self.line.points.first().copied();
            if let Some(f) = first {
                // 已经有首点且和落笔点几乎重合就不用补
                let dx = down.x - f.x;
                let dy = down.y - f.y;
                if dx * dx + dy * dy > 1.0 {
                    let p0 = Point {
                        x: down.x,
                        y: down.y,
                        speed: f.speed,
                        dir: f.dir,
                        width: f.width,
                        pressure: f.pressure,
                    };
                    self.line.points.insert(0, p0);
                    self.line.bounds.include(down.x - f.width * 0.5, down.y - f.width * 0.5);
                    self.line.bounds.include(down.x + f.width * 0.5, down.y + f.width * 0.5);
                }
            }
        }
        self.line
    }

    /// 落笔位置——只用于给第一段补一个起始圆头，不进 Line。
    pub fn down(&self) -> Option<InputPoint> {
        self.down
    }

    pub fn is_empty(&self) -> bool {
        self.line.points.is_empty()
    }

    pub fn tool(&self) -> Tool {
        self.tool
    }

    pub fn color(&self) -> Color {
        self.color
    }
}

/// 两段方向的平均。
///
/// 官方写法（`0x5673d4`）是各自折进 [0,2π) 后取算术平均，跨 +x 轴时会给出 180° 而不是 0°。
/// 这里改成「单位向量求和再取角」，结果在 (-π, π]，最后折到 [0, 2π)。
fn mean_direction(cur: &InputPoint, prev: &InputPoint, prevprev: Option<&InputPoint>) -> f32 {
    let a = (cur.y - prev.y).atan2(cur.x - prev.x);
    let b = match prevprev {
        Some(pp) => (prev.y - pp.y).atan2(prev.x - pp.x),
        None => return norm_tau(a),
    };
    let (sa, ca) = (a.sin(), a.cos());
    let (sb, cb) = (b.sin(), b.cos());
    norm_tau((sa + sb).atan2(ca + cb))
}

fn norm_tau(mut a: f32) -> f32 {
    if a < 0.0 {
        a += std::f32::consts::TAU;
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(x: f32, y: f32, p: f32) -> InputPoint {
        InputPoint {
            x,
            y,
            pressure: p,
            tilt_x: 0.0,
            tilt_y: 0.0,
            rubber: false,
        }
    }

    #[test]
    fn down_point_is_not_stored() {
        let mut b = StrokeBuilder::new(Tool::Fineliner, Color::Black, 2.0, 1.0);
        b.begin(ip(100.0, 100.0, 0.5));
        assert!(b.is_empty());
        b.add(ip(110.0, 100.0, 0.5));
        assert_eq!(b.line().points.len(), 1);
    }

    #[test]
    fn speed_is_displacement_not_time() {
        let mut b = StrokeBuilder::new(Tool::Fineliner, Color::Black, 2.0, 2.0);
        b.begin(ip(0.0, 0.0, 0.5));
        // 第一点没有 prevprev，用 prev 顶替 → d = 10
        let p = b.add(ip(10.0, 0.0, 0.5));
        assert!((p.speed - 20.0).abs() < 1e-4, "{}", p.speed);
        // 第二点两段都是 10 → 还是 10
        let p = b.add(ip(20.0, 0.0, 0.5));
        assert!((p.speed - 20.0).abs() < 1e-4, "{}", p.speed);
    }

    #[test]
    fn pressure_is_averaged_with_previous() {
        let mut b = StrokeBuilder::new(Tool::Fineliner, Color::Black, 2.0, 1.0);
        b.begin(ip(0.0, 0.0, 0.0));
        let p = b.add(ip(1.0, 0.0, 1.0));
        assert!((p.pressure - 0.5).abs() < 1e-6, "{}", p.pressure);
    }

    #[test]
    fn direction_follows_motion() {
        let mut b = StrokeBuilder::new(Tool::Fineliner, Color::Black, 2.0, 1.0);
        b.begin(ip(0.0, 0.0, 0.5));
        let p = b.add(ip(10.0, 0.0, 0.5));
        assert!(p.dir.abs() < 1e-3, "{}", p.dir);
        // 拐角处官方是"两段平均"，所以 0° 与 90° 之间得到 45°——这是设计如此，
        // 它让 dir 在小抖动下平滑，代价是直角拐点被压成对角。
        let p = b.add(ip(10.0, 10.0, 0.5));
        assert!((p.dir - std::f32::consts::FRAC_PI_4).abs() < 1e-3, "{}", p.dir);
    }

    #[test]
    fn fineliner_width_is_flat_along_the_stroke() {
        let mut b = StrokeBuilder::new(Tool::Fineliner, Color::Black, 3.0, 1.0);
        b.begin(ip(0.0, 0.0, 0.1));
        for i in 1..50 {
            let p = b.add(ip(i as f32 * 3.0, 0.0, i as f32 / 50.0));
            assert_eq!(p.width, 6.0);
        }
    }

    #[test]
    fn v2_path_uses_raw_pressure_not_the_average() {
        // 官方 v2 路径（0x566d20）只读当前点压力；通用路径才做「相邻两点均值」。
        // BallpointV2 的 pv = smoothstep((p - 0.15) / 0.85)，落在压力上能直接看出来。
        let mut b = StrokeBuilder::new(Tool::BallpointV2, Color::Black, 2.0, 1.0);
        b.begin(ip(0.0, 0.0, 0.0));
        let p = b.add(ip(1.0, 0.0, 1.0));
        assert!((p.pressure - 1.0).abs() < 1e-6, "{}", p.pressure);

        // 同样的输入走通用路径（Paintbrush）时压力是均值 0.5，再乘位移衰减
        // k = 1 - 1/100 = 0.99
        let mut b = StrokeBuilder::new(Tool::PaintbrushV2, Color::Black, 2.0, 1.0);
        b.begin(ip(0.0, 0.0, 0.0));
        let p = b.add(ip(1.0, 0.0, 1.0));
        assert!((p.pressure - 0.5 * 0.99).abs() < 1e-6, "{}", p.pressure);
    }

    #[test]
    fn highlighter_stays_30_whatever_the_pressure_and_speed() {
        // 官方 0x567808 写死 30 × 厚度 1.0；这里厚度给 3.0 也不该变。
        let mut b = StrokeBuilder::new(Tool::HighlighterV2, Color::Black, 3.0, 2.0);
        b.begin(ip(0.0, 0.0, 0.0));
        for i in 1..40 {
            let far = if i % 2 == 0 { 60.0 } else { 3.0 };
            let p = b.add(ip(i as f32 * far, 0.0, (i as f32) / 40.0));
            assert_eq!(p.width, 30.0);
        }
    }
}
