//! rm-sketch — M1 skeleton for a from-scratch reMarkable 2 note app.
//!
//! Scope of M1: prove that a Rust process can own the display, see the pen at
//! full event rate, draw ink, and repaint only the dirty rectangle — fast
//! enough that writing feels right. Everything else (pages, `.rm` storage,
//! toolbar chrome) comes later.
//!
//! Runs only under toltec rm2fb: `rm2fb-client ./rm-sketch`. Without that
//! preload `/dev/fb0` is the raw mxsfb line buffer (260x1408 @32bpp) and the
//! program refuses to start.
//!
//! Exit: tap the bottom-left corner three times, or send SIGTERM.

use std::fs::OpenOptions;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd};

// ---------------------------------------------------------------- framebuffer

const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;
const FBIOGET_FSCREENINFO: libc::c_ulong = 0x4602;
/// _IOW('F', 0x2E, struct mxcfb_update_data)  — 72-byte struct, arm32
const MXCFB_SEND_UPDATE: libc::c_ulong = 0x4048_462E;

const UPDATE_MODE_PARTIAL: u32 = 0;
const UPDATE_MODE_FULL: u32 = 1;

// Waveform modes as understood by the mxcfb driver / rm2fb shim.
const WF_DU: u32 = 1; // fastest, monochrome
const WF_GC16: u32 = 2; // full 16-level grayscale, slower, no ghosting

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct Rect {
    top: u32,
    left: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct AltBufferData {
    phys_addr: u32,
    width: u32,
    height: u32,
    alt_update_region: Rect,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct UpdateData {
    update_region: Rect,
    waveform_mode: u32,
    update_mode: u32,
    update_marker: u32,
    temp: i32,
    flags: u32,
    dither_mode: i32,
    quant_bit: i32,
    alt_buffer_data: AltBufferData,
}

const BLACK: u16 = 0x0000;
const WHITE: u16 = 0xFFFF;

struct Fb {
    file: std::fs::File,
    base: *mut u8,
    len: usize,
    pub width: i32,
    pub height: i32,
    stride: usize,
    bpp: usize,
}

impl Fb {
    fn open() -> io::Result<Fb> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/fb0")?;
        let fd = file.as_raw_fd();

        let mut var = [0u8; 160];
        let mut fix = [0u8; 68];
        unsafe {
            if libc::ioctl(fd, FBIOGET_VSCREENINFO, var.as_mut_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(fd, FBIOGET_FSCREENINFO, fix.as_mut_ptr()) != 0 {
                return Err(io::Error::last_os_error());
            }
        }

        let u32_at = |b: &[u8], o: usize| u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
        let width = u32_at(&var, 0) as i32;
        let height = u32_at(&var, 4) as i32;
        // FBIOGET_VSCREENINFO reports bits per pixel; we index in bytes.
        let bits = u32_at(&var, 24);
        let bpp = (bits / 8) as usize;
        let stride = u32_at(&fix, 44) as usize;

        // Guard against running without the rm2fb shim: the raw mxsfb buffer
        // reports a 260x1408/32bpp line-buffer geometry instead of the panel.
        if width != 1404 || height != 1872 || bits != 16 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "unexpected framebuffer {width}x{height} @{bits}bpp \
                     (found {stride} byte stride). Run under /opt/bin/rm2fb-client."
                ),
            ));
        }

        let len = stride * height as usize;
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }

        Ok(Fb {
            file,
            base: base as *mut u8,
            len,
            width,
            height,
            stride,
            bpp,
        })
    }

    #[inline]
    fn put(&mut self, x: i32, y: i32, color: u16) {
        if x < 0 || y < 0 || x >= self.width || y >= self.height {
            return;
        }
        let off = y as usize * self.stride + x as usize * self.bpp;
        if off + 1 < self.len {
            unsafe {
                let p = self.base.add(off) as *mut u16;
                p.write_unaligned(color);
            }
        }
    }

    fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u16) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.put(xx, yy, color);
            }
        }
    }

    fn clear(&mut self, color: u16) {
        for y in 0..self.height {
            for x in 0..self.width {
                self.put(x, y, color);
            }
        }
    }

    /// Round stamp; together with short segments this gives a smooth stroke
    /// without needing per-pixel anti-aliasing yet.
    fn stamp(&mut self, cx: i32, cy: i32, r: i32, color: u16) {
        if r <= 0 {
            self.put(cx, cy, color);
            return;
        }
        for dy in -r..=r {
            for dx in -r..=r {
                if dx * dx + dy * dy <= r * r {
                    self.put(cx + dx, cy + dy, color);
                }
            }
        }
    }

    fn line(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, r: i32, color: u16) {
        let dx = (x1 - x0).abs();
        let dy = (y1 - y0).abs();
        let steps = dx.max(dy).max(1);
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            let x = x0 + ((x1 - x0) as f32 * t).round() as i32;
            let y = y0 + ((y1 - y0) as f32 * t).round() as i32;
            self.stamp(x, y, r, color);
        }
    }

    fn refresh(&mut self, x: i32, y: i32, w: i32, h: i32, waveform: u32, mode: u32) {
        let left = x.clamp(0, self.width - 1);
        let top = y.clamp(0, self.height - 1);
        let width = w.clamp(1, self.width - left);
        let height = h.clamp(1, self.height - top);

        let mut ud = UpdateData {
            update_region: Rect {
                top: top as u32,
                left: left as u32,
                width: width as u32,
                height: height as u32,
            },
            waveform_mode: waveform,
            update_mode: mode,
            update_marker: 0,
            temp: 0,
            flags: 0,
            dither_mode: 0,
            quant_bit: 0,
            alt_buffer_data: AltBufferData::default(),
        };

        let fd = self.file.as_raw_fd();
        unsafe {
            let r = libc::ioctl(fd, MXCFB_SEND_UPDATE, &mut ud as *mut UpdateData);
            if r != 0 {
                let e = io::Error::last_os_error();
                eprintln!("[refresh] ioctl failed: {e} (region {left},{top} {width}x{height})");
            }
        }
    }
}

