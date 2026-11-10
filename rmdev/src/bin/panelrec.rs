//! 面板驱动的录制 / 回放测试台。
//!
//! 手写手感没法自动化，但**驱动收到的东西**可以：每一次 `Fb::refresh()`
//! 就是一份 `(矩形, waveform, mode, 该矩形的 4-bit 灰度)`。把它录下来，
//! 就能在任何一版驱动上按同一时间轴重放，用数字比对——不用人一直坐在那儿写。
//!
//! ```text
//! panelrec gen  /tmp/stroke.rmrec        合成一段确定性笔迹（快/慢/擦除）
//! panelrec stat /tmp/stroke.rmrec        档案摘要（条数、时长、矩形/波形分布）
//! panelrec play /tmp/stroke.rmrec        在本机面板上按原时间轴重放
//! panelrec play /tmp/stroke.rmrec 2.0    2 倍速
//! ```
//!
//! 合成那段刻意贴近 app 的真实投递方式：笔每 ~10 ms 投一块「这次新画的点」
//! 的小矩形，waveform = DU；擦除则是一次大矩形 + GC16。
//!
//! 录真机（跑 rmdev 时）：
//! ```sh
//! RM_PANEL_RECORD=/tmp/real.rmrec RM_PANEL_RECORD_CAP=200000000 ./rmdev
//! ```

use std::io::Read;

use rmdev::fb::{Fb, UPDATE_MODE_FULL, UPDATE_MODE_PARTIAL, WAVEFORM_MODE_DU};

const MAGIC: &[u8; 8] = b"RMREC1\0\0";
const W: usize = 1404;
const H: usize = 1872;

struct Rec {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    wave: u32,
    mode: u32,
    t: f64,
    levels: Vec<u8>,
}

fn read_all(path: &str) -> Vec<Rec> {
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .expect("打开录制文件")
        .read_to_end(&mut buf)
        .expect("读录制文件");
    assert!(buf.len() >= 8 && &buf[..8] == MAGIC, "不是 RMREC1 文件");
    let mut out = Vec::new();
    let mut p = 8usize;
    while p + 14 <= buf.len() {
        let g = |o: usize| u16::from_le_bytes([buf[p + o], buf[p + o + 1]]) as usize;
        let (x, y, w, h) = (g(0), g(2), g(4), g(6));
        let wave = buf[p + 8] as u32;
        let mode = buf[p + 9] as u32;
        let t = f32::from_le_bytes([buf[p + 10], buf[p + 11], buf[p + 12], buf[p + 13]]) as f64;
        p += 14;
        let n = w * h;
        assert!(p + n <= buf.len(), "录制文件被截断");
        out.push(Rec {
            x,
            y,
            w,
            h,
            wave,
            mode,
            t,
            levels: buf[p..p + n].to_vec(),
        });
        p += n;
    }
    out
}

fn wave_name(w: u32) -> &'static str {
    match w {
        0 => "INIT",
        1 => "DU",
        2 => "GC16",
        3 => "GC4",
        6 => "GL16",
        8 => "GLD16",
        _ => "?",
    }
}

// ------------------------------------------------------------------ 合成

/// 合成一段笔迹录制，覆盖真机上最难的几种动作。
///
/// 为什么用合成而不是录真人：**确定性**。同一个文件在每一版驱动上重放，
/// 结果可以直接逐帧比对；真人手写没法复现。
///
/// 动作序列（刻意覆盖：慢写、快到慢、曲线、折线、从轻到重、大块擦除）：
///   1. 横线，300 px/s（慢写，最考验跟手）
///   2. S 形曲线，速度 200 → 1600 px/s 渐变
///   3. 折线（之字形），900 px/s
///   4. 斜线，笔宽 1 → 6 px 渐变（轻 → 重）
///   5. 大矩形擦除（waveform = GC16，和 app 的擦除一样）
///
/// 每次笔事件的投递方式和 app 一致：包围盒 = 这次新落下的点，waveform = DU。
struct Gen {
    canvas: Vec<u8>,
    out: Vec<u8>,
    t: f64,
}

type Rect = (usize, usize, usize, usize);

impl Gen {
    fn new() -> Gen {
        Gen {
            canvas: vec![15u8; W * H], // 4-bit 灰度，15 = 白
            out: Vec::new(),
            t: 0.0,
        }
    }

