//! rM2 面板驱动（纯 Rust 版，端口自真机验证过的 `epd.c`）。
//!
//! 背景：这台机器**没有 EPDC**，`epd@306f0000` 是 `disabled`。上屏靠 LCDIF 每帧
//! 串出一页「32-bit cell 码」：每行 260 个 cell，每个 cell 装 8 个面板行 × 2-bit
//! 驱动码 + 一个命令字节。官方 xochitl 的 `EPFramebufferSwtcon` 就是用户态干这件事。
//!
//! 这个模块把它重做了一遍：自己解析 `.wbf` 波形、按温度选档、把 16 级灰度图编码成
//! 逐帧 cell 码、用 `FBIOPAN_DISPLAY` 换页。
//!
//! # 三条铁律
//!
//! 1. **每个波形相位必须恰好占一个帧周期**（11.76 ms）。写 fb 必须整字 32 位，
//!    而且不能边播边编还编不完——所以整屏更新是"先全部编码进 RAM，再逐帧播"。
//! 2. **每次更新结束必须留一帧全 0（无驱动）**。LCDIF 会永远重复扫最后那一页，
//!    不留空白帧的话最后一相位会被无限驱动，把前面所有相位覆盖掉。
//! 3. **`prv` 必须和屏幕实际状态同步**，否则"没变"的像素会被判成"要驱动"，
//!    或者该驱动的被跳过。状态文件按脏矩形增量落盘，不整块重写。

pub mod encode;
mod raw;
pub mod wbf;

use std::fs::{File, OpenOptions};

use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

/// 用户坐标宽（屏宽）。
pub const UW: usize = 1404;
/// 用户坐标高。
pub const UH: usize = 1872;
/// 面板列数（= 用户高 1872 / 8）。
pub const COLS: usize = 1404;
/// 每行 cell 数：26 个命令 cell + 234 个内容 cell。
pub const CELLS: usize = 260;
/// 每行字节数（= fix.line_length，也是 `fb_var.xres_virtual` 的像素数 × 4）。
pub const LINE: usize = CELLS * 4;
/// 每槽行数：4 行前导 + 1404 内容列。
pub const ROWS: usize = 1408;
/// 一个槽的字节数。
pub const SLOT: usize = LINE * ROWS;
/// 每列内容 cell 数 = 1872 / 8。
pub const CSTR: usize = 234;

/// `phc` 里"这一格从没盖过相位戳"的哨兵。
///
/// 不能用 0/0x8000：全局帧时钟从 0 开始数，0 表示"第 0 帧"，而 0x8000 会在
/// 开机 32768 帧（约 6.4 分钟）之后被 `cur` 自己撞上 —— 从那一刻起全屏 cell
/// 的相位都会算错。取一个远到不可能到达的值，`cur.wrapping_sub(它)` 永远很大。
pub const PHC_NONE: u32 = 0xC000_0000;
/// 内容 cell 在一行里的起始下标。
pub const NCELL: usize = 26;
/// 一个相位展开后的字节数。
pub const BYTES_PER_PHASE: usize = wbf::BYTES_PER_PHASE;

/// 灰度波形模式。名字对应 `.wbf` 里的模式名。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// 只对极性翻转的像素定向推 N 帧——手写/局部动作用。
    Du,
    /// 16 级灰度，慢、无残影——整块重绘用（会整块闪黑再回白，专门清残影）。
    Gc16,
    /// 和 GC16 驱动序列一样，但**不变像素完全不驱动**——局部刷新用，不闪。
    Gl16,
    /// 二值快刷。
    A2,
    /// 4 帧 DU 变体。
    Du4,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Du => "DU",
            Mode::Gc16 => "GC16",
            Mode::Gl16 => "GL16",
            Mode::A2 => "A2",
            Mode::Du4 => "DU4",
        }
    }
}

fn read_temp() -> i32 {
    for p in [
        "/sys/class/thermal/thermal_zone0/temp",
        "/sys/class/hwmon/hwmon1/temp0",
    ] {
        if let Ok(s) = std::fs::read_to_string(p) {
            if let Ok(v) = s.trim().parse::<i32>() {
                return if v > 1000 { v / 1000 } else { v };
            }
        }
    }
    22
}

fn state_path() -> PathBuf {
    std::env::var("RM_PANEL_STATE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp/rm2epd.state"))
}