impl Drop for Fb {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.base as *mut libc::c_void, self.len);
        }
    }
}

// ---------------------------------------------------------------------- input

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct InputEvent {
    tv_sec: i32,
    tv_usec: i32,
    typ: u16,
    code: u16,
    value: i32,
}

const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_ABS: u16 = 0x03;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
const ABS_PRESSURE: u16 = 0x18;
const BTN_TOOL_PEN: u16 = 0x140;
const BTN_TOUCH: u16 = 0x14A;

// Digitizer native ranges on the reMarkable 2 (landscape, panel space).
const RAW_MAX_X: f32 = 20966.0;
const RAW_MAX_Y: f32 = 15725.0;

struct Pen {
    file: std::fs::File,
    raw_x: i32,
    raw_y: i32,
    pressure: i32,
    /// pen is within digitiser range (hovering counts, this is BTN_TOOL_PEN)
    in_range: bool,
    /// pen tip is touching the glass (BTN_TOUCH) — only this draws
    btn_touch: bool,
    /// some firmware never emits BTN_TOUCH; then pressure decides contact
    saw_touch: bool,
    /// derived contact state for the current frame
    down: bool,
}

impl Pen {
    fn open(dev: &str) -> io::Result<Pen> {
        let file = OpenOptions::new().read(true).open(dev)?;
        Ok(Pen {
            file,
            raw_x: 0,
            raw_y: 0,
            pressure: 0,
            in_range: false,
            btn_touch: false,
            saw_touch: false,
            down: false,
        })
    }

    fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    fn read_batch(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        unsafe {
            let n = libc::read(self.fd(), buf.as_mut_ptr() as *mut libc::c_void, buf.len());
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }
    }
}

/// Panel space (what the pen reports) -> framebuffer space (what we draw into).
/// rm2fb exposes the panel rotated: fb is portrait 1404x1872 while the
/// digitizer stays in landscape 20966x15725.
fn map_to_fb(raw_x: i32, raw_y: i32, fb_w: i32, fb_h: i32) -> (i32, i32) {
    let x_norm = raw_y as f32 / RAW_MAX_Y; // panel Y -> fb X
    let y_norm = 1.0 - raw_x as f32 / RAW_MAX_X; // panel X -> fb Y (inverted)
    (
        (x_norm * fb_w as f32).round() as i32,
        (y_norm * fb_h as f32).round() as i32,
    )
}

// ----------------------------------------------------------------------- app

struct Stroke {
    points: Vec<(i32, i32, i32)>, // x, y, radius
}

// ------------------------------------------------------------------- brushes

/// Pressure -> pen width mapping.
///
/// `k` is the "hardness" knob: the width is `min + (max-min) * p^k`, so
///   k > 1  the pen stays thin until you press hard  (dry, hard nib)
///   k = 1  linear pressure response                (fountain pen)
///   k < 1  light touch already spreads            (soft, inky)
/// `fixed` ignores pressure entirely, like a ballpoint / fineliner.
#[derive(Clone, Copy)]
struct Brush {
    name: &'static str,
    min_w: f32,
    max_w: f32,
    k: f32,
    fixed: bool,
}

