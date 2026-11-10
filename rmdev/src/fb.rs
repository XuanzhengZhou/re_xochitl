//! 帧缓冲：1404×1872 的 RGB565 画布 + 我们自己的面板驱动。
//!
//! # 这条路是怎么来的
//!
//! 以前这里的 `open()` 打开的是 rm2fb client 伪造的 `/dev/fb0`（1404×1872@16bpp），
//! `refresh()` 发 `MXCFB_SEND_UPDATE` ioctl，交给官方 xochitl 里的
//! `EPFramebufferSwtcon` 去驱动面板——也就是说，屏幕实际上一直攥在官方进程手里。
//!
//! 现在不用了：这台机器根本没有 EPDC，上屏就是「把 32-bit cell 码写进
//! `/dev/fb0` 的槽里再 `FBIOPAN_DISPLAY` 翻页」。这部分在 `crate::panel` 里
//! 重写成了纯 Rust，`Fb` 只负责：
//!
//! * 持有一块 RGB565 平面（绘图 API 全在它上面，和以前一模一样）；
//! * `refresh()` 时把脏矩形转成 4-bit 灰度，交给 `Panel` 编码 + 播放。
//!
//! 对上层暴露的东西（`put` / `row_mut` / `blit_gray` / `fill_rect` / `dim` /
//! `get` / `refresh` / `gray565` / `dump_pgm`）一个签名都没变，所以 `main.rs`、
//! `canvas.rs`、`chrome.rs` 那些一行都不用改。

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::panel::{Mode, Panel};

// ---------------------------------------------------------------- 面板常量

/// 用户宽（屏宽）。
pub const W: usize = crate::panel::UW;
/// 用户高。
pub const H: usize = crate::panel::UH;

pub const BLACK: u16 = 0x0000;
pub const WHITE: u16 = 0xFFFF;

/// 二值绘制（`Fb::set_mono`）用的黑白分界，按 `r5 + g6 + b5` 求和。
///
/// 界面上的颜色全是 `gray565(g8)` 出来的，而 5-6-5 三段和随 g8 单调递增：
/// `g8 = 127` → `15+31+15 = 61`，`g8 = 128` → `16+32+16 = 64`。取 64 就是
/// 精确的「g8 ≥ 128 算白」，和灰度域的直觉一致（单测钉住了这个等价关系）。
const MONO_CUT: u32 = 64;

/// RGB565 → 纯黑 / 纯白。
///
/// 二值波形（A2 / DU）只认两种极性：**目标色是中间灰的像素一个驱动码都没有**
/// （见 `panel/encode.rs` 的注释），屏上保持原样。所以"灰底输入框 + 抗锯齿
/// 文字"放进 A2 区域里，退格时旧字会被钉在屏上。要走二值刷新的区域必须先
/// 把颜色压成黑白，这个函数就是那个压法。
pub fn mono565(c: u16) -> u16 {
    let r = ((c >> 11) & 0x1f) as u32;
    let g = ((c >> 5) & 0x3f) as u32;
    let b = (c & 0x1f) as u32;
    if r + g + b >= MONO_CUT {
        WHITE
    } else {
        BLACK
    }
}

pub const UPDATE_MODE_PARTIAL: u32 = 0;
pub const UPDATE_MODE_FULL: u32 = 1;

/// 只推极性翻转的像素——手写、局部动作用。
pub const WAVEFORM_MODE_DU: u32 = 1;
pub const WAVEFORM_MODE_GC16: u32 = 2;
/// 最快的二值刷：整轮只有 4 帧 ≈ 47 ms。编号沿用 NXP `mxcfb.h` 那一套
/// （INIT 0 / DU 1 / GC16 2 / GC4 3 / A2 4 / GL16 5 / GLR16 6 / GLD16 7 / DU4 8），
/// 我们的 A2=4、GL16=5 正好和它对上。
///
/// 4 帧基本推不满墨，方块边缘会发灰、残影压不干净 —— 它存在的意义是
/// "追手优先于画质"，滚动预览那种场景。
pub const WAVEFORM_MODE_DU4: u32 = 8;
/// 二值快刷。和 DU 语义相同（只推极性翻转的像素），但波形短得多：
/// DU = 25 帧 ≈ 294 ms，A2 = 10 帧 ≈ 118 ms。官网的快速滚动/手写用它。
///
/// 以前驱动里**根本没接这个数**，`WAVEFORM_MODE_A2` 会被落到 GL16 分支上
/// （等于换了一张完全不匹配的波形表）。现在按官方语义映射。
pub const WAVEFORM_MODE_A2: u32 = 4;
/// 16 级灰度、慢、无残影——整页重绘/撤销用。
pub const WAVEFORM_MODE_GL16: u32 = 5;

/// 8 位灰度 → RGB565。
///
/// 面板最后只留 4 bit（`level = gray8 * 15 / 255`），所以按 5-6-5 量化就够。
/// 256 项查表：逐像素算要 4 次移位 + 3 次掩码，整屏贴图时是实打实的开销。
#[inline]
pub fn gray565(g8: u8) -> u16 {
    GRAY565[g8 as usize]
}

pub static GRAY565: [u16; 256] = {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let g = i as u16;
        t[i] = (((g >> 3) & 0x1f) << 11) | (((g >> 2) & 0x3f) << 5) | ((g >> 3) & 0x1f);
        i += 1;
    }
    t
};

/// 统一时间轴：进程内第一次调用 `now_ms` 的时刻当 0。
static EPOCH: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// 进程启动至今的毫秒数（浮点，够看 0.1 ms 级的差）。
pub fn now_ms() -> f64 {
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_secs_f64() * 1000.0
}

fn env_flag(k: &str) -> bool {
    std::env::var(k).map(|v| v != "0").unwrap_or(false)
}

/// 一次待播的刷新：矩形 + 该矩形的 4-bit 灰度快照。
///
/// 快照必须是"投递那一刻"的像素：UI 线程投完就能继续画下一帧，
/// 工作线程晚一点才播也不会串画面。
struct Job {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    levels: Vec<u8>,
    mode: Mode,
    /// 调用方请求的原始 `waveform_mode`（官方 swtcon 用它查跳转表选表）。
    wf: u32,
    /// 投递时刻（`now_ms()`）。用来量「投递 → 上屏」的端到端延迟：
    /// 如果这个数一直是十几毫秒，说明笔迹慢是面板物理上升时间，不是排队。
    t: f64,
    /// 能不能把驱动矩形收缩到「真正变了的像素」。
    ///
    /// 官方 `dispatch_processed_regions` 的 incremental 模式就是这么干的：
    /// 提交前把 Item 的矩形收窄到实际变化像素的包围盒（y 向外取整到 8 行，
    /// x 精确）。只有 `UPDATE_MODE_FULL`（客户端要求整块重刷）才不收缩。
    ///
    /// 这一步直接决定「落笔时背景闪不闪」：一块 400×300 的脏矩形里如果
    /// 只有 20×20 的墨变了，不收缩就是把 12 万像素全按波形驱一遍。
    narrow: bool,
}

/// RGB565 平面（**行主序**，`plane[row*stride + col]`）→ 4-bit 灰度
/// （**列主序**，`out[col*h + row]`）。
///
/// 为什么要在 UI 线程转置：面板线程是 SCHED_FIFO 实时线程，一个相位的预算
/// 只有 11.93 ms（一个面板帧），而 `img`/`prv`/`kp` 都是列主序的
/// （`x*1872 + y`）。把行主序的 `levels` 摊进列主序以前在面板线程里做，
/// 实测整屏 **46.5 ms**（17 ns/像素）—— 一个相位吃掉 4 帧，波形被拉长
/// 4 倍，屏上就是一路闪。放到 UI 线程这边，它和面板线程的 vsync 等待
/// 天然并行（i.MX7D 是双核 A7），而且这里读的 `plane` 是刚被 `blit_gray`
/// 写热的。
///
/// 64×64 分块：块内两侧都是完整的 cache line。
///
/// **实测这个函数曾经是 app 侧最大的单笔开销**：整屏 `1404×1872` 要
/// `57.8 ms`（22 ns/像素，见 `[pf] push … 灰度 57815us`）。两个原因：
/// * 每个像素做一次 `(g6 * 15 + 31) / 63` —— ARMv7 没有硬件整数除法，
///   那是软件模拟，一次几十个周期。现在换成 64 项查表。
/// * 暂存缓冲是**列主序**（`buf[col][row]`），对固定 `row` 写 64 个相隔
///   64 字节的地址 = 每个像素摸一条新的 cache line。改成行主序之后写是
///   连续的，跨步只留在最后那次转置**读**上（4 KB 全在 L1）。
const fn g6_lut() -> [u8; 64] {
    let mut t = [0u8; 64];
    let mut i = 0usize;
    while i < 64 {
        t[i] = ((i * 15 + 31) / 63) as u8;
        i += 1;
    }
    t
}

/// 6 位绿通道 → 4 位灰度的查表版（`g6_lut` 的常量求值结果）。
static G6_TO_G4: [u8; 64] = g6_lut();

// ---------------------------------------------------------------- 横屏

/// 横屏模式（用户 2026-11-10 要求）：**整幅逻辑画面顺时针转 90° 再上屏**。
///
/// 面板本身只有一种物理方向（1404 宽 × 1872 高），所以"横屏"只能是软件转：
/// 逻辑画面变成 1872×1404（`Fb::width/height` 跟着换），`plane` 仍按逻辑
/// 行主序放，"转"发生在 `push_rect` 把逻辑矩形翻成物理列主序的那一刻。
///
/// 为什么不在绘制原语里逐像素转：那样 `row_mut`（贴图整行写）没法返回连续
/// 切片，`fill_rect` 也失去按行 `fill` 的向量化。放在 `push_rect` 里转，
/// 绘制侧一行都不用改，而且灰度转换本来就是"读行主序、写列主序"，旋转刚好
/// 是同一趟：逻辑行 → 物理列，两边都连续。
///
/// 这个开关是**进程级**的（一台机器只有一块面板），`chrome`/`viewport`/
/// 输入解码都从这里问方向，避免同一条事实存好几份。
static LANDSCAPE: AtomicBool = AtomicBool::new(false);

/// 横屏往哪边转：`false` = 逆时针（充电口在左，用户 2026-11-10 选的默认），
/// `true` = 顺时针（充电口在右）。
///
/// 这只是"逻辑画面怎么摆到面板上"，和逻辑布局无关：两种方向下画面都是正的，
/// 区别只在用户把设备往哪边转 90°。所以它只影响 `rot_rect` / `to_logical` /
/// `gray_to_cols_rot` 三处。
static LAND_CW: AtomicBool = AtomicBool::new(false);

#[inline]
pub fn land_dir_cw() -> bool {
    LAND_CW.load(Ordering::Relaxed)
}

/// 左利手（设置 → Accessibility → Handedness → Left）：整块 UI 左右对调。
///
/// 官方左利手就是把**工具条挪到右边**（`×`、锁、页码、各个浮层跟着换边），
/// 内容不镜像 —— 字还是从左往右读。所以这里不用"翻转像素"那种做法，而是把
/// 每个横向锚点过一遍 [`flip_x`]：谁靠左边多少，镜像后就靠右边多少。
///
/// 和横屏是**正交**的两件事：横屏转的是整幅画面（在 `push_rect` 里），镜像
/// 转的是布局锚点（在绘制/命中那一层），两者相乘也不会打架。
static MIRROR: AtomicBool = AtomicBool::new(false);

#[inline]
pub fn mirrored() -> bool {
    MIRROR.load(Ordering::Relaxed)
}

/// 设置左利手布局。返回 `true` = 状态真的变了（调用方要整页重绘）。
pub fn set_mirrored(on: bool) -> bool {
    MIRROR.swap(on, Ordering::Relaxed) != on
}

