//! evdev 原始事件读取 + `EVIOCGABS` 量程查询。
//!
//! 只处理 reMarkable 2 的 `Wacom I2C Digitizer`（`/dev/input/event1`）。

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};

// `struct input_event` 在 32 位 ARM 上是 16 字节。
pub const EVENT_SIZE: usize = 16;

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_ABS: u16 = 0x03;

pub const SYN_REPORT: u16 = 0x00;
pub const SYN_DROPPED: u16 = 0x03;

pub const ABS_X: u16 = 0x00;
pub const ABS_Y: u16 = 0x01;
pub const ABS_PRESSURE: u16 = 0x18;
pub const ABS_DISTANCE: u16 = 0x19;
pub const ABS_TILT_X: u16 = 0x1a;
pub const ABS_TILT_Y: u16 = 0x1b;

pub const BTN_TOOL_PEN: u16 = 0x140;
pub const BTN_TOOL_RUBBER: u16 = 0x141;
pub const BTN_TOUCH: u16 = 0x14a;

const _IOC_READ: libc::c_ulong = 2 << 30;
/// `_IOR('E', 0x40 + axis, struct input_absinfo)`，struct 24 字节。
const EVIOCGABS: libc::c_ulong = _IOC_READ | ((24 as libc::c_ulong) << 16) | (0x45 << 8) | 0x40;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct AbsInfo {
    pub value: i32,
    pub minimum: i32,
    pub maximum: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

/// `EVIOCGBIT(EV_KEY, 96)`：按键能力位图（`KEY_MAX` = 0x2ff → 768 bit = 96 字节）。
const EVIOCGBIT_KEY: libc::c_ulong =
    _IOC_READ | ((96 as libc::c_ulong) << 16) | (0x45 << 8) | 0x21;

/// 这个输入设备支不支持某个按键码。
///
/// 用来**认出电源键**：`/dev/input/eventN` 的编号在不同固件里会变
/// （这台是 event0 = `snvs-powerkey`），按「谁有 KEY_POWER」找比写死路径稳。
pub fn has_key(fd: RawFd, code: u16) -> bool {
    let mut bits = [0u8; 96];
    let r = unsafe { libc::ioctl(fd, EVIOCGBIT_KEY, bits.as_mut_ptr() as *mut libc::c_void) };
    if r < 0 {
        return false;
    }
    let i = code as usize / 8;
    i < bits.len() && bits[i] & (1u8 << (code % 8)) != 0
}

/// 等一下这个 fd 可不可读（`poll(2)`，最多 `timeout_ms` 毫秒）。
///
/// 为什么需要：`read()` 是阻塞的。一个**再也不会来事件**的设备（注入脚本被 kill
/// 在半路、设备被解绑）会让线程永远堵在 `read()` 里，`ReleaseGuard` 的超时兜底
/// 根本没机会跑 —— 那正是"笔/手指一起没反应"的形态。所以线程用 `poll` 每
/// 250 ms 醒一次，静默时也能做超时判定。
///
/// 返回 `Ok(false)` = 等超时了、没有事件；设备挂断/出错返回 `Err`（调用方重建线程）。
pub fn poll_readable(fd: RawFd, timeout_ms: i32) -> io::Result<bool> {
    let mut p = libc::pollfd {
        fd,
        events: libc::POLLIN as libc::c_short,
        revents: 0,
    };
    let r = unsafe { libc::poll(&mut p, 1, timeout_ms) };
    if r < 0 {
        let e = io::Error::last_os_error();
        if e.kind() == io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(e);
    }
    if r == 0 {
        return Ok(false);
    }
    // 挂断/出错：别当成"没有事件"空转到天荒地老，交给外面重建
    if p.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
        return Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "poll: 设备已断开",
        ));
    }
    Ok(p.revents & libc::POLLIN != 0)
}

pub fn absinfo(fd: RawFd, axis: u16) -> io::Result<AbsInfo> {
    let mut info = AbsInfo::default();
    let req = EVIOCGABS + axis as libc::c_ulong;
    let r = unsafe { libc::ioctl(fd, req, &mut info as *mut AbsInfo) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(info)
    }
}

/// 一个解码后的 evdev 事件。
#[derive(Clone, Copy, Debug)]
pub struct Ev {
    pub typ: u16,
    pub code: u16,
    pub value: i32,
}

pub struct Evdev {
    file: File,
    buf: Vec<u8>,
    out: Vec<Ev>,
}

impl Evdev {
    pub fn open(path: &str) -> io::Result<Evdev> {
        let file = OpenOptions::new().read(true).open(path)?;
        Ok(Evdev {
            file,
            buf: vec![0u8; EVENT_SIZE * 64],
            out: Vec::with_capacity(64),
        })
    }

    pub fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    /// 最多等 `timeout_ms` 毫秒看有没有事件（不阻塞：静默时用来跑超时兜底）。
    pub fn poll(&self, timeout_ms: i32) -> io::Result<bool> {
        poll_readable(self.fd(), timeout_ms)
    }

    /// 阻塞读一批事件，按 16 字节固定布局逐个解析。
    ///
    /// **不能**把缓冲当成 `(i32, u16, u16, i32)` 直接切片——那个 tuple 只有 12 字节，
    /// 会和内核的 16 字节记录错位（踩过：整条事件流全是垃圾，屏幕上什么都不动）。
    pub fn read(&mut self) -> io::Result<&[Ev]> {
        let n = unsafe {
            libc::read(
                self.fd(),
                self.buf.as_mut_ptr() as *mut libc::c_void,
                self.buf.len(),
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                self.out.clear();
                return Ok(&self.out);
            }
            return Err(e);
        }
        if n == 0 {
            // EOF = 设备被解绑/拔了。以前这里返回空切片、调用方 `continue`，
            // 就成了 100% CPU 的空转（poll 永远说可读、read 永远 0 字节）。
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "evdev: 设备已断开",
            ));
        }
        let n = n as usize;
        self.out.clear();
        let mut off = 0;
        while off + EVENT_SIZE <= n {
            let b = &self.buf[off..off + EVENT_SIZE];
            self.out.push(Ev {
                // struct input_event { timeval(8) , type u16, code u16, value i32 }
                typ: u16::from_le_bytes([b[8], b[9]]),
                code: u16::from_le_bytes([b[10], b[11]]),
                value: i32::from_le_bytes([b[12], b[13], b[14], b[15]]),
            });
            off += EVENT_SIZE;
        }
        Ok(&self.out)
    }
}
