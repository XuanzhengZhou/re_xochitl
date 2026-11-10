//! 触摸屏（`/dev/input/event2`，`pt_mt`，多点协议 B）。
//!
//! 手指只用来点 UI 和手势，不落墨——这是官方的分工，也是我们照抄的行为。
//!
//! 解码和手指表全在 [`Fingers`] 里：`apply()` 只吃一条 evdev 记录，
//! 所以单测不用碰设备。线程那边只剩"读 → apply → 转发"三步。
//!
//! 对外两样东西：
//! - [`TouchEvent`]：单指的 Down / Move / Up 生命周期，主循环的点击逻辑照旧；
//! - `Fingers::fingers()`：当前**所有**按着的手指快照，双指缩放/平移用它。

use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::AsRawFd;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::evdev::*;
use super::guard::{self, GuardAct, LoopEnd, ReleaseGuard, TOUCH_TIMEOUT_MS};
use super::POLL_MS;

const ABS_MT_SLOT: u16 = 0x2f;
/// 触点面积（`ABS_MT_TOUCH_MAJOR`）：手掌判定只认它。
const ABS_MT_TOUCH_MAJOR: u16 = 0x30;
const ABS_MT_POSITION_X: u16 = 0x35;
const ABS_MT_POSITION_Y: u16 = 0x36;
const ABS_MT_TRACKING_ID: u16 = 0x39;
const ABS_MT_PRESSURE: u16 = 0x3a;

/// 面板 10 点触控，留点余量。
const MAX_SLOTS: usize = 16;

/// 假 tracking_id 的起点：面板只发 `BTN_TOUCH`、不发 `MT_TRACKING_ID` 时用它，
/// 和内核从 0 往上分配的真 id 不会撞。
const SYNTH_TID: i64 = 1 << 40;

#[derive(Clone, Copy, Debug)]
pub enum TouchEvent {
    Down { x: f32, y: f32 },
    Move { x: f32, y: f32 },
    Up,
}

/// 一条裸 evdev 记录。
///
/// `type/code` 内核里是 `u16`，这里统一成 `i32`：`value` 本来就有负数
/// （`MT_TRACKING_ID = -1` 表示抬手），单测构造起来也省事。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawEvent {
    pub typ: i32,
    pub code: i32,
    pub value: i32,
}

/// 一个 MT slot 的状态。
#[derive(Clone, Copy)]
struct Slot {
    /// 协议 B 的 tracking_id；-1 = 这个 slot 上没有手指
    tid: i64,
    /// 已提交的屏幕坐标
    x: f32,
    y: f32,
    /// `x/y` 可信（按下但坐标还没到时是 false，这一帧先不报这根手指）
    valid: bool,
    /// 本帧收到的原始坐标（自上次 `SYN_REPORT` 以来）
    raw_x: i32,
    raw_y: i32,
    got: bool,
    /// 最近一次非零原始坐标：面板在空闲/抬起帧会把坐标归零
    last_x: i32,
    last_y: i32,
    has_last: bool,
    /// `ABS_MT_TOUCH_MAJOR`（触点面积，面板单位）；-1 = 这块面板没报过
    major: i32,
    /// `ABS_MT_PRESSURE`；-1 = 没报过。只进日志，不参与判定
    pressure: i32,
}

impl Slot {
    const IDLE: Slot = Slot {
        tid: -1,
        x: 0.0,
        y: 0.0,
        valid: false,
        raw_x: 0,
        raw_y: 0,
        got: false,
        last_x: 0,
        last_y: 0,
        has_last: false,
        major: -1,
        pressure: -1,
    };
}

/// 按 slot 索引的手指表。喂 `RawEvent`，`SYN_REPORT` 时结算一帧。
pub struct Fingers {
    slots: [Slot; MAX_SLOTS],
    /// 当前 MT slot，`ABS_MT_SLOT` 之后的事件都属于它
    cur: usize,
    /// `BTN_TOUCH`：有些面板不发 tracking_id，只靠它表示"有手指"
    btn: bool,
    /// 见过真的 `MT_TRACKING_ID` 吗。见过就是协议 B，别再走 BTN_TOUCH 兜底，
    /// 否则最后一根手指抬起、按钮还没跟着松的那一帧会凭空多出一根手。
    seen_tid: bool,
    /// 见过 `ABS_MT_POSITION_*` 吗。见过就说明坐标是 per-slot 的，
    /// 全局 `ABS_X/ABS_Y` 不能再往当前 slot 里塞。
    seen_mt_pos: bool,
    w: f32,
    h: f32,
    /// 面板量程（`maximum - minimum + 1`），和旧代码一个口径
    x_max: f32,
    y_max: f32,
    /// 手掌面积阈值（`ABS_MT_TOUCH_MAJOR`）。`0` = 关闭面积判定
    /// （面板不报 `ABS_MT_TOUCH_MAJOR` 时就是这个值）。
    ///
    /// 面积大于等于它的触点**不算手指**：不进 `fingers()` 快照（双指缩放 /
    /// 撤销 / 重做都读它）、不产生 `TouchEvent`（点 UI 也点不到）。
    /// 写字时搭在屏上的右手掌就是被这一条挡掉的——以前它会被数成 2~3 个触点，
    /// 直接送进双指/三指识别器。
    palm_major: i32,
    /// 上一次上报的快照，用来判"手指集合变没变"
    sent: Vec<(i64, f32, f32)>,
}

impl Fingers {
    pub fn new(w: f32, h: f32) -> Fingers {
        Fingers {
            slots: [Slot::IDLE; MAX_SLOTS],
            cur: 0,
            btn: false,
            seen_tid: false,
            seen_mt_pos: false,
            w,
            h,
            // 先按"量程等于屏幕"起步，真量程由 spawn 用 EVIOCGABS 补上
            x_max: w,
            y_max: h,
            // 没见过 `ABS_MT_TOUCH_MAJOR` 之前不做面积判定
            palm_major: 0,
            sent: Vec::new(),
        }
    }

    /// 面板真实量程，来自 `EVIOCGABS`。
    pub fn set_ranges(&mut self, x_max: f32, y_max: f32) {
        self.x_max = x_max;
        self.y_max = y_max;
    }

    /// 手掌面积阈值（面板单位，0 = 关闭）。`spawn` 用 `EVIOCGABS` 的量程算，
    /// 真机上探到的面积可以用 `RM_PALM_MAJOR` 覆盖。
    pub fn set_palm_major(&mut self, major: i32) {
        self.palm_major = major.max(0);
    }

    pub fn palm_major(&self) -> i32 {
        self.palm_major
    }

    /// 这个 slot 上的触点是手掌吗（面积够大）。
    ///
    /// 没报面积的触点一律当手指：宁可回到旧行为，也不能因为面板不报面积就把
    /// 所有手指都滤掉（那样双指手势会整体失灵）。
    fn is_palm(&self, s: &Slot) -> bool {
        self.palm_major > 0 && s.major >= self.palm_major
    }