/// 把一个"从左边量"的横向矩形镜像成"从右边量同样距离"。
///
/// `x` 是左边距、`w` 是宽度：右利手原样返回；左利手返回 `逻辑宽 - x - w`。
/// 一个矩形不用说自己属于哪一边 —— 贴左的 `(0, 120)` 变成贴右的 `(lw-120, 120)`，
/// 贴右的 `(lw-104, 90)` 变成贴左的 `(14, 90)`，两边自动对称。
#[inline]
pub fn flip_x(x: i32, w: i32) -> i32 {
    flip_in(x, w, logical().0, mirrored())
}

/// [`flip_x`] 的纯函数版：只依赖入参，好写单测（全局开关在测试里会串台）。
#[inline]
fn flip_in(x: i32, w: i32, lw: i32, on: bool) -> i32 {
    if on {
        lw - x - w
    } else {
        x
    }
}

/// 当前是不是横屏。
#[inline]
pub fn landscape() -> bool {
    LANDSCAPE.load(Ordering::Relaxed)
}

/// 逻辑画面尺寸：竖屏 1404×1872，横屏 1872×1404。
#[inline]
pub fn logical() -> (i32, i32) {
    if landscape() {
        (H as i32, W as i32)
    } else {
        (W as i32, H as i32)
    }
}

/// 物理点 → 逻辑点（横屏才换）。输入设备只认面板物理坐标，主循环只认逻辑
/// 坐标，所以笔和手指在**解码那一层**就转过来（`input::decode` /
/// `input::touch`），到 `handle()` 时已经全是逻辑坐标。
///
/// 逻辑 `(x, y)` 上屏成物理 `(W-1-y, x)`，反过来就是 `x_l = y_p`、
/// `y_l = W - x_p`（`W` = 物理屏宽 1404）。
#[inline]
pub fn to_logical(x: f32, y: f32) -> (f32, f32) {
    if !landscape() {
        return (x, y);
    }
    let (lw, lh) = logical();
    let (lx, ly) = if land_dir_cw() {
        // 顺时针：px = 逻辑高-1-y、py = x  →  x = py、y = 逻辑高-px
        (y, W as f32 - x)
    } else {
        // 逆时针：px = y、py = 逻辑宽-1-x  →  x = 逻辑宽-py、y = px
        (H as f32 - y, x)
    };
    (
        lx.clamp(0.0, lw as f32 - 1.0),
        ly.clamp(0.0, lh as f32 - 1.0),
    )
}

/// 逻辑矩形 → 物理矩形（转 90°，宽高对调）。
///
/// 纯函数，单测直接钉它 —— 这里曾经把"逻辑高"写成物理高 1872，整幅画面先
/// 右移 468 列再被夹到 936 宽：上屏的内容是错位的，而截屏（读的是逻辑缓冲）
/// 看着完全正常，只有真机屏上才看得出来。
///
/// * 顺时针（`cw`）：`px = 逻辑高 - y - h`、`py = x`
/// * 逆时针：　　　　`px = y`、`py = 逻辑宽 - x - w`
#[inline]
fn rot_rect((x, y, w, h): Rect, (lw, lh): (usize, usize), cw: bool) -> Rect {
    if cw {
        (lh.saturating_sub(y + h), x, h, w)
    } else {
        (y, lw.saturating_sub(x + w), h, w)
    }
}

fn gray_to_cols(
    plane: &[u16],
    stride: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    out: &mut [u8],
) {
    debug_assert!(out.len() >= w * h);
    debug_assert!(x + w <= stride);
    const BH: usize = 64;
    const BW: usize = 64;
    // 行主序：`buf[row][col]`
    let mut buf = [[0u8; BW]; BH];
    let mut bx = 0;
    while bx < w {
        let cw = BW.min(w - bx);
        let mut by = 0;
        while by < h {
            let ch = BH.min(h - by);
            for row in 0..ch {
                let base = (y + by + row) * stride + x + bx;
                let src = &plane[base..base + cw];
                let dst = &mut buf[row][..cw];
                for (d, &v) in dst.iter_mut().zip(src) {
                    *d = G6_TO_G4[((v >> 5) & 0x3f) as usize];
                }
            }
            for col in 0..cw {
                // **一定要加 `bx`**：整屏 1404 列会切成 22 个 64 列的小块，
                // 漏掉块偏移的话每一块都往 `0..64*h` 写，最后只有开头那一
                // 小块有内容、其余全 0（黑）——屏上就是"只有一个三角形区域
                // 在变白"。这个 bug 真踩过。
                let d = (bx + col) * h + by;
                let o = &mut out[d..d + ch];
                for (row, v) in o.iter_mut().enumerate() {
                    *v = buf[row][col];
                }
            }
            by += ch;
        }
        bx += cw;
    }
}

/// 横屏版：逻辑矩形 `w×h` **顺时针转 90°** 之后写成物理列主序。
///
/// 逻辑 `(x+i, y+j)` → 物理列 `h-1-j`、物理行 `i`（转完的矩形宽 `h`、高 `w`），
/// 而 `out[col*hphys + row]` 里 `hphys == w`，于是 `out[(h-1-j)*w + i]`：
/// 外层 `j`、内层 `i` 时写地址连续，读也是连续一行 —— 和竖屏那趟转置同一个
/// 代价，没有额外的 cache 损失。
fn gray_to_cols_rot(
    plane: &[u16],
    stride: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    out: &mut [u8],
    cw: bool,
) {
    debug_assert!(out.len() >= w * h);
    debug_assert!(x + w <= stride);
    // 兜底：横屏上屏这条路一旦算错矩形就是整机 panic（UI 线程 panic = 进程没了，
    // 屏停在最后一帧）。宁可少画一块，也不能把 app 带走 —— 少画的那块下次
    // 脏区差分还会抓到。
    let need = y.saturating_add(h).saturating_sub(1) * stride + x + w;
    if w == 0 || h == 0 || need > plane.len() || out.len() < w * h {
        eprintln!(
            "[fb] rot 越界：plane={} 需要 {need} (w={w} h={h} x={x} y={y} stride={stride} out={})",
            plane.len(),
            out.len()
        );
        return;
    }
    for j in 0..h {
        let base = (y + j) * stride + x;
        let src = &plane[base..base + w];
        if cw {
            // 顺时针：物理列 = h-1-j，物理行 = i（顺着写）
            let o = &mut out[(h - 1 - j) * w..(h - 1 - j) * w + w];
            for (d, &v) in o.iter_mut().zip(src) {
                *d = G6_TO_G4[((v >> 5) & 0x3f) as usize];
            }
        } else {
            // 逆时针：物理列 = j，物理行 = w-1-i（倒着写）
            let o = &mut out[j * w..j * w + w];
            for (k, &v) in src.iter().enumerate() {
                o[w - 1 - k] = G6_TO_G4[((v >> 5) & 0x3f) as usize];
            }
        }
    }
}

pub struct Fb {
    /// RGB565 平面，行优先，`stride = width * 2`。
    plane: Vec<u16>,
    pub width: i32,
    pub height: i32,
    /// 面板工作线程的投递端。`None` = 无头模式。
    /// 面板工作线程的投递端。
    ///
    /// **有界**（`sync_channel`）。以前是无界 channel，面板线程跟不上时
    /// job 会在队列里无限堆积，而每个 job 都带着一整块灰度数据（整屏
    /// 2.6 MB）—— 画满一屏就能把内存顶爆，Rust 的分配失败是 `abort`
    /// （systemd 里看到的就是 `status=134`，直接回库界面）。
    /// 满了就 `try_send` 失败、把缓冲收回池里，那块**不记已推**，下一次
    /// 脏区差分还会重新抓到，不会漏内容。
    tx: Option<std::sync::mpsc::SyncSender<Job>>,
    worker: Option<std::thread::JoinHandle<()>>,
    /// 灰度转换的临时缓冲，避免每次刷新都分配。
    levels: Vec<u8>,
    /// 「上一次已经交给面板线程时，`plane` 长什么样」。
    ///
    /// `refresh` 会把它和 `plane` 逐像素比一遍，把**真正变了的**像素包围盒
    /// 并进驱动矩形 —— 这是本驱动唯一一处"不完全信任调用方给的矩形"，而
    /// 它是必需的：app 是照着 rm2fb 写的，那个 server 每次更新都把整块
    /// 16bpp 帧缓冲全量转成 cell 码推上去，`refresh` 里的矩形对它只是参考。
    /// 所以 app 的脏矩形从来没被校验过（实测它**从不**为 `y < 120` 的区域发
    /// 刷新，菜单的重绘矩形也和真正画的地方错位）。
    ///
    /// 为什么用逐像素差分而不是"绘制原语记账"的粗包围盒：粗包围盒会把
    /// 两次刷新之间画过的所有东西并进来，一行字没变也会把整屏算进去 ——
    /// 实测驱动矩形被顶到 `1404×1872`，帧周期 37 ms，波形被拉长 3 倍，
    /// 笔迹直接落不住。差分只算真的变了的像素，代价是每次刷新扫一遍
    /// 2.6 Mpx（8 字节一比，约 0.5 ms，在 UI 线程，不进帧预算）。
    pushed: Vec<u16>,
    /// 是否把每条刷新的**调用点**打进日志（`RM_PANEL_JOBS=1`）。
    ///
    /// 只是为了回答"这个 600×720 到底是谁要的"。app 里同一个区域可能被好几处
    /// 代码重画，光看矩形分不出来；`#[track_caller]` 直接给出 `文件:行号`。
    jobs: bool,
    /// 诊断：把每次 `refresh` 最终决定要刷的矩形**用边框画到屏幕上**。
    ///
    /// 开着的时候（`RM_VIS=1`）每一块被投递的矩形都会描一圈 2 px 的边，
    /// 同一次 refresh 里第 1/2/3+ 块用黑 / 深灰 / 浅灰，好区分是哪一块。
    /// 边框像素画进 `plane` 之后立刻 `mark_pushed`，所以它不会把脏区差分
    /// 搞成死循环。用完记得关 —— 边框会留在屏上直到那块被重画。
    vis: bool,
    /// 还没被面板线程消化完的 job 数（`wait_idle` 用）。
    ///
    /// 睡眠前必须等它归零：挂起时面板线程正卡在某个波形中间的话，
    /// 那一帧的相位就永远停在那里（复位后屏上留一块半驱动的灰）。
    /// `None` = 无头模式（`open_mem`），没有面板线程，永远算空闲。
    pending: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    /// 面板线程归还灰度缓冲的通道。
    ///
    /// `levels` 是每次 `refresh` 都要交出去的一整块灰度数据（整屏 2.6 MB）。
    /// 以前是 `mem::take` 之后 move 进 channel，UI 线程下一次就得重新
    /// `malloc` 同样大的一块 —— 新页第一次写要缺页，整屏实测多出 **146 ms**
    /// （`[pf] refresh … 总 212418us = … 推送 211445`，而 push 内部只报了
    /// 灰度 54.8 + 记账 10.4）。面板线程用完从这里还回来，UI 侧复用。
    back_rx: Option<std::sync::mpsc::Receiver<Vec<u8>>>,
    /// 空闲的灰度缓冲池。
    pool: Vec<Vec<u8>>,
    /// app 自上次 `refresh` 以来**通过 `put`/`row_mut`/`fill_rect`/`dim`/`clear`
    /// 改过的**区域的包围盒。
    ///
    /// 脏区差分以前每次都把整屏 `plane` 和 `pushed` 比一遍：2.6 M 像素、
    /// 读 10.4 MB，DDR 带宽跑满也要 **32 ms**（`[pf] refresh 120×480@0
    /// 总 62375us = 差分 32423 + …`），而且跟请求多大完全无关。app 每次
    /// 落笔都付一次，刷新率被压到 16 Hz —— 手感就是"笔跟不上"。
    ///
    /// 现在只在写入口记一个包围盒，差分只扫这里。写字时它就是那几十像素。
    touch: Option<Rect>,
    /// 二值绘制开关（`set_mono`）。只给"要用 A2 局部刷新"的那几屏用。
    /// 见 `mono565` 的注释：关闭时行为一个字节都不变。
    mono: bool,
    /// 录制器（`RM_PANEL_RECORD` 指向文件时启用）。
    rec: Option<Recorder>,
}

