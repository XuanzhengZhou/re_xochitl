//! 刷大块时临时把 CPU 频率钉高。
//!
//! 为什么需要：面板线程一个相位只有 11.76 ms 的预算，整屏编码实测要
//! 9~17 ms（`gray_to_cols` + 相位编码）。而 rM2 的 cpufreq 是 `ondemand`，
//! 空闲时降到 396 MHz；一次整屏刷新（睡眠图、关机框、翻页）从按下到出图
//! 要 1.1 s 而不是 0.55 s —— 日志实证（同一块 Gl16 整屏）：
//!
//! ```text
//! ondemand             1146 ms（编码 794 / 46 相位 → 24.7 ms 帧周期，超帧 45/45）
//! scaling_min_freq 钉满  553 ms（编码 233 / 46 相位 → 11.9 ms 帧周期，超帧 0/45）
//! ```
//!
//! 所以只在"这一轮要驱的面积够大"时把 `scaling_min_freq` 顶到最高，刷完立刻
//! 还原。小刷新（一笔、一个开关）不动它：那点活不值得改全局调频策略，
//! 而且频繁写 sysfs 本身也有代价。

use std::path::PathBuf;

const CPUS: usize = 2;

fn cpu_path(i: usize, name: &str) -> PathBuf {
    PathBuf::from(format!(
        "/sys/devices/system/cpu/cpu{i}/cpufreq/{name}"
    ))
}

fn read_trim(p: &PathBuf) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

/// 把两个核的 `scaling_min_freq` 顶到 `cpuinfo_max_freq`；`drop` 时还原。
pub struct Boost {
    saved: Vec<(PathBuf, String)>,
}

impl Boost {
    /// 起一次提频。设备上没有 cpufreq（本机、或者以后内核配置变了）就返回一个
    /// 什么都不做的 Boost——调用方不用判空。
    pub fn new() -> Boost {
        let mut saved = Vec::new();
        for i in 0..CPUS {
            let max = cpu_path(i, "cpuinfo_max_freq");
            let min = cpu_path(i, "scaling_min_freq");
            let (Some(hi), Some(lo)) = (read_trim(&max), read_trim(&min)) else {
                continue;
            };
            // 已经是最高就不用动（也别记，免得还原时把它压回去）
            if hi == lo {
                continue;
            }
            if std::fs::write(&min, &hi).is_ok() {
                saved.push((min, lo));
            }
        }
        Boost { saved }
    }

    /// 有没有真的动过（诊断用）。
    pub fn active(&self) -> bool {
        !self.saved.is_empty()
    }
}

impl Drop for Boost {
    fn drop(&mut self) {
        for (p, v) in self.saved.drain(..) {
            let _ = std::fs::write(p, v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 没有 cpufreq 的环境（CI / 开发机）必须安静退化，不能 panic、不能卡住。
    #[test]
    fn boost_is_a_noop_when_cpufreq_is_missing() {
        if cpu_path(0, "scaling_min_freq").exists() {
            // 有 cpufreq 的机器上只验证"能建能放"，不去改真实策略：
            // 这里读一下当前值，确保 new/drop 不改坏它。
            let before = read_trim(&cpu_path(0, "scaling_min_freq"));
            {
                let b = Boost::new();
                let _ = b.active();
            }
            assert_eq!(before, read_trim(&cpu_path(0, "scaling_min_freq")));
        } else {
            let b = Boost::new();
            assert!(!b.active());
        }
    }
}