    /// 当前按着的手掌个数（诊断用）。
    pub fn palm_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.tid >= 0 && s.valid && self.is_palm(s))
            .count()
    }

    /// 喂一条 evdev 记录。
    ///
    /// 返回 `Some` 表示这一帧该往上报：自上次上报以来手指集合（按下 / 抬起 / 移动）
    /// 真的变了。没变化就返回 `None`，主循环不会被无意义的帧吵醒。
    pub fn apply(&mut self, ev: &RawEvent) -> Option<TouchEvent> {
        // RawEvent 的 type/code 是 i32，常量是 u16，这里转一次就够
        match (ev.typ as u16, ev.code as u16) {
            (EV_SYN, SYN_REPORT) => return self.commit(),
            (EV_KEY, BTN_TOUCH) => self.btn = ev.value != 0,
            (EV_ABS, ABS_MT_SLOT) => {
                // 越界的 slot 别崩，夹到表的范围内
                self.cur = (ev.value.max(0) as usize).min(MAX_SLOTS - 1);
            }
            (EV_ABS, ABS_MT_TRACKING_ID) => {
                let tid = ev.value as i64;
                let s = &mut self.slots[self.cur];
                if tid < 0 {
                    // 抬起：整个 slot 作废，之后被别的手指复用时带不出旧坐标
                    *s = Slot::IDLE;
                } else {
                    self.seen_tid = true;
                    if s.tid != tid {
                        // 这个 slot 换了根手指：旧位置一律不继承
                        s.has_last = false;
                        s.valid = false;
                        // 面积/压力也不能继承：否则新手指出现在旧手掌的 slot 里
                        // 会被判成手掌（或者反过来）
                        s.major = -1;
                        s.pressure = -1;
                    }
                    s.tid = tid;
                }
            }
            (EV_ABS, ABS_MT_POSITION_X) => {
                self.seen_mt_pos = true;
                let s = &mut self.slots[self.cur];
                s.raw_x = ev.value;
                s.got = true;
            }
            (EV_ABS, ABS_MT_POSITION_Y) => {
                self.seen_mt_pos = true;
                let s = &mut self.slots[self.cur];
                s.raw_y = ev.value;
                s.got = true;
            }
            // 没有 MT 坐标的老式面板才走全局 ABS_X/ABS_Y：MT 面板上这两个是
            // "整体坐标"，写进当前 slot 会让两根手指串位。
            (EV_ABS, ABS_X) | (EV_ABS, ABS_Y) if self.seen_mt_pos => {}
            (EV_ABS, ABS_X) => {
                let s = &mut self.slots[self.cur];
                s.raw_x = ev.value;
                s.got = true;
            }
            (EV_ABS, ABS_Y) => {
                let s = &mut self.slots[self.cur];
                s.raw_y = ev.value;
                s.got = true;
            }
            // 面积/压力：只用来判手掌和写诊断日志，不改坐标
            (EV_ABS, ABS_MT_TOUCH_MAJOR) => {
                let s = &mut self.slots[self.cur];
                s.major = ev.value;
            }
            (EV_ABS, ABS_MT_PRESSURE) => {
                let s = &mut self.slots[self.cur];
                s.pressure = ev.value;
            }
            _ => {}
        }
        None
    }

    /// 当前所有按着的手指 `(tracking_id, x, y)`，屏幕坐标。
    ///
    /// **手掌不算**：面积超过阈值（[`Fingers::set_palm_major`]）的触点被滤掉，
    /// 所以双指缩放 / 双指撤销 / 三指重做都看不到它。
    pub fn fingers(&self) -> Vec<(i64, f32, f32)> {
        self.snapshot()
    }

    pub fn finger_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.tid >= 0 && s.valid && !self.is_palm(s))
            .count()
    }

    /// 把手指表整个作废（按下超时的兜底用）。
    ///
    /// 丢了 `MT_TRACKING_ID = -1` 之后，那个 slot 会永远"按着"：`sent` 清不空，
    /// 之后每次点击都只报 `Move`（`Down` 只在 `sent` 由空变非空时报），主循环
    /// 就再也收不到按下 —— 表现就是"手指点哪儿都没反应"。这里清空表并让
    /// 下一帧从头报 `Down`。
    ///
    /// 面板量程（`x_max`/`y_max`/`w`/`h`）和"这块面板是哪一种"（`seen_tid`、
    /// `seen_mt_pos`）不重置：它们是设备属性，不是接触状态。
    pub fn reset(&mut self) {
        self.slots = [Slot::IDLE; MAX_SLOTS];
        self.cur = 0;
        self.btn = false;
        self.sent.clear();
    }

    /// 一帧结束：把 raw 坐标换成屏幕坐标，和上一次上报比对。
    fn commit(&mut self) -> Option<TouchEvent> {
        // 只发 BTN_TOUCH、不发 MT_TRACKING_ID 的面板：表里是空的但按钮按下了，
        // 就把当前 slot 当成一根手指，否则这种面板点哪儿都没反应。
        if self.btn && !self.seen_tid && !self.slots.iter().any(|s| s.tid >= 0) {
            let cur = self.cur;
            let s = &mut self.slots[cur];
            if s.got || s.has_last {
                if !s.got {
                    s.raw_x = s.last_x;
                    s.raw_y = s.last_y;
                    s.got = true;
                }
                s.tid = SYNTH_TID + cur as i64;
            }
        } else if !self.btn {
            // 按钮抬起 = 这块面板认为没有手指了
            for s in self.slots.iter_mut() {
                if s.tid >= SYNTH_TID {
                    *s = Slot::IDLE;
                }
            }
        }

        let (w, h) = (self.w, self.h);
        let (x_max, y_max) = (self.x_max, self.y_max);
        for s in self.slots.iter_mut() {
            if s.tid < 0 || !s.got {
                continue;
            }
            s.got = false;
            let (mut rx, mut ry) = (s.raw_x, s.raw_y);
            if rx == 0 && ry == 0 {
                // 空闲/抬起帧面板会把坐标归零，那不是真实落点：沿用上一处
                if s.has_last {
                    rx = s.last_x;
                    ry = s.last_y;
                }
            } else {
                s.last_x = rx;
                s.last_y = ry;
                s.has_last = true;
            }
            let (sx, sy) = crate::fb::to_logical(map_x(rx, w, x_max), map_y(ry, h, y_max));
            let (lw, lh) = crate::fb::logical();
            s.x = sx.clamp(0.0, lw as f32 - 1.0);
            s.y = sy.clamp(0.0, lh as f32 - 1.0);
            s.valid = true;
        }

        let now = self.snapshot();
        if now == self.sent {
            return None;
        }
        // 单指生命周期照旧：0→n 报 Down，n→n 报 Move，n→0 报 Up。
        // 中间掉一根（2→1）只能报 Move——报 Up 会让主循环把上一次点击结算掉。
        let ev = match (self.sent.is_empty(), now.first().copied()) {
            (true, Some((_, x, y))) => Some(TouchEvent::Down { x, y }),
            (false, Some((_, x, y))) => Some(TouchEvent::Move { x, y }),
            (false, None) => Some(TouchEvent::Up),
            (true, None) => None,
        };
        self.sent = now;
        ev
    }

    fn snapshot(&self) -> Vec<(i64, f32, f32)> {
        let mut v: Vec<(i64, f32, f32)> = self
            .slots
            .iter()
            .filter(|s| s.tid >= 0 && s.valid && !self.is_palm(s))
            .map(|s| (s.tid, s.x, s.y))
            .collect();
        // 按 tracking_id 排 = 按下的先后（内核从小到大发），取前两根时顺序才稳定
        v.sort_by_key(|f| f.0);
        v
    }

    /// 诊断用：所有 slot 的一行摘要（`RM_TOUCH_LOG=1` 时由触摸线程打出来）。
    ///
    /// 每个触点都带 `mj`（面积）和 `pr`（压力），手掌标 `PALM` —— 真机上取证时
    /// 一眼就能看出"手掌进来以后面板报的是几个触点、面积多大"。
    pub fn dump(&self) -> String {
        let mut out = String::new();
        for (i, s) in self.slots.iter().enumerate() {
            if s.tid < 0 {
                continue;
            }
            let (x, y) = if s.valid {
                (s.x, s.y)
            } else {
                (
                    map_x(s.raw_x, self.w, self.x_max),
                    map_y(s.raw_y, self.h, self.y_max),
                )
            };
            out.push_str(&format!(
                "slot{i} tid={} {x:.0},{y:.0} mj={} pr={}{} | ",
                s.tid,
                s.major,
                s.pressure,
                if self.is_palm(s) { " PALM" } else { "" }
            ));
        }
        if out.is_empty() {
            out.push_str("（无触点）");
        }
        out
    }
}

/// 面板 x → 屏幕 x。**照抄旧代码的映射**，不要另发明一套。
fn map_x(x: i32, w: f32, x_max: f32) -> f32 {
    x as f32 / (x_max - 1.0).max(1.0) * (w - 1.0)
}

/// 面板 y → 屏幕 y。触摸面板的 Y 轴原点在**下方**（和笔不一样），必须翻过来；
/// 不翻的话手指点上面会被判成下面——表现就是"怎么点都点不中"。
fn map_y(y: i32, h: f32, y_max: f32) -> f32 {
    (1.0 - y as f32 / (y_max - 1.0).max(1.0)) * (h - 1.0)
}

/// `RM_TOUCH_LOG=1`：把每个触摸 / 笔事件（类型、指针 id、坐标、面积、压力、
/// 手势判定）打到 stderr。**默认关闭**，只给真机取证用。
pub fn tlog() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("RM_TOUCH_LOG") {
        Ok(v) => v != "0" && !v.is_empty(),
        Err(_) => false,
    })
}

/// 手掌面积阈值。
///
/// 面板不报 `ABS_MT_TOUCH_MAJOR`（`EVIOCGABS` 失败/量程太小）就返回 0 =
/// **关闭面积判定**，行为和以前一模一样；报了就取量程的**上 1/4**——手指尖
/// 够不到那么大的面积，手掌/掌根稳稳超过它，同时不会因为单位换算（0..255 还是
/// 传感器原始单位）误伤手指。真机上还能用 `RM_PALM_MAJOR=<number>` 覆盖
/// （0 = 关掉），不需要重新编译。
fn resolve_palm_major(ai: Option<super::evdev::AbsInfo>) -> i32 {
    let over = std::env::var("RM_PALM_MAJOR")
        .ok()
        .and_then(|v| v.trim().parse::<i32>().ok());
    palm_major_of(ai, over)
}