/// 把每一次 `refresh()` 原样录下来，供 `panelrec` 重放。
///
/// 记录的就是驱动**真正收到**的东西：矩形、waveform、mode、那一刻的 4-bit
/// 灰度。重放时走的还是同一条 `refresh()` 路径，所以测出来的就是真行为。
///
/// 格式（小端）：`b"RMREC1\0\0"` 头 + 逐条
/// `x u16, y u16, w u16, h u16, wave u8, mode u8, t_ms f32` + `w*h` 字节灰度。
pub struct Recorder {
    f: std::fs::File,
    t0: std::time::Instant,
    written: u64,
    cap: u64,
    /// 超出上限后只提示一次。
    warned: bool,
}

const REC_MAGIC: &[u8; 8] = b"RMREC1\0\0";

impl Recorder {
    fn open(path: &str, cap: u64) -> io::Result<Recorder> {
        use std::io::Write;
        let mut f = std::fs::File::create(path)?;
        f.write_all(REC_MAGIC)?;
        Ok(Recorder {
            f,
            t0: std::time::Instant::now(),
            written: REC_MAGIC.len() as u64,
            cap,
            warned: false,
        })
    }

    fn push(&mut self, x: usize, y: usize, w: usize, h: usize, wave: u32, mode: u32, levels: &[u8]) {
        use std::io::Write;
        let n = 14 + levels.len();
        if self.written + n as u64 > self.cap {
            if !self.warned {
                self.warned = true;
                eprintln!(
                    "[rec] 到上限 {:.0} MB，停止录制（文件里已有的仍然可重放）",
                    self.cap as f64 / 1e6
                );
            }
            return;
        }
        let t = self.t0.elapsed().as_secs_f64() * 1e3;
        let mut hdr = [0u8; 14];
        hdr[0..2].copy_from_slice(&(x as u16).to_le_bytes());
        hdr[2..4].copy_from_slice(&(y as u16).to_le_bytes());
        hdr[4..6].copy_from_slice(&(w as u16).to_le_bytes());
        hdr[6..8].copy_from_slice(&(h as u16).to_le_bytes());
        hdr[8] = wave as u8;
        hdr[9] = mode as u8;
        hdr[10..14].copy_from_slice(&(t as f32).to_le_bytes());
        if self.f.write_all(&hdr).is_ok() && self.f.write_all(levels).is_ok() {
            self.written += n as u64;
        }
    }
}

impl Fb {
    /// 真机：打开面板驱动，并把"播相位"交给一个独立线程。
    ///
    /// 为什么一定要独立线程：一次整屏 GC16 要 46 帧 ≈ 550 ms，局部 GC16 也是
    /// 46 帧（波形是定时长的，跟面积无关）。如果在 UI 线程里同步播，输入事件
    /// 只能排在这一帧后面——点一下菜单就卡半秒。官方 xochitl 也是把驱动丢给
    /// 一个实时优先级线程，应用只管 `sendUpdate`。
    pub fn open() -> io::Result<Fb> {
        let panel = Panel::open().map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        eprintln!("[panel] {}", panel.describe());
        // 冷启动刚跑完 INIT（111 帧连续驱动）：墨水的物理状态还没落定。
        // 这时候立刻推第一帧，得到的是**低对比度**的一张图——真机症状就是
        // "开机后库界面淡灰、像残影"，甚至一片白。等 2 s 再开始画。
        if panel.inited {
            eprintln!("[panel] INIT 刚跑完，等 2 s 让墨水平复再画第一帧");
            std::thread::sleep(std::time::Duration::from_millis(2000));
        }
        // 8 深：普通笔迹 job 只有几十 KB，8 个是几百 KB；整屏 job 是 2.6 MB，
        // 8 个也就 20 MB 左右，给面板线程留够追赶余地，又不至于吃爆内存。
        let (tx, rx) = std::sync::mpsc::sync_channel::<Job>(8);
        let (back_tx, back_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let pending = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pending_t = pending.clone();
        let worker = std::thread::Builder::new()
            .name("framegen".into())
            .spawn(move || framegen(panel, rx, back_tx, pending_t))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        let rec = match std::env::var("RM_PANEL_RECORD") {
            Ok(p) if !p.is_empty() => {
                let cap = std::env::var("RM_PANEL_RECORD_CAP")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(200_000_000u64);
                match Recorder::open(&p, cap) {
                    Ok(r) => {
                        eprintln!("[rec] 录制到 {p}（上限 {:.0} MB）", cap as f64 / 1e6);
                        Some(r)
                    }
                    Err(e) => {
                        eprintln!("[rec] 打不开 {p}: {e}");
                        None
                    }
                }
            }
            _ => None,
        };
        let (lw, lh) = logical();
        Ok(Fb {
            plane: vec![WHITE; (lw as usize) * (lh as usize)],
            width: lw,
            height: lh,
            tx: Some(tx),
            worker: Some(worker),
            levels: Vec::new(),
            pushed: vec![WHITE; (lw as usize) * (lh as usize)],
            jobs: std::env::var("RM_PANEL_JOBS").is_ok(),
            vis: std::env::var("RM_VIS").map(|v| v != "0").unwrap_or(false),
            pending: Some(pending),
            back_rx: Some(back_rx),
            pool: Vec::new(),
            touch: None,
            mono: false,
            rec,
        })
    }

    /// 无头帧缓冲：本机渲染界面用，几何与真机一致。
    pub fn open_mem() -> Fb {
        let (lw, lh) = logical();
        Fb {
            plane: vec![0u16; (lw as usize) * (lh as usize)],
            width: lw,
            height: lh,
            tx: None,
            worker: None,
            levels: Vec::new(),
            pushed: vec![0u16; (lw as usize) * (lh as usize)],
            jobs: std::env::var("RM_PANEL_JOBS").is_ok(),
            vis: std::env::var("RM_VIS").map(|v| v != "0").unwrap_or(false),
            pending: None,
            back_rx: None,
            pool: Vec::new(),
            touch: None,
            mono: false,
            rec: None,
        }
    }

    /// 开/关二值绘制。开关只在调用方那一段生效（画完自己关掉），不要常开。
    ///
    /// 开着的时候 `put` / `fill_rect` 的颜色都先过一遍 `mono565`，文字
    /// （`ui::text`）改成按覆盖度 50% 阈值化的 **1-bit** 画法。给"要用 A2
    /// 局部刷新"的界面用：A2 对中间灰一个驱动码都没有，灰阶内容留在那里
    /// 只会变成残影。`blit_gray`（贴图）和 `dim`（压暗）不吃这个开关。
    pub fn set_mono(&mut self, on: bool) {
        self.mono = on;
    }

    /// 当前是不是二值绘制。文字渲染要按它切 1-bit 路径。
    #[inline]
    pub fn mono(&self) -> bool {
        self.mono
    }

    /// 横屏往哪边转（`left = true` → 逆时针，充电口在左）。返回 `true` = 变了。
    ///
    /// 只改上屏的映射，不改逻辑布局；变了下一次 `refresh` 会把整屏重推一遍，
    /// 所以调用方**必须重画**（不然屏上是错的）。
    pub fn set_land_dir_left(&mut self, left: bool) -> bool {
        let cw = !left;
        if land_dir_cw() == cw {
            return false;
        }
        LAND_CW.store(cw, Ordering::Relaxed);
        if let Some(t) = self.touch {
            let _ = t;
        }
        self.touch = Some((0, 0, self.width as usize, self.height as usize));
        eprintln!("[fb] 横屏方向 = {}", if left { "左" } else { "右" });
        true
    }

    /// 切横屏 / 竖屏。返回 `true` = 真的换了方向（调用方要整屏重画）。
    ///
    /// 换方向等于换了一整块画布：`plane` / `pushed` 的尺寸和 stride 都变了，
    /// 旧像素没有意义（也没法一一对应），所以两块都重新分配成白板，
    /// `touch` 记成整屏 —— 紧接着的那次 `refresh` 会把整屏重推一遍。
    ///
    /// 代价：两次 5.2 MB 的分配 + 清零，实测几毫秒，只在切方向时发生。
    pub fn set_landscape(&mut self, on: bool) -> bool {
        if landscape() == on {
            return false;
        }
        LANDSCAPE.store(on, Ordering::Relaxed);
        let (w, h) = logical();
        self.width = w;
        self.height = h;
        let n = (w as usize) * (h as usize);
        self.plane = vec![WHITE; n];
        self.pushed = vec![WHITE; n];
        self.touch = Some((0, 0, w as usize, h as usize));
        eprintln!(
            "[fb] 切到{}：逻辑 {}×{} plane={} pushed={}",
            if on { "横屏" } else { "竖屏" },
            w,
            h,
            self.plane.len(),
            self.pushed.len()
        );
        true
    }

    /// 把当前帧缓冲存成 8 位灰度 PGM（P5）。
    pub fn dump_pgm(&self, path: &str) -> io::Result<()> {
        let mut out = Vec::with_capacity(W * H + 32);
        out.extend_from_slice(format!("P5\n{} {}\n255\n", self.width, self.height).as_bytes());
        for y in 0..self.height as usize {
            for x in 0..self.width as usize {
                out.push(self.get(x as i32, y as i32));
            }
        }
        std::fs::write(path, out)
    }

    #[inline]
    fn idx(&self, x: i32, y: i32) -> Option<usize> {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            None
        } else {
            Some(y as usize * self.width as usize + x as usize)
        }
    }

    /// `plane` 相对 `pushed` 真正变了的区域，**按 tile 收拢**成一串矩形
    /// `(x, y, w, h)`。
    ///
    /// 以前这里返回一个**包围盒**。包围盒的致命毛病是「远程 union」：两块
    /// 相距很远的改动（左上角工具条 + 右下角当场写的字）会被并成一个横跨
    /// 整屏的大盒子。而 1404×1872 的 GC16 是 46 帧 / 540 ms —— 手写完全插
    /// 不进去。
    ///
    /// 日志实证（改之前）：
    /// * app 只请求了 `120×480@0,120` 和 `400×1080@120,120`（并起来 520×1080），
    ///   驱动却发出 `1296×1777@0,79 Gc16` —— 46 帧跑了 **823 ms**，帧周期 17.5。
    /// * 一条笔迹的 12 个 A2 小块（每个 12 相位）跑了 **119 帧 / 1417 ms**，
    ///   其中「显示 1169 ms」全是在排队等面板帧，编码只花了 215 ms。
    ///
    /// 现在按 `TW×TH` 扫，同一行带里**连续的**脏 tile 才收成一条，中间空着
    /// 的列不算钱；`merge_rects` 再把上下相邻的接上。局部改动只剩局部矩形。
    fn diff_pushed_tiles(&self, region: Option<Rect>) -> Vec<Rect> {
        let w = self.width as usize;
        let h = self.height as usize;
        if self.pushed.len() != w * h {
            return Vec::new();
        }
        // 只扫 `region`（app 这次改过的那块）。`None` = 整屏兜底。
        let (bx0, by0, bx1, by1) = match region {
            Some((x, y, rw, rh)) => (
                x.min(w),
                y.min(h),
                (x + rw).min(w),
                (y + rh).min(h),
            ),
            None => (0, 0, w, h),
        };
        if bx0 >= bx1 || by0 >= by1 {
            return Vec::new();
        }
        let tx0 = bx0 / TW;
        let tx1 = (bx1 + TW - 1) / TW;
        let ty0 = by0 / TH;
        let ty1 = (by1 + TH - 1) / TH;
        let ntx = tx1 - tx0;
        let mut out: Vec<Rect> = Vec::new();
        let mut hit = vec![false; ntx.max(1)];
        for ty in ty0..ty1 {
            let y0 = ty * TH;
            let y1 = (y0 + TH).min(h);
            hit.fill(false);
            for tx in tx0..tx1 {
                let s = tx * TW;
                let e = (s + TW).min(w);
                let n = e - s;
                for y in y0..y1 {
                    let base = y * w + s;
                    if bytes(&self.plane[base..base + n]) != bytes(&self.pushed[base..base + n]) {
                        hit[tx - tx0] = true;
                        break;
                    }
                }
            }
            let mut i = 0;
            while i < ntx {
                if !hit[i] {
                    i += 1;
                    continue;
                }
                let s = i;
                while i < ntx && hit[i] {
                    i += 1;
                }
                let x0 = (tx0 + s) * TW;
                let x1 = ((tx0 + i) * TW).min(w);
                out.push((x0, y0, x1 - x0, y1 - y0));
            }
        }
        // 碎得过分就退化成一个大包围盒。`merge_rects` 是 O(n²) 且每次合并
        // 都重头重扫，几十条以上它自己的开销就超过省下来的像素了；而且碎片
        // 这么多本来也说明整屏在动，整块刷不吃亏。
        if out.len() > 48 {
            if let Some(b) = out.iter().copied().reduce(union) {
                return vec![b];
            }
        }
        out
    }