    /// 投递一次局部更新（把矩形里的画布内容抄进录制）。
    fn flush(&mut self, r: Rect, wave: u32, mode: u32) {
        let (x, y, w, h) = r;
        let mut b = Vec::with_capacity(14 + w * h);
        b.extend_from_slice(&(x as u16).to_le_bytes());
        b.extend_from_slice(&(y as u16).to_le_bytes());
        b.extend_from_slice(&(w as u16).to_le_bytes());
        b.extend_from_slice(&(h as u16).to_le_bytes());
        b.push(wave as u8);
        b.push(mode as u8);
        b.extend_from_slice(&(self.t as f32).to_le_bytes());
        for row in 0..h {
            let base = (y + row) * W + x;
            b.extend_from_slice(&self.canvas[base..base + w]);
        }
        self.out.extend_from_slice(&b);
    }

    /// 在画布上落一个圆点，返回它影响到的包围盒。
    fn dot(&mut self, cx: f64, cy: f64, r: f64) -> Option<Rect> {
        let r = r.max(0.6);
        let ir = r.ceil() as i32;
        let (mut xa, mut ya, mut xb, mut yb) = (W as i32, H as i32, -1i32, -1i32);
        for oy in -ir..=ir {
            for ox in -ir..=ir {
                if ((ox * ox + oy * oy) as f64) > r * r {
                    continue;
                }
                let x = cx as i32 + ox;
                let y = cy as i32 + oy;
                if x < 1 || y < 1 || x >= W as i32 - 1 || y >= H as i32 - 1 {
                    continue;
                }
                // 笔心全黑，边缘一档灰（模拟笔尖的软边，也更像真机）
                let d = ((ox * ox + oy * oy) as f64).sqrt() / r;
                let v = if d < 0.7 { 0 } else { 6 };
                self.canvas[y as usize * W + x as usize] = v;
                xa = xa.min(x);
                ya = ya.min(y);
                xb = xb.max(x);
                yb = yb.max(y);
            }
        }
        if xb < xa {
            None
        } else {
            Some((xa as usize, ya as usize, (xb - xa + 1) as usize, (yb - ya + 1) as usize))
        }
    }

    /// 沿参数曲线走一笔。
    ///
    /// `path(u)` 给出 u∈[0,1] 上的点；`v0→v1` 是速度渐变（px/s），
    /// `r0→r1` 是笔宽渐变（px，模拟压感从轻到重）。每 10 ms 一个笔事件，
    /// 和真机 100 Hz 的事件率一致。
    fn stroke(
        &mut self,
        path: &dyn Fn(f64) -> (f64, f64),
        v0: f64,
        v1: f64,
        r0: f64,
        r1: f64,
        samples: usize,
    ) {
        // 先把曲线离散成折线并算累积弧长
        let n = samples.max(64);
        let mut pts = Vec::with_capacity(n);
        for i in 0..n {
            pts.push(path(i as f64 / (n - 1) as f64));
        }
        let mut cum = Vec::with_capacity(n);
        let mut acc = 0.0;
        cum.push(0.0);
        for i in 1..n {
            acc += (pts[i].0 - pts[i - 1].0).hypot(pts[i].1 - pts[i - 1].1);
            cum.push(acc);
        }
        let total = acc;
        if total < 1.0 {
            return;
        }
        let seg = |s: f64| -> (f64, f64) {
            // 二分找 s 落在哪一段
            let mut lo = 0usize;
            let mut hi = n - 1;
            while lo + 1 < hi {
                let mid = (lo + hi) / 2;
                if cum[mid] <= s {
                    lo = mid;
                } else {
                    hi = mid;
                }
            }
            let d = (cum[hi] - cum[lo]).max(1e-9);
            let f = ((s - cum[lo]) / d).clamp(0.0, 1.0);
            (
                pts[lo].0 + (pts[hi].0 - pts[lo].0) * f,
                pts[lo].1 + (pts[hi].1 - pts[lo].1) * f,
            )
        };

        let mut s = 0.0;
        while s < total {
            let u = s / total;
            let v = v0 + (v1 - v0) * u;
            let r = r0 + (r1 - r0) * u;
            let ds = v * 0.010; // 10 ms
            let s2 = (s + ds).min(total);
            let steps = ((s2 - s) as i32).max(1);
            let mut union: Option<Rect> = None;
            for k in 0..=steps {
                let f = k as f64 / steps as f64;
                let (x, y) = seg(s + (s2 - s) * f);
                if let Some(b) = self.dot(x, y, r) {
                    union = Some(match union {
                        Some(a) => (
                            a.0.min(b.0),
                            a.1.min(b.1),
                            (a.0 + a.2).max(b.0 + b.2) - a.0.min(b.0),
                            (a.1 + a.3).max(b.1 + b.3) - a.1.min(b.1),
                        ),
                        None => b,
                    });
                }
            }
            if let Some(r) = union {
                self.flush(r, 1, UPDATE_MODE_PARTIAL);
            }
            self.t += 10.0;
            s = s2;
        }
    }

