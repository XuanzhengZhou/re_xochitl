//! `/dev/fb0`（mxsfb/LCDIF）——整个面板驱动里**唯一**有 `unsafe` 的地方。
//!
//! 这里只做四件事：打开设备、把 var 改成 260×1408@32bpp 的面板几何、
//! mmap 出两个槽、按帧 pan。所有业务逻辑都在 `encode`/`wbf` 里，纯安全代码。
//!
//! 两个必须守住的约束（`epd.c` 时代踩出来的）：
//!
//! 1. **只能整字 32 位写**。这块映射对半字/`memcpy`（可能被编译成 64/128 位搬运）
//!    会静默丢数据——读回看着是对的，屏上一动不动。所以这里用
//!    `write_volatile::<u32>`，volatile 语义禁止编译器把相邻写合并成宽写。
//! 2. **`FBIOPAN_DISPLAY` 成功时返回正数**（剩余 jiffies，常见 48/49），
//!    不是 0。判失败只能用 `< 0`。

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};

use super::SLOT;

const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;
const FBIOPUT_VSCREENINFO: libc::c_ulong = 0x4601;
const FBIOGET_FSCREENINFO: libc::c_ulong = 0x4602;
const FBIOPAN_DISPLAY: libc::c_ulong = 0x4606;
/// `FBIOBLANK`：0 = 退出省电（unblank）。
///
/// 这一条是面板的**电源开关**：lcdif 节点的 `lcd-supply`/`lcd2-supply`
/// 就是面板的 VCOM/VDD/VEE/VGH/VGL。官方 xochitl 每次开始画之前都会
/// `FBIOBLANK(0)`；它退出时会掉电。少了这一条，码写得再对屏也不动（全白）。
const FBIOBLANK: libc::c_ulong = 0x4611;
const FB_BLANK_UNBLANK: libc::c_int = 0;

/// 28.8 ns/像素：配 263 htotal × 1553 vtotal → 11.76 ms/帧（85 Hz）。
const PIXCLK: u32 = 0x7080;
/// 下边距。143 这个数是官方时序，改小会让 vtotal 变、帧率跟着变。
const LOWMARG: u32 = 143;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Bitfield {
    offset: u32,
    length: u32,
    msb_right: u32,
}

/// `struct fb_var_screeninfo`（arm32，160 字节）。
#[repr(C)]
#[derive(Clone, Copy)]
struct Var {
    xres: u32,
    yres: u32,
    xres_virtual: u32,
    yres_virtual: u32,
    xoffset: u32,
    yoffset: u32,
    bits_per_pixel: u32,
    grayscale: u32,
    red: Bitfield,
    green: Bitfield,
    blue: Bitfield,
    transp: Bitfield,
    nonstd: u32,
    activate: u32,
    height: u32,
    width: u32,
    accel_flags: u32,
    pixclock: u32,
    left_margin: u32,
    right_margin: u32,
    upper_margin: u32,
    lower_margin: u32,
    hsync_len: u32,
    vsync_len: u32,
    sync: u32,
    vmode: u32,
    rotate: u32,
    colorspace: u32,
    reserved: [u32; 4],
}

const _: () = assert!(std::mem::size_of::<Var>() == 160);

impl Default for Var {
    fn default() -> Self {
        // 全零是合法值（都是整数/位域），不需要更精细的默认。
        unsafe { std::mem::zeroed() }
    }
}

/// `struct fb_fix_screeninfo`（arm32，68 字节）。只用得到 id/smem_start/line_length。
#[repr(C)]
#[derive(Clone, Copy)]
struct Fix {
    id: [u8; 16],
    smem_start: u32,
    smem_len: u32,
    fb_type: u32,
    type_aux: u32,
    visual: u32,
    xpanstep: u16,
    ypanstep: u16,
    ywrapstep: u16,
    _pad: u16,
    line_length: u32,
    mmio_start: u32,
    mmio_len: u32,
    accel: u32,
    capabilities: u16,
    reserved: [u16; 2],
}

impl Default for Fix {
    fn default() -> Self {
        unsafe { std::mem::zeroed() }
    }
}