/// 打开好的面板：一块状态（`prv`/`img`）加一套编码器。
pub struct Panel {
    raw: raw::RawFb,
    wf: wbf::Waveform,
    band: usize,
    temp_c: i32,
    /// 当前屏上状态，x-major：`prv[x*UH + y]`，0 = 黑，15 = 白。
    prv: Vec<u8>,
    /// 目标图像，同布局。
    img: Vec<u8>,
    /// `(目标 << 4) | 波形的起点`。
    kp: Vec<u8>,
    /// 每个内容 cell 的**相位 0 是哪一帧**：`phc[x*CSTR + s]`。
    ///
    /// 官方 swtcon 的 `WorkItem` 各自带 `phase`（`display.cpp:748`），新区域从
    /// 相位 0 起算、老区域继续数，互不重置。这里用同构的做法：记下起点帧号，
    /// 相位 = `cur - phc`，到 `nph` 就算到位（码 0）。
    ///
    /// 以前只有一个全局相位，一来新墨就归零，连续输入下永远停在相位 0 ——
    /// 实测一次 DU 刷新跑 350 帧 / **4.1 s**（本该 25 帧 / 300 ms），10 秒里
    /// 只完成 9 次视觉更新，屏上就是浓拖尾。
    phc: Vec<u32>,
    /// 编码出来「一个驱动码都没有」的帧数 —— 直接反映 app 要的东西
    /// 这批波形给不了（比如 DU 碰上中间灰）。
    pub nodrive: usize,
    /// 全局帧时钟：每翻一页 +1。
    ///
    /// 这里**必须**是 u32：以前是 u16，而 `phc` 的"从没盖过戳"哨兵是 0x8000，
    /// 开机 32768 帧（约 6.4 分钟）之后 `cur` 本身就会撞上哨兵值 —— 那一刻起
    /// 屏上每个 cell 的 `cur - phc` 都会算成一个小数字，全屏被当成"还有相位
    /// 没吃完"，或者反过来永远判"已经到位"，两种都是屏上直接坏掉。
    cur: u32,
    /// 「整块用同一个相位戳盖过章」的区间：`(x0, x1, slo, shi, stamp)`（左闭右开）。
    ///
    /// 整屏 `UPDATE_MODE_FULL` 就是这种情况：`force_changed` 一次把整块的 cell
    /// 都盖成 `cur`，于是整帧只有**一张**相位表，编码可以走最快那条路
    /// （每 cell 一次查表，不逐 cell 读时间戳）。整屏 GC16 全靠它：
    /// 编码 12.8 ms → ~4 ms，帧周期回到 11.76 ms，46 相位正好 **541 ms**。
    ///
    /// 任何 `set_rect` 都会把它清掉——那说明有 cell 被盖上了不一样的戳。
    ///
    /// 末尾还带一个波形下标：整块同相位**且同波形**才能走这张单表。
    uni: Option<(usize, usize, usize, usize, u32, u8)>,
    /// 每个内容 cell 用的是**哪张波形表**（`wf` 里的模式下标）：`pmc[x*CSTR+s]`。
    ///
    /// 这是官方 swtcon「每个 `WorkItem` 自带 phase，同一帧里多个波形并行推」的
    /// 关键一半：`phc` 记"从哪一帧开始"，`pmc` 记"吃哪张表"。少了它就只能
    /// 一轮一种波形，笔的 DU 得等菜单的 GC16（46 帧 = 550 ms）走完 —— 实测
    /// 日志里 `延迟≤1229 ms`、`待办 3`。加上以后一个 cell 只要 age < 自己那张
    /// 表的相位数就继续驱，短的（DU 25）到点自然变 0 码，不影响长的（GC16 46）。
    pmc: Vec<u8>,
    /// 每个模式的「相位矩阵」：`luts[mi] = (nph, (nph+1)×256)`。
    ///
    /// 按 `wf` 的模式下标记，`pmc` 里直接存这个下标，编码时一次索引拿到表，
    /// 不用查 HashMap。
    luts: Vec<(usize, Vec<u8>)>,
    /// 和 `luts` 同形，但把**键的两个半字节相等**的那些表项改成 0。
    ///
    /// 键是 `(目标 << 4) | 上一帧`。两个半字节相等 = "这个像素本来就在目标值
    /// 上，这一轮没有它的事"。而 GC16 的这类键给的是**自愈序列**：
    /// 实测 `GC16 白←白` = 推黑 22 帧 + 推白 20 帧，`黑←黑` = 先推白 18 帧。
    ///
    /// 于是"一个 cell 8 行里只有 1 行真变了"时，另外 7 行会被推黑 —— 屏上
    /// 就是擦一笔留下**一串念珠样的黑格子**；整块走自愈序列时就是一整块
    /// **巨大黑矩形**（2 帧就够肉眼看见，22 帧是 260 ms）。
    ///
    /// 逐像素流式驱动（按 cell 相位）必须用这张零化的表；只有"整块重刷"
    /// （`UPDATE_MODE_FULL`、`force_changed` 盖过戳的 `uni` 快路）才用上面
    /// 那张原表 —— 那里本来就是要整块闪一次清残影的。
    luts2: Vec<(usize, Vec<u8>)>,
    /// RAM 模板槽（`SLOT` 字节）。
    slab: Vec<u32>,
    /// 一帧的编码结果：`COLS × CSTR` 个 u16。
    one: Vec<u16>,
    /// 预编码好的全部相位（`play × 脏列数 × CSTR`）。
    ///
    /// **必须先全部编完再播**：边播边编会让每个相位超过一个帧周期，
    /// 波形被拉长 —— 症状就是"全白 + 残影"（C 版当年踩过同一个坑）。
    phases: Vec<u16>,
    /// 解码好的波形表缓存：`(模式下标, 温度档) -> nph × 256 B`。
    tables: std::collections::HashMap<(usize, usize), Vec<u8>>,
    /// 解码好的「相位矩阵」：`(模式下标, 温度档) -> (nph, (nph+1)×256)`。
    ///
    /// 每帧都重新 `prep` 一遍 25 个相位纯属浪费（一帧 11.76 ms 是硬预算），
    /// 这里建一次就一直用。第 `nph` 行全 0 = "相位吃完了，不驱动"。
    lut_aged: std::collections::HashMap<(usize, usize), (usize, Vec<u8>)>,
    /// 自上次整轮提交以来「真的变了」的像素包围盒 `(x0, y0, x1, y1)`（右下开区间）。
    ///
    /// 照官方 `dispatch_processed_regions` 的 incremental 语义：提交前把驱动
    /// 矩形收窄到**实际变化像素**的包围盒。没有这一步，一块大脏矩形里只变
    /// 了几个像素也要整块按波形驱一遍——屏上就是「落笔时背景闪一下」。
    ///
    /// `None` = 目前没有变化的像素（这一轮什么都不用驱）。
    chg: Option<(usize, usize, usize, usize)>,
    /// 槽里当前还留着驱动的区域：面板列 `[pclo, pchi]` × 内容 cell `[slo, shi]`。
    /// 有了它，空白帧和逐帧刷新都能只写脏块，而不是把 1.46 MB 全刷一遍。
    lit: Option<(usize, usize, usize, usize)>,
    /// 当前正在扫描的槽（0/1）。双缓冲：写背面那一槽，翻过去，下次写另一槽。
    cur_slot: usize,
    /// 逐帧驱动码追踪点 `(x, y, 已收集的码)`。
    ///
    /// 设 `RM_PANEL_TRACE=x,y` 后每一帧都把该像素真正写进槽的 2-bit 码追加进来，
    /// 一轮结束时打出来。这是判断"某个点到底吃了几帧驱动"的唯一可靠手段 ——
    /// 屏上的拖尾到底是没驱够，还是 `prv` 记错了，一眼就能分开。
    trace: Option<(usize, usize, String)>,
    /// 调试用：已经打过几次 pan 计时。
    dbg_pans: u32,
    // 帧周期统计（见 `mark_frame`）。
    fr_last: Option<std::time::Instant>,
    fr_n: usize,
    fr_slow: usize,
    fr_sum: f64,
    fr_max: f64,
    /// 状态文件（按脏矩形增量写，避免每帧重写 2.6 MB）。
    state: Option<File>,
    pub wbf_path: String,
    pub why: &'static str,
    pub inited: bool,
}