    /// 把这块矩形的内容记为"已经交给面板线程了"。
    fn mark_pushed(&mut self, x: usize, y: usize, w: usize, h: usize) {
        let sw = self.width as usize;
        let sh = self.height as usize;
        if self.pushed.len() != sw * sh {
            self.pushed = self.plane.clone();
            return;
        }
        let x1 = (x + w).min(sw);
        let y1 = (y + h).min(sh);
        for yy in y..y1 {
            let base = yy * sw;
            let src = &self.plane[base + x..base + x1];
            self.pushed[base + x..base + x1].copy_from_slice(src);
        }
    }

    pub fn put(&mut self, x: i32, y: i32, color: u16) {
        // 二值模式（`set_mono`）：颜色先压成纯黑白。分支几乎不可预测地错，
        // 但只在那一屏发生，正常路径只是多读一个 bool。
        let color = if self.mono { mono565(color) } else { color };
        if let Some(i) = self.idx(x, y) {
            // 值没变就别记脏。浮层/图标的重绘里绝大多数像素是原地不动重写
            // 一遍，无脑记脏会把 `touch` 撑到整屏，差分和灰度都跟着白跑。
            if self.plane[i] == color {
                return;
            }
            self.plane[i] = color;
            let (ux, uy) = (x as usize, y as usize);
            let t = self.touch.get_or_insert((ux, uy, 0, 0));
            let nx0 = t.0.min(ux);
            let ny0 = t.1.min(uy);
            let nx1 = (t.0 + t.2).max(ux + 1);
            let ny1 = (t.1 + t.3).max(uy + 1);
            *t = (nx0, ny0, nx1 - nx0, ny1 - ny0);
        }
    }

    /// 写一个像素但**不**记进脏区。只给调试边框（`paint_vis`）用 —— 否则
    /// 它自己就会把差分搞脏，逼着下一次 refresh 多扫一遍。
    fn put_raw(&mut self, x: i32, y: i32, color: u16) {
        if let Some(i) = self.idx(x, y) {
            self.plane[i] = color;
        }
    }

    /// 把 `(x0, y0)`–`(x1, y1)`（半开区间）并进脏区包围盒。
    #[inline]
    fn mark_dirty(&mut self, x0: usize, y0: usize, x1: usize, y1: usize) {
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let r = (x0, y0, x1 - x0, y1 - y0);
        self.touch = Some(match self.touch {
            Some(o) => union(o, r),
            None => r,
        });
    }

    /// 拿一行里连续 `w` 个像素的可写切片（整块贴图用，调用方自己夹好范围）。
    pub fn row_mut(&mut self, y: i32, x: i32, w: i32) -> Option<&mut [u16]> {
        if y < 0 || y >= self.height || x < 0 || x >= self.width || w <= 0 {
            return None;
        }
        let n = (w.min(self.width - x)) as usize;
        if n == 0 {
            return None;
        }
        self.mark_dirty(x as usize, y as usize, x as usize + n, y as usize + 1);
        let base = y as usize * self.width as usize + x as usize;
        Some(&mut self.plane[base..base + n])
    }

    /// 把一块灰度位图（`w×h`，行优先，0=黑 255=白）整块贴到 `(x,y)`。
    pub fn blit_gray(&mut self, x: i32, y: i32, w: i32, h: i32, src: &[u8]) {
        if w <= 0 || h <= 0 {
            return;
        }
        for row in 0..h {
            let yy = y + row;
            let Some(dst) = self.row_mut(yy, x, w) else { continue };
            let base = (row * w) as usize;
            let n = dst.len().min(src.len().saturating_sub(base));
            for (i, d) in dst[..n].iter_mut().enumerate() {
                *d = GRAY565[src[base + i] as usize];
            }
        }
    }

    /// 读回一个像素的灰度（0..255）。只给"把字混到已有背景上"这种场合用。
    pub fn get(&self, x: i32, y: i32) -> u8 {
        let Some(i) = self.idx(x, y) else { return 255 };
        let c = self.plane[i];
        let g6 = ((c >> 5) & 0x3f) as u32;
        ((g6 * 255 + 31) / 63) as u8
    }