fn get_var(fd: RawFd) -> io::Result<Var> {
    let mut v = Var::default();
    let r = unsafe { libc::ioctl(fd, FBIOGET_VSCREENINFO, &mut v as *mut Var) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(v)
    }
}

/// 读出 LCDIF 当前在扫哪一槽，作为我们写帧的基准。
///
/// 为什么必须问硬件、不能假设槽 0：这块 fb 有两槽，而 `FBIOPAN_DISPLAY`
/// 只在内核里存着的 `yoffset` 变化时才真正改 `NEXT_BUF`；帧结束的中断又会把
/// `NEXT_BUF` 改回它记着的那个槽。实测开机后 `CUR_BUF = smem_start + SLOT`
/// （= 槽 1），此时写槽 0 的后果是：屏上是上一个进程留下的旧画面，我们的
/// 驱动只是把它"洗"成灰色，局部永远不刷新。
///
/// 打个比方：这就像往一块没人看的画布上画，然后纳闷屏幕怎么不变。
fn probe_slot(smem_start: u32) -> usize {
    const LCDIF: libc::off_t = 0x3073_0000;
    const CUR_BUF: usize = 0x40;
    let fallback = 0usize;
    let Ok(f) = File::open("/dev/mem") else {
        return fallback;
    };
    let len = 0x100usize;
    let m = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            f.as_raw_fd(),
            LCDIF,
        )
    };
    if m == libc::MAP_FAILED {
        return fallback;
    }
    let cur = unsafe { std::ptr::read_volatile((m as *const u32).add(CUR_BUF / 4)) };
    unsafe { libc::munmap(m, len) };
    if cur <= smem_start {
        return fallback;
    }
    let off = (cur - smem_start) as usize;
    // 落在槽 1 里就当槽 1，其余（含越界）一律退回槽 0。
    if (SLOT..2 * SLOT).contains(&off) {
        1
    } else {
        0
    }
}

/// 打开并 mmap 好的 LCDIF 帧缓冲。
pub struct RawFb {
    fd: File,
    /// 映射起点。
    mem: *mut u32,
    /// 面板实际在扫的那一槽（0/1）。**开机时从 LCDIF 的 `CUR_BUF` 读出来**，
    /// 不是猜的：这块 fb 有两槽，`FBIOPAN_DISPLAY` 只在内核里存的 `yoffset`
    /// 变化时才真正改 `NEXT_BUF`，而帧结束的中断会把 `NEXT_BUF` 改回它自己
    /// 记着的那个槽。写错槽的后果是"画面是旧的、还发灰、局部不刷新"——
    /// 我们驱的是一槽屏上根本没在扫的内存。
    slot: usize,
    /// 映射字节数（两个槽）。
    bytes: usize,
    var: Var,
    pub id: String,
    pub smem_start: u32,
    pub line_length: u32,
    pub yres_virtual: u32,
}

// 只在单线程里用（rmdev 的 UI 线程）；将来加 framegen 线程时再收紧。
unsafe impl Send for RawFb {}