impl Panel {
    /// 选波形、开 `/dev/fb0`、改几何、建模板槽、载入状态。
    ///
    /// 状态文件不存在（通常是刚开机）时会先跑一遍官方 INIT 序列，
    /// 把面板驱到已知的白色基线——这一步 111 帧、约 1.3 s，只在首次发生。
    pub fn open() -> Result<Panel, String> {
        let (cand, why) = wbf::pick()?;
        let wf = wbf::Waveform::open(Path::new(&cand.path))?;
        let raw = raw::RawFb::open().map_err(|e| format!("/dev/fb0: {e}"))?;
        let temp_c = read_temp();
        let band = wf.band_for(temp_c);
        let slab = encode::slab_new();

        let (state, prv, fresh) = open_state();
        let img = prv.clone();

        let mut p = Panel {
            raw,
            wf,
            band,
            temp_c,
            prv,
            img,
            kp: vec![0u8; UW * UH],
            phc: vec![PHC_NONE; UW * CSTR],
            nodrive: 0,
            cur: 0,
            uni: None,
            pmc: vec![0u8; UW * CSTR],
            luts: Vec::new(),
            luts2: Vec::new(),
            slab,
            one: vec![0u16; COLS * CSTR],
            phases: Vec::new(),
            tables: std::collections::HashMap::new(),
            lut_aged: std::collections::HashMap::new(),
            chg: None,
            lit: None,
            // 开机时内核把 NEXT_BUF 指着槽 1，所以第一次 `emit` 应该写槽 0。
            cur_slot: 1,
            trace: std::env::var("RM_PANEL_TRACE").ok().and_then(|v| {
                let mut it = v.split(',');
                let x: usize = it.next()?.trim().parse().ok()?;
                let y: usize = it.next()?.trim().parse().ok()?;
                Some((x, y, String::new()))
            }),
            dbg_pans: 0,
            fr_last: None,
            fr_n: 0,
            fr_slow: 0,
            fr_sum: 0.0,
            fr_max: 0.0,
            state,
            wbf_path: cand.path.clone(),
            why,
            inited: false,
        };
        // 两槽都先放一帧"无驱动"的模板。内核随时可能把 NEXT_BUF 指回槽 1，
        // 那一槽要是上电垃圾，面板就会按垃圾码去驱（花屏/整片发黑）。
        //
        // 顺手把 `kp` 全量建一次：开机时 `prv`/`img` 就是屏上的基线，
        // 之后 `set_rect` 只重算真正变过的那几个字。
        encode::build_kp_cols(&p.img, &p.prv, &mut p.kp, 0, COLS - 1, 0, UH);
        p.raw.write_slot_at(0, &p.slab);
        p.raw.write_slot_at(1, &p.slab);
        if fresh {
            // **默认不再自己跑 INIT。**
            //
            // 官方开机链（内核 + 官方 splash）已经把面板初始化过了；我们自己再
            // 跑一遍 111 帧的"净零"序列，唯一的效果是把面板从"白"推到某个不确定
            // 的中间灰，而模型里写着白 —— 于是第一帧里"白→白"的像素一格都不驱动，
            // 真机表现就是"开机后库界面淡灰/像残影"，甚至整片白。
            //
            // 现在：直接当屏上是白（`prv`/`img` 都是 15），第一帧从白驱动到内容，
            // 每个像素都是满对比度。需要老行为时 `RM_PANEL_INIT=1`。
            // **默认必须跑**：实测跳过 INIT 之后，后续 `pan()` 出去的帧在面板上
            // 完全不生效（屏上停在官方的 "reMarkable is starting" splash），
            // 说明这套 INIT 序列不只"建立基线"，它还是后面驱动可见的前提。
            // 要试"不跑 INIT"用 `RM_PANEL_INIT=0`。
            if std::env::var("RM_PANEL_INIT").map(|v| v != "0").unwrap_or(true) {
                eprintln!("[panel] 没有屏上状态，按 RM_PANEL_INIT 跑 INIT（111 帧，约 1.3 s）");
                p.init_sequence()?;
            } else {
                eprintln!("[panel] 没有屏上状态：按 RM_PANEL_INIT=0 跳过 INIT（实测会让帧不生效）");
                p.prv.fill(15);
                p.img.fill(15);
                p.save_all();
            }
        }
        Ok(p)
    }

    /// 当前温度档（每档一套不同相位数的波形）。
    pub fn band(&self) -> usize {
        self.band
    }

    pub fn temp_c(&self) -> i32 {
        self.temp_c
    }

    /// 重新读温度并切换波段。温度变化很慢，不需要每次刷新都读。
    pub fn refresh_band(&mut self) {
        self.temp_c = read_temp();
        self.band = self.wf.band_for(self.temp_c);
    }

    /// 一行日志：选了哪个波形、指纹是什么、当前温度档。
    pub fn describe(&self) -> String {
        format!(
            "{} ({}) | FPL_LOT=0x{:02X} TFT_VID={} mv=0x{:02X} {} 模式 × {} 温度档 | {} C → band {} [{}, {}] | fb={} {}×{} @{}bpp",
            self.wbf_path,
            self.why,
            self.wf.fpl_lot,
            if self.wf.tft_vid.is_empty() {
                "?"
            } else {
                &self.wf.tft_vid
            },
            self.wf.mode_version,
            self.wf.nmode,
            self.wf.nband,
            self.temp_c,
            self.band,
            self.wf.bounds[self.band],
            self.wf.bounds[(self.band + 1).min(self.wf.nband)] as i32 - 1,
            self.raw.id,
            self.raw.line_length,
            self.raw.yres_virtual,
            32,
        )
    }

    /// 官方 INIT：双极性、净零驱动，结束时为白色。
    ///
    /// 这是实测通过的固定序列，比按 `.wbf` 的 INIT 表逐相位编码更稳
    /// （官方的 INIT 表在不同温度档下相位数差很多，直接用容易过驱）。
    fn init_sequence(&mut self) -> Result<(), String> {
        const SEQ: [(u16, usize); 8] = [
            (1, 24),
            (0, 4),
            (2, 24),
            (0, 5),
            (1, 24),
            (0, 4),
            (2, 24),
            (0, 2),
        ];
        for (code, n) in SEQ {
            let v = match code {
                0 => 0x0000,
                1 => 0x5555,
                _ => 0xAAAA,
            };
            self.one.fill(v);
            for _ in 0..n {
                self.show_frame_all()?;
            }
        }
        self.blank()?;
        self.prv.fill(15);
        self.img.fill(15);
        self.save_all();
        self.inited = true;
        Ok(())
    }

    /// 把 `slab` 写进**背面**那一槽、翻过去、并停一个帧周期。
    ///
    /// 双缓冲不是优化，是正确性：1.46 MB 要写 2.6 ms ≈ 一帧的 22%，如果写的
    /// 正是 LCDIF 正在扫的那一槽，正在扫的行会被撕成一条**竖带**（屏上表现为
    /// "竖条里还是上一屏的画面"）。另外内核每次 pan 之后会把 `NEXT_BUF` 指回
    /// 槽 1（防烧屏），所以两槽都必须一直放着有效帧。
    fn emit(&mut self) -> Result<(), String> {
        // **两槽都写**。开机那一刻 LCDIF 可能还没被程序化，`CUR_BUF` 读出来是
        // 0/垃圾 → `probe_slot` 退回槽 0；要是面板其实在扫槽 1，这一帧就喷到
        // 没人看的那块画布上了 —— 真机症状正是"间歇性白屏：帧都发出去了，
        // 屏上一个像素都不变"。整屏刷新多写 1.4 MB DRAM（约 5 ms），
        // 换来"无论扫哪一槽都看得见"。
        self.raw.write_slot_at(0, &self.slab);
        self.raw.write_slot_at(1, &self.slab);
        let s = self.raw.slot();
        self.raw.pan_to(s).map_err(|e| format!("pan: {e}"))?;
        self.mark_frame();
        // **等一帧之后再探**：刚 `pan` 完那一刻内核还没切 `CUR_BUF`，
        // 立刻读会读到旧值，反而把槽位改错。这一帧之后读到的才是真的。
        self.raw.reprobe();
        Ok(())
    }

    /// 只写 `[lo, hi]` 这些面板列对应的行，再翻页。
    fn emit_rows(&mut self, lo: usize, hi: usize) -> Result<(), String> {
        self.raw.write_rows(&self.slab, lo, hi);
        self.raw.pan().map_err(|e| format!("pan: {e}"))?;
        self.mark_frame();
        Ok(())
    }

    /// 只写「列 `[lo, hi]` × cell `[ulo, uhi]`」这一块字，再翻页。
    ///
    /// 一个内容 cell 覆盖 8 个用户行，所以脏行也能折成一段连续字：
    /// 写 68 个 cell 就是 68 个字（272 B），比写满一整行 1040 B 少 3/4 的
    /// DRAM 流量。
    fn emit_cells(&mut self, lo: usize, hi: usize, ulo: usize, uhi: usize) -> Result<(), String> {
        if let Some((tx, ty, buf)) = self.trace.as_mut() {
            let pc = COLS - 1 - (*tx).min(COLS - 1);
            let s = (UH - 1 - (*ty).min(UH - 1)) / 8;
            let base_y = UH - 8 - s * 8;
            let i = (*ty).min(UH - 1).saturating_sub(base_y);
            // 驱动码直接从 `one` 取（不再经过 slab：那一遍读写已经省掉了）。
            let w = self.one[pc * CSTR + s];
            let code = (w >> (2 * i)) & 3;
            buf.push(char::from(b'0' + code as u8));
        }
        // 把 `one` 和模板槽的高 16 位合好，直接写进槽（融合版：不再先改写
        // 一遍 slab 再写 fb）。整屏 GC16 每帧 328k 个字，`/dev/fb0` 是非缓存
        // 映射（一次 store 二十几个周期），所以这一遍就是 6.5 ms —— 计不进
        // 帧预算就得掉帧。融合以后从 903 ms 降到 614 ms。
        self.raw
            .write_cells_merge(&self.slab, &self.one, lo, hi, ulo, uhi);
        self.raw.pan().map_err(|e| format!("pan: {e}"))?;
        self.mark_frame();
        Ok(())
    }