/// [`resolve_palm_major`] 的纯函数部分（单测直接喂量程和覆盖值）。
fn palm_major_of(ai: Option<super::evdev::AbsInfo>, over: Option<i32>) -> i32 {
    if let Some(n) = over {
        return n.max(0);
    }
    let Some(ai) = ai else { return 0 };
    let span = ai.maximum - ai.minimum;
    if span < 16 {
        return 0;
    }
    ai.minimum + span * 3 / 4
}

/// 诊断日志里的一帧事件名（`-` = 这一帧手指集合没变）。
fn ev_name(e: Option<TouchEvent>) -> String {
    match e {
        Some(TouchEvent::Down { x, y }) => format!("Down({x:.0},{y:.0})"),
        Some(TouchEvent::Move { x, y }) => format!("Move({x:.0},{y:.0})"),
        Some(TouchEvent::Up) => "Up".to_string(),
        None => "-".to_string(),
    }
}

/// 起触摸线程。手指表由调用方持有（`Arc<Mutex<_>>`），主循环读快照用。
///
/// 和笔线程一样套一层 [`guard::supervise`]：读设备失败（EOF / ENODEV / poll
/// 挂断）或者线程 panic 都会**重新打开设备再跑**，不再静默死掉。
pub fn spawn(
    path: &str,
    tx: Sender<TouchEvent>,
    fingers: &Arc<Mutex<Fingers>>,
) -> io::Result<()> {
    let file = OpenOptions::new().read(true).open(path)?;
    let fd = file.as_raw_fd();

    let rx = absinfo(fd, ABS_MT_POSITION_X).or_else(|_| absinfo(fd, ABS_X))?;
    let ry = absinfo(fd, ABS_MT_POSITION_Y).or_else(|_| absinfo(fd, ABS_Y))?;
    let rm = absinfo(fd, ABS_MT_TOUCH_MAJOR).ok();
    let palm = resolve_palm_major(rm);
    eprintln!(
        "[touch] {path} x=[{},{}] y=[{},{}] 面积 max={:?} 手掌阈值={}",
        rx.minimum,
        rx.maximum,
        ry.minimum,
        ry.maximum,
        rm.map(|a| a.maximum),
        if palm > 0 {
            format!("{palm}")
        } else {
            "关".to_string()
        }
    );
    fingers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .set_ranges(
            (rx.maximum - rx.minimum + 1) as f32,
            (ry.maximum - ry.minimum + 1) as f32,
        );
    fingers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .set_palm_major(palm);
    drop(file);

    let fingers = Arc::clone(fingers);
    let path = path.to_string();
    std::thread::Builder::new()
        .name("touch".into())
        .spawn(move || {
            guard::supervise("touch", || match OpenOptions::new().read(true).open(&path) {
                Ok(file) => run(file, &tx, &fingers),
                Err(e) => LoopEnd::device(e),
            })
        })
        .map_err(|e| io::Error::new(io::ErrorKind::Other, format!("touch thread: {e}")))?;
    Ok(())
}

fn run(file: std::fs::File, tx: &Sender<TouchEvent>, fingers: &Arc<Mutex<Fingers>>) -> LoopEnd {
    let fd = file.as_raw_fd();
    let mut buf = vec![0u8; EVENT_SIZE * 64];
    let mut guard = ReleaseGuard::new(TOUCH_TIMEOUT_MS);
    let t0 = Instant::now();
    let now_ms = || t0.elapsed().as_secs_f64() * 1000.0;
    let log = tlog();
    if log {
        eprintln!("[tlog] 触摸诊断开启（RM_TOUCH_LOG=1）");
    }

    loop {
        // 静默 250 ms 醒一次：手指按下之后设备再没动静时，超时兜底才有机会跑。
        match poll_readable(fd, POLL_MS) {
            Ok(true) => {}
            Ok(false) => {
                if guard.poll(now_ms()) == GuardAct::Forced {
                    let mut st = fingers.lock().unwrap_or_else(|e| e.into_inner());
                    st.reset();
                    drop(st);
                    eprintln!(
                        "[touch] DOWN 后 {} s 没有 UP，强制复位",
                        (TOUCH_TIMEOUT_MS / 1000.0) as u64
                    );
                    // 主循环那边的"按下"也得收尾，否则它一直以为手指还按着
                    // （`pending_tap` / `drag` 不会结算，下一次点按只能当 Move）。
                    if tx.send(TouchEvent::Up).is_err() {
                        return LoopEnd::ConsumerGone;
                    }
                }
                continue;
            }
            Err(e) => return LoopEnd::device(e),
        }

        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            // 以前这里是 `continue`：设备没了会变成 100% CPU 的空转。
            return LoopEnd::device(e);
        }
        if n == 0 {
            // EOF = 设备被解绑/拔了
            return LoopEnd::device("EOF");
        }
        let n = n as usize;
        let mut gone = false;
        {
            let mut st = fingers.lock().unwrap_or_else(|e| e.into_inner());
            let mut last_n = st.finger_count();
            let mut off = 0;
            while off + EVENT_SIZE <= n {
                let b = &buf[off..off + EVENT_SIZE];
                off += EVENT_SIZE;
                let raw = RawEvent {
                    typ: u16::from_le_bytes([b[8], b[9]]) as i32,
                    code: u16::from_le_bytes([b[10], b[11]]) as i32,
                    value: i32::from_le_bytes([b[12], b[13], b[14], b[15]]),
                };
                let is_syn = raw.typ as u16 == EV_SYN && raw.code as u16 == SYN_REPORT;
                let ev = st.apply(&raw);
                if is_syn {
                    // 每帧把"设备还宣称按着吗"喂给看门狗。`apply` 只在手指集合
                    // 真的变了时才给事件，看门狗需要的是**每一帧**的接触状态。
                    let _ = guard.frame(st.finger_count() > 0, now_ms());
                    // 诊断：一帧一行。手掌进来以后这里会多出 `mj=` 很大的 slot，
                    // 并且带着 `PALM` 标记（= 已被踢出触摸链路）。
                    if log {
                        eprintln!(
                            "[tlog] T {:.1} ev={} n={} palm={} {}",
                            now_ms(),
                            ev_name(ev),
                            st.finger_count(),
                            st.palm_count(),
                            st.dump()
                        );
                    }
                }
                if let Some(ev) = ev {
                    let cnt = st.finger_count();
                    match ev {
                        // 真机核对映射用：屏幕坐标 + 手指数
                        TouchEvent::Down { x, y } => {
                            eprintln!("[touch] down n={cnt} x={x:.0} y={y:.0}")
                        }
                        // 手指数变了但没走 Down/Up（1→2 / 2→1）：真机靠这行确认多指解到了
                        _ if cnt != last_n => eprintln!("[touch] fingers={cnt}"),
                        _ => {}
                    }
                    last_n = cnt;
                    if tx.send(ev).is_err() {
                        gone = true;
                        break;
                    }
                }
            }
        }
        if gone {
            return LoopEnd::ConsumerGone;
        }
    }
}

// ---------------------------------------------------------------- 双指整轮手势

/// 双指手势的结算结果。一轮双指动作（从两指按下到松手）最多报一次。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TwoFingerAct {
    None,
    /// 两指同时按下又抬起（位移 < 30px、时长 < 400ms）→ 撤销
    Undo,
    /// 两指一起向下拖 > 200px 且竖向为主 → 收起工具条
    HideToolbar,
}

/// 点按的最大位移：超过就不算点按。
const TAP_MOVE_MAX: f32 = 30.0;
/// 点按窗口：< 400ms 才算点按；太短（一两帧）的是面板在抬指帧冒出来的
/// 假第二根手指——`main.rs` 的 `on_gesture` 里记过"一次点击会被触摸板识别成
/// 两指捏合 1.011 + 平移 8px"，不设下限的话每点一下都会撤销一次。
const TAP_MS_MAX: f64 = 400.0;
const TAP_MS_MIN: f64 = 30.0;
/// 两指向下拖多少才算"下滑收工具条"。
const DOWN_MIN: f32 = 200.0;
/// 竖向为主的判据：平均横向位移必须小于平均下移的 0.6 倍。
const STRAIGHT: f32 = 0.6;

/// 一轮双指的起点。`ids` 用来认"还是这两根手指"。
#[derive(Clone, Debug)]
struct Hold {
    ids: (i64, i64),
    pts: [(f32, f32); 2],
    t0: f64,
}

/// 双指**整轮**手势识别（6b 点按撤销 / 6c 下滑收工具条）。
///
/// `viewport::Tracker` 只报每帧增量，管不了"按下多久、整轮走了多远"，
/// 所以这一层自己记起点和时长。主循环每帧把手指快照 + 当前时刻喂进来。
#[derive(Clone, Debug, Default)]
pub struct TwoFinger {
    hold: Option<Hold>,
    /// 这一轮已经报过动作（点按撤销 / 下滑收条都只报一次）
    fired: bool,
    /// 两指各自离开起点的最大位移
    max_disp: f32,
}