const BRUSHES: [Brush; 4] = [
    Brush { name: "pencil", min_w: 0.0, max_w: 1.5, k: 2.0, fixed: false },
    Brush { name: "pen", min_w: 0.0, max_w: 2.4, k: 1.7, fixed: false },
    Brush { name: "fineliner", min_w: 1.2, max_w: 1.2, k: 1.0, fixed: true },
    Brush { name: "marker", min_w: 1.5, max_w: 3.6, k: 0.9, fixed: false },
];

impl Brush {
    fn radius(&self, pressure: f32) -> i32 {
        if self.fixed {
            return self.max_w.round() as i32;
        }
        let p = (pressure / 4096.0).clamp(0.0, 1.0);
        let r = self.min_w + (self.max_w - self.min_w) * p.powf(self.k);
        r.round() as i32
    }
}

/// Optional environment overrides so the feel can be tuned without rebuilding:
///   RM_BRUSH=0..3   preset index
///   RM_BRUSH_K=1.7  override hardness
///   RM_BRUSH_W=0,2.4 override min,max radius
fn brush_from_env() -> (usize, Brush) {
    // 默认用恒宽签字笔：手感最接近官方钢笔，且不受压力抖动影响
    let mut idx = 2usize;
    if let Ok(v) = std::env::var("RM_BRUSH") {
        idx = v.trim().parse::<usize>().unwrap_or(2).min(BRUSHES.len() - 1);
    }
    let mut b = BRUSHES[idx];
    if let Ok(v) = std::env::var("RM_BRUSH_K") {
        if let Ok(k) = v.trim().parse::<f32>() {
            b.k = k.clamp(0.2, 6.0);
            b.fixed = false;
        }
    }
    if let Ok(v) = std::env::var("RM_BRUSH_W") {
        let parts: Vec<f32> = v
            .split(',')
            .filter_map(|t| t.trim().parse::<f32>().ok())
            .collect();
        if parts.len() == 2 {
            b.min_w = parts[0].clamp(0.0, 8.0);
            b.max_w = parts[1].clamp(b.min_w, 8.0);
            b.fixed = (b.max_w - b.min_w).abs() < 0.05;
        }
    }
    (idx, b)
}

const TOOLBAR_H: i32 = 72;
const UNDO_BTN_W: i32 = 120;

const BRUSH_BTN_X: i32 = 168;
const BRUSH_BTN_W: i32 = 140;

fn draw_chrome(fb: &mut Fb, brush: &Brush) {
    fb.fill_rect(0, 0, fb.width, TOOLBAR_H, 0xE71C); // light grey bar
    fb.fill_rect(0, TOOLBAR_H - 2, fb.width, 2, BLACK);

    // Undo button.
    fb.fill_rect(24, 20, UNDO_BTN_W, TOOLBAR_H - 40, WHITE);
    for (i, y) in [26, 32, 38, 44].iter().enumerate() {
        fb.fill_rect(36, *y, 24 + (i as i32 * 6), 2, BLACK);
    }

    // Brush button: a live sample drawn with the current pressure curve.
    fb.fill_rect(BRUSH_BTN_X, 20, BRUSH_BTN_W, TOOLBAR_H - 40, WHITE);
    let (x0, y0) = (BRUSH_BTN_X + 16, 46);
    let (x1, y1) = (BRUSH_BTN_X + BRUSH_BTN_W - 16, 28);
    let steps = (x1 - x0).max(1);
    let mut prev = (x0, y0, brush.radius(200.0));
    for i in 1..=steps {
        let t = i as f32 / steps as f32;
        let x = x0 + i;
        let y = y0 + ((y1 - y0) as f32 * t).round() as i32;
        let p = 200.0 + 3800.0 * t;
        let cur = (x, y, brush.radius(p));
        fb.line(prev.0, prev.1, cur.0, cur.1, cur.2, BLACK);
        prev = cur;
    }
}

fn redraw_all(fb: &mut Fb, strokes: &[Stroke], brush: &Brush) {
    fb.clear(WHITE);
    for s in strokes {
        for w in s.points.windows(2) {
            let (x0, y0, r0) = w[0];
            let (x1, y1, r1) = w[1];
            fb.line(x0, y0, x1, y1, (r0 + r1) / 2, BLACK);
        }
        if s.points.len() == 1 {
            let (x, y, r) = s.points[0];
            fb.stamp(x, y, r, BLACK);
        }
    }
    draw_chrome(fb, brush);
    let (w, h) = (fb.width, fb.height);
    fb.refresh(0, 0, w, h, WF_GC16, UPDATE_MODE_FULL);
}