    // -------------------------------------------------- 帧周期统计
    //
    // 目标只有一条：**一个相位恰好占一个面板帧**（263×1553×28.8 ns =
    // 11.76 ms，85 Hz）。超了波形就被拉长，屏上看到的是"一路闪 / 拖影"；
    // 少了说明我们比面板快，没关系。所以每翻一页记一次时间戳，把超帧的
    // 次数和最大周期统计出来——这是唯一能自动判断"快不快"的指标。

    fn mark_frame(&mut self) {
        // 全局帧时钟：cell 的相位 = `cur - phc[cell]`，所以每显示一帧 +1。
        self.cur = self.cur.wrapping_add(1);
        let now = std::time::Instant::now();
        if let Some(prev) = self.fr_last.replace(now) {
            let dt = now.duration_since(prev).as_secs_f64() * 1e3;
            self.fr_n += 1;
            self.fr_sum += dt;
            if dt > self.fr_max {
                self.fr_max = dt;
            }
            // 11.76 ms 是理论值，留 15% 余量再算"超了"。
            if dt > 13.5 {
                self.fr_slow += 1;
            }
        }
    }

    /// 重置帧周期统计（一次刷新开始前调）。
    pub fn frame_stat_reset(&mut self) {
        self.fr_last = None;
        self.fr_n = 0;
        self.fr_slow = 0;
        self.fr_sum = 0.0;
        self.fr_max = 0.0;
    }

    /// `(帧数, 平均周期 ms, 最大周期 ms, 超帧数)`。
    pub fn frame_stat(&self) -> (usize, f64, f64, usize) {
        let avg = if self.fr_n == 0 {
            0.0
        } else {
            self.fr_sum / self.fr_n as f64
        };
        (self.fr_n, avg, self.fr_max, self.fr_slow)
    }

    /// 把 `one` 的全部分列写进模板槽、落到 fb、翻页（停一帧）。
    fn show_frame_all(&mut self) -> Result<(), String> {
        let one = std::mem::take(&mut self.one);
        encode::put_cols(&mut self.slab, &one, 0, COLS - 1);
        self.one = one;
        self.lit = Some((0, COLS - 1, 0, CSTR - 1));
        self.emit()
    }

    /// 只写 `[pclo, pchi]` 这些面板列 × cell 区间 `[slo, shi]`，再翻页。
    ///
    /// 槽是「行 = 面板列」，所以脏列就是连续的几行；每行 1040 B，
    /// 一次局部刷新通常几十列 → 几十 KB，而不是 1.46 MB。
    ///
    /// **必须带上「上一帧写过」的区间一起写**：`one` 里这一帧没编到的 cell
    /// 是全 0，写下去正是把上一帧的驱动码清掉。漏了这一步，上一相的驱动码
    /// 会一直留在槽里，面板就一直按老码驱——屏上是一条不会消失的带。
    fn show_frame_cols(&mut self, pclo: usize, pchi: usize, slo: usize, shi: usize) -> Result<(), String> {
        // 广角（整屏）只看头几帧；窄条（笔迹）多打一些，用来量单帧真实耗时。
        let dbg = self.dbg_pans < 6 || (pchi - pclo) < 200 && self.dbg_pans < 40;
        let t0 = std::time::Instant::now();
        let (lo, hi, ulo, uhi) = match self.lit {
            Some((a, b, c, d)) => (a.min(pclo), b.max(pchi), c.min(slo), d.max(shi)),
            None => (pclo, pchi, slo, shi),
        };
        // 并集里「这一帧没编到」的部分必须写 0：那是把上一帧的驱动码清掉。
        //
        // 少了这一步，矩形一变窄，被扔掉的那些列会**一直**按上一帧的码驱
        // 下去（屏上就是一条不会消失的带）；cell 窗口同理。以前两处都漏了，
        // 因为 `one` 里存着上一帧的残值。
        for pc in lo..pclo {
            self.one[pc * CSTR + ulo..pc * CSTR + ulo + (uhi + 1 - ulo)].fill(0);
        }
        for pc in pchi + 1..=hi {
            self.one[pc * CSTR + ulo..pc * CSTR + ulo + (uhi + 1 - ulo)].fill(0);
        }
        for pc in pclo..=pchi {
            let b = pc * CSTR;
            if ulo < slo {
                self.one[b + ulo..b + slo].fill(0);
            }
            if uhi > shi {
                self.one[b + shi + 1..b + uhi + 1].fill(0);
            }
        }
        self.lit = Some((pclo, pchi, slo, shi));
        let tw = t0.elapsed();
        let r = self.emit_cells(lo, hi, ulo, uhi);
        if dbg {
            self.dbg_pans += 1;
            eprintln!(
                "[panel] pan#{} 列 {}..{} cell {}..{} {:.1} ms / 合计 {:.1} ms",
                self.dbg_pans, lo, hi, ulo, uhi,
                tw.as_secs_f64() * 1e3,
                t0.elapsed().as_secs_f64() * 1e3
            );
        }
        r
    }

    /// 调试：跳过编码，直接给所有内容 cell 灌同一个 2-bit 码，播 `n` 帧。
    ///
    /// 用来把「写 fb + 翻页」和「波形编码」分开——这个能把屏推黑，
    /// 就说明底层通路是好的，问题在编码侧。
    pub fn raw_frames(&mut self, code: u16, n: usize) -> Result<(), String> {
        let v = match code {
            0 => 0x0000u16,
            1 => 0x5555,
            2 => 0xAAAA,
            other => other,
        };
        self.one.fill(v);
        for _ in 0..n {
            self.show_frame_all()?;
        }
        // 故意不补空白帧：raw 测试要看的就是"最后一帧到底有没有被扫出去"。
        Ok(())
    }

    pub fn fb_line_length(&self) -> u32 {
        self.raw.line_length
    }
    pub fn fb_yres_virtual(&self) -> u32 {
        self.raw.yres_virtual
    }
    pub fn fb_smem_start(&self) -> u32 {
        self.raw.smem_start
    }

    /// 调试：往指定槽写一整帧同一个驱动码（不翻页）。
    pub fn debug_write_code(&mut self, slot: usize, code: u16) {
        let v = code.wrapping_mul(0x5555);
        self.one.fill(v);
        encode::put_cols(&mut self.slab, &self.one, 0, COLS - 1);
        self.raw.write_slot_at(slot, &self.slab);
    }

    /// 调试：翻到指定槽（不写）。
    pub fn debug_pan_to(&mut self, slot: usize) -> Result<(), String> {
        self.raw.pan_to(slot).map_err(|e| e.to_string())?;
        self.cur_slot = slot;
        Ok(())
    }