impl TwoFinger {
    pub fn new() -> TwoFinger {
        TwoFinger::default()
    }

    /// 喂一帧手指快照。`now_ms` 是单调毫秒（测试里直接给数）。
    pub fn update(&mut self, fingers: &[(i64, f32, f32)], now_ms: f64) -> TwoFingerAct {
        // 只用前两根（`Fingers::fingers()` 已按 tracking_id 排好 = 按下先后）
        let mut cur = [(0i64, 0.0f32, 0.0f32); 2];
        let mut n = 0;
        for f in fingers {
            if n == 2 {
                break;
            }
            if !f.1.is_finite() || !f.2.is_finite() {
                continue;
            }
            cur[n] = *f;
            n += 1;
        }

        let Some(start) = self.hold.as_ref() else {
            // 只在"一上来就是两指"时开始记；两指之后掉成一根不算新的一轮
            if n == 2 {
                self.hold = Some(Hold {
                    ids: (cur[0].0, cur[1].0),
                    pts: [(cur[0].1, cur[0].2), (cur[1].1, cur[1].2)],
                    t0: now_ms,
                });
                self.fired = false;
                self.max_disp = 0.0;
            }
            return TwoFingerAct::None;
        };

        if n < 2 {
            // 两指松开：没报过动作的才可能是"点按"
            let held = now_ms - start.t0;
            let tap = !self.fired
                && self.max_disp < TAP_MOVE_MAX
                && held >= TAP_MS_MIN
                && held < TAP_MS_MAX;
            self.hold = None;
            self.fired = false;
            return if tap {
                TwoFingerAct::Undo
            } else {
                TwoFingerAct::None
            };
        }

        // 同一帧里换了手指（一根起一根落）：这一轮的起点作废，重新记
        if (cur[0].0, cur[1].0) != start.ids {
            self.hold = Some(Hold {
                ids: (cur[0].0, cur[1].0),
                pts: [(cur[0].1, cur[0].2), (cur[1].1, cur[1].2)],
                t0: now_ms,
            });
            self.fired = false;
            self.max_disp = 0.0;
            return TwoFingerAct::None;
        }

        let (p0, p1) = (start.pts[0], start.pts[1]);
        let (dx0, dy0) = (cur[0].1 - p0.0, cur[0].2 - p0.1);
        let (dx1, dy1) = (cur[1].1 - p1.0, cur[1].2 - p1.1);
        let d0 = (dx0 * dx0 + dy0 * dy0).sqrt();
        let d1 = (dx1 * dx1 + dy1 * dy1).sqrt();
        self.max_disp = self.max_disp.max(d0).max(d1);
        if self.fired {
            return TwoFingerAct::None;
        }
        // 6c：两指**都**向下、平均下移过 200px、且竖向为主
        let (dx, dy) = ((dx0 + dx1) * 0.5, (dy0 + dy1) * 0.5);
        if dy0 > 0.0 && dy1 > 0.0 && dy > DOWN_MIN && dx.abs() < dy * STRAIGHT {
            self.fired = true;
            return TwoFingerAct::HideToolbar;
        }
        TwoFingerAct::None
    }

    pub fn reset(&mut self) {
        *self = TwoFinger::default();
    }
}

// ---------------------------------------------------------------- 三指点按 = 重做

/// 三指手势的结算结果。一轮三指动作（从三指按下到松手）最多报一次。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ThreeFingerAct {
    None,
    /// 三指同时按下又抬起 → 重做（官方 `res_067887b.qml:1161` `threeFingerTapFilter`，
    /// `enabled: sceneController.redoAvailable`）
    Redo,
}

/// 三指点按的位移上限。官方那个 filter 是
/// `TouchAreaClickFilter { fingers: 3; palmAreaThreshold: 20 * 20 }`，
/// 20×20 的掌面阈值 = 一根手指最多动 20px。两指那套 30px 是当年调手感调出来的，
/// 不共用（官方两个 filter 本来就是各自一套阈值）。
const TAP3_MOVE_MAX: f32 = 20.0;

/// 一轮三指的起点（`ids` 用来认"还是这三根手指"）。
#[derive(Clone, Debug)]
struct Hold3 {
    ids: (i64, i64, i64),
    pts: [(f32, f32); 3],
    t0: f64,
}

/// 三指**整轮**手势：点按 → 重做。
///
/// 和 [`TwoFinger`] 一样自己记起点和时长（`viewport::Tracker` 只报每帧增量）。
/// 只认**恰好三根**手指：四根以上多半是手掌压上来，这一轮直接作废，免得不小心重做。
#[derive(Clone, Debug, Default)]
pub struct ThreeFinger {
    hold: Option<Hold3>,
    /// 这一轮已经报过动作
    fired: bool,
    /// 三根手指各自离开起点的最大位移
    max_disp: f32,
}

impl ThreeFinger {
    pub fn new() -> ThreeFinger {
        ThreeFinger::default()
    }

    /// 喂一帧手指快照。`now_ms` 是单调毫秒（测试里直接给数）。
    pub fn update(&mut self, fingers: &[(i64, f32, f32)], now_ms: f64) -> ThreeFingerAct {
        // 四根以上：这是整只手，不是三指点按
        if fingers.len() > 3 {
            self.reset();
            return ThreeFingerAct::None;
        }
        // 只用前三根（`Fingers::fingers()` 已按 tracking_id 排好 = 按下先后）
        let mut cur = [(0i64, 0.0f32, 0.0f32); 3];
        let mut n = 0;
        for f in fingers {
            if n == 3 {
                break;
            }
            if !f.1.is_finite() || !f.2.is_finite() {
                continue;
            }
            cur[n] = *f;
            n += 1;
        }

        let Some(start) = self.hold.as_ref() else {
            // 只在"一上来就是三根"时开始记；三指之后掉成两根不算新的一轮
            if n == 3 {
                self.hold = Some(Hold3 {
                    ids: (cur[0].0, cur[1].0, cur[2].0),
                    pts: [(cur[0].1, cur[0].2), (cur[1].1, cur[1].2), (cur[2].1, cur[2].2)],
                    t0: now_ms,
                });
                self.fired = false;
                self.max_disp = 0.0;
            }
            return ThreeFingerAct::None;
        };

        if n < 3 {
            // 三根里松了一根：没报过动作的才可能是"点按"
            let held = now_ms - start.t0;
            let tap = !self.fired
                && self.max_disp < TAP3_MOVE_MAX
                && held >= TAP_MS_MIN
                && held < TAP_MS_MAX;
            self.hold = None;
            self.fired = false;
            return if tap {
                ThreeFingerAct::Redo
            } else {
                ThreeFingerAct::None
            };
        }

        // 同一帧里换了手指：这一轮的起点作废，重新记
        if (cur[0].0, cur[1].0, cur[2].0) != start.ids {
            self.hold = Some(Hold3 {
                ids: (cur[0].0, cur[1].0, cur[2].0),
                pts: [(cur[0].1, cur[0].2), (cur[1].1, cur[1].2), (cur[2].1, cur[2].2)],
                t0: now_ms,
            });
            self.fired = false;
            self.max_disp = 0.0;
            return ThreeFingerAct::None;
        }

        for i in 0..3 {
            let (px, py) = start.pts[i];
            let d = (cur[i].1 - px).hypot(cur[i].2 - py);
            self.max_disp = self.max_disp.max(d);
        }
        ThreeFingerAct::None
    }

    pub fn reset(&mut self) {
        *self = ThreeFinger::default();
    }
}