fn selftest() {
    let mut fb = match Fb::open() {
        Ok(fb) => fb,
        Err(e) => {
            eprintln!("selftest: {e}");
            std::process::exit(1);
        }
    };
    eprintln!("selftest: fb {}x{} stride {}", fb.width, fb.height, fb.stride);
    let checksum = |fb: &Fb| -> u64 {
        let mut sum: u64 = 0;
        unsafe {
            let px = std::slice::from_raw_parts(fb.base as *const u16, fb.len / 2);
            for (i, v) in px.iter().enumerate() {
                if i % 512 == 0 {
                    sum = sum.wrapping_mul(31).wrapping_add(*v as u64);
                }
            }
        }
        sum
    };
    fb.clear(WHITE);
    let non_white = unsafe {
        let px = std::slice::from_raw_parts(fb.base as *const u16, fb.len / 2);
        px.iter().filter(|v| **v != WHITE).count()
    };
    eprintln!("selftest: non-white pixels right after clear = {non_white} / {}", fb.len / 2);
    // Hammer the whole mapping with a raw memset: no loop/bounds bug can survive
    // this, so anything still non-white afterwards was written by another process.
    unsafe { std::ptr::write_bytes(fb.base, 0xFF, fb.len) };
    let after_memset = unsafe {
        let px = std::slice::from_raw_parts(fb.base as *const u16, fb.len / 2);
        px.iter().filter(|v| **v != WHITE).count()
    };
    eprintln!("selftest: non-white pixels right after memset = {after_memset}");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let after_sleep = unsafe {
        let px = std::slice::from_raw_parts(fb.base as *const u16, fb.len / 2);
        px.iter().filter(|v| **v != WHITE).count()
    };
    eprintln!("selftest: non-white pixels 300ms later       = {after_sleep}");
    let before = checksum(&fb);
    eprintln!("selftest: checksum AFTER clear, BEFORE refresh = {before:016x}");
    let (w, h) = (fb.width, fb.height);
    fb.refresh(0, 0, w, h, WF_GC16, UPDATE_MODE_FULL);
    let after = checksum(&fb);
    eprintln!("selftest: checksum AFTER refresh            = {after:016x}");
    if before != after {
        eprintln!("selftest: >>> the refresh wrote back into the buffer <<<");
    }
    eprintln!("selftest: sleeping 15s so the host can snapshot /dev/shm/swtfb.01");
    std::thread::sleep(std::time::Duration::from_secs(15));
    eprintln!("selftest: bye");
}