    /// 调试：读回槽 0 里第 `word` 个 32 位字（验证写入有没有真的落到 fb）。
    pub fn peek_slot(&self, word: usize) -> u32 {
        self.raw.read_word(word)
    }

    /// 调试：不驱动面板，直接列出某个 (目标, 上一帧) 组合逐相位的驱动码。
    ///
    /// 这是验证「wbf 解码 + LUT 索引」最直接的手段：GC16 从白到黑应该是
    /// 一串 0、再一串 1、最后一个 0。
    pub fn debug_pair_codes(&self, mode: Mode, tgt: u8, prv: u8) -> Result<Vec<u8>, String> {
        let mi = self
            .wf
            .mode_index(mode.name(), 0)
            .ok_or_else(|| format!("没有 {} 模式", mode.name()))?;
        let addr = self
            .wf
            .table_addr(mi, self.band)
            .ok_or_else(|| "没有表地址".to_string())?;
        let tab = self.wf.decode_table(addr)?;
        let nph = tab.len() / BYTES_PER_PHASE;
        let mut t = [0u8; 256];
        let key = ((tgt & 0xF) << 4) | (prv & 0xF);
        let mut out = Vec::with_capacity(nph);
        for ph in 0..nph {
            encode::prep(&tab[ph * BYTES_PER_PHASE..], &mut t);
            out.push(t[key as usize]);
        }
        Ok(out)
    }

    /// 写一帧全 0（无驱动）并翻页。**每次更新结束都要调用。**
    pub fn blank(&mut self) -> Result<(), String> {
        let Some((a, b, _, _)) = self.lit.take() else {
            // 槽里本来就没有任何驱动，再翻一页没有意义。
            return Ok(());
        };
        for pc in a..=b {
            let base = (4 + pc) * CELLS + NCELL;
            for v in &mut self.slab[base..base + CSTR] {
                *v &= 0xFFFF_0000;
            }
        }
        self.chg = None;
        self.emit()
    }

    /// 设置目标图像里的一块矩形（0 = 黑，15 = 白）。
    ///
    /// `levels` 是**列主序**（`levels[col*h + row]`），和 `img`/`prv`/`kp`
    /// 的布局（`x*UH + y`）一致——所以这里就是三条连续流，没有任何转置。
    ///
    /// 转置搬到了 UI 线程（`Fb::refresh`）。原因：面板线程一个相位只有
    /// 11.93 ms，而转置一个整屏矩形实测要 **46.5 ms**（17 ns/像素）——
    /// 波形会被拉长 4 倍。搬过去以后它和面板的 vsync 等待并行，不进帧预算。
    ///
    /// 顺手把 `kp = (目标 << 4) | 波形起点` 一起算掉：三个缓冲的同一段地址
    /// 都在 cache 里，比之后单独再扫一遍（实测 3.0 ms/帧）便宜得多。
    ///
    /// **这里决定屏上有没有拖尾**，规则只有两条：
    ///
    /// * 目标**变了**的像素：`prv = 旧目标`、`kp = (新目标 << 4) | 旧目标`、
    ///   相位归零重开一轮波形。
    /// * 目标**没变**的像素：`prv`/`kp`/相位一律不动 —— 它可能正处在上一轮
    ///   波形中途，动了就等于掐掉它的驱动。
    ///
    /// 为什么 `prv` 要取"旧目标"而不是"上一帧真的驱到哪"：波形被打断时我们
    /// 不知道墨走到哪了，但**旧目标**是上一轮一直在推的方向，按它当起点重新
    /// 推满一轮总能饱和到新目标。以前 `prv` 取的是"上次提交值"，于是
    /// 反色方块压过棋盘格黑方块（推成白）之后走开，目标回到黑、`prv` 还是黑，
    /// 键变成 `黑←黑`、DU 查表是 **0（不驱动）** —— 那个黑方块就永远白着。
    /// 屏上看到的就是"浓脱尾 + 一串白色小方块"。
    pub fn set_rect(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        levels: &[u8],
        mi: u8,
    ) -> Option<(usize, usize, usize, usize)> {
        debug_assert_eq!(levels.len(), w * h);
        let lstride = h; // 列距永远是调用者给的 h
        let w = w.min(UW.saturating_sub(x));
        let h = h.min(UH.saturating_sub(y));
        if w == 0 || h == 0 {
            return None;
        }
        /// 低 4 位掩码（8 个字节一起处理用）。
        const M0F: u64 = 0x0F0F_0F0F_0F0F_0F0F;
        let cur = self.cur;
        let (mut bx0, mut by0, mut bx1, mut by1) = (usize::MAX, usize::MAX, 0usize, 0usize);
        for col in 0..w {
            let d = (x + col) * UH + y;
            let lv = &levels[col * lstride..col * lstride + h];
            let (pv, iv, kv) = (
                &mut self.prv[d..d + h],
                &mut self.img[d..d + h],
                &mut self.kp[d..d + h],
            );
            let cx = (x + col) * CSTR;
            let mut clo = usize::MAX;
            let mut chi = 0usize;
            let mut i = 0;
            // 8 个像素一起判"目标变了没有"。逐字节写时 LLVM 不肯自动向量化
            // （三个切片混在一起，别名分析过不去），A7 上就是每个像素 5~6 个
            // 周期；改成 u64 一次 8 像素后整屏 set_rect 从 22.5 ms 降到个位数。
            while i + 8 <= h {
                let sw = u64::from_le_bytes(lv[i..i + 8].try_into().unwrap());
                let iw = u64::from_le_bytes(iv[i..i + 8].try_into().unwrap());
                if ((sw ^ iw) & M0F) != 0 {
                    for j in 0..8 {
                        let l = lv[i + j] & 0xF;
                        let old = iv[i + j] & 0xF;
                        if l != old {
                            pv[i + j] = old;
                            iv[i + j] = l;
                        }
                    }
                    for j in 0..8 {
                        kv[i + j] = ((iv[i + j] & 0xF) << 4) | (pv[i + j] & 0xF);
                    }
                    // 这一小块（8 行）覆盖到的内容 cell：相位归零。
                    let s_lo = (UH - 1 - (y + i + 7)) / 8;
                    let s_hi = (UH - 1 - (y + i)) / 8;
                    for s in s_lo..=s_hi {
                        self.phc[cx + s] = cur;
                        self.pmc[cx + s] = mi;
                    }
                    if clo == usize::MAX {
                        clo = i;
                    }
                    chi = i + 8;
                }
                i += 8;
            }
            while i < h {
                let l = lv[i] & 0xF;
                let old = iv[i] & 0xF;
                if l != old {
                    pv[i] = old;
                    iv[i] = l;
                    kv[i] = (l << 4) | old;
                    let s = (UH - 1 - (y + i)) / 8;
                    self.phc[cx + s] = cur;
                    self.pmc[cx + s] = mi;
                    if clo == usize::MAX {
                        clo = i;
                    }
                    chi = i + 1;
                }
                i += 1;
            }
            if clo != usize::MAX {
                bx0 = bx0.min(x + col);
                by0 = by0.min(y + clo);
                bx1 = bx1.max(x + col + 1);
                by1 = by1.max(y + chi);
            }
        }
        if bx0 == usize::MAX {
            return None;
        }
        self.union_chg(bx0, by0, bx1, by1);
        // 有 cell 被盖上了新戳 → 整块已不再同相位。
        self.uni = None;
        Some((bx0, by0, bx1 - bx0, by1 - by0))
    }