    /// 填充矩形。
    ///
    /// 用 `i64` 算边界再裁剪：UI 那边偶尔会传来越界的矩形（滚动到边界时
    /// `y+h` 能把 i32 撑爆），以前这里直接 panic，整个进程就没了、屏停在
    /// 最后一帧，看着像"卡死"。驱动层不该因为上层算错一个矩形就崩。
    pub fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u16) {
        // 二值模式（`set_mono`）：灰底一律压成黑或白。灰底输入框在 A2 区域里
        // 本来就显示不出来，提前压掉还能少记一次脏区。
        let color = if self.mono { mono565(color) } else { color };
        let (x0, x1, y0, y1) = self.clip(x, y, w, h);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let stride = self.width as usize;
        for yy in y0..y1 {
            let base = yy * stride;
            // 整行已经是目标色就跳过。浮层背景经常大面积重填同一个颜色，
            // 无脑记脏会把 `touch` 撑到整屏 —— 于是每次落笔的差分都要重扫
            // 10.4 MB（实测 `[pf] refresh 11×11 … 差分 27349`），比真笔迹
            // 贵几千倍。
            if self.plane[base + x0..base + x1].iter().all(|&v| v == color) {
                continue;
            }
            self.mark_dirty(x0, yy, x1, yy + 1);
            self.plane[base + x0..base + x1].fill(color);
        }
    }

    /// 把调用方给的矩形裁成屏内矩形 `(x, y, w, h)`。
    ///
    /// **单独抽出来是为了能被测试打到。** `clip()` 的返回顺序是
    /// `(x0, x1, y0, y1)`，和它并列的 `fill_rect` / `dim` 都按这个顺序解构；
    /// `refresh` 里一度写成 `(x0, y0, x1, y1)`，于是 `y0` 拿到 x 的右边界、
    /// `x1` 拿到 y 的上边界，`w`/`h` 变成 `y1-x0` 和 `y0-x1` 那种跨屏的
    /// 大方块 —— 一个 `11×11` 的笔迹会被撑成 `740×740`，然后
    /// `mark_pushed` 的切片起点超过终点，直接 panic 回库界面。
    ///
    /// 而且它是**间歇性**的：`x = 0, w = 120` 时 `x1` 恰好等于 `y`，错位后
    /// 结果碰巧正确，所以大部分工具栏刷新看起来没毛病，只有离原点越远的
    /// 笔迹才暴露 —— 这正是"角落特别不正常"的来源。
    fn region(&self, x: i32, y: i32, w: i32, h: i32, mode: u32) -> Option<Rect> {
        if mode == UPDATE_MODE_FULL {
            return Some((0, 0, self.width as usize, self.height as usize));
        }
        let (x0, x1, y0, y1) = self.clip(x, y, w, h);
        if x0 >= x1 || y0 >= y1 {
            return None;
        }
        Some((x0, y0, x1 - x0, y1 - y0))
    }

    /// 把一个矩形裁进画布，返回 `(x0, x1, y0, y1)`（半开区间）。
    fn clip(&self, x: i32, y: i32, w: i32, h: i32) -> (usize, usize, usize, usize) {
        let (cw, ch) = (self.width as i64, self.height as i64);
        let x1 = (x as i64 + w as i64).clamp(0, cw);
        let y1 = (y as i64 + h as i64).clamp(0, ch);
        let x0 = (x as i64).clamp(0, cw).min(x1);
        let y0 = (y as i64).clamp(0, ch).min(y1);
        (x0 as usize, x1 as usize, y0 as usize, y1 as usize)
    }

    /// 把一块区域按比例混向某个颜色（弹窗背景压暗用）。
    pub fn dim(&mut self, x: i32, y: i32, w: i32, h: i32, target: u16, num: u32, den: u32) {
        let (x0, x1, y0, y1) = self.clip(x, y, w, h);
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        let (tr, tg, tb) = (
            ((target >> 11) & 0x1f) as i32,
            ((target >> 5) & 0x3f) as i32,
            (target & 0x1f) as i32,
        );
        let (k, d) = (num as i32, den as i32);
        self.mark_dirty(x0, y0, x1, y1);
        let stride = self.width as usize;
        for yy in y0..y1 {
            for p in &mut self.plane[yy * stride + x0..yy * stride + x1] {
                let c = *p;
                let r = ((c >> 11) & 0x1f) as i32;
                let g = ((c >> 5) & 0x3f) as i32;
                let b = (c & 0x1f) as i32;
                let nr = r + (tr - r) * k / d;
                let ng = g + (tg - g) * k / d;
                let nb = b + (tb - b) * k / d;
                *p = ((nr as u16) << 11) | ((ng as u16) << 5) | (nb as u16);
            }
        }
    }

    pub fn clear(&mut self, color: u16) {
        self.mark_dirty(0, 0, self.width as usize, self.height as usize);
        self.plane.fill(color);
    }

    /// 把一块区域推到面板。
    ///
    /// `waveform` 是 rmdev 一直在用的那几个常量（DU/GC16/GL16）；
    /// `mode` 是 PARTIAL/FULL，FULL 表示"整屏重刷"（清残影）。
    #[track_caller]
    pub fn refresh(&mut self, x: i32, y: i32, w: i32, h: i32, waveform: u32, mode: u32) {
        if self.tx.is_none() {
            return;
        }
        if self.jobs {
            let at = std::panic::Location::caller();
            eprintln!(
                "[req] {}×{}@{},{} wf={waveform} mode={mode} <- {}:{}",
                w,
                h,
                x,
                y,
                at.file(),
                at.line()
            );
        }
        let t_all = std::time::Instant::now();
        let Some((x, y, w, h)) = self.region(x, y, w, h, mode) else {
            return;
        };
        // **不只信调用方给的矩形**：并上「自上次刷新以来真正画过的像素」。
        //
        // 这是为了让 app 完全不用改（详见 `pushed` 字段的注释）：app 的脏矩形
        // 是给 rm2fb 那种"整屏全量推"的 server 写的，它从不给 `y < 120` 的
        // 区域发更新、菜单的重绘矩形也和真正画的地方错位。只按矩形推，屏上
        // 就是"菜单缺半边、角落写不上"。
        //
        // **但绝不能把它们并成一个包围盒**。真正变的那几块和调用方请求的那块
        // 通常是分开的，一并就成了横跨整屏的大矩形（见 `diff_pushed_tiles`
        // 的注释）。这里只在"合起来几乎不多花面积"时才合，否则各走各的 job：
        // 面板侧按 cell 记相位、按 cell 记波形，本来就能并行。
        let mut rs: Vec<Rect> = vec![(x, y, w, h)];
        // 请求本身已经盖住大半个屏时不用差分：那块马上会被整块 `mark_pushed`，
        // 差分出来的东西也只会被 `merge_rects` 并回去，白扫一遍 5.3 MB。
        let t0 = std::time::Instant::now();
        let area = (self.width as usize) * (self.height as usize);
        // app 这次改过的范围。`refresh` 是唯一消费点，take 出来就清空。
        let touched = self.touch.take();
        let ntile = if mode != UPDATE_MODE_FULL && w * h * 4 < area * 3 {
            let t = self.diff_pushed_tiles(touched);
            let n = t.len();
            rs.extend(t);
            n
        } else {
            0
        };
        let t_diff = t0.elapsed();
        let t1 = std::time::Instant::now();
        let rects = merge_rects(&rs);
        let t_merge = t1.elapsed();
        if self.jobs && (t_diff.as_micros() > 500 || t_merge.as_micros() > 500) {
            eprintln!(
                "[pf] {}×{}@{} 差分 {}us（{} 条 → {} 块）合并 {}us",
                w, h, x, t_diff.as_micros(), ntile, rects.len(), t_merge.as_micros()
            );
        }
        let nrect = rects.len();
        let t_vis = std::time::Instant::now();
        if self.vis {
            for (i, r) in rects.iter().enumerate() {
                self.paint_vis(*r, i);
            }
        }
        let d_vis = t_vis.elapsed();
        let t_push = std::time::Instant::now();
        for r in rects {
            self.push_rect(r, waveform, mode);
        }
        let d_push = t_push.elapsed();
        let d_all = t_all.elapsed();
        if self.jobs && d_all.as_micros() > 800 {
            eprintln!(
                "[pf] refresh {}×{}@{} 总 {}us = 差分 {} + 合并 {} + {}块描边 {} + 推送 {}",
                w,
                h,
                x,
                d_all.as_micros(),
                t_diff.as_micros(),
                t_merge.as_micros(),
                nrect,
                d_vis.as_micros(),
                d_push.as_micros()
            );
        }
    }

    /// 把一块矩形的边框画到帧缓冲上（`RM_VIS=1`），用来肉眼确认"这次到底
    /// 刷了哪几块"。同一次 refresh 里第 1/2/3+ 块分别是黑 / 深灰 / 浅灰。
    ///
    /// 边框像素立刻记成"已推"，否则下一次差分又会把它们当成新变化。
    fn paint_vis(&mut self, (x, y, w, h): Rect, idx: usize) {
        if w == 0 || h == 0 {
            return;
        }
        let g = match idx {
            0 => 0u8,
            1 => 6,
            _ => 11,
        };
        let c = gray565(g);
        let x0 = x as i32;
        let y0 = y as i32;
        let x1 = (x + w) as i32 - 1;
        let y1 = (y + h) as i32 - 1;
        for b in 0..2i32 {
            for xx in x0..=x1 {
                self.put_raw(xx, y0 + b, c);
                self.put_raw(xx, y1 - b, c);
            }
            for yy in y0..=y1 {
                self.put_raw(x0 + b, yy, c);
                self.put_raw(x1 - b, yy, c);
            }
        }
        // **只记边框**，不能 `mark_pushed(x, y, w, h)` 整个矩形。
        //
        // 那样子会把"根本没发出去的内容"也标成已推：`paint_vis` 跑在
        // `push_rect` 之前，一旦后者因为越界提前 return，这块区域在
        // `pushed` 里已经等于 `plane`，脏区差分再也看不见它 —— 屏上就是
        // 「框一圈圈缩小、最后彻底不显示，按一下菜单才被大刷新带出来」。
        self.mark_pushed(x, y, w, 2);
        self.mark_pushed(x, (y + h).saturating_sub(2), w, 2);
        self.mark_pushed(x, y, 2, h);
        self.mark_pushed((x + w).saturating_sub(2), y, 2, h);
    }

    /// 把一块矩形转成列主序灰度，投递给面板线程，再记成"已推"。
    /// 等面板线程把手上的活干完（最多 `max`）。
    ///
    /// **睡眠/挂起前必须调**：挂起那一刻面板线程要是正卡在波形中间，
    /// 那一帧的相位就永远停在原地，醒来后屏上会留一块半驱动的灰。
    /// 返回 `true` = 已经空闲。
    pub fn wait_idle(&self, max: std::time::Duration) -> bool {
        use std::sync::atomic::Ordering;
        let Some(p) = self.pending.as_ref() else {
            return true;
        };
        let t0 = std::time::Instant::now();
        while p.load(Ordering::Relaxed) > 0 {
            if t0.elapsed() >= max {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        true
    }

    fn push_rect(&mut self, r: Rect, waveform: u32, mode: u32) {
        let (x, y, w, h) = r;
        // 再夹一次：`clip()` 之后理论上已经在屏内，但 `refresh` 是 app 直接
        // 调的窄接口，参数写歪一次就是整机 panic（panic 在 UI 线程 = 进程没了）。
        let wu = w.min((self.width as usize).saturating_sub(x));
        let hu = h.min((self.height as usize).saturating_sub(y));
        if wu == 0 || hu == 0 {
            return;
        }
        let stride = self.width as usize;
        // 横屏：投递前把**逻辑**矩形顺时针转 90° 成**物理**矩形（宽高对调）。
        // 逻辑内容是 `wu×hu`，转完在物理屏上是 `hu×wu`，位置见 `rect_to_phys`。
        let land = landscape();
        // 逻辑 (x,y,wu,hu) → 物理 (px,py,pw,ph)：顺时针 90°，宽高对调
        let (px, py, pw, ph) = if land {
            rot_rect(
                (x, y, wu, hu),
                (self.width as usize, self.height as usize),
                land_dir_cw(),
            )
        } else {
            (x, y, wu, hu)
        };
        let (pw, ph) = (
            pw.min(W.saturating_sub(px)),
            ph.min(H.saturating_sub(py)),
        );
        if pw == 0 || ph == 0 {
            return;
        }

        // RGB565 → 4-bit 灰度，**并且转成列主序**（`levels[col*hu + row]`）。
        //
        // 这一步必须在 UI 线程做：它是"投递那一刻"的像素快照。
        //
        // 为什么在这里转置：面板线程是 SCHED_FIFO 实时线程，一个相位的预算
        // 只有 11.93 ms（一个面板帧），而 `img`/`prv`/`kp` 都是列主序的
        // （`x*1872 + y`）。把行主序的 `levels` 摊进列主序，以前在面板线程里
        // 做，实测整屏 **46.5 ms**（17 ns/像素）—— 一个相位就吃掉 4 帧，
        // 波形被拉长 4 倍，屏上就是一路闪。放到 UI 线程这边，它和面板线程的
        // vsync 等待天然并行（i.MX7D 是双核 A7），而且这里读的 `plane`
        // 是刚被 `blit_gray` 写热的。
        //
        // 用 64×64 分块：块内两侧都是完整的 cache line，整屏 2.6 Mpx 大约
        // 十几毫秒，一次 update 一次，不进帧预算。
        let tg = std::time::Instant::now();
        // 先把面板线程还回来的缓冲收进池里
        let mut got: Vec<Vec<u8>> = Vec::new();
        if let Some(rx) = self.back_rx.as_ref() {
            while let Ok(v) = rx.try_recv() {
                got.push(v);
            }
        }
        if !got.is_empty() {
            self.pool.append(&mut got);
            // 池子不用太大：UI 线程同时最多压着几块
            self.pool.truncate(4);
        }
        // 复用同一块内存。**不要再 move 进 channel 然后重新分配** —— 新页
        // 第一次写要缺页，整屏 2.6 MB 实测多出 146 ms。
        let mut levels = self.pool.pop().unwrap_or_default();
        levels.clear();
        levels.resize(pw * ph, 0);
        if land {
            // 逻辑行 → 物理列，顺带把画面转 90°（见 `gray_to_cols_rot`）
            gray_to_cols_rot(
                &self.plane,
                stride,
                x,
                y,
                wu,
                hu,
                &mut levels,
                land_dir_cw(),
            );
        } else {
            gray_to_cols(&self.plane, stride, x, y, wu, hu, &mut levels);
        }

        // 波形选择（照官方 swtcon 的分工）：
        // * DU：手写/局部二值
        // * GC16：整屏重绘，会整块闪黑再回白——专门用来清残影
        // * GL16：和 GC16 同样的驱动序列，但不变像素完全不驱动——局部刷新不闪
        let m = match waveform {
            WAVEFORM_MODE_DU => Mode::Du,
            WAVEFORM_MODE_A2 => Mode::A2,
            WAVEFORM_MODE_DU4 => Mode::Du4,
            WAVEFORM_MODE_GC16 => Mode::Gc16,
            WAVEFORM_MODE_GL16 => Mode::Gl16,
            // 没指定/没接的：部分更新用 GL16（不变像素不驱动），整块重刷用 GC16。
            _ if mode == UPDATE_MODE_FULL => Mode::Gc16,
            _ => Mode::Gl16,
        };

        // 二值波形（DU / A2 / DU4）**不量化**，和官方语义一致。
        //
        // 实测这张波形表（`320_R355_AF0A11`，band 7）里，除了
        // `DU 黑←白` / `DU 白←黑` / `DU 黑←中灰` 这几条，**目标为中间灰的
        // 组合全是 0 码** —— 也就是"二值波形遇到中间灰一帧都不驱，像素保持
        // 原状"。这是设计好的：DU 只翻极性，灰阶归 GC16/GL16 管。
        //
        // 这里以前加过"把整个矩形的中间灰量化成 0/15"，想让黑笔的羽化边也能
        // 被 DU 推出来。后果很糟：一条橡皮/手写的 DU 矩形会把**里面别人的**
        // 笔迹羽化边一起翻成纯白或纯黑 —— 屏上就是"擦一笔冒出一堆小黑方块，
        // 还把周围的墨啃掉一块"，工具条里带灰阶的 pencil 图标也跟着变样。
        //
        // 正确做法就是**别碰**：`set_rect` 存真实灰阶，DU 表自然给中间灰全 0。
        // 羽化边会比"量化过"的样子细 1 px，那正是官方驱动的样子。

        // 录一段再投递：重放时按同样的时间轴重新投，走的是同一条路径。
        //
        // 录像文件里存的是**行主序**（和 `blit_gray` 同一个约定），所以这里
        // 要把刚转好的列主序再摊回去。只有开了录像才会走这条路。
        if self.rec.is_some() {
            let mut row = vec![0u8; wu * hu];
            if land {
                // 横屏：`levels` 是转完的列主序，按逻辑行摊回去
                for j in 0..hu {
                    let base = (hu - 1 - j) * wu;
                    row[j * wu..j * wu + wu].copy_from_slice(&levels[base..base + wu]);
                }
            } else {
                for col in 0..wu {
                    let src = &levels[col * hu..col * hu + hu];
                    for (row_, &v) in src.iter().enumerate() {
                        row[row_ * wu + col] = v;
                    }
                }
            }
            if let Some(r) = self.rec.as_mut() {
                r.push(x, y, wu, hu, waveform, mode, &row);
            }
        }

        let t_gray = tg.elapsed();
        // 这块的内容马上要交给面板线程了，先把 `pushed` 对上，下一次差分
        // 才不会又把同一块算进来。
        let ts2 = std::time::Instant::now();
        let job = Job {
            x: px,
            y: py,
            w: pw,
            h: ph,
            levels,
            mode: m,
            wf: waveform,
            t: now_ms(),
            narrow: mode != UPDATE_MODE_FULL,
        };
        let res = match self.tx.as_ref() {
            Some(tx) => tx.try_send(job),
            None => return,
        };
        let t_send = ts2.elapsed();
        // 只有真的投进去了才记"已推"。队列满（面板跟不上）就把缓冲收回来，
        // 那块下次差分还会抓到 —— 宁可晚一帧，也不能把没发出去的内容
        // 标成已推（那会永久缺失，得等一次大刷新才出来）。
        match res {
            Ok(()) => {
                if let Some(p) = &self.pending {
                    p.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                let tm = std::time::Instant::now();
                self.mark_pushed(x, y, wu, hu);
                let t_mark = tm.elapsed();
                if self.jobs && t_gray.as_micros() + t_mark.as_micros() > 300 {
                    eprintln!(
                        "[pf] push {}×{}@{},{} 灰度 {}us 投递 {}us 记账 {}us",
                        pw,
                        ph,
                        px,
                        py,
                        t_gray.as_micros(),
                        t_send.as_micros(),
                        t_mark.as_micros()
                    );
                }
            }
            Err(std::sync::mpsc::TrySendError::Full(job)) => {
                // 队列满 = 面板线程还在追。**把这块重新标回脏区**，不然
                // `refresh` 已经把 `touch` take 走了，这块就再也没人记得，
                // 屏上会一直缺到下一次大刷新。
                self.mark_dirty(x, y, x + wu, y + hu);
                if self.pool.len() < 4 {
                    self.pool.push(job.levels);
                }
            }
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {}
        }
    }
}

impl Drop for Fb {
    fn drop(&mut self) {
        // 丢掉投递端 → 工作线程收到 Disconnected → 补 settle、写空白帧、落状态。
        self.tx.take();
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

type Rect = (usize, usize, usize, usize);

/// 脏区扫描的 tile 尺寸。128×64 是折中：太小会让一条斜线碎成几十个 job
/// （每个 job 都有一次 `gray_to_cols` + 一次 send），太大又会把不相关的
/// 改动重新粘成一条。
const TW: usize = 128;
const TH: usize = 64;

/// `&[u16]` 的字节视图。**只为了比较**：Rust 对 `&[u8]` 的 `==` 会下沉到
/// `bcmp`，对 `&[u16]` 则是逐元素循环 —— 后者慢 20~50 倍，而脏区差分每次
/// `refresh` 都要把整屏扫一遍。
#[inline]
fn bytes(v: &[u16]) -> &[u8] {
    // SAFETY: `u16` 的每一个字节都是有效的 `u8`；`v.len() * 2` 不会超过
    // 原切片的分配（切片内存连续且长度已由 `from_raw_parts` 的契约保证）。
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast::<u8>(), v.len() * 2) }
}

/// 把一串矩形**贪心**合并，只在"合起来几乎不多花面积"时才合。
///
/// 判据是 `union 后面积 ≤ 两者之和 × 1.25`：上下相邻的两条横带（面积刚好
/// 相等）会并成一块，而「左上角工具条」和「右下角笔迹」并起来要多出几倍
/// 面积，就保持分开 —— 这样它们仍然是两个独立 job，A2 的小笔迹不会被
/// GC16 的大矩形吞掉。
fn merge_rects(rs: &[Rect]) -> Vec<Rect> {
    let mut out: Vec<Rect> = Vec::with_capacity(rs.len());
    for &r in rs {
        if r.2 == 0 || r.3 == 0 {
            continue;
        }
        let mut cur = r;
        let mut i = 0;
        while i < out.len() {
            let m = union(out[i], cur);
            let a = (out[i].2 as u64 * out[i].3 as u64) + (cur.2 as u64 * cur.3 as u64);
            let b = m.2 as u64 * m.3 as u64;
            if b * 4 <= a * 5 {
                cur = m;
                out.swap_remove(i);
                i = 0; // 合完可能又能和别的合上，重头再来
            } else {
                i += 1;
            }
        }
        out.push(cur);
    }
    out
}

fn union(a: Rect, b: Rect) -> Rect {
    let x0 = a.0.min(b.0);
    let y0 = a.1.min(b.1);
    let x1 = (a.0 + a.2).max(b.0 + b.2);
    let y1 = (a.1 + a.3).max(b.1 + b.3);
    (x0, y0, x1 - x0, y1 - y0)
}

/// `a - b`（两个矩形的差），拆成最多 4 个矩形。
///
/// 用途：一轮波形结束时，`frozen - live` 里的像素才算"被推满了一整轮"，
/// 可以提交；`live` 里的（这一轮中途才落下的墨）没推满，留到下一轮。
#[allow(dead_code)]
fn subtract(a: Rect, b: Rect) -> Vec<Rect> {
    let (ax0, ay0) = (a.0 as i64, a.1 as i64);
    let (ax1, ay1) = (ax0 + a.2 as i64, ay0 + a.3 as i64);
    let (bx0, by0) = (b.0 as i64, b.1 as i64);
    let (bx1, by1) = (bx0 + b.2 as i64, by0 + b.3 as i64);
    if bx1 <= ax0 || bx0 >= ax1 || by1 <= ay0 || by0 >= ay1 {
        return vec![a];
    }
    let mut out = Vec::with_capacity(4);
    let mut push = |x0: i64, y0: i64, x1: i64, y1: i64| {
        if x1 > x0 && y1 > y0 {
            out.push((x0 as usize, y0 as usize, (x1 - x0) as usize, (y1 - y0) as usize));
        }
    };
    // 上 / 下 / 左 / 右
    push(ax0, ay0, ax1, by0.min(ay1));
    push(ax0, by1.max(ay0), ax1, ay1);
    let (iy0, iy1) = (by0.max(ay0), by1.min(ay1));
    push(ax0, iy0, bx0.min(ax1), iy1);
    push(bx1.max(ax0), iy0, ax1, iy1);
    out
}

/// 面板工作线程：收刷新、合并、播相位；空闲时把"欠账"补成满幅。
///
/// 合并的依据是 `img` 本身就是累加缓冲：每个 Job 带的是它那块矩形的**绝对**
/// 灰度快照，按到达顺序 `set_rect` 之后，`img` 里就是最新的整幅目标图，
/// 所以可以用一个并集矩形只播一次波形。手写时一批事件会合并成一帧，
/// 连点菜单也只会播一次。
/// 面板线程：忠实复刻官方 swtcon 的语义。
///
/// 规则只有一条——**一次 update 从相位 0 开始播，一帧一个相位**：
///
/// * 播到一半来了新 update：把新目标合进 `img`，换它的矩形/波形，相位归 0 重播；
/// * 没人打断：一直播到波形结束（DU 25 帧 / GC16 46 帧 / A2 12 帧）。
///
/// 手写时 update 每 ~24 ms 来一次，每次都只活 1~2 帧 —— 墨靠**次数**累积；
/// 抬笔后没人打断，最后一个 update 播满，自然就"定影"成黑。没有 settle、
/// 没有滑动窗口、没有波形替换：这些都是驱动不该有的发明。
fn framegen(
    mut panel: Panel,
    rx: std::sync::mpsc::Receiver<Job>,
    back_tx: std::sync::mpsc::Sender<Vec<u8>>,
    pending: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) {
    let timing = std::env::var("RM_PANEL_TIMING").map(|v| v != "0").unwrap_or(false);
    // 这一轮一共从 channel 里取走了几条 job。轮末把它们从 `pending` 里减掉，
    // 减的是"取走"而不是"驱完"——取走时它已经不在队列里了，`wait_idle`
    // 要等的是**面板线程手上这一轮做完**，所以放在轮末减。
    let mut consumed = 0usize;

    // 喂帧线程必须是实时的。官方 swtcon 的 framegen / vsync-flip 就是
    // SCHED_FIFO 98/99：手写时 UI 线程在画、在重排，普通优先级会被饿死，
    // 一帧就从 12 ms 变成几十甚至几百毫秒（实测过 359 ms），笔就"写不出来"。
    // 只在 Linux 上做：宿主（macOS）上跑 lib 单测时没有这个调用。
    #[cfg(target_os = "linux")]
    unsafe {
        let param = libc::sched_param { sched_priority: 90 };
        if libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) != 0 {
            eprintln!(
                "[panel] 设 SCHED_FIFO 失败：{}（喂帧可能被 UI 线程抢 CPU）",
                std::io::Error::last_os_error()
            );
        }
    }

    loop {
        let first = match rx.recv() {
            Ok(j) => {
                consumed += 1;
                j
            }
            Err(_) => {
                let _ = panel.blank();
                panel.flush_state();
                return;
            }
        };

        // 驱动窗口：**最近 nph 帧里真正变过的像素**的矩形（滑动窗口）。
        //
        // 这是「一个像素必须连续吃满一整轮波形」的精确实现。相位是
        // `cur - phc[cell]`（`set_rect` 盖戳那一帧为 0），而 e-ink 的波形是
        // 一张**按帧索引**的表：某个 cell 只要漏掉一帧没驱，它的相位就跳了，
        // 后面每一帧用的都是错的驱动码 —— 屏上就是"笔迹先碎成一片格子的
        // 连线，再慢慢变正常"。所以规则只有一条：
        //
        //   **盖过相位戳的 cell，必须在接下来的 nph 帧里连续被驱满。**
        //
        // 实现方式就是窗口：窗口 = 最近 nph 帧里盖过戳的像素的并集，驱动矩形
        // 直接取窗口并集，**不再和别的矩形求交集**。以前这里是
        // `窗口并集 ∩ changed()`，`changed()` 是"这一轮变过的包围盒"，只要
        // 某块 cell 落在包围盒外面（橡皮拖着走、工具条重绘）它就从驱动矩形里
        // 掉出去，相位于是永远停在中途。
        //
        // 「以前是整轮推满才提交」那条规则在手写/方块走位下**永远等不到**：
        // 实测一次 DU 刷新跑了 350 帧 / 4.1 s（本该 25 帧 / 300 ms）。官方是
        // 每个 `WorkItem` 各带 `phase`（`display.cpp:748`
        // `new_phase = item->phase + frame_count`），新区域从 0 起算、老区域
        // 继续数自己的相位，互不重置。`Panel::phc` 就是按 cell 记这个相位。
        let mut win: std::collections::VecDeque<(usize, Rect)> =
            std::collections::VecDeque::new();
        // 端到端延迟：投递时刻 → 真正被驱动的那一帧。
        let mut lat_max = now_ms() - first.t;
        let wf0 = first.wf;
        let mut dirty = (first.x, first.y, first.w, first.h);
        let mut frame_no = 0usize;
        let mut n_rect = 0usize;
        // 这一轮里出现过几种波形（0/1 = 纯的，2 以上 = 并行）。
        let mut mi_seen: Vec<u8> = Vec::new();
        if timing {
            eprintln!(
                "[job] {}×{}@{},{} wf={} {:?} narrow={}",
                first.w, first.h, first.x, first.y, first.wf, first.mode, first.narrow
            );
        }
        if let Some(mi) = panel.mode_slot(first.mode) {
            let nph = panel.phases(first.mode).unwrap_or(1);
            let mut ch =
                panel.set_rect(first.x, first.y, first.w, first.h, &first.levels, mi);
            if repaints(first.narrow) {
                ch = or_opt(
                    ch,
                    panel.force_changed(first.x, first.y, first.w, first.h, mi),
                );
            }
            if let Some(r) = ch {
                win.push_back((frame_no + nph, r));
                n_rect += 1;
            }
            mi_seen.push(mi);
            // 用完把灰度缓冲还回去（`first` 之后只用它的 Copy 字段）
            let _ = back_tx.send(first.levels);
        }
        let mode = first.mode;
        // 大块（整屏重绘）走两段式：先把所有相位编进 RAM，再严格一帧一相位地播。
        //
        // 原因：整屏一个相位编码约 12 ms，超过帧周期；流式「编下一帧」会和
        // 扫描抢时间，实测每相位 16.5 ms > 11.76 ms —— 波形被拉长就废了
        // （「全白 + 残影」那个老坑）。现在整屏一相位 9.6 ms + 写帧 1.2 ms
        // 已经能流式（见下面注释），所以默认关掉；`RM_PANEL_TWOPASS=1` 打开对照。
        let two_pass_wanted = std::env::var("RM_PANEL_TWOPASS").map(|v| v != "0").unwrap_or(false);
        if two_pass_wanted && first.w * first.h > 1_000_000 {
            match panel.update(first.x, first.y, first.w, first.h, mode, 0, true) {
                Ok((pn, enc, play)) => {
                    if timing {
                        eprintln!(
                            "[panel] {}×{}@{},{} {:?} 两段式 {pn} 相位 编码 {enc:.0} ms 播放 {play:.0} ms",
                            first.w, first.h, first.x, first.y, mode
                        );
                    }
                }
                Err(e) => eprintln!("[panel] 刷新失败：{e}"),
            }
            if consumed > 0 {
                pending.fetch_sub(consumed, std::sync::atomic::Ordering::Relaxed);
                consumed = 0;
            }
            continue;
        }

        // 大块（整屏/翻页/睡眠图/关机框）这一轮开始前提频：ondemand 在空闲时
        // 会掉到 396 MHz，整屏编码就从 9 ms/相位涨到 17 ms，端到端 1.1 s。
        // 小刷新（一笔、一个开关）不动它 —— 见 `cpu.rs` 的实测数字。
        //
        // **不能只看本轮第一条 job**：一条整屏刷新常常跟在几条小刷新后面
        // 搭同一轮播（实测睡眠图那次就是这样：`first` 是解锁后重画的工具栏，
        // 结果 719 ms 编码、超帧 45/45）。所以本轮里只要出现过大块就提频。
        let big = |w: usize, h: usize| w * h >= 700_000;
        let mut _boost = if big(first.w, first.h) {
            Some(crate::cpu::Boost::new())
        } else {
            None
        };
        panel.frame_stat_reset();
        let t0 = std::time::Instant::now();
        let mut t_enc = std::time::Duration::ZERO;
        let mut t_show = std::time::Duration::ZERO;
        let mut t_set = std::time::Duration::ZERO;
        let mut t_set_max = std::time::Duration::ZERO;
        let mut set_px = 0u64;
        let mut set_max_px = 0u64;
        let mut frames = 0usize;
        let nd0 = panel.nodrive;
        let mut drive_px = 0u64;
        // 一轮里最多同时编了几条带（诊断用：>1 说明确实在分开驱）。
        let mut parts_max = 1usize;

        // 一轮里**只允许一种波形**。
        //
        // 每个 cell 的相位按「cur - phc」算，而 nph 是当前波形的相位数：一轮里
        // 先来 GC16（46 相位）再来笔的 DU（25 相位），nph 一变小，那批 GC16 的
        // cell 只吃了 25/46 就被判「到期」，可 `img` 已经记成到位 —— 屏上那块
        // 永远是空的，之后每次刷新都判「没变化」，再也救不回来。
        // （日志实证：`600×720@0,120 Gc16 ... 空帧 49`）
        //
        // 所以波形不同的 job 一律攒进 `carry`，等这一轮走完再开下一轮。
        loop {
            // 1. 这一轮新到的目标。
            let mut batch: Vec<Job> = Vec::new();
            while let Ok(j) = rx.try_recv() {
                consumed += 1;
                if _boost.is_none() && big(j.w, j.h) {
                    _boost = Some(crate::cpu::Boost::new());
                }
                batch.push(j);
            }
            // 同一批里可能躺着好几条"整屏重绘"（用户在设置页连点左侧条目）。
            //
            // 每一条都是 46 相位 ≈ 600 ms，而中间那几帧**根本没人看得见** ——
            // 连点三下就是 1.8 s 的陈旧画面排队播，手感就是"卡非常久"。
            // 只留最后一条（它的 `levels` 就是最新状态），前面的灰度缓冲直接
            // 还给池子。`mark_pushed` 在发送时就记过了，丢掉中间帧不会让
            // 脏区差分漏内容：最后那条会把同一块重新记一遍。
            if batch.len() > 1 {
                let mut last_full = None;
                for (i, j) in batch.iter().enumerate() {
                    if !j.narrow && j.w * j.h >= 1_000_000 {
                        last_full = Some(i);
                    }
                }
                if let Some(keep) = last_full {
                    let mut idx = 0usize;
                    batch.retain_mut(|j| {
                        let mine = idx;
                        idx += 1;
                        if mine != keep && !j.narrow && j.w * j.h >= 1_000_000 {
                            let _ = back_tx.send(std::mem::take(&mut j.levels));
                            return false;
                        }
                        true
                    });
                }
            }
            for j in batch {
                let Some(mi) = panel.mode_slot(j.mode) else {
                    continue;
                };
                let nph = panel.phases(j.mode).unwrap_or(1);
                if timing {
                    eprintln!(
                        "[job] {}×{}@{},{} wf={} {:?} narrow={}",
                        j.w, j.h, j.x, j.y, j.wf, j.mode, j.narrow
                    );
                }
                let ts2 = std::time::Instant::now();
                let mut ch = panel.set_rect(j.x, j.y, j.w, j.h, &j.levels, mi);
                if repaints(j.narrow) {
                    ch = or_opt(ch, panel.force_changed(j.x, j.y, j.w, j.h, mi));
                }
                let d = ts2.elapsed();
                t_set += d;
                t_set_max = t_set_max.max(d);
                let px = (j.w * j.h) as u64;
                set_px += px;
                set_max_px = set_max_px.max(px);
                let jr = (j.x, j.y, j.w, j.h);
                dirty = union(dirty, jr);
                lat_max = lat_max.max(now_ms() - j.t);
                if !mi_seen.contains(&mi) {
                    mi_seen.push(mi);
                }
                // 2. 真正变过的那块进窗口，签一份「可以驱到第 frame_no + nph 帧」。
                //    每条 job 按**它自己的**相位数签：DU 25 帧、GC16 46 帧，
                //    同一轮里并存正是"并行推"的意思。
                if let Some(r) = ch {
                    win.push_back((frame_no + nph, r));
                    n_rect += 1;
                }
                let _ = back_tx.send(j.levels);
            }
            // 3. 滚出去的 = 已经吃满自己那一轮（相位自然变成 0 码）。
            win.retain(|&(e, _)| e > frame_no);
            // 4. 驱动区域。窗口里的每个 cell 都必须每帧被驱（否则它的相位就
            //    跳），所以**不收缩、不交集**。但也不能直接求一个大并集：
            //    左上角工具条 + 右下角笔迹的并集就是整屏，2.3 Mpx 一帧编码
            //    超出 11.76 ms 预算，整轮波形被拉长 —— 日志里 `1296×1777` 那条
            //    帧周期 17.5 就是这么来的。这里先收成几条互不重叠的带，
            //    逐条编（`one` 是整帧缓冲，多条按列区间累积，互不覆盖），
            //    最后一次性显示它们的并集（show 只是写 cell 码，快）。
            let parts = merge_rects(&win.iter().map(|&(_, r)| r).collect::<Vec<_>>());
            if parts.is_empty() {
                break;
            }
            let drive = parts.iter().copied().reduce(union).unwrap_or(parts[0]);
            parts_max = parts_max.max(parts.len());
            // 5. 编一帧（相位由每个 cell 自己算）并显示。
            let te = std::time::Instant::now();
            for p in &parts {
                if let Err(e) = panel.encode_phase(p.0, p.1, p.2, p.3) {
                    eprintln!("[panel] 编码失败：{e}");
                    break;
                }
                drive_px += (p.2 * p.3) as u64;
            }
            t_enc += te.elapsed();
            let ts = std::time::Instant::now();
            if let Err(e) = panel.show_pending(drive.0, drive.1, drive.2, drive.3) {
                eprintln!("[panel] 刷新失败：{e}");
                break;
            }
            t_show += ts.elapsed();
            frames += 1;
            frame_no += 1;
        }
        panel.clear_changed();

        // 兜底自检：这一轮碰过的 cell 里还有没有「相位没吃完」的？
        // 正常情况下永远是 None（窗口 = 变过的像素，一定被驱满 nph 帧）。
        // 一旦不是，说明驱动逻辑有洞，直接报出来 —— 这类洞在屏上表现为
        // "某个角/某半边永远画不上"，靠肉眼很难定位到是哪个矩形。
        if let Some((ux, uy, uw, uh)) = panel.unfinished(dirty.0, dirty.1, dirty.2, dirty.3) {
            eprintln!(
                "[panel] 未完成 {uw}×{uh}@{ux},{uy}（{nmode} 种波形并行）",
                nmode = mi_seen.len()
            );
        }
        if let Ok(path) = std::env::var("RM_PANEL_DUMP_IMG") {
            let _ = std::fs::write(&path, panel.img_bytes());
        }

        if let Some(t) = panel.take_trace() {
            eprintln!("[trace] {} 逐帧码：{}", t.0, t.1);
        }
        if timing {
            let ms = t0.elapsed().as_secs_f64() * 1e3;
            let (fnum, favg, fmax, fslow) = panel.frame_stat();
            eprintln!(
                "[panel] {}×{}@{},{} {:?}(wf={wf0}) {frames} 帧 / {n_rect} 块 / 混 {} 波 {:.0} ms（编码 {:.0} / 显示 {:.0} / 写入 {:.0} / 延迟≤{:.0}）| 帧周期 {favg:.1} 均 {fmax:.1} 峰，超帧 {fslow}/{fnum} | 空帧 {} 带 {} | 驱动 {:.1} Mpx（{:.3} Mpx/帧，编码 {:4.1} ns/px）| 投递 {:.1} Mpx 最大块 {:.0} px 单次≤{:.0} ms | 提频 {}",
                dirty.2, dirty.3, dirty.0, dirty.1, mode, mi_seen.len(), ms,
                t_enc.as_secs_f64() * 1e3,
                t_show.as_secs_f64() * 1e3,
                t_set.as_secs_f64() * 1e3,
                lat_max,
                panel.nodrive - nd0,
                parts_max,
                drive_px as f64 / 1e6,
                if frames > 0 { drive_px as f64 / frames as f64 / 1e6 } else { 0.0 },
                if drive_px > 0 {
                    t_enc.as_secs_f64() * 1e9 / drive_px as f64
                } else {
                    0.0
                },
                set_px as f64 / 1e6,
                set_max_px as f64,
                t_set_max.as_secs_f64() * 1e3,
                _boost.is_some()
            );
        }
        let _ = panel.blank();
        // 这一轮结束了：手上拿到的 job 全部驱完（或被丢弃），记的回执交掉。
        if consumed > 0 {
            pending.fetch_sub(consumed, std::sync::atomic::Ordering::Relaxed);
            consumed = 0;
        }
    }
}

/// 这一条 job 要不要**整块重刷**（连目标没变的像素也驱一遍）。
///
/// 只有 app 明确要 `UPDATE_MODE_FULL` 时才重刷。
///
/// 试过"GC16 一律整块重刷"，看上去很美 —— GC16 对"目标 ← 目标"给出的序列
/// 会把像素先推到反极性再推回目标（实测 `GC16 白←白` = 推黑 22 帧 + 推白 20
/// 帧），所以理论上只要重刷一遍，屏上实际状态和 `img` 不一致的区域就能自愈。
///
/// **但代价是那一整块先黑 260 ms。** app 给橡皮擦完之后的收尾、菜单开合都是
/// `GC16 + PARTIAL`，于是擦一笔就在纸上闪一块几百像素见方的黑方块 —— 用户
/// 一眼就能看见。官方的 GC16 也不是这么用的：它只重刷**污点矩形**（用户真正
/// 碰过的那块），不会把周围没动过的内容一起推黑。
///
/// 所以这里退回去：只驱变过的像素。`img` 与屏上不一致的兜底交给
/// `UPDATE_MODE_FULL`（整页重绘、开机第一帧）——那些地方本来就该整屏闪一次。
fn repaints(narrow: bool) -> bool {
    !narrow
}

/// `Option<Rect>` 的并集。
fn or_opt(a: Option<Rect>, b: Option<Rect>) -> Option<Rect> {
    match (a, b) {
        (Some(x), Some(y)) => Some(union(x, y)),
        (x, None) => x,
        (None, y) => y,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 二值压色必须是「g8 ≥ 128 算白」——界面所有颜色都是 `gray565(g8)` 出来的，
    /// 这个等价关系一破，二值界面就会出现「本该白的灰底变黑」（整块翻面）或者
    /// 「深灰按钮字变没」。顺便钉住 5-6-5 三段和的阈值 `MONO_CUT = 64`。
    #[test]
    fn mono565_cuts_exactly_at_gray_128() {
        for g in 0..=255u8 {
            let want = if g >= 128 { WHITE } else { BLACK };
            assert_eq!(mono565(gray565(g)), want, "g8={g}");
        }
        // 纯色两端
        assert_eq!(mono565(WHITE), WHITE);
        assert_eq!(mono565(BLACK), BLACK);
        // 关掉二值之后不能再动颜色（这是正常路径，必须一个字节都不改）
        let c = gray565(200);
        assert_eq!(mono565(c), WHITE, "200 是白");
        assert_ne!(mono565(gray565(100)), WHITE, "100 是黑");
    }

    /// `set_mono` 之后 `put` / `fill_rect` 都只能落 0 / 0xFFFF。
    ///
    /// 这是 A2 局部刷新的前提：中间灰在二值波形里一个驱动码都没有，屏上会
    /// 保持原样（退格时旧字被钉住）。画的时候就得压干净，不能指望面板。
    #[test]
    fn mono_put_and_fill_rect_snap_to_bw() {
        let mut fb = Fb::open_mem();
        fb.set_mono(true);
        fb.fill_rect(10, 10, 20, 20, gray565(238)); // 输入框的灰底
        fb.fill_rect(40, 10, 20, 20, gray565(40)); // 深色按钮
        fb.put(100, 100, gray565(90));
        for (x, y, want) in [(15, 15, WHITE), (45, 15, BLACK), (100, 100, BLACK)] {
            assert_eq!(fb.plane[y * W + x], want, "({x},{y})");
        }
        // 关掉之后灰阶恢复
        fb.set_mono(false);
        fb.fill_rect(200, 10, 4, 4, gray565(238));
        assert_eq!(fb.plane[10 * W + 200], gray565(238));
    }

    /// 行主序平面 → 列主序灰度，必须逐像素对得上。
    ///
    /// 这里特意用**大于一个分块**的尺寸（150×200，分块 64×64 ⇒ 3×4 块），
    /// 因为漏掉块偏移 `bx` 的 bug 只有在多块时才暴露：以前整整屏 1404 列
    /// 被切成 22 块，每块都写回开头，屏上只有一个三角形区域有内容。
    /// 逻辑矩形 → 物理矩形：全屏正好铺满面板，工具条那格贴到物理右上角。
    #[test]
    fn rot_rect_maps_logical_frame_onto_the_panel() {
        let l = (1872usize, 1404usize);
        // 两个方向都必须把整幅铺满面板
        assert_eq!(rot_rect((0, 0, 1872, 1404), l, true), (0, 0, 1404, 1872));
        assert_eq!(rot_rect((0, 0, 1872, 1404), l, false), (0, 0, 1404, 1872));
        // 顺时针：左上角那格工具条 → 物理右边缘、贴顶
        let (px, py, pw, ph) = rot_rect((0, 0, 120, 93), l, true);
        assert_eq!((px + pw, py), (1404, 0), "顺时针贴右边缘和顶边");
        assert_eq!((pw, ph), (93, 120), "宽高对调");
        assert_eq!(rot_rect((0, 93, 120, 93), l, true).0, 1404 - 186);
        // 逆时针：左上角那格工具条 → 物理左边缘、贴底（充电口在左那档）
        let (px, py, pw, ph) = rot_rect((0, 0, 120, 93), l, false);
        assert_eq!((px, py + ph), (0, 1872), "逆时针贴左边缘和底边");
        assert_eq!((pw, ph), (93, 120), "宽高对调");
    }

    /// 左利手镜像：贴左的锚点跑到贴右，贴右的锚点跑到贴左，中间的不动。
    #[test]
    fn flip_in_mirrors_horizontal_anchors() {
        let lw = 1404;
        // 工具条（贴左，宽 120）→ 贴右
        assert_eq!(flip_in(0, 120, lw, true), lw - 120);
        // 收起 ×（贴右，宽 90）→ 贴左，且左右余量对称
        assert_eq!(flip_in(lw - 104, 90, lw, true), 14);
        // 弹出面板：贴工具条右边 → 贴工具条左边
        assert_eq!(flip_in(120, 400, lw, true), lw - 520);
        // 居中面板：镜像后自己
        assert_eq!(flip_in((lw - 620) / 2, 620, lw, true), (lw - 620) / 2);
        // 关掉时原样返回
        assert_eq!(flip_in(120, 400, lw, false), 120);
        // 横屏逻辑宽 1872 同样成立
        assert_eq!(flip_in(0, 120, 1872, true), 1872 - 120);
    }

    /// 横屏那趟转置：逻辑矩形顺时针转 90°，写进物理列主序。
    ///
    /// 这是横屏模式唯一的"坐标变换执行点"（绘制侧一行都不改），所以逐像素
    /// 钉住：逻辑 `(x, y)` 必须出现在物理 `(h-1-y, x)` 上。
    #[test]
    fn gray_to_cols_rot_rotates_clockwise() {
        let (w, h) = (4usize, 3usize);
        let stride = 6usize;
        let mut plane = vec![0u16; stride * h];
        for y in 0..h {
            for x in 0..w {
                plane[y * stride + x] = gray565(((y * 10 + x) * 8) as u8);
            }
        }
        let mut out = vec![0u8; w * h];
        gray_to_cols_rot(&plane, stride, 0, 0, w, h, &mut out, true);
        for col in 0..h {
            for row in 0..w {
                // 顺时针：物理 (col, row) ← 逻辑 (row, h-1-col)
                let (x, y) = (row, h - 1 - col);
                let g6 = (gray565(((y * 10 + x) * 8) as u8) >> 5 & 0x3f) as u32;
                let want = ((g6 * 15 + 31) / 63) as u8;
                assert_eq!(out[col * w + row], want, "顺时针 物理列 {col} 行 {row}");
            }
        }
        // 逆时针：物理 (col, row) ← 逻辑 (w-1-row, col)
        let mut out2 = vec![0u8; w * h];
        gray_to_cols_rot(&plane, stride, 0, 0, w, h, &mut out2, false);
        for col in 0..h {
            for row in 0..w {
                let (x, y) = (w - 1 - row, col);
                let g6 = (gray565(((y * 10 + x) * 8) as u8) >> 5 & 0x3f) as u32;
                let want = ((g6 * 15 + 31) / 63) as u8;
                assert_eq!(out2[col * w + row], want, "逆时针 物理列 {col} 行 {row}");
            }
        }
    }

    #[test]
    fn gray_to_cols_transposes_exactly() {
        let (stride, w, h) = (200usize, 150usize, 200usize);
        let mut plane = vec![0u16; stride * h];
        // 造一个"每个像素都不一样"的图：灰度 = (x*7 + y*3) % 256
        for y in 0..h {
            for x in 0..w {
                let g = ((x * 7 + y * 3) % 256) as u8;
                plane[y * stride + x] = gray565(g);
            }
        }
        let mut out = vec![0u8; w * h];
        gray_to_cols(&plane, stride, 0, 0, w, h, &mut out);
        for y in 0..h {
            for x in 0..w {
                let g = ((x * 7 + y * 3) % 256) as u8;
                let want = gray565(g) >> 5 & 0x3f; // 绿通道 6 bit
                let want4 = ((want as u32 * 15 + 31) / 63) as u8;
                assert_eq!(
                    out[x * h + y], want4,
                    "({x},{y}) 列主序错位：期望 {want4}，实际 {}",
                    out[x * h + y]
                );
            }
        }
    }

    /// 偏移窗口（x/y 非零）也要对。
    #[test]
    fn gray_to_cols_handles_offset() {
        let (stride, w, h) = (64usize, 20usize, 30usize);
        let (ox, oy) = (17usize, 11usize);
        let mut plane = vec![0u16; stride * 48];
        for y in 0..48 {
            for x in 0..stride {
                plane[y * stride + x] = gray565(((x + y) % 256) as u8);
            }
        }
        let mut out = vec![0u8; w * h];
        gray_to_cols(&plane, stride, ox, oy, w, h, &mut out);
        for row in 0..h {
            for col in 0..w {
                let g = (((ox + col) + (oy + row)) % 256) as u8;
                let g6 = gray565(g) >> 5 & 0x3f;
                let want4 = ((g6 as u32 * 15 + 31) / 63) as u8;
                assert_eq!(out[col * h + row], want4, "偏移窗口 ({col},{row}) 错位");
            }
        }
    }

    /// 像素级往返：任意 `(x, y, w, h)`（含跨 64 分块、贴四个角、不满一块的
    /// 尾巴）转成列主序之后，每个像素都必须落在唯一正确的位置上。
    ///
    /// 这条测试盯的是"**菜单只出来一半**"：`gray_to_cols` 是按 64×64 分块转
    /// 置的，块偏移少加一个 `bx`/`by`，表现就是"一块一块的地方没内容"或
    /// "内容整体错位"，而按行主序看 `plane` 完全正常 —— 光看代码看不出来。
    #[test]
    fn gray_to_cols_round_trips_every_corner() {
        let (w_screen, h_screen) = (1404usize, 1872usize);
        let mut plane = vec![0u16; w_screen * h_screen];
        // 每个像素给一个只由 (x, y) 决定的 16 级灰度，错位立刻暴露。
        let level = |x: usize, y: usize| -> u8 { (((x / 7) + (y / 11)) % 16) as u8 };
        for y in 0..h_screen {
            for x in 0..w_screen {
                let g = level(x, y) as u16;
                // 4-bit → RGB565 的绿通道（平移 5 位，和 `gray565` 一致）
                let g6 = (g * 63 / 15) as u16;
                plane[y * w_screen + x] = g6 << 5;
            }
        }
        let rects = [
            (0usize, 0usize, 1usize, 1usize),
            (1403, 1871, 1, 1),
            (0, 0, 64, 64),
            (0, 0, 65, 65),
            (120, 0, 240, 649),
            (120, 120, 400, 1056),
            (0, 120, 120, 480),
            (120, 240, 400, 1056),
            (1300, 0, 104, 60),
            (1340, 1808, 64, 64),
            (0, 0, 1404, 1872),
            (0, 1812, 1404, 60),
            (1340, 0, 64, 1872),
        ];
        for &(x, y, w, h) in &rects {
            if x + w > w_screen || y + h > h_screen {
                continue;
            }
            let mut out = vec![0u8; w * h];
            gray_to_cols(&plane, w_screen, x, y, w, h, &mut out);
            for col in 0..w {
                for row in 0..h {
                    assert_eq!(
                        out[col * h + row],
                        level(x + col, y + row),
                        "rect {w}×{h}@{x},{y} 列 {col} 行 {row} 错位"
                    );
                }
            }
        }
    }

    /// `clip()` 返回的是 `(x0, x1, y0, y1)` —— `refresh` 以前按
    /// `(x0, y0, x1, y1)` 解构，把 20×20 的笔迹膨胀成两百多像素的方块。
    /// `refresh` 的裁剪不能把矩形放大。
    ///
    /// 回归用例：`clip()` 返回 `(x0, x1, y0, y1)`，曾经在 `refresh` 里被
    /// 当成 `(x0, y0, x1, y1)` 解构。`x = 0, w = 120` 那种请求因为 `x1`
    /// 恰好等于 `y` 而"碰巧正确"，只有远离原点的笔迹才炸：`11×11@720,1460`
    /// 会变成 `720,731,740,740`，接着 `mark_pushed` 切片起点超过终点、
    /// panic 回库界面。这个用例把它们钉死。
    #[test]
    fn region_never_enlarges_the_request() {
        let fb = Fb::open_mem();
        for &(x, y, w, h) in &[
            (720, 1460, 11, 11),
            (0, 120, 120, 480),
            (1300, 10, 90, 90),
            (0, 0, 1404, 1872),
        ] {
            let r = fb.region(x, y, w, h, 0).expect("在屏内");
            assert_eq!((r.0, r.1, r.2, r.3), (x as usize, y as usize, w as usize, h as usize));
        }
        // 越界要裁，不能原样吐出去
        let r = fb.region(1400, 1870, 100, 100, 0).expect("部分在屏内");
        assert_eq!(r, (1400, 1870, 4, 2));
        // 完全在屏外：None
        assert!(fb.region(2000, 2000, 10, 10, 0).is_none());
    }

    #[test]
    fn clip_order_is_x0_x1_y0_y1() {
        let fb = Fb::open_mem();
        let (x0, x1, y0, y1) = fb.clip(400, 600, 20, 20);
        assert_eq!((x0, x1, y0, y1), (400, 420, 600, 620));
        // 越界的那一头要被夹住，而且夹成一个空矩形
        let (x0, x1, y0, y1) = fb.clip(0, 5000, 1404, 200);
        assert!(x0 >= x1 || y0 >= y1, "越界刷新应该被夹成空矩形");
    }
}