impl RawFb {
    pub fn open() -> io::Result<RawFb> {
        let fd = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/fb0")
            .map_err(|e| io::Error::new(e.kind(), format!("/dev/fb0: {e}")))?;

        let mut fx = Fix::default();
        let r = unsafe { libc::ioctl(fd.as_raw_fd(), FBIOGET_FSCREENINFO, &mut fx as *mut Fix) };
        if r < 0 {
            return Err(io::Error::last_os_error());
        }

        // 改成面板几何：260 cell/行 × 1408 行，32bpp，两个槽。
        let mut v = get_var(fd.as_raw_fd())?;
        v.xres = super::CELLS as u32;
        v.yres = super::ROWS as u32;
        v.xres_virtual = super::CELLS as u32;
        v.yres_virtual = super::ROWS as u32 * 2;
        v.xoffset = 0;
        v.yoffset = 0;
        v.bits_per_pixel = 32;
        v.red = Bitfield {
            offset: 16,
            length: 8,
            msb_right: 0,
        };
        v.green = Bitfield {
            offset: 8,
            length: 8,
            msb_right: 0,
        };
        v.blue = Bitfield {
            offset: 0,
            length: 8,
            msb_right: 0,
        };
        v.pixclock = PIXCLK;
        v.left_margin = 1;
        v.right_margin = 1;
        v.upper_margin = 1;
        v.lower_margin = LOWMARG;
        v.hsync_len = 1;
        v.vsync_len = 1;
        v.sync = 0;
        v.vmode = 0; // FB_VMODE_NONINTERLACED
        v.activate = 0; // FB_ACTIVATE_NOW
        let r = unsafe { libc::ioctl(fd.as_raw_fd(), FBIOPUT_VSCREENINFO, &mut v as *mut Var) };
        if r < 0 {
            return Err(io::Error::new(
                io::Error::last_os_error().kind(),
                format!("FBIOPUT_VSCREENINFO: {}", io::Error::last_os_error()),
            ));
        }
        let var = get_var(fd.as_raw_fd())?;

        // 先把面板电打开。放在 mmap 之前：早开一秒，第一帧就能落地。
        let r = unsafe {
            libc::ioctl(
                fd.as_raw_fd(),
                FBIOBLANK,
                FB_BLANK_UNBLANK as libc::c_int,
            )
        };
        if r < 0 {
            eprintln!(
                "[panel] FBIOBLANK(UNBLANK) 失败：{}（面板可能仍是掉电状态）",
                io::Error::last_os_error()
            );
        }

        let bytes = SLOT * 2;
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                bytes,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        let id = String::from_utf8_lossy(&fx.id)
            .trim_end_matches('\0')
            .to_string();
        let slot = probe_slot(fx.smem_start);
        eprintln!(
            "[panel] LCDIF 正在扫槽 {}（CUR_BUF-SLOT 判出来的），写帧固定写这一槽",
            slot
        );
        Ok(RawFb {
            fd,
            mem: p as *mut u32,
            slot,
            bytes,
            var,
            id,
            smem_start: fx.smem_start,
            line_length: fx.line_length,
            yres_virtual: var.yres_virtual,
        })
    }

    /// 把一整槽（`SLOT` 字节）写进帧缓冲。
    ///
    /// 逐 32 位 `write_volatile`：绝不能换成 `copy_from_slice`/`memcpy`，
    /// 那些会被编译成宽搬运，写进去的东西面板收不到。
    pub fn write_slot(&self, src: &[u32]) {
        self.write_words(self.slot * (SLOT / 4), src);
    }

    /// 面板当前在扫的那一槽。
    pub fn slot(&self) -> usize {
        self.slot
    }

    /// 从第 `start` 个 32 位字开始写一段。
    ///
    /// 槽的布局是「行 = 面板列」，所以一次局部刷新只需要重写脏列对应的那几行
    /// （每行 1040 B），不用碰整个 1.46 MB——每帧能省掉 ~2 ms，把帧周期压回
    /// 11.76 ms（85 Hz）。
    pub fn write_words(&self, start: usize, src: &[u32]) {
        let limit = self.bytes / 4;
        if start >= limit {
            return;
        }
        let n = src.len().min(limit - start);
        for (i, &v) in src.iter().enumerate().take(n) {
            unsafe { std::ptr::write_volatile(self.mem.add(start + i), v) };
        }
    }

    /// 调试：读回映射里的某个 32 位字。
    pub fn read_word(&self, i: usize) -> u32 {
        if i * 4 + 4 > self.bytes {
            return 0;
        }
        unsafe { std::ptr::read_volatile(self.mem.add(i)) }
    }

    /// 翻到槽 0，并**停一个帧周期**（驱动里 `wait_for_completion_timeout`）。
    ///
    /// 返回正数是正常现象（剩余 jiffies）。
    pub fn pan(&mut self) -> io::Result<()> {
        let s = self.slot;
        self.pan_to(s)
    }