    /// 取走当前累积的追踪码（`(坐标, 逐帧码)`）。
    pub fn take_trace(&mut self) -> Option<(String, String)> {
        let (x, y, buf) = self.trace.as_mut()?;
        if buf.is_empty() {
            return None;
        }
        let out = (format!("({x},{y})"), std::mem::take(buf));
        Some(out)
    }

    /// 把一块矩形并进「变了」的包围盒。
    fn union_chg(&mut self, x0: usize, y0: usize, x1: usize, y1: usize) {
        if x0 >= x1 || y0 >= y1 {
            return;
        }
        self.chg = Some(match self.chg {
            Some((a, b, c, d)) => (a.min(x0), b.min(y0), c.max(x1), d.max(y1)),
            None => (x0, y0, x1, y1),
        });
    }

    /// 整块都不许收缩（`UPDATE_MODE_FULL` 要的是把整块重刷一遍）。
    ///
    /// 除了不让驱动矩形收缩，还得把**相位也归零**：整块重刷的意思是
    /// "这块的每个像素都从相位 0 完整吃一轮"，包括那些目标值没变的
    /// （GC16 对"白←白"也会推黑再推白，那正是清残影的动作）。
    pub fn force_changed(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        mi: u8,
    ) -> Option<(usize, usize, usize, usize)> {
        let x1 = (x + w).min(UW);
        let y0 = y.min(UH);
        let y1 = (y + h).min(UH);
        if x >= x1 || y0 >= y1 {
            return None;
        }
        let cur = self.cur;
        let n = y1 - y0;
        let (slo, shi) = encode::cell_window(y0, n);
        for col in x..x1 {
            let base = col * UH + y0;
            // 整块重刷：波形的起点就是自己的目标（屏上此刻已是这个值）。
            self.prv[base..base + n].copy_from_slice(&self.img[base..base + n]);
            for i in 0..n {
                let g = self.img[base + i] & 0xF;
                self.kp[base + i] = (g << 4) | g;
            }
            for s in slo..=shi {
                self.phc[col * CSTR + s] = cur;
                self.pmc[col * CSTR + s] = mi;
            }
        }
        self.uni = Some((x, x1, slo, shi, cur, mi));
        self.union_chg(x, y0, x1, y1);
        Some((x, y0, x1 - x, y1 - y0))
    }

    /// 驱动眼里的"屏上应该是什么"（`img`，x-major 1 字节/像素，0 黑 15 白）。
    ///
    /// 调试用：`RM_PANEL_DUMP_IMG=/tmp/img.raw` 每轮落一份。屏上缺的块到底是
    /// "没驱到位"还是"驱动压根以为它是白的"，把这份摊成图一对照就分开了。
    pub fn img_bytes(&self) -> &[u8] {
        &self.img
    }

    /// 这一轮真正变了的那块矩形；`None` = 没有任何变化，什么都不用驱。
    pub fn changed(&self) -> Option<(usize, usize, usize, usize)> {
        self.chg
    }

    /// 整轮提交完（屏上已经收敛）之后清掉，下一轮重新攒。
    pub fn clear_changed(&mut self) {
        self.chg = None;
    }

    /// 局部刷新：只编码脏列，播 `frames` 个相位（0 = 用满该温度档的全部相位）。
    ///
    /// 返回 `(相位数, 编码耗时 ms, 播放耗时 ms)`。
    pub fn update(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        mode: Mode,
        frames: usize,
        blank_after: bool,
    ) -> Result<(usize, f64, f64), String> {
        let t0 = std::time::Instant::now();
        if w == 0 || h == 0 || x >= UW || y >= UH {
            return Ok((0, 0.0, 0.0));
        }
        let w = w.min(UW - x);
        let h = h.min(UH - y);

        let mi = self
            .wf
            .mode_index(mode.name(), 0)
            .ok_or_else(|| format!("波形里没有 {} 模式", mode.name()))?;
        let addr = self
            .wf
            .table_addr(mi, self.band)
            .ok_or_else(|| format!("{} 在 band {} 没有表地址", mode.name(), self.band))?;
        let tab = self.wf.decode_table(addr)?;
        let nph = tab.len() / BYTES_PER_PHASE;
        if nph == 0 {
            return Err(format!("{} 的波形表是空的", mode.name()));
        }
        let play = if frames == 0 { nph } else { frames.min(nph) };

        // 面板列是用户 x 的镜像：pc = COLS-1-x。
        let pclo = COLS - (x + w);
        let pchi = COLS - 1 - x;

        encode::build_kp_cols(&self.img, &self.prv, &mut self.kp, pclo, pchi, y, y + h);

        // 第一段：把所有相位编进 RAM（这一步慢没关系，还没往面板上推）。
        let (slo, shi) = encode::cell_window(y, h);
        let all = COLS * CSTR;
        self.phases.clear();
        self.phases.resize(play * all, 0);
        let mut t = [0u8; 256];
        let mut used = vec![false; play];
        for ph in 0..play {
            encode::prep(&tab[ph * BYTES_PER_PHASE..], &mut t);
            let from = ph * all + pclo * CSTR;
            let to = ph * all + (pchi + 1) * CSTR;
            used[ph] = encode::encode_cells_range(
                &t,
                &self.kp,
                &mut self.phases[from..to],
                pclo,
                pchi,
                slo,
                shi,
            );
        }
        let enc_ms = t0.elapsed().as_secs_f64() * 1e3;

        // 剪掉首尾"整相位没有任何驱动"的部分。
        //
        // GL16/GC16 的前 21 个相位对**任何** (目标,上一帧) 都是 0；没变白的区域
        // 更是整轮全 0。这些相位不该占帧——剪掉它们不改变物理效果，只省时间。
        let first = used.iter().position(|&b| b);
        let last = used.iter().rposition(|&b| b);
        let (first, last) = match (first, last) {
            (Some(a), Some(b)) => (a, b),
            _ => {
                // 整轮没有任何驱动：什么都不会变，直接返回。
                if blank_after {
                    self.blank()?;
                }
                if play >= nph {
                    for col in x..x + w {
                        let base = col * UH + y;
                        self.prv[base..base + h].copy_from_slice(&self.img[base..base + h]);
                    }
                    self.save_rect(x, y, w, h);
                }
                return Ok((play, enc_ms, 0.0));
            }
        };

        // 第二段：严格一个相位一帧地播。
        let t1 = std::time::Instant::now();
        for ph in first..=last {
            let from = ph * all + pclo * CSTR;
            let to = ph * all + (pchi + 1) * CSTR;
            self.one[pclo * CSTR..(pchi + 1) * CSTR].copy_from_slice(&self.phases[from..to]);
            self.show_frame_cols(pclo, pchi, slo, shi)?;
        }
        if blank_after {
            self.blank()?;
        }

        // 只有跑满相位才能认为像素到位；截断的不动 prv（让后续更新继续叠加驱动）。
        if play >= nph {
            for col in x..x + w {
                let base = col * UH + y;
                self.prv[base..base + h].copy_from_slice(&self.img[base..base + h]);
            }
            // 两段式走完一整轮：把相位时间戳推到"已经过期"，否则后面按 cell
            // 相位编码时会以为这块还想从相位 0 重来一遍。
            let stamp = self.cur.wrapping_sub(play as u32);
            for col in x..x + w {
                for s in slo..=shi {
                    self.phc[col * CSTR + s] = stamp;
                }
            }
            self.save_rect(x, y, w, h);
        }
        Ok((play, enc_ms, t1.elapsed().as_secs_f64() * 1e3))
    }

