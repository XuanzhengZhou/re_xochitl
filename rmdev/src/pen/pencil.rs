//! 铅笔硬度档位（2H…6B）——官方没有这个功能，是一支固定灰的 Pencil。
//!
//! 官方只有"一支铅笔"，浓淡写死；真实铅笔靠**石墨/黏土配比**分硬度，越软石墨越多：
//! 同样力道下越黑、越吃压力、笔画粗细也越随倾角起伏；越硬则相反。
//! 这里把它做成三件事（都是抓笔时算的，只影响观感，不改 `.rm` 的写法）：
//!
//! | 档位 | 满压浓度 | 压力指数 γ | 粗细变化 |
//! |---|---|---|---|
//! | 2H / H / HB | 浅 | 大（要用力才变深）| 小（几乎不变粗）|
//! | B / 2B | 中 | 中 | 中 |
//! | 4B / 6B | 深 | 小（轻轻一碰就黑）| 大 |
//!
//! 浓度不是"灰色值"，而是**网点覆盖率**（0..255）：面板只认二值波形，
//! 中间灰得靠黑点阵凑，见 `canvas.rs` 的 `TEX`。

/// 一个硬度档位。
#[derive(Clone, Copy, Debug)]
pub struct Grade {
    /// 面板上显示的名字
    pub name: &'static str,
    /// 满压时的网点覆盖率（0..255）。255 = 全黑。
    pub tone: u8,
    /// 压力曲线指数：`tone · (0.45 + 0.55·p^γ)`。越硬 γ 越大。
    pub gamma: f32,
    /// 粗细变化系数：1 = 官方原样，<1 表示把波动往基准宽度收（硬铅更"稳"）。
    pub var: f32,
}

/// 七档，覆盖从"写字很浅"到"速写很黑"。
///
/// 数值取法：HB 在常用力道（p≈0.6）下落在 0.4 左右的覆盖率——正好是官方
/// `Gray`（110/255 ≈ 0.43）的观感；上下各档按石墨比例的常见跨度等距铺开。
pub const GRADES: [Grade; 7] = [
    Grade { name: "2H", tone: 70,  gamma: 1.9,  var: 0.45 },
    Grade { name: "H",  tone: 95,  gamma: 1.55, var: 0.58 },
    Grade { name: "HB", tone: 125, gamma: 1.25, var: 0.72 },
    Grade { name: "B",  tone: 152, gamma: 1.0,  var: 0.86 },
    Grade { name: "2B", tone: 180, gamma: 0.8,  var: 1.0 },
    Grade { name: "4B", tone: 212, gamma: 0.62, var: 1.12 },
    Grade { name: "6B", tone: 240, gamma: 0.5,  var: 1.22 },
];

/// 默认档位 = HB（官方那支铅笔的浓淡）。
pub const DEFAULT: usize = 2;

impl Grade {
    /// 某个压力（0..1）下的实际覆盖率。
    pub fn tone_at(&self, p: f32) -> u8 {
        let p = p.clamp(0.0, 1.0);
        let v = self.tone as f32 * (0.45 + 0.55 * p.powf(self.gamma));
        v.round().clamp(0.0, 255.0) as u8
    }
}

/// 按下标取档位（越界夹到最后一档）。
pub fn grade(i: usize) -> &'static Grade {
    &GRADES[i.min(GRADES.len() - 1)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 越软越黑() {
        for w in GRADES.windows(2) {
            assert!(w[1].tone > w[0].tone, "{} 应该比 {} 深", w[1].name, w[0].name);
            assert!(w[1].gamma < w[0].gamma, "{} 应该比 {} 更吃压力", w[1].name, w[0].name);
        }
    }

    #[test]
    fn 压力越大越黑且不超过满压() {
        for g in GRADES.iter() {
            let a = g.tone_at(0.0);
            let b = g.tone_at(0.5);
            let c = g.tone_at(1.0);
            assert!(a < b && b < c, "{} 的压力曲线不单调: {a} {b} {c}", g.name);
            assert_eq!(c, g.tone, "{} 满压应该正好等于档位浓度", g.name);
            assert!(a >= 30, "{} 轻压也要看得见（{a}）", g.name);
        }
    }

    #[test]
    fn 同一力道下软铅一定更黑() {
        for p in [0.1, 0.2, 0.5, 1.0] {
            for w in GRADES.windows(2) {
                assert!(
                    w[1].tone_at(p) > w[0].tone_at(p),
                    "p={p} 时 {} 应该比 {} 深（{} vs {}）",
                    w[1].name,
                    w[0].name,
                    w[1].tone_at(p),
                    w[0].tone_at(p)
                );
            }
        }
    }

    #[test]
    fn 软铅更吃压力() {
        // 同样从 p=0.2 按到满压，6B 变黑得多，2H 几乎不动
        let delta = |g: &Grade| g.tone_at(1.0) as i32 - g.tone_at(0.2) as i32;
        let soft = delta(&GRADES[6]);
        let hard = delta(&GRADES[0]);
        assert!(soft > hard + 30, "6B 只变黑 {soft}，2H 变黑 {hard}");
    }

    #[test]
    fn 档位查表不越界() {
        assert_eq!(grade(DEFAULT).name, "HB");
        assert_eq!(grade(999).name, "6B");
    }
}