    /// 重新探一次"正在扫哪一槽"。
    ///
    /// `pan()` 之后内核一定把 `CUR_BUF` 指到新的那一槽，所以**喷完第一帧再探**
    /// 比开机时探可靠得多：开机那一刻 LCDIF 可能还没被程序化，`CUR_BUF` 是 0
    /// 或垃圾，`probe_slot` 就退回槽 0 —— 而我们写槽 0、面板扫槽 1，帧就白喷了。
    /// 真机症状：间歇性"帧发出去了但屏上一片白/淡灰"。
    pub fn reprobe(&mut self) -> usize {
        let s = probe_slot(self.smem_start);
        if s != self.slot {
            eprintln!(
                "[panel] 槽位纠正：{} → {}（pan 之后按 CUR_BUF 重探）",
                self.slot, s
            );
            self.slot = s;
        }
        s
    }

    /// 只把第 `pclo..=pchi` 个面板列对应的那几行写下去（每行 1040 B）。
    ///
    /// 槽的布局是「行 = 面板列」，所以局部刷新只需要碰脏列那几行，而不是
    /// 整个 1.46 MB。每帧省掉的 DRAM 流量对这块板子很关键：LCDIF 的扫描
    /// DMA 和我们抢同一条内存总线，写多了它会掉帧（实测帧周期从 11.76 被
    /// 顶到 82 ms）。
    pub fn write_rows(&self, slab: &[u32], pclo: usize, pchi: usize) {
        let start = (4 + pclo) * super::CELLS;
        let end = (5 + pchi) * super::CELLS;
        let base = self.slot * (SLOT / 4);
        self.write_words(base + start, &slab[start..end]);
    }

    /// 写到第 `slot` 槽（0/1）。
    pub fn write_slot_at(&self, slot: usize, src: &[u32]) {
        self.write_words(slot * (SLOT / 4), src);
    }

    /// 只写「列 `[pclo, pchi]` × 内容 cell `[slo, shi]`」那一块。
    ///
    /// 槽的布局是「行 = 面板列」：`(4 + pc) * 260` 是第 `pc` 列的起点，前 26 个
    /// 是命令行，后面才是 234 个内容 cell。所以一块矩形就是每列 `shi-slo+1`
    /// 个连续字 —— 笔迹那种「宽几百列、高几百行」的块，只脏 234 个 cell 里的
    /// 几十个，写下去的面积直接小一个数量级。
    pub fn write_cells(&self, slab: &[u32], pclo: usize, pchi: usize, slo: usize, shi: usize) {
        let shi = shi.min(super::CSTR - 1);
        if slo > shi {
            return;
        }
        let n = shi + 1 - slo;
        let base = self.slot * (SLOT / 4);
        for pc in pclo..=pchi {
            let off = (4 + pc) * super::CELLS + super::NCELL + slo;
            if off + n > slab.len() {
                break;
            }
            self.write_words(base + off, &slab[off..off + n]);
        }
    }

    /// 同 [`write_cells`]，但**顺便把一帧的驱动码并进去**：
    /// `word = (模板 & 0xFFFF_0000) | one[pc*CSTR + s]`。
    ///
    /// 以前是先把 `one` 合进 `slab`（整屏 1.3 MB 的读改写），再把 `slab`
    /// 写进 fb（又是 1.3 MB）—— 并到目标地址以后只剩一遍，整屏 GC16 从
    /// 903 ms 降到 620 ms。
    ///
    /// 试过用 `copy_nonoverlapping` 整列块写，**没有更快**（625 vs 620）：
    /// 限制在非缓存映射的写带宽（`/dev/fb0` ≈ 200 MB/s），不在指令。
    pub fn write_cells_merge(
        &self,
        slab: &[u32],
        one: &[u16],
        pclo: usize,
        pchi: usize,
        slo: usize,
        shi: usize,
    ) {
        let shi = shi.min(super::CSTR - 1);
        if slo > shi {
            return;
        }
        let n = shi + 1 - slo;
        let base = self.slot * (SLOT / 4);
        let limit = self.bytes / 4;
        for pc in pclo..=pchi {
            let off = (4 + pc) * super::CELLS + super::NCELL + slo;
            let dst = base + off;
            let ob = pc * super::CSTR + slo;
            if off + n > slab.len() || dst + n > limit || ob + n > one.len() {
                break;
            }
            let src = &slab[off..off + n];
            let lv = &one[ob..ob + n];
            for i in 0..n {
                let v = (src[i] & 0xFFFF_0000) | lv[i] as u32;
                unsafe { std::ptr::write_volatile(self.mem.add(dst + i), v) };
            }
        }
    }