    /// 整屏刷新（`mode` 的全部相位）。
    pub fn update_full(&mut self, mode: Mode) -> Result<(usize, f64, f64), String> {
        self.update(0, 0, UW, UH, mode, 0, true)
    }

    // ---------------------------------------------------------- 状态文件

    fn save_rect(&mut self, x: usize, y: usize, w: usize, h: usize) {
        // 布局是 x-major，一个矩形在文件里不是连续的：窄条就按列写
        // （w 次 pwrite），宽了就整份重写一次——2.6 MB 顺序写比上千次小
        // pwrite 快得多。
        if w > 200 {
            self.save_all();
            return;
        }
        let Some(f) = self.state.as_ref() else { return };
        for col in x..x + w {
            let base = col * UH + y;
            let _ = f.write_at(&self.prv[base..base + h], base as u64);
        }
    }

    fn save_all(&mut self) {
        let Some(f) = self.state.as_ref() else { return };
        let _ = f.write_at(&self.prv, 0);
    }

    /// 把一块矩形标记为"已到位"（`prv = img`）并落盘。
    ///
    /// 手写是"同一块区域反复推几帧"，驱动线程自己记着窗口；等这块滑出窗口时
    /// 再提交，这样它既拿到了足够帧数，又不会在后续刷新里被重复判成"有变化"。
    pub fn commit_rect(&mut self, x: usize, y: usize, w: usize, h: usize) {
        let x1 = (x + w).min(UW);
        let y1 = (y + h).min(UH);
        for col in x..x1 {
            let base = col * UH + y;
            self.prv[base..col * UH + y1].copy_from_slice(&self.img[base..col * UH + y1]);
        }
        self.save_rect(x, y, (x1 - x) as usize, y1 - y);
    }

    /// 某模式在当前温度档有多少个相位。
    pub fn phases(&mut self, mode: Mode) -> Result<usize, String> {
        Ok(self.table(mode)?.len() / BYTES_PER_PHASE)
    }

    fn table(&mut self, mode: Mode) -> Result<&[u8], String> {
        let mi = self
            .wf
            .mode_index(mode.name(), 0)
            .ok_or_else(|| format!("波形里没有 {} 模式", mode.name()))?;
        let key = (mi, self.band);
        if !self.tables.contains_key(&key) {
            let addr = self
                .wf
                .table_addr(mi, self.band)
                .ok_or_else(|| format!("{} 在 band {} 没有表地址", mode.name(), self.band))?;
            let tab = self.wf.decode_table(addr)?;
            self.tables.insert(key, tab);
        }
        Ok(&self.tables[&key])
    }

    /// 目标图变了之后调一次：只重算脏列的 `(目标, 上一帧)` 键。
    pub fn prepare(&mut self, x: usize, y: usize, w: usize, h: usize) {
        if w == 0 || h == 0 || x >= UW || y >= UH {
            return;
        }
        let w = w.min(UW - x);
        let pclo = COLS - (x + w);
        let pchi = COLS - 1 - x;
        encode::build_kp_cols(&self.img, &self.prv, &mut self.kp, pclo, pchi, y, y + h);
    }

    /// 某个模式在波形文件里的下标（`pmc` 里存的就是它）。
    pub fn mode_slot(&mut self, mode: Mode) -> Option<u8> {
        self.wf.mode_index(mode.name(), 0).map(|v| v as u8)
    }

    /// 把每个模式的相位矩阵一次建好（8 张 × 47 × 256 ≈ 96 KB）。
    ///
    /// 只在第一次编码时建。`luts[mi]` 的下标就是 `pmc` 里存的值，所以编码
    /// 循环里一次索引就能拿到"这个 cell 该吃哪张表"，不用查 HashMap。
    fn build_luts(&mut self) -> Result<(), String> {
        if !self.luts.is_empty() {
            return Ok(());
        }
        let mut out: Vec<(usize, Vec<u8>)> = Vec::new();
        for mode in [Mode::Du, Mode::A2, Mode::Du4, Mode::Gc16, Mode::Gl16] {
            let Ok(mi) = self.wf.mode_index(mode.name(), 0).ok_or(()) else {
                continue;
            };
            let Ok(tab) = self.table(mode).map(|t| t.to_vec()) else {
                continue;
            };
            let nph = tab.len() / BYTES_PER_PHASE;
            if nph == 0 {
                continue;
            }
            let mut lut = vec![0u8; (nph + 1) * 256];
            for p in 0..nph {
                let mut t = [0u8; 256];
                encode::prep(&tab[p * BYTES_PER_PHASE..], &mut t);
                lut[p * 256..p * 256 + 256].copy_from_slice(&t);
            }
            while out.len() <= mi {
                out.push((1, vec![0u8; 2 * 256]));
            }
            out[mi] = (nph, lut);
        }
        if out.is_empty() {
            return Err("没有可用的波形表".into());
        }
        // 零化版：键的两个半字节相等 → 一整行全 0（这个像素这一轮不用驱）。
        let mut out2 = Vec::with_capacity(out.len());
        for (nph, lut) in &out {
            let mut l2 = lut.clone();
            for p in 0..=*nph {
                for k in 0..256usize {
                    if (k >> 4) == (k & 0xF) {
                        l2[p * 256 + k] = 0;
                    }
                }
            }
            out2.push((*nph, l2));
        }
        self.luts2 = out2;
        self.luts = out;
        Ok(())
    }

    /// 某个 cell 还要不要继续驱（`age < 它自己那张表的相位数`）。
    fn cell_live(&self, age: u32, m: u8) -> bool {
        if self.luts.is_empty() {
            return false;
        }
        let m = (m & 0x7F).min((self.luts.len() - 1) as u8) as usize;
        (age as usize) < self.luts[m].0
    }

    /// 编一帧进 `one`（不碰面板）。**相位不再由调用者给** —— 每个 cell 按
    /// 自己的时间戳算（`cur - phc`），所以同一帧里不同 cell 可以处在不同
    /// 相位，正是官方 `WorkItem` 各自带 `phase` 的效果。
    ///
    /// 和 `show_pending` 分开是为了流水线：pan 完这一帧，立刻编下一帧，
    /// 编码吃的是面板扫描时间，不占帧周期。
    pub fn encode_phase(
        &mut self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
    ) -> Result<(), String> {
        let w = w.min(UW.saturating_sub(x));
        if w == 0 || h == 0 || x >= UW || y >= UH {
            return Ok(());
        }
        let pclo = COLS - (x + w);
        let pchi = COLS - 1 - x;
        self.build_luts()?;
        // 脏行折成 cell 区间 —— 一个 cell 覆盖 8 个用户行。
        let (slo, shi) = encode::cell_window(y, h);
        // 整块同相位**且同波形**：整帧只有一张表，走最快那条编码路径。
        // 整屏 GC16 永远命中这里，这是 541 ms 能不能达到的关键。
        if let Some((u0, u1, s0, s1, st, umi)) = self.uni {
            let uage = self.cur.wrapping_sub(st);
            let umi = (umi & 0x7F).min((self.luts.len() - 1) as u8) as usize;
            let (unph, ulut) = &self.luts[umi];
            let unph = *unph;
            if (uage as usize) < unph && x >= u0 && x + w <= u1 && slo >= s0 && shi <= s1 {
                let mut t = [0u8; 256];
                t.copy_from_slice(&ulut[uage as usize * 256..uage as usize * 256 + 256]);
                if !encode::encode_cells_range(&t, &self.kp, &mut self.one, pclo, pchi, slo, shi) {
                    self.nodrive += 1;
                }
                return Ok(());
            }
        }
        if !encode::encode_cells_range_multi(
            &self.luts2,
            &self.pmc,
            &self.kp,
            &self.phc,
            self.cur,
            &mut self.one,
            pclo,
            pchi,
            slo,
            shi,
        ) {
            self.nodrive += 1;
        }
        Ok(())
    }