/// 一帧手势结算：**三指优先**。
///
/// 三指按着的时候双指那一轮一律作废 —— 否则三指一抬，[`TwoFinger`] 只看到
/// "前两根按了一会儿又松了"，会顺手再判一次撤销。官方这两个 filter 是互斥的
/// （`fingers: 2` / `fingers: 3`），所以这里也分开。
///
/// 返回 `(三指动作, 双指动作)`，同一帧最多只有一个不是 `None`。
pub fn classify(
    fingers: &[(i64, f32, f32)],
    three: &mut ThreeFinger,
    two: &mut TwoFinger,
    now_ms: f64,
) -> (ThreeFingerAct, TwoFingerAct) {
    if fingers.len() >= 3 {
        two.reset();
        return (three.update(fingers, now_ms), TwoFingerAct::None);
    }
    let t3 = three.update(fingers, now_ms);
    if t3 != ThreeFingerAct::None {
        two.reset();
        return (t3, TwoFingerAct::None);
    }
    (ThreeFingerAct::None, two.update(fingers, now_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: f32 = 1404.0;
    const H: f32 = 1872.0;

    /// 面板量程按屏幕算：x 原样，y 翻转。真机量程由 ioctl 补。
    fn table() -> Fingers {
        let mut f = Fingers::new(W, H);
        f.set_ranges(W, H);
        f
    }

    fn abs(code: u16, value: i32) -> RawEvent {
        RawEvent {
            typ: EV_ABS as i32,
            code: code as i32,
            value,
        }
    }

    fn key(code: u16, value: i32) -> RawEvent {
        RawEvent {
            typ: EV_KEY as i32,
            code: code as i32,
            value,
        }
    }

    fn syn() -> RawEvent {
        RawEvent {
            typ: EV_SYN as i32,
            code: SYN_REPORT as i32,
            value: 0,
        }
    }

    /// 屏幕 y 和面板 y 差一个翻转
    fn sy(y: i32) -> f32 {
        (H - 1.0) - y as f32
    }

    fn frame_down(f: &mut Fingers, slot: i32, tid: i32, x: i32, y: i32) -> Option<TouchEvent> {
        f.apply(&abs(ABS_MT_SLOT, slot));
        f.apply(&abs(ABS_MT_TRACKING_ID, tid));
        f.apply(&abs(ABS_MT_POSITION_X, x));
        f.apply(&abs(ABS_MT_POSITION_Y, y));
        f.apply(&key(BTN_TOUCH, 1));
        f.apply(&syn())
    }

    fn frame_move(f: &mut Fingers, slot: i32, x: i32, y: i32) -> Option<TouchEvent> {
        f.apply(&abs(ABS_MT_SLOT, slot));
        f.apply(&abs(ABS_MT_POSITION_X, x));
        f.apply(&abs(ABS_MT_POSITION_Y, y));
        f.apply(&syn())
    }

    // ---- 手掌（面积判定）：C18 ------------------------------------------

    /// 带手掌阈值的表：`palm > 0` = 面积 ≥ 它就算手掌。
    fn table_palm(palm: i32) -> Fingers {
        let mut f = table();
        f.set_palm_major(palm);
        f
    }

    /// 按下，并带上触点面积（`ABS_MT_TOUCH_MAJOR`）。
    fn frame_down_area(
        f: &mut Fingers,
        slot: i32,
        tid: i32,
        x: i32,
        y: i32,
        major: i32,
    ) -> Option<TouchEvent> {
        f.apply(&abs(ABS_MT_SLOT, slot));
        f.apply(&abs(ABS_MT_TRACKING_ID, tid));
        f.apply(&abs(ABS_MT_POSITION_X, x));
        f.apply(&abs(ABS_MT_POSITION_Y, y));
        f.apply(&abs(ABS_MT_TOUCH_MAJOR, major));
        f.apply(&abs(ABS_MT_PRESSURE, 40));
        f.apply(&key(BTN_TOUCH, 1));
        f.apply(&syn())
    }

    /// 手掌只压上来、面积够大 → **不算手指**，也不产生任何点击事件。
    ///
    /// 这是这条 bug 的核心防护：手掌以前会被数成 1~3 个触点，直接喂进双指/三指
    /// 识别器。
    #[test]
    fn palm_contact_is_not_a_finger() {
        let mut f = table_palm(150);
        let ev = frame_down_area(&mut f, 0, 70, 900, 1500, 210);
        assert!(ev.is_none(), "手掌不该报 Down（会被当成点按）：{ev:?}");
        assert!(f.fingers().is_empty(), "手掌不在手指快照里");
        assert_eq!(f.finger_count(), 0);
        assert_eq!(f.palm_count(), 1, "但它要能被诊断日志看见");
        // 手掌挪动也不该产出 Move
        assert!(frame_move(&mut f, 0, 910, 1500).is_none());
        // 抬手也不该产出 Up（否则主循环会结算一次点击）
        assert!(frame_up(&mut f, 0, 0).is_none());
        assert_eq!(f.palm_count(), 0);
    }

    /// 手指 + 手掌同时按着：快照里只留手指，Down 坐标来自手指。
    #[test]
    fn palm_never_covers_the_real_finger() {
        let mut f = table_palm(150);
        // 手掌先落（面积大）
        assert!(frame_down_area(&mut f, 0, 70, 1100, 1600, 220).is_none());
        // 手指后落（面积小）
        match frame_down_area(&mut f, 1, 71, 300, 400, 30) {
            Some(TouchEvent::Down { x, y }) => {
                assert!((x - 300.0).abs() < 0.01 && (y - sy(400)).abs() < 0.01, "{x},{y}");
            }
            other => panic!("手指按下必须报 Down：{other:?}"),
        }
        expect(&f, &[(71, 300.0, sy(400))]);
        assert_eq!(f.finger_count(), 1);
        assert_eq!(f.palm_count(), 1);
    }

    /// 面板不报面积（`major` 一直是 -1）时，绝不能把手指当手掌滤掉。
    #[test]
    fn unknown_area_is_still_a_finger() {
        let mut f = table_palm(150);
        frame_down(&mut f, 0, 10, 300, 400);
        frame_down(&mut f, 1, 11, 900, 700);
        assert_eq!(f.finger_count(), 2, "没面积信息就回到旧行为");
        assert_eq!(f.palm_count(), 0);
        assert_eq!(f.fingers().len(), 2);
    }

    /// 阈值 0 = 关闭面积判定（面板量程读不到时的默认值）。
    #[test]
    fn zero_threshold_disables_palm_filtering() {
        let mut f = table_palm(0);
        frame_down_area(&mut f, 0, 70, 900, 1500, 255);
        assert_eq!(f.finger_count(), 1);
        assert_eq!(f.palm_count(), 0);
    }

    /// slot 复用时不能继承上一根手掌的面积，否则新手指出现在手掌的 slot 里
    /// 会被当成手掌（触摸就"死了"）。
    #[test]
    fn reused_slot_does_not_inherit_palm_area() {
        let mut f = table_palm(150);
        frame_down_area(&mut f, 0, 70, 900, 1500, 220);
        frame_up(&mut f, 0, 0);
        // 同一 slot 立刻派给一根手指，但这一帧**没带面积**（面板只在变化时发）
        f.apply(&abs(ABS_MT_SLOT, 0));
        f.apply(&abs(ABS_MT_TRACKING_ID, 71));
        f.apply(&abs(ABS_MT_POSITION_X, 300));
        f.apply(&abs(ABS_MT_POSITION_Y, 400));
        f.apply(&key(BTN_TOUCH, 1));
        let ev = f.apply(&syn());
        assert!(matches!(ev, Some(TouchEvent::Down { .. })), "{ev:?}");
        assert_eq!(f.finger_count(), 1, "不该继承旧手掌的面积");
        assert_eq!(f.palm_count(), 0);
    }

    /// 回归证据（这条 bug 的机理）：**不开**面积判定时，手掌的 2 个 / 3 个触点
    /// 会被 `classify` 判成"双指点按 = 撤销" / "三指点按 = 重做"；开了之后
    /// 一个动作都不会有。
    #[test]
    fn palm_contacts_used_to_fire_undo_and_redo() {
        // ① 不开面积判定：复现 bug
        let mut f = table_palm(0);
        frame_down_area(&mut f, 0, 70, 1000, 1500, 210);
        frame_down_area(&mut f, 1, 71, 1040, 1560, 205);
        let snap = f.fingers();
        assert_eq!(snap.len(), 2, "旧行为：手掌被当成两根手指");
        let mut three = ThreeFinger::new();
        let mut two = TwoFinger::new();
        classify(&snap, &mut three, &mut two, 0.0);
        assert_eq!(
            classify(&snap[..1], &mut three, &mut two, 120.0),
            (ThreeFingerAct::None, TwoFingerAct::Undo),
            "旧行为：手掌抬起来就是一次撤销"
        );

        // ② 开面积判定：同样一段手掌，识别器什么都收不到
        let mut f = table_palm(150);
        frame_down_area(&mut f, 0, 70, 1000, 1500, 210);
        frame_down_area(&mut f, 1, 71, 1040, 1560, 205);
        let snap = f.fingers();
        assert!(snap.is_empty(), "手掌一个触点都不该进快照");
        let mut three = ThreeFinger::new();
        let mut two = TwoFinger::new();
        assert_eq!(
            classify(&snap, &mut three, &mut two, 0.0),
            (ThreeFingerAct::None, TwoFingerAct::None)
        );
        assert_eq!(
            classify(&snap, &mut three, &mut two, 120.0),
            (ThreeFingerAct::None, TwoFingerAct::None),
            "抬手也不能补一个撤销"
        );

        // ③ 三指同理（手掌压出三块）
        let mut f = table_palm(150);
        for (i, tid) in [70, 71, 72].iter().enumerate() {
            frame_down_area(&mut f, i as i32, *tid, 1000 + i as i32 * 30, 1500, 200);
        }
        let snap = f.fingers();
        assert!(snap.is_empty());
        let mut three = ThreeFinger::new();
        let mut two = TwoFinger::new();
        assert_eq!(
            classify(&snap, &mut three, &mut two, 0.0),
            (ThreeFingerAct::None, TwoFingerAct::None)
        );
        assert_eq!(
            classify(&snap, &mut three, &mut two, 130.0),
            (ThreeFingerAct::None, TwoFingerAct::None),
            "不能让手掌凑出一次重做"
        );
    }

    /// 面积阈值默认取"量程的上 1/4"：手指够不到，手掌稳稳超过。
    #[test]
    fn palm_threshold_is_the_top_quarter_of_the_panel_range() {
        let ai = |min: i32, max: i32| super::super::evdev::AbsInfo {
            value: 0,
            minimum: min,
            maximum: max,
            fuzz: 0,
            flat: 0,
            resolution: 0,
        };
        assert_eq!(palm_major_of(Some(ai(0, 255)), None), 191);
        assert_eq!(palm_major_of(Some(ai(0, 100)), None), 75);
        // 量程读不到 / 太小 → 关闭（回到旧行为）
        assert_eq!(palm_major_of(None, None), 0);
        assert_eq!(palm_major_of(Some(ai(0, 8)), None), 0);
        // 环境变量覆盖（真机标定用），0 = 关掉
        assert_eq!(palm_major_of(Some(ai(0, 255)), Some(120)), 120);
        assert_eq!(palm_major_of(Some(ai(0, 255)), Some(0)), 0);
    }

    /// 抬指。`btn` 是面板此刻的 `BTN_TOUCH`：还有别的手指按着时仍是 1。
    fn frame_up(f: &mut Fingers, slot: i32, btn: i32) -> Option<TouchEvent> {
        f.apply(&abs(ABS_MT_SLOT, slot));
        f.apply(&abs(ABS_MT_TRACKING_ID, -1));
        f.apply(&key(BTN_TOUCH, btn));
        f.apply(&syn())
    }

    fn expect(f: &Fingers, want: &[(i64, f32, f32)]) {
        let got = f.fingers();
        assert_eq!(got.len(), want.len(), "手指数不对：{got:?}");
        assert_eq!(f.finger_count(), want.len(), "finger_count 和 fingers() 打架：{got:?}");
        for (g, w) in got.iter().zip(want) {
            assert_eq!(g.0, w.0, "tracking_id 串了：{got:?}，期望 {want:?}");
            assert!((g.1 - w.1).abs() < 0.01, "x 不对：{got:?}，期望 {want:?}");
            assert!((g.2 - w.2).abs() < 0.01, "y 不对：{got:?}，期望 {want:?}");
        }
    }

    #[test]
    fn single_finger_down_move_up() {
        let mut f = table();
        assert!(f.fingers().is_empty());
        match frame_down(&mut f, 0, 100, 300, 200) {
            Some(TouchEvent::Down { x, y }) => {
                assert!((x - 300.0).abs() < 0.01, "{x}");
                assert!((y - sy(200)).abs() < 0.01, "{y}");
            }
            other => panic!("按下该报 Down：{other:?}"),
        }
        expect(&f, &[(100, 300.0, sy(200))]);

        assert!(matches!(
            frame_move(&mut f, 0, 320, 210),
            Some(TouchEvent::Move { .. })
        ));
        expect(&f, &[(100, 320.0, sy(210))]);

        assert!(matches!(frame_up(&mut f, 0, 0), Some(TouchEvent::Up)));
        assert!(f.fingers().is_empty(), "抬手后必须是空的");
    }

    #[test]
    fn only_syn_frames_emit_events() {
        let mut f = table();
        assert!(f.apply(&abs(ABS_MT_SLOT, 0)).is_none());
        assert!(f.apply(&abs(ABS_MT_TRACKING_ID, 10)).is_none());
        assert!(f.apply(&abs(ABS_MT_POSITION_X, 300)).is_none());
        assert!(f.apply(&abs(ABS_MT_POSITION_Y, 400)).is_none());
        assert!(f.apply(&key(BTN_TOUCH, 1)).is_none());
        assert_eq!(f.finger_count(), 0, "SYN 之前不算按下");
        assert!(matches!(f.apply(&syn()), Some(TouchEvent::Down { .. })));
        assert_eq!(f.finger_count(), 1);
    }

    #[test]
    fn two_fingers_keep_their_own_coordinates() {
        let mut f = table();
        frame_down(&mut f, 0, 10, 200, 300);
        frame_down(&mut f, 1, 11, 1000, 1400);
        // 交错移动：改 slot 0 的时候 slot 1 的坐标绝不能跟着动
        frame_move(&mut f, 0, 210, 300);
        expect(&f, &[(10, 210.0, sy(300)), (11, 1000.0, sy(1400))]);
        frame_move(&mut f, 1, 1000, 1410);
        expect(&f, &[(10, 210.0, sy(300)), (11, 1000.0, sy(1410))]);
    }

    #[test]
    fn two_fingers_in_one_frame() {
        let mut f = table();
        f.apply(&abs(ABS_MT_SLOT, 0));
        f.apply(&abs(ABS_MT_TRACKING_ID, 10));
        f.apply(&abs(ABS_MT_POSITION_X, 100));
        f.apply(&abs(ABS_MT_POSITION_Y, 100));
        f.apply(&abs(ABS_MT_SLOT, 1));
        f.apply(&abs(ABS_MT_TRACKING_ID, 11));
        f.apply(&abs(ABS_MT_POSITION_X, 1200));
        f.apply(&abs(ABS_MT_POSITION_Y, 1700));
        f.apply(&key(BTN_TOUCH, 1));
        match f.apply(&syn()) {
            Some(TouchEvent::Down { x, y }) => {
                assert!((x - 100.0).abs() < 0.01, "{x}");
                assert!((y - sy(100)).abs() < 0.01, "{y}");
            }
            other => panic!("0 → 2 根也只报一个 Down：{other:?}"),
        }
        expect(&f, &[(10, 100.0, sy(100)), (11, 1200.0, sy(1700))]);
    }

    #[test]
    fn second_finger_down_is_move_not_down() {
        let mut f = table();
        assert!(matches!(
            frame_down(&mut f, 0, 10, 200, 300),
            Some(TouchEvent::Down { .. })
        ));
        let ev = frame_down(&mut f, 1, 11, 900, 700);
        assert!(
            matches!(ev, Some(TouchEvent::Move { .. })),
            "第二根手指按下不能让主循环再收一个 Down（会打断点击）：{ev:?}"
        );
        expect(&f, &[(10, 200.0, sy(300)), (11, 900.0, sy(700))]);
    }

    /// 单指 → 两指 → 单指 → 全松，每一步的事件类型和快照
    #[test]
    fn lifecycle_one_two_one_zero() {
        let mut f = table();
        let mut evs = Vec::new();
        evs.push(frame_down(&mut f, 0, 10, 200, 300));
        expect(&f, &[(10, 200.0, sy(300))]);

        evs.push(frame_down(&mut f, 1, 11, 900, 700));
        expect(&f, &[(10, 200.0, sy(300)), (11, 900.0, sy(700))]);

        evs.push(frame_move(&mut f, 0, 210, 310));
        evs.push(frame_move(&mut f, 1, 910, 710));
        expect(&f, &[(10, 210.0, sy(310)), (11, 910.0, sy(710))]);

        // 松一根：还剩一根，绝不能报 Up
        evs.push(frame_up(&mut f, 1, 1));
        expect(&f, &[(10, 210.0, sy(310))]);

        evs.push(frame_move(&mut f, 0, 220, 320));
        evs.push(frame_up(&mut f, 0, 0));
        expect(&f, &[]);

        let downs = evs
            .iter()
            .filter(|e| matches!(e, Some(TouchEvent::Down { .. })))
            .count();
        let ups = evs.iter().filter(|e| matches!(e, Some(TouchEvent::Up))).count();
        assert_eq!((downs, ups), (1, 1), "Down/Up 必须一一配对：{evs:?}");
        assert!(
            matches!(evs[4], Some(TouchEvent::Move { .. })),
            "还剩手指时不能报 Up：{evs:?}"
        );
        assert!(evs.iter().all(|e| e.is_some()), "每一步都该有事件：{evs:?}");
    }

    #[test]
    fn reused_slot_does_not_inherit_old_coordinates() {
        let mut f = table();
        frame_down(&mut f, 0, 10, 100, 200);
        frame_up(&mut f, 0, 0);
        // 面板把 slot 0 立刻派给新手指
        frame_down(&mut f, 0, 11, 1000, 1500);
        expect(&f, &[(11, 1000.0, sy(1500))]);
    }

    /// 丢了 UP 之后表里那根"幽灵手指"必须能被兜底清掉：
    /// 复位后新的一下要**照旧报 Down**（否则主循环永远收不到按下）。
    #[test]
    fn reset_drops_a_phantom_finger_and_reports_down_again() {
        let mut f = table();
        frame_down(&mut f, 0, 10, 300, 400);
        // UP 丢了：slot 0 一直"按着"，之后的新手指只能报 Move
        let ev = frame_down(&mut f, 1, 11, 900, 900);
        assert!(
            matches!(ev, Some(TouchEvent::Move { .. })),
            "幽灵手指还在时确实只能报 Move：{ev:?}"
        );
        assert_eq!(f.finger_count(), 2);

        // 超时兜底：清表
        f.reset();
        assert_eq!(f.finger_count(), 0, "复位后不能还有手指");
        assert!(f.fingers().is_empty());
        assert!(f.apply(&syn()).is_none(), "复位这一帧不用再报事件");

        // 下一根手指：照旧 Down + 自己的坐标（不继承幽灵的位置）
        match frame_down(&mut f, 0, 20, 700, 800) {
            Some(TouchEvent::Down { x, y }) => {
                assert!((x - 700.0).abs() < 0.01, "{x}");
                assert!((y - sy(800)).abs() < 0.01, "{y}");
            }
            other => panic!("复位后第一根手指必须报 Down：{other:?}"),
        }
        expect(&f, &[(20, 700.0, sy(800))]);
    }

    /// 复位不能把"这块面板是哪一种"也抹掉：MT 面板复位后仍不该被
    /// 全局 `ABS_X/ABS_Y` 抢走坐标（否则两根手指会串位）。
    #[test]
    fn reset_keeps_panel_capabilities() {
        let mut f = table();
        frame_down(&mut f, 0, 10, 200, 300);
        f.reset();
        f.apply(&abs(ABS_X, 50));
        f.apply(&abs(ABS_Y, 60));
        assert!(f.apply(&syn()).is_none(), "MT 面板的全局坐标不算手指");
        assert_eq!(f.finger_count(), 0);
    }

    #[test]
    fn press_without_coordinates_does_not_reuse_stale_position() {
        let mut f = table();
        frame_down(&mut f, 0, 10, 100, 200);
        frame_up(&mut f, 0, 0);
        // 只有 tracking_id，坐标下一帧才来：这一帧不能把旧位置当成新手指
        f.apply(&abs(ABS_MT_SLOT, 0));
        f.apply(&abs(ABS_MT_TRACKING_ID, 11));
        assert!(f.apply(&syn()).is_none(), "坐标没到，不该报事件");
        assert_eq!(f.finger_count(), 0, "坐标没到就别报手指");
        // 坐标到了，位置必须是新的
        frame_move(&mut f, 0, 1000, 1500);
        expect(&f, &[(11, 1000.0, sy(1500))]);
    }

    #[test]
    fn coordinates_are_clamped_to_the_screen() {
        let mut f = table();
        frame_down(&mut f, 0, 10, -50, 9999);
        expect(&f, &[(10, 0.0, 0.0)]);
        frame_move(&mut f, 0, 9999, -50);
        expect(&f, &[(10, W - 1.0, H - 1.0)]);
    }

    #[test]
    fn unchanged_frame_does_not_repeat_the_event() {
        let mut f = table();
        frame_down(&mut f, 0, 10, 300, 400);
        assert!(f.apply(&syn()).is_none(), "没有变化的一帧不该再报");
        frame_move(&mut f, 0, 310, 400);
        assert!(matches!(f.apply(&syn()), None), "坐标没变也不该再报");
    }

    #[test]
    fn btn_touch_only_panel_still_taps() {
        let mut f = table();
        // 这种面板只发坐标 + BTN_TOUCH，没有 MT_TRACKING_ID
        f.apply(&abs(ABS_MT_SLOT, 0));
        f.apply(&abs(ABS_MT_POSITION_X, 700));
        f.apply(&abs(ABS_MT_POSITION_Y, 900));
        f.apply(&key(BTN_TOUCH, 1));
        match f.apply(&syn()) {
            Some(TouchEvent::Down { x, y }) => {
                assert!((x - 700.0).abs() < 0.01, "{x}");
                assert!((y - sy(900)).abs() < 0.01, "{y}");
            }
            other => panic!("BTN_TOUCH 兜底也该报 Down：{other:?}"),
        }
        expect(&f, &[(SYNTH_TID, 700.0, sy(900))]);

        f.apply(&key(BTN_TOUCH, 0));
        assert!(matches!(f.apply(&syn()), Some(TouchEvent::Up)));
        assert!(f.fingers().is_empty());
    }

    #[test]
    fn legacy_abs_x_never_hijacks_an_mt_slot() {
        let mut f = table();
        frame_down(&mut f, 0, 10, 200, 300); // 见过 MT 坐标了
        frame_down(&mut f, 1, 11, 1000, 1400);
        // MT 面板若还顺带报全局 ABS_X/ABS_Y，它们落在"当前 slot"上，绝不能改到手指
        f.apply(&abs(ABS_X, 50));
        f.apply(&abs(ABS_Y, 60));
        assert!(f.apply(&syn()).is_none(), "{:?}", f.fingers());
        expect(&f, &[(10, 200.0, sy(300)), (11, 1000.0, sy(1400))]);
    }

    #[test]
    fn absurd_slot_index_does_not_panic() {
        let mut f = table();
        frame_down(&mut f, 0, 10, 300, 400);
        // 越界 slot 夹到最后一格，别 panic，也别影响真手指
        f.apply(&abs(ABS_MT_SLOT, 9999));
        f.apply(&abs(ABS_MT_TRACKING_ID, 77));
        f.apply(&abs(ABS_MT_POSITION_X, 50));
        f.apply(&abs(ABS_MT_POSITION_Y, 60));
        assert!(matches!(f.apply(&syn()), Some(TouchEvent::Move { .. })));
        expect(&f, &[(10, 300.0, sy(400)), (77, 50.0, sy(60))]);
        // 负数 slot 当 0
        f.apply(&abs(ABS_MT_SLOT, -3));
        f.apply(&abs(ABS_MT_POSITION_X, 320));
        f.apply(&abs(ABS_MT_POSITION_Y, 410));
        assert!(matches!(f.apply(&syn()), Some(TouchEvent::Move { .. })));
        expect(&f, &[(10, 320.0, sy(410)), (77, 50.0, sy(60))]);
    }

    // ---- 6b / 6c：双指整轮手势（喂手指快照序列，不碰设备） --------------

    /// 两指点按（位移 5px、140ms）→ 撤销；抬完手不能再报一次。
    #[test]
    fn two_finger_tap_undoes() {
        let mut g = TwoFinger::new();
        assert_eq!(
            g.update(&[(1, 300.0, 400.0), (2, 900.0, 400.0)], 0.0),
            TwoFingerAct::None
        );
        assert_eq!(
            g.update(&[(1, 302.0, 403.0), (2, 902.0, 403.0)], 60.0),
            TwoFingerAct::None
        );
        assert_eq!(
            g.update(&[(1, 303.0, 405.0), (2, 903.0, 405.0)], 120.0),
            TwoFingerAct::None
        );
        // 先抬一根：点按在这一刻结算
        assert_eq!(g.update(&[(1, 303.0, 405.0)], 140.0), TwoFingerAct::Undo);
        // 全抬：不能重复触发
        assert_eq!(g.update(&[], 160.0), TwoFingerAct::None);
        // 再点一轮还能触发
        assert_eq!(
            g.update(&[(5, 200.0, 200.0), (6, 800.0, 200.0)], 1000.0),
            TwoFingerAct::None
        );
        assert_eq!(g.update(&[], 1100.0), TwoFingerAct::Undo);
    }

    /// 双指捏合（拉开 120px）不能误判成撤销；> 400ms 的长按也不行。
    #[test]
    fn pinch_and_long_hold_are_not_undo() {
        let mut g = TwoFinger::new();
        g.update(&[(1, 400.0, 500.0), (2, 800.0, 500.0)], 0.0);
        // 两指分开 120px：位移 > 40px，整轮归原来的捏合手势
        g.update(&[(1, 340.0, 500.0), (2, 860.0, 500.0)], 100.0);
        assert_eq!(g.update(&[(1, 340.0, 500.0)], 150.0), TwoFingerAct::None);

        let mut g = TwoFinger::new();
        g.update(&[(1, 400.0, 500.0), (2, 800.0, 500.0)], 0.0);
        g.update(&[(1, 402.0, 503.0), (2, 802.0, 503.0)], 200.0);
        assert_eq!(
            g.update(&[(1, 402.0, 503.0)], 520.0),
            TwoFingerAct::None,
            "520ms 才松是长按，不是点按"
        );
    }

    /// 抬指帧冒出来的假第二根手指（只有一两帧）不能当点按，否则单击也会撤销。
    #[test]
    fn single_frame_second_finger_is_not_a_tap() {
        let mut g = TwoFinger::new();
        assert_eq!(g.update(&[(1, 500.0, 600.0)], 0.0), TwoFingerAct::None);
        assert_eq!(
            g.update(&[(1, 500.0, 600.0), (2, 502.0, 601.0)], 9.0),
            TwoFingerAct::None
        );
        assert_eq!(
            g.update(&[(1, 500.0, 600.0)], 16.0),
            TwoFingerAct::None,
            "一帧就抬的是假手指"
        );
    }

    /// 两指同时向下拖 260px → 收工具条（拖的过程中就报，不等松手）；
    /// 横向为主 / 一上一下都不算。
    #[test]
    fn two_finger_swipe_down_hides_toolbar() {
        let mut g = TwoFinger::new();
        assert_eq!(
            g.update(&[(1, 400.0, 300.0), (2, 900.0, 300.0)], 0.0),
            TwoFingerAct::None
        );
        // 只走 150px：还没到 200
        assert_eq!(
            g.update(&[(1, 410.0, 450.0), (2, 910.0, 450.0)], 80.0),
            TwoFingerAct::None
        );
        // 到 260px：够
        assert_eq!(
            g.update(&[(1, 420.0, 560.0), (2, 920.0, 560.0)], 160.0),
            TwoFingerAct::HideToolbar
        );
        // 同一轮不重复报
        assert_eq!(
            g.update(&[(1, 430.0, 700.0), (2, 930.0, 700.0)], 220.0),
            TwoFingerAct::None
        );
        // 松手也不能再结算成撤销
        assert_eq!(g.update(&[], 240.0), TwoFingerAct::None);

        // 斜着拖：下移 250px 但横向也 300px —— 竖向不占主，不算下滑
        let mut g = TwoFinger::new();
        g.update(&[(1, 200.0, 300.0), (2, 600.0, 300.0)], 0.0);
        assert_eq!(
            g.update(&[(1, 500.0, 550.0), (2, 900.0, 550.0)], 100.0),
            TwoFingerAct::None
        );

        // 一根向下、一根向上：不是"两指一起向下"
        let mut g = TwoFinger::new();
        g.update(&[(1, 300.0, 300.0), (2, 700.0, 900.0)], 0.0);
        assert_eq!(
            g.update(&[(1, 300.0, 600.0), (2, 700.0, 400.0)], 100.0),
            TwoFingerAct::None
        );
    }

    // ---- 三指点按 = 重做（官方 `threeFingerTapFilter`） --------------------

    /// 三指点按（位移 6px、150ms）→ 重做；抬完不能再报一次。
    #[test]
    fn three_finger_tap_redoes() {
        let mut g = ThreeFinger::new();
        let pts = [(1, 300.0, 400.0), (2, 700.0, 400.0), (3, 1100.0, 400.0)];
        assert_eq!(g.update(&pts, 0.0), ThreeFingerAct::None);
        let down = [
            (1, 302.0, 402.0),
            (2, 703.0, 401.0),
            (3, 1102.0, 403.0),
        ];
        assert_eq!(g.update(&down, 70.0), ThreeFingerAct::None);
        // 松一根：点按在这一刻结算
        assert_eq!(
            g.update(&down[..2], 150.0),
            ThreeFingerAct::Redo,
            "三指点按应当报重做"
        );
        // 全抬：不能重复触发
        assert_eq!(g.update(&[], 170.0), ThreeFingerAct::None);
        // 再点一轮还能触发
        assert_eq!(g.update(&pts, 1000.0), ThreeFingerAct::None);
        assert_eq!(g.update(&[], 1100.0), ThreeFingerAct::Redo);
    }

    /// 三指张开 / 按住超过 400ms / 只按了两根：都不是三指点按。
    #[test]
    fn three_finger_long_hold_and_spread_are_not_redo() {
        // 张开 80px（超过官方 20px 掌面阈值）
        let mut g = ThreeFinger::new();
        g.update(&[(1, 300.0, 400.0), (2, 700.0, 400.0), (3, 1100.0, 400.0)], 0.0);
        g.update(&[(1, 260.0, 400.0), (2, 700.0, 400.0), (3, 1140.0, 400.0)], 100.0);
        assert_eq!(g.update(&[], 150.0), ThreeFingerAct::None);

        // 按住 600ms：长按不是点按
        let mut g = ThreeFinger::new();
        g.update(&[(1, 300.0, 400.0), (2, 700.0, 400.0), (3, 1100.0, 400.0)], 0.0);
        g.update(&[(1, 302.0, 402.0), (2, 702.0, 402.0), (3, 1102.0, 402.0)], 200.0);
        assert_eq!(g.update(&[], 600.0), ThreeFingerAct::None);

        // 只有两根：一轮都开不起来
        let mut g = ThreeFinger::new();
        g.update(&[(1, 300.0, 400.0), (2, 700.0, 400.0)], 0.0);
        assert_eq!(g.update(&[], 120.0), ThreeFingerAct::None);
    }

    /// 四根以上（手掌压上来）不算三指点按，也不会误重做。
    #[test]
    fn palm_with_four_fingers_is_not_a_three_finger_tap() {
        let four = [
            (1, 300.0, 400.0),
            (2, 600.0, 400.0),
            (3, 900.0, 400.0),
            (4, 1200.0, 400.0),
        ];
        // 手掌整块压上来又整块抬起（两帧之内）：不报重做
        let mut g = ThreeFinger::new();
        assert_eq!(g.update(&four, 0.0), ThreeFingerAct::None);
        assert_eq!(g.update(&four, 10.0), ThreeFingerAct::None);
        assert_eq!(g.update(&[], 20.0), ThreeFingerAct::None);

        // 已经三指按住了，第四根（手掌）落下来：这一轮作废，抬手也不补报
        let mut g = ThreeFinger::new();
        assert_eq!(
            g.update(&[(1, 300.0, 400.0), (2, 600.0, 400.0), (3, 900.0, 400.0)], 0.0),
            ThreeFingerAct::None
        );
        assert_eq!(g.update(&four, 50.0), ThreeFingerAct::None);
        assert_eq!(g.update(&[], 120.0), ThreeFingerAct::None);
    }

    /// 三指点一下只能重做，不能顺带撤销（两指识别器会看到前两根按了又松）。
    #[test]
    fn three_finger_tap_never_also_undoes() {
        let mut three = ThreeFinger::new();
        let mut two = TwoFinger::new();
        let pts = [(1, 300.0, 400.0), (2, 700.0, 400.0), (3, 1100.0, 400.0)];
        assert_eq!(classify(&pts, &mut three, &mut two, 0.0), (ThreeFingerAct::None, TwoFingerAct::None));
        assert_eq!(
            classify(&pts, &mut three, &mut two, 60.0),
            (ThreeFingerAct::None, TwoFingerAct::None)
        );
        // 三根一起抬
        assert_eq!(
            classify(&[], &mut three, &mut two, 140.0),
            (ThreeFingerAct::Redo, TwoFingerAct::None),
            "三指点按只能给重做，不能再给一次撤销"
        );
        // 三根 → 两根：重做已经结算，剩下的两根不能补一个撤销
        let mut three = ThreeFinger::new();
        let mut two = TwoFinger::new();
        classify(&pts, &mut three, &mut two, 0.0);
        assert_eq!(
            classify(&pts[..2], &mut three, &mut two, 90.0),
            (ThreeFingerAct::Redo, TwoFingerAct::None)
        );
        assert_eq!(
            classify(&[], &mut three, &mut two, 110.0),
            (ThreeFingerAct::None, TwoFingerAct::None)
        );
    }

    /// 两指撤销照旧：`classify` 不能把原来的两指手势弄丢。
    #[test]
    fn classify_keeps_two_finger_undo() {
        let mut three = ThreeFinger::new();
        let mut two = TwoFinger::new();
        let pts = [(1, 300.0, 400.0), (2, 900.0, 400.0)];
        assert_eq!(classify(&pts, &mut three, &mut two, 0.0), (ThreeFingerAct::None, TwoFingerAct::None));
        assert_eq!(classify(&pts, &mut three, &mut two, 50.0), (ThreeFingerAct::None, TwoFingerAct::None));
        assert_eq!(
            classify(&pts[..1], &mut three, &mut two, 120.0),
            (ThreeFingerAct::None, TwoFingerAct::Undo)
        );
        // 两指下滑收工具条也照旧
        let mut three = ThreeFinger::new();
        let mut two = TwoFinger::new();
        classify(&[(1, 400.0, 300.0), (2, 900.0, 300.0)], &mut three, &mut two, 0.0);
        assert_eq!(
            classify(&[(1, 420.0, 560.0), (2, 920.0, 560.0)], &mut three, &mut two, 160.0),
            (ThreeFingerAct::None, TwoFingerAct::HideToolbar)
        );
    }
}