    /// 把已经合成好的字（`tmp[pc*CSTR + s]`）**按列块**拷进槽。
    ///
    /// 以前是逐字 `write_volatile`：整屏 328k 个字，实测每帧 6.5 ms，占掉
    /// 一帧预算的 55%（帧周期被顶到 13.3 ms，46 相位 = 614 ms）。
    /// 改成块拷贝以后写合并缓冲能吃满，整屏掉到 ~1 ms 量级。
    ///
    /// 后面那个 `fence` 是必须的：写合并缓冲要保证在下一次 `pan` 之前
    /// 落进 DRAM，否则面板会扫到半新半旧的一帧（竖带）。
    pub fn write_cells_bulk_unused(&self, tmp: &[u32], pclo: usize, pchi: usize, slo: usize, shi: usize) {
        let shi = shi.min(super::CSTR - 1);
        if slo > shi {
            return;
        }
        let n = shi + 1 - slo;
        let base = self.slot * (SLOT / 4);
        let limit = self.bytes / 4;
        for pc in pclo..=pchi {
            let dst = base + (4 + pc) * super::CELLS + super::NCELL + slo;
            let ob = pc * super::CSTR + slo;
            if dst + n > limit || ob + n > tmp.len() {
                break;
            }
            unsafe {
                std::ptr::copy_nonoverlapping(tmp.as_ptr().add(ob), self.mem.add(dst), n);
            }
        }
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }

    /// 写某一列里 `[slo, shi)` 这一段的字。
    ///
    /// `/dev/fb0` 是非缓存映射，实测**每次 store 要 20 多个周期**：整屏
    /// 328k 个字 = 6.5 ms/帧，46 相位就是 300 ms，帧周期被顶到 13.3 ms。
    /// 所以这里的关键不是写得多快，而是**写多少**——调用方只把和上一帧
    /// 不同的列段丢进来（GC16 的 46 个相位里一大半是逐字相同的）。
    pub fn write_cell_run(&self, pc: usize, slo: usize, shi: usize, words: &[u32]) {
        let n = shi - slo;
        if n == 0 || words.len() < n {
            return;
        }
        let dst = self.slot * (SLOT / 4) + (4 + pc) * super::CELLS + super::NCELL + slo;
        if dst + n > self.bytes / 4 {
            return;
        }
        for i in 0..n {
            unsafe { std::ptr::write_volatile(self.mem.add(dst + i), words[i]) };
        }
    }

    /// 翻到第 `slot` 槽。
    ///
    /// 双缓冲必须这么翻：LCDIF 正在扫的那一槽不能被写（1.46 MB 写 2.6 ms，
    /// 等于一帧的 22%，正在扫的行会被撕成一条**竖带**——屏上看到的就是
    /// "一条竖条里还是上一屏的内容"）。写背面、翻过去，下次写另一槽。
    ///
    /// 另一个必要性：内核的防烧屏逻辑会在每次 pan 之后把 `NEXT_BUF` 指回
    /// 槽 1，好让用户态挂掉时面板不再被驱动。如果槽 1 是空的，那部分时间
    /// 面板就在扫一页全 0（不驱动）→ 又是"某段竖条不动"。两槽都放有效帧，
    /// 它愿意扫哪槽都行。
    pub fn pan_to(&mut self, slot: usize) -> io::Result<()> {
        self.var.yoffset = (slot as u32) * (super::ROWS as u32);
        let r = unsafe {
            libc::ioctl(
                self.fd.as_raw_fd(),
                FBIOPAN_DISPLAY,
                &mut self.var as *mut Var,
            )
        };
        if r < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

impl Drop for RawFb {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.mem as *mut libc::c_void, self.bytes);
        }
    }
}