    /// 把 `one` 里准备好的这一帧写进槽、翻页（阻塞一个帧周期）。
    pub fn show_pending(&mut self, x: usize, y: usize, w: usize, h: usize) -> Result<(), String> {
        let w = w.min(UW.saturating_sub(x));
        if w == 0 || h == 0 || x >= UW || y >= UH {
            return Ok(());
        }
        let h = h.min(UH - y);
        let pclo = COLS - (x + w);
        let pchi = COLS - 1 - x;
        let (slo, shi) = encode::cell_window(y, h);
        self.show_frame_cols(pclo, pchi, slo, shi)
    }

    /// 这块矩形里还有没有「相位没吃完」的 cell（`cur - phc < nph`）。
    ///
    /// 有就返回它们的包围盒（用户坐标 `(x, y, w, h)`）。喂帧循环在窗口滚空
    /// 之后用它兜底再推一轮 —— 这是「一个像素必须吃满一整轮波形」的强制
    /// 保证：任何被盖过相位戳的 cell，都一定要被驱到 `nph` 帧，否则 `img`
    /// 会先更新成目标、`prv`/相位却停在半路，屏上就是"画了没反应"，而且
    /// 之后每次刷新都会判"没变化"，永久错下去。
    ///
    /// 代价是每轮结束时扫一遍这一轮脏区的 cell（整屏 33 万个 = 0.3 ms）。
    pub fn unfinished(
        &self,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
    ) -> Option<(usize, usize, usize, usize)> {
        if self.luts.is_empty() || w == 0 || h == 0 || x >= UW || y >= UH {
            return None;
        }
        let x1 = (x + w).min(UW);
        let (slo, shi) = encode::cell_window(y, h.min(UH - y));
        let mut acc: Option<(usize, usize, usize, usize)> = None;
        for col in x..x1 {
            for s in slo..=shi {
                let age = self.cur.wrapping_sub(self.phc[col * CSTR + s]);
                if !self.cell_live(age, self.pmc[col * CSTR + s]) {
                    continue;
                }
                let y0 = UH - 8 - s * 8;
                let y1 = UH - s * 8;
                acc = Some(match acc {
                    Some((a, b, c, d)) => (a.min(col), b.min(y0), c.max(col + 1), d.max(y1)),
                    None => (col, y0, col + 1, y1),
                });
            }
        }
        acc.map(|(a, b, c, d)| (a, b, c - a, d - b))
    }

    /// 调试：把整屏的**目标图**重置成某个灰度，同时把相位/波形戳全部作废。
    ///
    /// 基准测试要用它把每一档拉回同一个起点：`img` 一改，后面的 `set_rect`
    /// 才会真的判"变了"、才会盖上新戳，否则第二档全是"没变化"，测出来是 0。
    pub fn debug_reset_img(&mut self, level: u8) {
        let l = level & 0xF;
        self.img.fill(l);
        self.prv.fill(l);
        self.kp.fill(l << 4 | l);
        self.phc.fill(PHC_NONE);
        self.pmc.fill(0);
        self.uni = None;
        self.chg = None;
    }

    /// 调试：把 `prv` 强制设成某个灰度（用来单独验证编码，不受历史状态影响）。
    pub fn debug_reset_prv(&mut self, level: u8) {
        self.prv.fill(level & 0xF);
    }

    /// 调试：把整屏所有 cell 的相位强制设成 `ph`
    /// （配合 `debug_reset_prv` 单独验证某一个相位的编码结果）。
    pub fn debug_set_phase(&mut self, mode: Mode, ph: usize) {
        let _ = self.build_luts();
        let mi = self.mode_slot(mode).unwrap_or(0);
        let stamp = self.cur.wrapping_sub(ph as u32);
        self.phc.fill(stamp);
        self.pmc.fill(mi);
        self.uni = Some((0, UW, 0, CSTR - 1, stamp, mi));
    }

    /// 调试：量「写 fb」和「翻页」各自的真实节奏，返回连续若干帧的间隔（ms）。
    ///
    /// `kind` 0 = 只翻页不写；1 = 整槽写 + 翻页；2 = 只写 12 列 + 翻页。
    /// 三个数一比就知道 pan 是"从调用起堵一帧"还是"堵到下一个帧边界"，
    /// 也就知道写 fb 到底是不是白花的串行时间。
    pub fn debug_pace(&mut self, kind: u8, n: usize) -> Vec<f64> {
        let (plo, phi) = (600usize, 612usize);
        let mut out = Vec::with_capacity(n);
        let mut last = std::time::Instant::now();
        for _ in 0..n {
            match kind {
                1 => {}
                2 => {
                    self.one.fill(0);
                    encode::put_cols(&mut self.slab, &self.one, plo, phi);
                }
                _ => {}
            }
            let _ = self.emit();
            let now = std::time::Instant::now();
            out.push(now.duration_since(last).as_secs_f64() * 1e3);
            last = now;
        }
        out
    }

    /// 一整套波形播完了：提交 `prv` 并落盘。
    pub fn commit(&mut self, x: usize, y: usize, w: usize, h: usize) {
        self.commit_rect(x, y, w, h);
    }

    /// 供 UI 层在退出时调用（`Drop` 里也会调）。
    pub fn flush_state(&mut self) {
        self.save_all();
    }
}

impl Drop for Panel {
    fn drop(&mut self) {
        // 无论怎么退出，先把屏幕留在"无驱动"状态，再落状态文件。
        let _ = self.blank();
        self.save_all();
    }
}

/// 打开状态文件；文件不存在或大小不对就当成"刚开机"，回一份全白状态。
///
/// 之所以按文件大小判断而不是读魔数：这个文件就是裸的 `UW×UH` 字节屏上状态，
/// 大小不对只可能是旧的/别的尺寸，重写即可。
fn open_state() -> (Option<File>, Vec<u8>, bool) {
    let p = state_path();
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&p)
        .ok();
    if let Some(file) = f {
        if file.metadata().map(|m| m.len() as usize).unwrap_or(0) == UW * UH {
            let mut prv = vec![0u8; UW * UH];
            if file.read_exact_at(&mut prv, 0).is_ok() {
                return (Some(file), prv, false);
            }
        }
        // 大小不对：按"全白"重建。
        let prv = vec![15u8; UW * UH];
        let _ = file.set_len(0);
        let _ = file.write_at(&prv, 0);
        return (Some(file), prv, true);
    }
    (None, vec![15u8; UW * UH], true)
}