fn main() {
    if std::env::args().any(|a| a == "--selftest") {
        selftest();
        return;
    }
    let mut fb = match Fb::open() {
        Ok(fb) => fb,
        Err(e) => {
            eprintln!("rm-sketch: {e}");
            std::process::exit(1);
        }
    };

    let mut pen = match Pen::open("/dev/input/event1") {
        Ok(p) => p,
        Err(e) => {
            eprintln!("rm-sketch: cannot open pen device: {e}");
            std::process::exit(1);
        }
    };

    eprintln!(
        "rm-sketch: fb {}x{} stride {}",
        fb.width, fb.height, fb.stride
    );

    let (mut brush_idx, mut brush) = brush_from_env();
    eprintln!(
        "rm-sketch: brush = {} (k={}, w={}..{}, fixed={})",
        brush.name, brush.k, brush.min_w, brush.max_w, brush.fixed
    );

    let mut strokes: Vec<Stroke> = Vec::new();
    let mut smooth_p: f32 = 0.0;
    let mut current: Option<Stroke> = None;
    let mut last_pt: Option<(i32, i32)> = None;

    // Corner-tap exit counter, and undo tap tracking.
    let mut corner_taps = 0;
    let mut tap_start: Option<(i32, i32)> = None;
    let mut tap_moved = false;
    // A pen-down arrives before the first ABS_* of the stroke, so hold off on
    // stamping until we actually have a position for this contact.
    let mut have_pos = false;

    eprintln!("[rm-sketch] drawing initial frame");
    redraw_all(&mut fb, &strokes, &brush);
    eprintln!("[rm-sketch] initial frame drawn + refresh sent");

    let mut buf = [0u8; 16 * 64];
    'outer: loop {
        let n = match pen.read_batch(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                eprintln!("rm-sketch: pen read failed: {e}");
                break;
            }
        };

        let mut syn = false;
        for chunk in buf[..n].chunks_exact(16) {
            let ev = unsafe { std::ptr::read_unaligned(chunk.as_ptr() as *const InputEvent) };
            match (ev.typ, ev.code) {
                (EV_KEY, BTN_TOOL_PEN) => {
                    // Proximity only: a hovering pen is in range but NOT touching.
                    pen.in_range = ev.value != 0;
                }
                (EV_KEY, BTN_TOUCH) => {
                    pen.saw_touch = true;
                    pen.btn_touch = ev.value != 0;
                }
                (EV_ABS, ABS_X) => {
                    pen.raw_x = ev.value;
                    have_pos = true;
                }
                (EV_ABS, ABS_Y) => {
                    pen.raw_y = ev.value;
                    have_pos = true;
                }
                (EV_ABS, ABS_PRESSURE) => pen.pressure = ev.value,
                (EV_SYN, _) => syn = true,
                _ => {}
            }
        }

        if !syn {
            continue;
        }

        // Contact requires proximity AND tip-down. On firmware that never sends
        // BTN_TOUCH, fall back to pressure.
        let contact = pen.in_range
            && if pen.saw_touch {
                pen.btn_touch
            } else {
                pen.pressure > 40
            };

        if contact && !pen.down {
            pen.down = true;
            tap_start = None;
            tap_moved = false;
            // A fresh contact must supply its own position before we start ink.
            have_pos = false;
            smooth_p = pen.pressure as f32;
        } else if !contact && pen.down {
            pen.down = false;
            if let Some(s) = current.take() {
                strokes.push(s);
            }
            last_pt = None;
            if !tap_moved {
                if let Some((x, y)) = tap_start {
                    if x < 64 && y > fb.height - 64 {
                        corner_taps += 1;
                        if corner_taps >= 3 {
                            eprintln!("rm-sketch: exit");
                            break 'outer;
                        }
                    } else if y < TOOLBAR_H && x < UNDO_BTN_W {
                        if strokes.pop().is_some() {
                            redraw_all(&mut fb, &strokes, &brush);
                        }
                    } else if y < TOOLBAR_H && x >= BRUSH_BTN_X && x < BRUSH_BTN_X + BRUSH_BTN_W {
                        brush_idx = (brush_idx + 1) % BRUSHES.len();
                        brush = BRUSHES[brush_idx];
                        eprintln!("rm-sketch: brush -> {} (k={})", brush.name, brush.k);
                        draw_chrome(&mut fb, &brush);
                        fb.refresh(0, 0, fb.width, TOOLBAR_H, WF_GC16, UPDATE_MODE_PARTIAL);
                    }
                }
            }
            tap_start = None;
        }

        if !pen.down {
            continue;
        }

        // Smooth the pressure so width does not jitter between samples.
        smooth_p = smooth_p * 0.6 + pen.pressure as f32 * 0.4;
        let (x, y) = map_to_fb(pen.raw_x, pen.raw_y, fb.width, fb.height);
        let radius = brush.radius(smooth_p);

        if current.is_none() {
            if have_pos {
                current = Some(Stroke {
                    points: vec![(x, y, radius)],
                });
                last_pt = Some((x, y));
                tap_start = Some((x, y));
                fb.stamp(x, y, radius, BLACK);
                fb.refresh(
                    x - 8,
                    y - 8,
                    16 + radius * 2,
                    16 + radius * 2,
                    WF_DU,
                    UPDATE_MODE_PARTIAL,
                );
            }
        } else if let Some(stroke) = current.as_mut() {
            if let Some((lx, ly)) = last_pt {
                if (x - lx).abs() + (y - ly).abs() >= 2 {
                    stroke.points.push((x, y, radius));
                    fb.line(lx, ly, x, y, radius, BLACK);
                    let (x0, y0) = (lx.min(x) - radius - 2, ly.min(y) - radius - 2);
                    let (x1, y1) = (lx.max(x) + radius + 2, ly.max(y) + radius + 2);
                    fb.refresh(x0, y0, x1 - x0, y1 - y0, WF_DU, UPDATE_MODE_PARTIAL);
                    last_pt = Some((x, y));
                    if let Some((tx, ty)) = tap_start {
                        if (x - tx).abs() + (y - ty).abs() > 24 {
                            tap_moved = true;
                        }
                    }
                }
            }
        }
    }

    eprintln!("rm-sketch: done, {} strokes", strokes.len());
}

