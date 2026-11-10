//! 纯解码：一帧 evdev 事件 → 一个 `InputPoint`。
//!
//! 逐条对应 `M1-pen-input.md` §4.1（`0x176ac4`，全部常量有地址证据）。
//! 这段是"手感的地基"，一个符号错了整支笔的方向就翻了，所以单独成文件、单独单测。

use super::evdev::*;
use crate::pen::InputPoint;

/// 各轴量程，启动时从 `EVIOCGABS` 读，不硬编码。
#[derive(Clone, Copy, Debug)]
pub struct Axes {
    pub x_min: f32,
    pub x_max: f32,
    pub y_min: f32,
    pub y_max: f32,
    pub p_max: f32,
    pub tilt_x_max: f32,
    pub tilt_y_max: f32,
}

impl Default for Axes {
    /// rM2 的实测值，仅作兜底；正常路径一定走 `from_fd`。
    fn default() -> Self {
        Axes {
            x_min: 0.0,
            x_max: 20_966.0,
            y_min: 0.0,
            y_max: 15_725.0,
            p_max: 2047.0,
            tilt_x_max: 64.0,
            tilt_y_max: 64.0,
        }
    }
}

impl Axes {
    pub fn from_fd(fd: std::os::unix::io::RawFd) -> std::io::Result<Axes> {
        let x = absinfo(fd, ABS_X)?;
        let y = absinfo(fd, ABS_Y)?;
        let p = absinfo(fd, ABS_PRESSURE)?;
        let tx = absinfo(fd, ABS_TILT_X)?;
        let ty = absinfo(fd, ABS_TILT_Y)?;
        Ok(Axes {
            x_min: x.minimum as f32,
            x_max: x.maximum as f32,
            y_min: y.minimum as f32,
            y_max: y.maximum as f32,
            p_max: p.maximum as f32,
            tilt_x_max: tx.maximum.max(1) as f32,
            tilt_y_max: ty.maximum.max(1) as f32,
        })
    }
}

/// 一帧里累积的原始值，`SYN_REPORT` 时结算。
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    raw_x: i32,
    raw_y: i32,
    pressure: i32,
    tilt_x: i32,
    tilt_y: i32,
    pub in_range: bool,
    pub contact: bool,
    pub rubber: bool,
    pub dropped: bool,
    pub dirty: bool,
}

impl Default for Frame {
    fn default() -> Self {
        Frame {
            raw_x: 0,
            raw_y: 0,
            pressure: 0,
            tilt_x: 0,
            tilt_y: 0,
            in_range: false,
            contact: false,
            rubber: false,
            dropped: false,
            dirty: false,
        }
    }
}

impl Frame {
    /// 喂一个事件。返回 `true` 表示这是一帧的结束（`SYN_REPORT`）。
    pub fn feed(&mut self, typ: u16, code: u16, value: i32) -> bool {
        match (typ, code) {
            (EV_SYN, SYN_REPORT) => return true,
            (EV_SYN, SYN_DROPPED) => {
                // 官方：把 x/y 置 -1、p/t 清零作废本帧，并告警 "Dropped pen event!"
                self.raw_x = -1;
                self.raw_y = -1;
                self.pressure = 0;
                self.tilt_x = 0;
                self.tilt_y = 0;
                self.dropped = true;
                self.dirty = true;
            }
            (EV_ABS, ABS_X) => {
                self.raw_x = value;
                self.dirty = true;
            }
            (EV_ABS, ABS_Y) => {
                self.raw_y = value;
                self.dirty = true;
            }
            (EV_ABS, ABS_PRESSURE) => {
                self.pressure = value;
                self.dirty = true;
            }
            (EV_ABS, ABS_TILT_X) => {
                self.tilt_x = value;
                self.dirty = true;
            }
            (EV_ABS, ABS_TILT_Y) => {
                self.tilt_y = value;
                self.dirty = true;
            }
            (EV_ABS, ABS_DISTANCE) => { /* 官方存到 Digitizer+0x168，不进 InputPoint */ }
            (EV_KEY, BTN_TOUCH) => self.contact = value != 0,
            (EV_KEY, BTN_TOOL_PEN) => {
                self.in_range = value != 0;
                if value != 0 {
                    self.rubber = false;
                }
            }
            (EV_KEY, BTN_TOOL_RUBBER) => {
                self.in_range = value != 0;
                if value != 0 {
                    self.rubber = true;
                }
            }
            _ => {}
        }
        false
    }