    /// 擦掉一块区域（大矩形一次性刷白 + GC16，和 app 的擦除一致）。
    fn erase(&mut self, r: Rect) {
        let (x, y, w, h) = r;
        for row in 0..h {
            let base = (y + row) * W + x;
            self.canvas[base..base + w].fill(15);
        }
        self.flush(r, 2, UPDATE_MODE_PARTIAL);
        self.t += 60.0;
    }
}

fn gen(path: &str) {
    let mut g = Gen::new();

    // 1) 慢写横线，300 px/s
    g.stroke(
        &|u| (300.0 + 800.0 * u, 700.0 + 20.0 * u),
        300.0,
        300.0,
        3.0,
        3.0,
        256,
    );
    g.t += 400.0; // 抬笔，让波形自然跑完

    // 2) S 形曲线，速度 200 → 1600 px/s 渐变
    g.stroke(
        &|u| {
            let a = u * std::f64::consts::PI;
            (250.0 + 900.0 * u, 950.0 + 160.0 * (a - std::f64::consts::PI).sin())
        },
        200.0,
        1600.0,
        3.0,
        3.0,
        512,
    );
    g.t += 400.0;

    // 3) 折线（之字形），900 px/s
    let zig = |u: f64| {
        let legs = 5.0;
        let k = (u * legs).min(legs - 1e-9);
        let i = k.floor();
        let f = k - i;
        let f = if (i as i64) % 2 == 0 { f } else { 1.0 - f };
        (300.0 + 800.0 * (i + f) / legs, 1250.0 + 60.0 * f)
    };
    g.stroke(&zig, 900.0, 900.0, 3.0, 3.0, 512);
    g.t += 400.0;

    // 4) 从轻到重：笔宽 1 → 6 px
    g.stroke(
        &|u| (350.0 + 700.0 * u, 1450.0 + 90.0 * u),
        700.0,
        700.0,
        1.0,
        6.0,
        256,
    );
    g.t += 400.0;

    // 5) 擦除：一次大矩形 GC16
    g.erase((260, 1220, 900, 260));
    g.t += 500.0;

    let mut f = Vec::with_capacity(8 + g.out.len());
    f.extend_from_slice(MAGIC);
    f.extend_from_slice(&g.out);
    std::fs::write(path, &f).expect("写录制文件");
    println!(
        "已生成 {path}：{:.1} s，{} 条更新，{:.1} MB",
        g.t / 1000.0,
        count_recs(&f),
        (f.len() - 8) as f64 / 1e6
    );
}

fn count_recs(buf: &[u8]) -> usize {
    let mut p = 8usize;
    let mut n = 0;
    while p + 14 <= buf.len() {
        let g = |o: usize| u16::from_le_bytes([buf[p + o], buf[p + o + 1]]) as usize;
        let (w, h) = (g(4), g(6));
        p += 14 + w * h;
        n += 1;
    }
    n
}

// ------------------------------------------------------------------ 回放