    /// 结算成归一化坐标。
    ///
    /// 注意两个轴是**交叉**的：设备 X → 屏幕 Y 且取反，设备 Y → 屏幕 X。
    /// `+1` 的分母把 `max` 本身也映射进 `[0,1)`，与官方一致。
    pub fn point(&self, ax: &Axes, w: f32, h: f32) -> InputPoint {
        let (mut x, mut y) = (
            (self.raw_y as f32 - ax.y_min) / ((ax.y_max - ax.y_min) + 1.0) * w,
            (((ax.x_max - self.raw_x as f32) - ax.x_min) / ((ax.x_max - ax.x_min) + 1.0)) * h,
        );
        // 横屏：面板物理方向没变，变的是"逻辑画面转了 90°"（见
        // `fb::set_landscape`），所以这里要把物理坐标逆着转回去，下游
        // （命中、落墨、手势）才全都活在逻辑坐标系里。
        //
        // 逻辑 `(x, y)` 上屏成物理 `(W-1-y, x)`；反过来就是
        // `x_l = y_p`、`y_l = W - x_p`（`W` 是物理屏宽 1404）。
        (x, y) = crate::fb::to_logical(x, y);
        let pressure = if ax.p_max > 0.0 {
            (self.pressure as f32 / ax.p_max).clamp(0.0, 1.0)
        } else {
            0.0
        };
        // 官方把两根轴的正切值除以量程再乘 π/2：倾角直接落在 ±π/2 弧度。
        let half_pi = std::f32::consts::FRAC_PI_2;
        let tilt_x = -((self.tilt_x as f32) / ax.tilt_x_max) * half_pi;
        let tilt_y = ((self.tilt_y as f32) / ax.tilt_y_max) * half_pi;
        InputPoint {
            x,
            y,
            pressure,
            tilt_x,
            tilt_y,
            rubber: self.rubber,
        }
    }

    pub fn clear_dirty(&mut self) {
        self.dirty = false;
        self.dropped = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ax() -> Axes {
        Axes::default()
    }

    #[test]
    fn axes_are_crossed_and_x_is_mirrored() {
        let mut f = Frame::default();
        // 设备原点 (0,0) → 屏幕左下角
        f.raw_x = 0;
        f.raw_y = 0;
        let p = f.point(&ax(), 1404.0, 1872.0);
        assert!(p.x < 1.0, "{}", p.x);
        assert!(p.y > 1871.0, "{}", p.y);

        // 设备 (max_x, max_y) → 屏幕右上角
        f.raw_x = 20966;
        f.raw_y = 15725;
        let p = f.point(&ax(), 1404.0, 1872.0);
        assert!(p.x > 1403.0, "{}", p.x);
        assert!(p.y < 1.0, "{}", p.y);
    }

    #[test]
    fn pressure_normalizes_to_unit() {
        let mut f = Frame::default();
        f.pressure = 2047;
        assert!((f.point(&ax(), 1404.0, 1872.0).pressure - 1.0).abs() < 1e-6);
        f.pressure = 0;
        assert_eq!(f.point(&ax(), 1404.0, 1872.0).pressure, 0.0);
    }

    #[test]
    fn contact_uses_btn_touch_not_pressure() {
        let mut f = Frame::default();
        f.feed(EV_ABS, ABS_PRESSURE, 2000);
        assert!(!f.contact, "压力不能当落笔判定");
        f.feed(EV_KEY, BTN_TOUCH, 1);
        assert!(f.contact);
    }

    #[test]
    fn syn_report_terminates_frame() {
        let mut f = Frame::default();
        assert!(!f.feed(EV_ABS, ABS_Y, 100));
        assert!(f.feed(EV_SYN, SYN_REPORT, 0));
    }

    #[test]
    fn dropped_frame_is_flagged() {
        let mut f = Frame::default();
        f.feed(EV_SYN, SYN_DROPPED, 0);
        assert!(f.dropped);
        assert_eq!(f.raw_x, -1);
    }
}