fn play(path: &str, speed: f64) {
    let recs = read_all(path);
    let t_end = recs.last().map(|r| r.t).unwrap_or(0.0);
    println!(
        "回放 {path}：{} 条，{:.1} s（{speed}× 速）",
        recs.len(),
        t_end / 1000.0
    );
    let mut fb = Fb::open().expect("打开面板");
    let t0 = std::time::Instant::now();
    for r in &recs {
        let target = std::time::Duration::from_secs_f64(r.t / 1000.0 / speed);
        let now = t0.elapsed();
        if target > now {
            std::thread::sleep(target - now);
        }
        // 4-bit 灰度按 17 倍还原成 8 bit，再走 `refresh()`（和 app 同一条路）。
        let gray: Vec<u8> = r.levels.iter().map(|&l| l * 17).collect();
        fb.blit_gray(r.x as i32, r.y as i32, r.w as i32, r.h as i32, &gray);
        fb.refresh(
            r.x as i32,
            r.y as i32,
            r.w as i32,
            r.h as i32,
            r.wave,
            r.mode,
        );
    }
    // 等最后几次更新播完，再让 Drop 收尾（补空白帧、落状态文件）。
    std::thread::sleep(std::time::Duration::from_millis(1200));
    drop(fb);
    println!("回放结束（{:.1} s）", t0.elapsed().as_secs_f64());
    let _ = WAVEFORM_MODE_DU;
    let _ = UPDATE_MODE_FULL;
}

// ------------------------------------------------------------------ 摘要

fn stat(path: &str) {
    let recs = read_all(path);
    let t_end = recs.last().map(|r| r.t).unwrap_or(0.0);
    let mut area = 0usize;
    let mut by_wave: std::collections::BTreeMap<&str, usize> = Default::default();
    let mut by_start: std::collections::BTreeMap<i64, usize> = Default::default();
    for r in &recs {
        area += r.w * r.h;
        *by_wave.entry(wave_name(r.wave)).or_default() += 1;
        // 每 100 ms 一格的投递频率分布
        *by_start.entry((r.t / 100.0) as i64).or_default() += 1;
    }
    println!("文件      {path}");
    println!("条数      {}", recs.len());
    println!("时长      {:.2} s", t_end / 1000.0);
    println!(
        "平均面积  {:.0} px（总计 {:.1} Mpx）",
        area as f64 / recs.len() as f64,
        area as f64 / 1e6
    );
    println!("波形分布  {by_wave:?}");
    let max = by_start.values().copied().max().unwrap_or(1);
    let hot: Vec<i64> = by_start
        .iter()
        .filter(|(_, &v)| v * 2 >= max)
        .map(|(&k, _)| k)
        .collect();
    println!("峰值投递  {max} 次/100 ms；高频区间（×100 ms）= {hot:?}");
    // 最大的几个矩形和它们用的波形：真机录下来以后第一眼要看的就是这个
    // （app 到底把多大的块丢给驱动、用的是 DU 还是 GL16/GC16）。
    let mut big: Vec<&Rec> = recs.iter().collect();
    big.sort_by_key(|r| std::cmp::Reverse(r.w * r.h));
    println!("最大的 8 块：");
    for r in big.iter().take(8) {
        println!(
            "  {:4}×{:<5} @({:4},{:4})  {}  面积 {:8}",
            r.w, r.h, r.x, r.y, wave_name(r.wave), r.w * r.h
        );
    }
    let mut by_mode: std::collections::BTreeMap<u32, usize> = Default::default();
    for r in &recs {
        *by_mode.entry(r.mode).or_default() += 1;
    }
    println!("update_mode 分布  {by_mode:?}（0 = PARTIAL，1 = FULL）");
    let dts: Vec<f64> = recs.windows(2).map(|w| w[1].t - w[0].t).collect();
    if !dts.is_empty() {
        let mut s = dts.clone();
        s.sort_by(|a, b| a.partial_cmp(b).unwrap());
        println!(
            "投递间隔  中位 {:.1} ms，最小 {:.2}，最大 {:.1}",
            s[s.len() / 2],
            s[0],
            s[s.len() - 1]
        );
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(|s| s.as_str()).unwrap_or("") {
        "gen" => gen(a.get(2).map(|s| s.as_str()).unwrap_or("/tmp/stroke.rmrec")),
        "play" => play(
            a.get(2).map(|s| s.as_str()).unwrap_or("/tmp/stroke.rmrec"),
            a.get(3).and_then(|s| s.parse().ok()).unwrap_or(1.0),
        ),
        "stat" => stat(a.get(2).map(|s| s.as_str()).unwrap_or("/tmp/stroke.rmrec")),
        _ => eprintln!("用法：panelrec gen|play|stat <文件> [倍速]"),
    }
}
