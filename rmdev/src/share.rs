//! 屏幕共享：一个极小的 HTTP 服务，把当前帧以 BMP 发给浏览器。
//!
//! 官方是 `ScreenShare`（走云端），我们砍掉订阅后自己实现最直接的一种：
//! 直接读 rm2fb 的共享内存 `/dev/shm/swtfb.01`（1404×1872 @16bpp），
//! 收到请求就抓一帧转成 BMP。**不占渲染路径**——写帧的时候不做任何拷贝。
//!
//! 画质：默认按 1/2 缩放（702×936），24 位色，一帧 ≈ 2 MB，局域网够快。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::time::Duration;

/// rm2fb 的帧缓冲（rm2fb-client 会把 /dev/fb0 的 mmap 指到这里）
/// 我们自己的帧导出（`Fb::dump_pgm`）：`main.rs` 在共享打开时每秒刷一次。
/// 以前读 `/dev/shm/swtfb.01`（rm2fb server 的缓冲），xochitl 停掉以后那里永远是白的。
pub const FRAME_PGM: &str = "/tmp/rmdev.share.pgm";
const W: usize = 1404;
const H: usize = 1872;

static RUNNING: AtomicBool = AtomicBool::new(false);
static PORT: AtomicU16 = AtomicU16::new(0);

pub fn active_port() -> Option<u16> {
    if RUNNING.load(Ordering::Relaxed) {
        Some(PORT.load(Ordering::Relaxed))
    } else {
        None
    }
}

/// 起服务，返回端口。已经在跑就直接返回原端口。
pub fn start() -> Result<u16, String> {
    if let Some(p) = active_port() {
        return Ok(p);
    }
    let mut bound = None;
    for p in 8000..8010u16 {
        if let Ok(l) = TcpListener::bind(("0.0.0.0", p)) {
            PORT.store(p, Ordering::Relaxed);
            bound = Some(l);
            break;
        }
    }
    let Some(listener) = bound else {
        return Err("8000-8009 端口都被占用了".into());
    };
    listener.set_nonblocking(true).ok();
    RUNNING.store(true, Ordering::Relaxed);
    let port = PORT.load(Ordering::Relaxed);
    std::thread::spawn(move || {
        while RUNNING.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let _ = serve(stream);
                }
                Err(_) => std::thread::sleep(Duration::from_millis(40)),
            }
        }
    });
    Ok(port)
}

pub fn stop() {
    RUNNING.store(false, Ordering::Relaxed);
}

fn serve(mut s: TcpStream) -> std::io::Result<()> {
    s.set_read_timeout(Some(Duration::from_millis(400))).ok();
    s.set_write_timeout(Some(Duration::from_secs(10))).ok();
    let mut buf = [0u8; 1024];
    let n = s.read(&mut buf).unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);
    let path = req
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .split('?')
        .next()
        .unwrap_or("/");

    match path {
        "/frame.bmp" | "/frame" => match frame_bmp() {
            Some(b) => {
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: image/bmp\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
                    b.len()
                );
                s.write_all(head.as_bytes())?;
                s.write_all(&b)?;
            }
            None => s.write_all(b"HTTP/1.1 503 Busy\r\nContent-Length: 0\r\n\r\n")?,
        },
        _ => {
            let body = PAGE.as_bytes();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
                body.len()
            );
            s.write_all(head.as_bytes())?;
            s.write_all(body)?;
        }
    }
    s.flush().ok();
    Ok(())
}

const PAGE: &str = r#"<!doctype html><meta charset="utf-8">
<title>reMarkable 屏幕共享</title>
<style>html,body{margin:0;background:#111;height:100%}
img{width:100vw;height:100vh;object-fit:contain;display:block}</style>
<img id="f" src="/frame.bmp">
<script>
// 每 800ms 拉一帧；用时间戳破缓存
const f = document.getElementById('f');
setInterval(() => { f.src = '/frame.bmp?t=' + Date.now(); }, 800);
</script>"#;

/// 抓一帧 → 1/2 缩放 → 24 位 BMP（灰度 PGM 起点，够看）
fn frame_bmp() -> Option<Vec<u8>> {
    let raw = std::fs::read(FRAME_PGM).ok()?;
    // P5 头：`P5\n<w> <h>\n255\n`，后面是 w*h 字节灰度
    // 手拆 PGM 头：`P5\n<w> <h>\n255\n` + 灰度体（不引入 split 的闭包写法）
    let mut idx = 0usize;
    let mut parts: Vec<&[u8]> = Vec::with_capacity(4);
    // 只有 3 个换行（`P5` / 尺寸 / 255），第 4 个会把二进制体当文本找，必错
    for _ in 0..3 {
        let nl = raw[idx..].iter().position(|&b| b == b'\n')? + idx;
        parts.push(&raw[idx..nl]);
        idx = nl + 1;
    }
    let dims = String::from_utf8_lossy(parts[1]).to_string();
    let body = &raw[idx..];
    let mut ds = dims.split_whitespace();
    let pw: usize = ds.next()?.parse().ok()?;
    let ph: usize = ds.next()?.parse().ok()?;
    if pw != W || ph != H || body.len() < W * H {
        return None;
    }
    let gray = body;
    let (w, h) = (W / 2, H / 2);
    let stride = (w * 3 + 3) & !3; // BMP 行按 4 字节对齐
    let mut out = Vec::with_capacity(54 + stride * h);
    // BITMAPFILEHEADER
    out.extend_from_slice(b"BM");
    let filesize = 54 + stride * h;
    out.extend_from_slice(&(filesize as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    // BITMAPINFOHEADER
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&((stride * h) as u32).to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    let pad = stride - w * 3;
    // BMP 是自下而上
    for y in (0..h).rev() {
        let row = (y * 2) * W;
        for x in 0..w {
            let v = gray[row + x * 2];
            out.push(v);
            out.push(v);
            out.push(v);
        }
        for _ in 0..pad {
            out.push(0);
        }
    }
    Some(out)
}

// ============================================================ P2 begin ===
//
// 发邮件对话框（官方 `res_06b193d.qml`，`ShareEmailDialog`）：纯逻辑 + 附件编码。
//
// 对话框结构照官方：黑头 `Cancel` / 标题 / `Send`，下面是行列表
// （`Send to`、`Selected`、`Send as` 三个单选 PDF/PNG/SVG、`Subject`、`File name`），
// 键盘收起时底部还有一个主按钮 `Send`。
//
// 本模块只放**纯函数**（解析/布局/文案/编码），绘制和 IO 在 `main.rs` 的 P2 块里。
// 文案全部来自官方 QML，一个字都不要改。

/// 黑头标题（官方 `Strings.sendByEmailString`，`frag_0081_006bd97e.qml`）
pub const MAIL_TITLE: &str = "Send by email";
/// 黑头左键（官方 `Header.rejectText`，非 HwcText 分支）
pub const CANCEL: &str = "Cancel";
/// 黑头右键 + 底部主按钮（官方 `Header.acceptText`）
pub const SEND: &str = "Send";
/// 收件人一节的行标题（官方 `sendToSection.title`）
pub const SEND_TO: &str = "Send to";
/// 收件人为空时的占位（官方 `AddItemButton.title`）
pub const ADD_EMAIL: &str = "Add email address";
/// 页选择那一行的标题（官方 `selectedPagesItem.title`）
pub const SELECTED: &str = "Selected";
/// 格式一节的标题（官方 `formatTypeColumn` 的 `TextHeaderRegular`）
pub const SEND_AS: &str = "Send as";
/// 文件名 / 标题那两行（官方 `filenameInput.title` 的两个分支）
pub const SUBJECT: &str = "Subject";
pub const FILE_NAME: &str = "File name";
/// 页选择的两个值文案（官方 `selectedPagesText()`：`Page %1` / `All pages`）
pub const PAGE_LABEL: &str = "Page";
pub const ALL_PAGES: &str = "All pages";
/// 能力不足的选项。官方按能力显隐；我们沿用 `Text settings` 里的同一句。
pub const NOT_AVAILABLE: &str = "Not available";
/// 发送失败（官方串按契约用；QML 树里没抓到）
pub const SEND_FAILED: &str = "Could not send email.";
/// 正文尾部（沿用原 `send_mail_now` 的正文）
pub const SENT_FROM: &str = "Sent from reMarkable 2";

/// 设备是 16 级灰屏，"红字"只能用最深的灰。
pub const ERR_TONE: u8 = 40;

/// 一次最多渲染多少页（设备内存有限；服务端单封附件 ≤ 20 MB）。
pub const MAX_PAGES: usize = 20;
/// 服务端一封邮件最多 5 个附件（`docs/CLOUD.md` §8），PNG 多页按这个截断。
pub const MAX_PNG_ATTACHMENTS: usize = 5;

/// 键盘焦点 / 行下标。`F_NONE` 表示键盘收起。
pub const F_NONE: u8 = 0;
pub const F_TO: u8 = 1;
pub const F_SUBJECT: u8 = 2;
pub const F_FILE: u8 = 3;
pub const F_RANGE: u8 = 4;

/// 附件格式（官方 `ShareEmailDialog.ExportFormat`，只取前三个）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MailFormat {
    Pdf,
    Png,
    Svg,
}

/// 官方 `_formats` 的顺序：PDF / PNG / SVG。
pub const FORMATS: [MailFormat; 3] = [MailFormat::Pdf, MailFormat::Png, MailFormat::Svg];

impl MailFormat {
    /// 单选按钮上的字（官方 `RadioButton.text`）
    pub fn label(self) -> &'static str {
        match self {
            MailFormat::Pdf => "PDF",
            MailFormat::Png => "PNG",
            MailFormat::Svg => "SVG",
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            MailFormat::Pdf => "pdf",
            MailFormat::Png => "png",
            MailFormat::Svg => "svg",
        }
    }

    pub fn mime(self) -> &'static str {
        match self {
            MailFormat::Pdf => "application/pdf",
            MailFormat::Png => "image/png",
            MailFormat::Svg => "image/svg+xml",
        }
    }

    /// 有没有渲染器。SVG 没有，按官方"按能力显隐"画成灰的 `Not available`。
    pub fn available(self) -> bool {
        !matches!(self, MailFormat::Svg)
    }

    /// 点一下换下一个**可用**格式：PDF → PNG → PDF（没渲染器的跳过）。
    pub fn cycle(self) -> MailFormat {
        match self {
            MailFormat::Pdf => MailFormat::Png,
            _ => MailFormat::Pdf,
        }
    }
}

/// 发哪几页。**页号一律 1 起**（和界面文案一致），`resolve()` 再换成 0 起。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PagePick {
    /// 当前页（1 起）
    Current(usize),
    All,
    /// 挑出来的页（1 起，可能乱序/重复，`numbers()` 会整理）
    Range(Vec<usize>),
}

impl PagePick {
    /// 整理成 1 起、升序、去重、且落在 `1..=total` 的页号。
    pub fn numbers(&self, total: usize) -> Vec<usize> {
        match self {
            PagePick::Current(k) => {
                if *k >= 1 && *k <= total {
                    vec![*k]
                } else {
                    Vec::new()
                }
            }
            PagePick::All => (1..=total).collect(),
            PagePick::Range(v) => {
                let mut v: Vec<usize> = v
                    .iter()
                    .copied()
                    .filter(|n| *n >= 1 && *n <= total)
                    .collect();
                v.sort_unstable();
                v.dedup();
                v
            }
        }
    }

    /// 渲染用：0 起、升序、去重。
    pub fn resolve(&self, total: usize) -> Vec<usize> {
        self.numbers(total).into_iter().map(|n| n - 1).collect()
    }
}

/// 行列表上的动作。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Act {
    Cancel,
    Send,
    To,
    Pages,
    Subject,
    Filename,
    Format(MailFormat),
}

/// 黑头高（和 `nbsettings` 一样）
pub const HDR_H: i32 = 118;
/// 行高 / 第一行顶边
pub const ROW_H: i32 = 120;
const ROW_TOP: i32 = 170;
/// `Send to` 行顶边
pub const TO_Y: i32 = ROW_TOP;
/// `Selected` 行顶边
pub const PAGES_Y: i32 = ROW_TOP + ROW_H;
/// `Send as` 段头基线
pub const SEND_AS_Y: i32 = 470;
/// `Subject` / `File name` 行顶边
pub const SUBJECT_Y: i32 = 790;
pub const FILE_Y: i32 = 910;
/// 底部主按钮：距屏底 200，左右各留 100。
const SEND_BTN_BOTTOM: i32 = 200;

/// 格式单选行的圆心 y（行距 80，命中带 80 高、互不重叠）
pub fn radio_cy(i: usize) -> i32 {
    540 + i as i32 * 80
}

/// 整页 sheet 的命中区。后压的在上层（黑头 → 底部按钮 → 行）。
pub fn layout(w: i32, h: i32, kb_open: bool) -> Vec<(Act, (i32, i32, i32, i32))> {
    let mut v = Vec::with_capacity(10);
    v.push((Act::To, (0, TO_Y, w, ROW_H)));
    v.push((Act::Pages, (0, PAGES_Y, w, ROW_H)));
    for (i, f) in FORMATS.iter().enumerate() {
        let cy = radio_cy(i);
        v.push((Act::Format(*f), (0, cy - 40, w, 80)));
    }
    v.push((Act::Subject, (0, SUBJECT_Y, w, ROW_H)));
    v.push((Act::Filename, (0, FILE_Y, w, ROW_H)));
    // 键盘收起时才画/才可点底部主按钮——键盘会盖住它
    if !kb_open {
        v.push((
            Act::Send,
            (100, h - SEND_BTN_BOTTOM, w - 200, 90),
        ));
    }
    // 黑头最后压：`Cancel` 左、`Send` 右
    v.push((Act::Cancel, (0, 0, 240, HDR_H)));
    v.push((Act::Send, (w - 240, 0, 240, HDR_H)));
    v
}

/// 命中测试。键盘由调用方先处理；没有渲染器的格式点不到。
pub fn hit(x: f32, y: f32, w: i32, h: i32, kb_open: bool) -> Option<Act> {
    layout(w, h, kb_open).into_iter().rev().find_map(|(a, r)| {
        let inside = x >= r.0 as f32
            && y >= r.1 as f32
            && x < (r.0 + r.2) as f32
            && y < (r.1 + r.3) as f32;
        if !inside {
            return None;
        }
        if let Act::Format(f) = a {
            if !f.available() {
                return None;
            }
        }
        Some(a)
    })
}

/// 页选择的显示文案：`All pages` / `Page 1-3, 5`；一页都选不出来时给空串
/// （调用方这时显示用户原样输入的区间，`Send` 也灰着）。
pub fn pages_label(p: &PagePick) -> String {
    match p {
        PagePick::All => ALL_PAGES.to_string(),
        _ => {
            // total 传 usize::MAX：Range 已经过 `parse_range` 校验，
            // Current 只要 ≥1 就成立（真正的夹取在 `resolve`）。
            let nums = p.numbers(usize::MAX);
            if nums.is_empty() {
                String::new()
            } else {
                format!("{} {}", PAGE_LABEL, group_ranges(&nums))
            }
        }
    }
}

/// `[1,2,3,5]` → `1-3, 5`
pub fn group_ranges(nums: &[usize]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < nums.len() {
        let start = nums[i];
        let mut end = start;
        while i + 1 < nums.len() && nums[i + 1] == end + 1 {
            i += 1;
            end = nums[i];
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        if end == start {
            out.push_str(&start.to_string());
        } else if end == start + 1 {
            out.push_str(&format!("{start}, {end}"));
        } else {
            out.push_str(&format!("{start}-{end}"));
        }
        i += 1;
    }
    out
}

/// 行列表里的三种页模式：0 = 当前页，1 = 全部，2 = 区间。
pub fn pick(mode: usize, cur: usize, range: &str, total: usize) -> PagePick {
    match mode {
        0 => PagePick::Current(cur),
        1 => PagePick::All,
        // 区间解析不出来就给空选择：文案显示原文、`Send` 灰着，绝不悄悄发全部
        _ => parse_range(range, total)
            .map(PagePick::Range)
            .unwrap_or(PagePick::Range(Vec::new())),
    }
}

/// 页码区间：`1-3,5` / `2; 4` / 单个 `7`。返回 1 起、升序、去重的页号。
pub fn parse_range(s: &str, total: usize) -> Result<Vec<usize>, String> {
    let mut out: Vec<usize> = Vec::new();
    for tok in s.split([',', ';', ' ', '\t', '\n', '\r']) {
        let tok = tok.trim();
        if tok.is_empty() {
            continue;
        }
        let (a, b) = match tok.split_once('-') {
            Some((a, b)) => (a.trim(), Some(b.trim())),
            None => (tok, None),
        };
        let a: usize = a.parse().map_err(|_| format!("bad page: {tok}"))?;
        if a == 0 {
            return Err(format!("page 0: {tok}"));
        }
        if a > total {
            return Err(format!("page {a} > {total}"));
        }
        match b {
            None => out.push(a),
            Some(b) => {
                let b: usize = b.parse().map_err(|_| format!("bad page: {tok}"))?;
                if b < a {
                    return Err(format!("reversed range: {tok}"));
                }
                if b > total {
                    return Err(format!("page {b} > {total}"));
                }
                for n in a..=b {
                    out.push(n);
                }
            }
        }
    }
    if out.is_empty() {
        return Err("empty range".into());
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

/// 收件人：逗号 / 分号 / 空白分隔，简易 RFC5322 校验，去重（忽略大小写）。
pub fn validate_to(s: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    for tok in s.split([',', ';', ' ', '\t', '\n', '\r']) {
        let t = tok.trim();
        if t.is_empty() {
            continue;
        }
        if !is_email(t) {
            return Err(format!("bad address: {t}"));
        }
        let lower = t.to_ascii_lowercase();
        if !out.iter().any(|e| e.to_ascii_lowercase() == lower) {
            out.push(t.to_string());
        }
    }
    if out.is_empty() {
        return Err("no address".into());
    }
    Ok(out)
}

fn is_email(s: &str) -> bool {
    if s.len() > 254 {
        return false;
    }
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    if local.is_empty() || domain.is_empty() || domain.contains('@') {
        return false;
    }
    if local.starts_with('.') || local.ends_with('.') || local.contains("..") {
        return false;
    }
    if !local
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "._%+-".contains(c))
    {
        return false;
    }
    let labels: Vec<&str> = domain.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    let ok_label = |l: &str| {
        !l.is_empty()
            && !l.starts_with('-')
            && !l.ends_with('-')
            && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
    };
    if !labels.iter().all(|l| ok_label(l)) {
        return false;
    }
    // 顶级域：≥2 个字母
    labels
        .last()
        .map(|t| t.len() >= 2 && t.chars().all(|c| c.is_ascii_alphabetic()))
        .unwrap_or(false)
}

/// 要发的东西。`pages` 里的页号 1 起；`filename` 是 `File name` 行的原文。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct MailSpec {
    pub to: Vec<String>,
    pub pages: PagePick,
    pub format: MailFormat,
    pub subject: String,
    pub filename: String,
}

impl MailSpec {
    /// 附件主名：`File name` 留空就用文档名。
    fn base<'a>(&'a self, name: &'a str) -> &'a str {
        let f = self.filename.trim();
        if f.is_empty() {
            name
        } else {
            f
        }
    }
}

/// `Send` 能不能点：有合法收件人、选得出页、格式有渲染器、服务器配了、没在忙。
pub fn can_send(to: &str, pages: &PagePick, fmt: MailFormat, configured: bool, busy: bool) -> bool {
    validate_to(to).is_ok()
        && !pages_label(pages).is_empty()
        && fmt.available()
        && configured
        && !busy
}

/// 去掉结尾的附件扩展名（`.PDF` 也算）。
pub fn strip_ext(s: &str) -> &str {
    let low = s.to_ascii_lowercase();
    for f in FORMATS {
        let e = format!(".{}", f.ext());
        if low.ends_with(&e) {
            return &s[..s.len() - e.len()];
        }
    }
    s
}

/// 补上格式对应的扩展名（已经有了就不重复加）。
pub fn with_ext(s: &str, fmt: MailFormat) -> String {
    let low = s.to_ascii_lowercase();
    if low.ends_with(&format!(".{}", fmt.ext())) {
        s.to_string()
    } else {
        format!("{s}.{}", fmt.ext())
    }
}

/// 附件名：`File name` 行原文（空则文档名）+ 扩展名。
pub fn file_name(spec: &MailSpec, name: &str) -> String {
    with_ext(spec.base(name), spec.format)
}

/// PNG 多页的第 `page` 页附件名：`{主名}-{page}.png`。
pub fn page_file_name(spec: &MailSpec, name: &str, page: usize) -> String {
    if spec.format == MailFormat::Png {
        format!("{}-{page}.png", strip_ext(spec.base(name)))
    } else {
        file_name(spec, name)
    }
}

/// 邮件标题：`Subject` 留空就用文档名。
pub fn subject(spec: &MailSpec, name: &str) -> String {
    let s = spec.subject.trim();
    if s.is_empty() {
        name.to_string()
    } else {
        s.to_string()
    }
}

/// 邮件正文（`POST /api/v1/mail` 的 `body` 字段）。附件 blob 得先上传
/// 才能拼整包 JSON，所以整包留在 `net::send_mail_multi` 里，这里只产正文。
pub fn body(spec: &MailSpec, name: &str) -> String {
    let pages = pages_label(&spec.pages);
    let pages = if pages.is_empty() { ALL_PAGES } else { pages.as_str() };
    format!("{name}\n{pages}\n{}\n{SENT_FROM}", file_name(spec, name))
}

/// 两帧灰度合成：逐像素取更黑的那个。底图（PDF/电子书页）和墨迹层都是
/// 白底黑线，取 min 就是"把笔画叠在底图上"。
pub fn compose(base: &[u8], ink: &[u8]) -> Vec<u8> {
    base.iter().zip(ink.iter()).map(|(a, b)| (*a).min(*b)).collect()
}

/// 一页渲染好的整页灰度图（255 白 / 0 黑）。`number` 是 1 起的页码。
pub struct PageImage {
    pub number: usize,
    pub gray: Vec<u8>,
    pub w: usize,
    pub h: usize,
}

/// 要上传的附件
pub struct Attachment {
    pub filename: String,
    pub mime: &'static str,
    pub data: Vec<u8>,
}

/// 渲染结果 → 附件：PDF 合成一个多页文件；PNG 每页一张（≤5）；SVG 没有实现。
pub fn attachments(spec: &MailSpec, name: &str, pages: &[PageImage]) -> Vec<Attachment> {
    match spec.format {
        MailFormat::Pdf => {
            let pages: Vec<(Vec<u8>, usize, usize)> = pages
                .iter()
                .map(|p| (p.gray.clone(), p.w, p.h))
                .collect();
            let pdf = pdf_gray(&pages);
            if pdf.is_empty() {
                return Vec::new();
            }
            vec![Attachment {
                filename: file_name(spec, name),
                mime: MailFormat::Pdf.mime(),
                data: pdf,
            }]
        }
        MailFormat::Png => pages
            .iter()
            .take(MAX_PNG_ATTACHMENTS)
            .map(|p| {
                let png = png_gray(&p.gray, p.w, p.h);
                Attachment {
                    filename: page_file_name(spec, name, p.number),
                    mime: MailFormat::Png.mime(),
                    data: png,
                }
            })
            .filter(|a| !a.data.is_empty())
            .collect(),
        MailFormat::Svg => Vec::new(),
    }
}

// ---------------------------------------------------------------- 编码器
//
// 仓库零新增依赖：PNG 的 deflate 用已有的 `miniz_oxide`（zlib 流），
// PDF 的图片流是同一份 zlib 数据（`/FlateDecode`）。

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
        }
    }
    !c
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut body = kind.to_vec();
    body.extend_from_slice(data);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_be_bytes());
}

/// 灰度缓冲 → 8 位灰度 PNG（无隔行、每行 filter=0）。尺寸对不上就返回空。
pub fn png_gray(gray: &[u8], w: usize, h: usize) -> Vec<u8> {
    png_encode(gray, w, h, 6)
}

/// 灰度缓冲 → PNG（每行 filter 恒为 0，zlib 等级由调用方给）。
///
/// 不做行滤波是量出来的：整页大面积白，filter=None 让 deflate 直接吃到长串 0。
/// Mac 上 1404×1872 密排页：None+l6 = 78 ms / 420 KB，Up+l6 = 80 ms / 528 KB，
/// Sub+l6 = 71 ms / 497 KB —— Sub 略快但大 18%，整体不划算。
fn png_encode(gray: &[u8], w: usize, h: usize, level: u8) -> Vec<u8> {
    if w == 0 || h == 0 || gray.len() < w * h {
        return Vec::new();
    }
    let mut raw = Vec::with_capacity((w + 1) * h);
    for y in 0..h {
        raw.push(0);
        raw.extend_from_slice(&gray[y * w..(y + 1) * w]);
    }
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]); // 8 bit / 灰度 / deflate / 无过滤 / 无隔行
    png_chunk(&mut out, b"IHDR", &ihdr);
    png_chunk(
        &mut out,
        b"IDAT",
        &miniz_oxide::deflate::compress_to_vec_zlib(&raw, level),
    );
    png_chunk(&mut out, b"IEND", &[]);
    out
}

// ---------------------------------------------------------------- 屏幕共享预览

/// 屏幕共享（`cloud.rs::cmd_render`）的降采样倍率：1404×1872 → 702×936。
///
/// 为什么停在 2×：32 px 手写字的横切笔画中位 3 px（`note/page1.rm` 实测 5336 段），
/// 2× 均值之后笔画核心仍有 **74%** 落在深灰（中位灰度 63/255），
/// 3× 只剩 **54%**、中位 113（发灰）——再小就开始糊字了。
/// 3× 也量过：编码 10.5 ms / 74 KB，比 2× 省得有限，不值得拿可读性换。
pub const PREVIEW_DIV: usize = 2;

/// 预览 PNG 的 zlib 等级。实测 4 比 6 快 2.7×、字节只多 1.4%，是曲线的拐点。
const PREVIEW_LEVEL: u8 = 4;

/// 灰度页 → `k×k` 方块均值降采样，返回 `(像素, 宽, 高)`。
///
/// 面积平均（不是最近邻）：笔画边上是抗锯齿灰，最近邻会整条丢掉细线，
/// 均值能把 1 px 细线摊成看得见的浅灰。右边/下边不足 `k` 的列行直接丢掉，
/// 返回的尺寸是 `w / k × h / k`。尺寸或倍率不合法返回空。
pub fn downscale(gray: &[u8], w: usize, h: usize, k: usize) -> (Vec<u8>, usize, usize) {
    if k == 0 || w < k || h < k || gray.len() < w * h {
        return (Vec::new(), 0, 0);
    }
    let (dw, dh) = (w / k, h / k);
    let mut out = vec![0u8; dw * dh];
    let area = (k * k) as u32;
    for y in 0..dh {
        for x in 0..dw {
            let mut sum = 0u32;
            for yy in 0..k {
                let row = (y * k + yy) * w + x * k;
                for xx in 0..k {
                    sum += gray[row + xx] as u32;
                }
            }
            out[y * dw + x] = (sum / area) as u8;
        }
    }
    (out, dw, dh)
}

/// 屏幕共享预览用的 PNG：**入参是 [`downscale`] 的输出**，不是整页灰度。
///
/// 和 [`png_gray`] 只差 zlib 等级（4 而不是 6）。尺寸给错就返回空。
pub fn png_preview(small: &[u8], w: usize, h: usize) -> Vec<u8> {
    png_encode(small, w, h, PREVIEW_LEVEL)
}

/// A4 竖版（pt）
const PDF_PAGE_W: f32 = 595.0;
const PDF_PAGE_H: f32 = 842.0;

fn pdf_obj(out: &mut Vec<u8>, offs: &mut Vec<usize>, num: usize) {
    while offs.len() < num {
        offs.push(0);
    }
    offs[num - 1] = out.len();
    out.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
}

/// 每页一张灰度图 → 最小多页 PDF（图片 XObject + FlateDecode）。
/// 页数 0、或某页尺寸对不上，返回空。
pub fn pdf_gray(pages: &[(Vec<u8>, usize, usize)]) -> Vec<u8> {
    if pages.is_empty() {
        return Vec::new();
    }
    for (gray, w, h) in pages {
        if *w == 0 || *h == 0 || gray.len() < w * h {
            return Vec::new();
        }
    }
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");
    let mut offs: Vec<usize> = Vec::new();

    // 1 = Catalog，2 = Pages，之后每页 3 个对象（Page / Contents / Image）
    pdf_obj(&mut out, &mut offs, 1);
    out.extend_from_slice(b"<< /Type /Catalog /Pages 2 0 R >>\nendobj\n");

    let kids: String = (0..pages.len())
        .map(|i| format!("{} 0 R ", 3 + i * 3))
        .collect();
    pdf_obj(&mut out, &mut offs, 2);
    out.extend_from_slice(
        format!(
            "<< /Type /Pages /Count {} /Kids [ {kids}] >>\nendobj\n",
            pages.len()
        )
        .as_bytes(),
    );

    for (i, (gray, w, h)) in pages.iter().enumerate() {
        let pnum = 3 + i * 3;
        let cnum = pnum + 1;
        let inum = pnum + 2;
        // 等比缩放居中（页面 1404×1872 不是 A4 比例，别拉变形）
        let (iw, ih) = (*w as f32, *h as f32);
        let scale = (PDF_PAGE_W / iw).min(PDF_PAGE_H / ih);
        let (dw, dh) = (iw * scale, ih * scale);
        let (dx, dy) = ((PDF_PAGE_W - dw) / 2.0, (PDF_PAGE_H - dh) / 2.0);

        pdf_obj(&mut out, &mut offs, pnum);
        out.extend_from_slice(
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {PDF_PAGE_W} {PDF_PAGE_H}] \
                 /Resources << /XObject << /Im0 {inum} 0 R >> >> /Contents {cnum} 0 R >>\nendobj\n"
            )
            .as_bytes(),
        );

        let content = format!("q {dw:.2} 0 0 {dh:.2} {dx:.2} {dy:.2} cm /Im0 Do Q\n");
        pdf_obj(&mut out, &mut offs, cnum);
        out.extend_from_slice(
            format!(
                "<< /Length {} >>\nstream\n{content}\nendstream\nendobj\n",
                content.len()
            )
            .as_bytes(),
        );

        let z = miniz_oxide::deflate::compress_to_vec_zlib(gray, 6);
        pdf_obj(&mut out, &mut offs, inum);
        out.extend_from_slice(
            format!(
                "<< /Type /XObject /Subtype /Image /Width {w} /Height {h} \
                 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode \
                 /Length {} >>\nstream\n",
                z.len()
            )
            .as_bytes(),
        );
        out.extend_from_slice(&z);
        out.extend_from_slice(b"\nendstream\nendobj\n");
    }

    // 交叉引用表
    let xref = out.len();
    let count = pages.len() * 3 + 3;
    out.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
    for i in 0..count - 1 {
        out.extend_from_slice(format!("{:010} 00000 n \n", offs[i]).as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {count} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n").as_bytes(),
    );
    out
}

// ============================================================ P2 end ===

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------- 收件人

    #[test]
    fn validate_to_accepts_one_address() {
        assert_eq!(validate_to("me@example.com").unwrap(), vec!["me@example.com"]);
        // 首尾空白吃掉
        assert_eq!(validate_to("  me@example.com  ").unwrap(), vec!["me@example.com"]);
        // 加号与点分本地名都合法
        assert_eq!(
            validate_to("first.last+tag@mail.example.co.uk").unwrap(),
            vec!["first.last+tag@mail.example.co.uk"]
        );
    }

    #[test]
    fn validate_to_splits_on_comma_semicolon_space() {
        assert_eq!(
            validate_to("a@b.com, c@d.org; e@f.net").unwrap(),
            vec!["a@b.com", "c@d.org", "e@f.net"]
        );
        assert_eq!(validate_to("a@b.com c@d.org").unwrap().len(), 2);
    }

    #[test]
    fn validate_to_dedupes_ignoring_case() {
        assert_eq!(
            validate_to("A@B.com,a@b.com , a@b.com").unwrap(),
            vec!["A@B.com"]
        );
    }

    #[test]
    fn validate_to_rejects_empty() {
        assert!(validate_to("").is_err());
        assert!(validate_to("   ").is_err());
        assert!(validate_to(", ; ").is_err());
    }

    #[test]
    fn validate_to_rejects_garbage() {
        for bad in [
            "not-an-address",
            "a@",
            "@b.com",
            "a@b",           // 顶级域没有点
            "a@b.c",         // 顶级域只有 1 个字母
            "a b@c.com",     // 空格已经切开 → "a" 不合
            "a@@b.com",
            "a@-b.com",
            "a@b..com",
            ".a@b.com",
            "a.@b.com",
        ] {
            assert!(validate_to(bad).is_err(), "{bad} 应该被拒");
        }
    }

    #[test]
    fn validate_to_one_bad_address_kills_the_batch() {
        assert!(validate_to("good@example.com, oops").is_err());
    }

    // ---------------------------------------------------------- 页码区间

    #[test]
    fn parse_range_basic() {
        assert_eq!(parse_range("1-3,5", 10).unwrap(), vec![1, 2, 3, 5]);
        assert_eq!(parse_range(" 2 ; 4 ", 10).unwrap(), vec![2, 4]);
        assert_eq!(parse_range("7", 10).unwrap(), vec![7]);
        assert_eq!(parse_range("3-3", 10).unwrap(), vec![3]);
    }

    #[test]
    fn parse_range_sorts_and_dedupes() {
        assert_eq!(parse_range("5,1-2,2", 10).unwrap(), vec![1, 2, 5]);
    }

    #[test]
    fn parse_range_rejects_out_of_bounds_and_junk() {
        assert!(parse_range("0", 10).is_err());
        assert!(parse_range("11", 10).is_err());
        assert!(parse_range("8-12", 10).is_err());
        assert!(parse_range("5-3", 10).is_err());
        assert!(parse_range("", 10).is_err());
        assert!(parse_range("   ", 10).is_err());
        assert!(parse_range("abc", 10).is_err());
        assert!(parse_range("1-", 10).is_err());
        assert!(parse_range("1-2x", 10).is_err());
        // 总页数 0 时什么都选不出来
        assert!(parse_range("1", 0).is_err());
    }

    // ---------------------------------------------------------- 页选择

    #[test]
    fn page_pick_resolve_dedupes_and_sorts() {
        let p = PagePick::Range(vec![5, 1, 3, 3]);
        assert_eq!(p.numbers(10), vec![1, 3, 5]);
        assert_eq!(p.resolve(10), vec![0, 2, 4], "resolve 是 0 起");
        // 越界的页丢掉，不是夹到边界
        assert_eq!(PagePick::Range(vec![0, 2, 99]).numbers(10), vec![2]);
        assert_eq!(PagePick::Current(3).resolve(10), vec![2]);
        assert!(PagePick::Current(0).numbers(10).is_empty());
        assert!(PagePick::Current(11).numbers(10).is_empty());
        assert_eq!(PagePick::All.numbers(3), vec![1, 2, 3]);
        assert!(PagePick::All.numbers(0).is_empty());
    }

    #[test]
    fn pages_label_follows_official_text() {
        assert_eq!(pages_label(&PagePick::All), "All pages");
        assert_eq!(pages_label(&PagePick::Current(3)), "Page 3");
        assert_eq!(pages_label(&PagePick::Range(vec![1, 2, 3, 5])), "Page 1-3, 5");
        // 选不出页 → 空串（界面显示原始输入，Send 灰着）
        assert_eq!(pages_label(&PagePick::Range(Vec::new())), "");
        assert_eq!(pages_label(&PagePick::Current(0)), "");
    }

    #[test]
    fn group_ranges_collapses_runs() {
        assert_eq!(group_ranges(&[1, 2, 3, 5]), "1-3, 5");
        assert_eq!(group_ranges(&[1, 2]), "1, 2");
        assert_eq!(group_ranges(&[4]), "4");
        assert_eq!(group_ranges(&[]), "");
        assert_eq!(group_ranges(&[1, 2, 3, 7, 8, 12]), "1-3, 7, 8, 12");
    }

    #[test]
    fn pick_maps_the_three_modes() {
        assert_eq!(pick(0, 2, "1-3", 10), PagePick::Current(2));
        assert_eq!(pick(1, 2, "1-3", 10), PagePick::All);
        assert_eq!(pick(2, 2, "1-3", 10), PagePick::Range(vec![1, 2, 3]));
        // 区间写错 → 空选择（不会退化成"全部"）
        assert!(pick(2, 2, "oops", 10).numbers(10).is_empty());
        assert!(pick(2, 2, "99", 10).numbers(10).is_empty());
    }

    // ---------------------------------------------------------- 文案 / 开关

    #[test]
    fn can_send_needs_everything() {
        let ok = PagePick::All;
        assert!(can_send("a@b.com", &ok, MailFormat::Pdf, true, false));
        assert!(!can_send("", &ok, MailFormat::Pdf, true, false));
        assert!(!can_send("a@b.com", &ok, MailFormat::Pdf, false, false), "服务器没配");
        assert!(!can_send("a@b.com", &ok, MailFormat::Pdf, true, true), "忙");
        assert!(!can_send("a@b.com", &PagePick::Range(vec![]), MailFormat::Pdf, true, false));
        assert!(!can_send("a@b.com", &ok, MailFormat::Svg, true, false), "SVG 没有渲染器");
    }

    #[test]
    fn format_capabilities_and_cycle() {
        assert_eq!(MailFormat::Pdf.label(), "PDF");
        assert_eq!(MailFormat::Png.mime(), "image/png");
        assert_eq!(MailFormat::Svg.label(), "SVG");
        assert!(MailFormat::Pdf.available() && MailFormat::Png.available());
        assert!(!MailFormat::Svg.available(), "官方按能力显隐，我们没 SVG 渲染器");
        // 循环跳过不可用的
        assert_eq!(MailFormat::Pdf.cycle(), MailFormat::Png);
        assert_eq!(MailFormat::Png.cycle(), MailFormat::Pdf);
        assert_ne!(MailFormat::Png.cycle(), MailFormat::Svg);
        assert_eq!(FORMATS.map(|f| f.label()), ["PDF", "PNG", "SVG"]);
    }

    #[test]
    fn file_names_follow_the_format() {
        let spec = MailSpec {
            to: vec!["a@b.com".into()],
            pages: PagePick::All,
            format: MailFormat::Pdf,
            subject: String::new(),
            filename: "Notes".into(),
        };
        assert_eq!(file_name(&spec, "Doc"), "Notes.pdf");
        assert_eq!(page_file_name(&spec, "Doc", 1), "Notes.pdf");
        let mut png = spec.clone();
        png.format = MailFormat::Png;
        assert_eq!(file_name(&png, "Doc"), "Notes.png");
        assert_eq!(page_file_name(&png, "Doc", 3), "Notes-3.png");
        // 扩展名跟着格式走：`.PDF` 不是 `.png`，所以补 `.png`；同格式不重复加
        let mut with = png.clone();
        with.filename = "Notes.PDF".into();
        assert_eq!(file_name(&with, "Doc"), "Notes.PDF.png");
        let mut same = png.clone();
        same.filename = "Notes.PNG".into();
        assert_eq!(file_name(&same, "Doc"), "Notes.PNG");
        // 留空用文档名
        let mut empty = spec.clone();
        empty.filename = "  ".into();
        assert_eq!(file_name(&empty, "Quick sheets"), "Quick sheets.pdf");
        assert_eq!(strip_ext("a.png"), "a");
        assert_eq!(strip_ext("a.PDF"), "a");
        assert_eq!(strip_ext("a"), "a");
    }

    #[test]
    fn subject_and_body_carry_the_document_fields() {
        let spec = MailSpec {
            to: vec!["a@b.com".into()],
            pages: PagePick::Range(vec![1, 2, 3, 5]),
            format: MailFormat::Pdf,
            subject: "   ".into(),
            filename: "Notes".into(),
        };
        // 标题留空 → 文档名
        assert_eq!(subject(&spec, "Meeting"), "Meeting");
        let mut named = spec.clone();
        named.subject = "  Proposal  ".into();
        assert_eq!(subject(&named, "Meeting"), "Proposal");
        // 正文：文档名 / 页 / 附件名 / 尾注
        let b = body(&spec, "Meeting");
        let lines: Vec<&str> = b.lines().collect();
        assert_eq!(lines[0], "Meeting");
        assert_eq!(lines[1], "Page 1-3, 5");
        assert_eq!(lines[2], "Notes.pdf");
        assert_eq!(lines[3], "Sent from reMarkable 2");
        // 全部页
        let mut all = spec.clone();
        all.pages = PagePick::All;
        assert_eq!(body(&all, "Meeting").lines().nth(1), Some("All pages"));
    }

    // ---------------------------------------------------------- 布局 / 命中

    #[test]
    fn layout_rows_do_not_overlap() {
        let rows = layout(1404, 1872, false);
        // 6 行 + 底部按钮 + 黑头两个
        assert_eq!(rows.len(), 2 + 3 + 2 + 1 + 2);
        for (act, r) in &rows {
            assert!(r.2 > 0 && r.3 >= 80, "{act:?} 命中区 {r:?} 太扁");
            assert!(r.0 >= 0 && r.0 + r.2 <= 1404, "{act:?} 出屏 {r:?}");
            assert!(r.1 >= 0 && r.1 + r.3 <= 1872, "{act:?} 出屏 {r:?}");
        }
        // 四个文本行 + 三个单选 + 底部按钮互不重叠（黑头两条另算）
        let mut body: Vec<(Act, (i32, i32, i32, i32))> = rows
            .iter()
            .filter(|(a, r)| !matches!(a, Act::Cancel) && !(*a == Act::Send && r.3 == HDR_H))
            .copied()
            .collect();
        body.sort_by_key(|(_, r)| r.1);
        for pair in body.windows(2) {
            let (a, r) = pair[0];
            let (b, s) = pair[1];
            assert!(r.1 + r.3 <= s.1, "{a:?} {r:?} 和 {b:?} {s:?} 叠在一起");
        }
    }

    #[test]
    fn hit_finds_every_row() {
        let (w, h) = (1404, 1872);
        assert_eq!(hit(100.0, 60.0, w, h, false), Some(Act::Cancel));
        assert_eq!(hit(1350.0, 60.0, w, h, false), Some(Act::Send));
        assert_eq!(hit(300.0, (TO_Y + 10) as f32, w, h, false), Some(Act::To));
        assert_eq!(
            hit(300.0, (PAGES_Y + 10) as f32, w, h, false),
            Some(Act::Pages)
        );
        assert_eq!(
            hit(300.0, radio_cy(0) as f32, w, h, false),
            Some(Act::Format(MailFormat::Pdf))
        );
        assert_eq!(
            hit(300.0, radio_cy(1) as f32, w, h, false),
            Some(Act::Format(MailFormat::Png))
        );
        assert_eq!(
            hit(300.0, (SUBJECT_Y + 10) as f32, w, h, false),
            Some(Act::Subject)
        );
        assert_eq!(
            hit(300.0, (FILE_Y + 10) as f32, w, h, false),
            Some(Act::Filename)
        );
        // 底部主按钮
        assert_eq!(
            hit(700.0, (h - SEND_BTN_BOTTOM + 20) as f32, w, h, false),
            Some(Act::Send)
        );
        // 空白处没有动作
        assert_eq!(hit(700.0, 1050.0, w, h, false), None);
    }

    #[test]
    fn svg_is_not_clickable() {
        let (w, h) = (1404, 1872);
        let cy = radio_cy(2);
        assert_eq!(hit(300.0, cy as f32, w, h, false), None, "SVG 灰显不可点");
        // 但它的位置还是要画出来（layout 里有）
        assert!(layout(w, h, false)
            .iter()
            .any(|(a, _)| *a == Act::Format(MailFormat::Svg)));
    }

    #[test]
    fn keyboard_hides_bottom_button() {
        let (w, h) = (1404, 1872);
        let open = layout(w, h, true);
        assert!(!open
            .iter()
            .any(|(a, r)| *a == Act::Send && r.3 == 90));
        // 键盘开着时 y = 1872-180 落在键盘上，不能再命中底部按钮
        assert_ne!(hit(700.0, (h - SEND_BTN_BOTTOM + 20) as f32, w, h, true), Some(Act::Send));
        // 黑头的 Send 还在
        assert_eq!(hit(1350.0, 60.0, w, h, true), Some(Act::Send));
    }

    // ---------------------------------------------------------- 编码器

    fn gray_page(w: usize, h: usize, seed: u8) -> Vec<u8> {
        (0..w * h)
            .map(|i| ((i / 7 + seed as usize) % 256) as u8)
            .collect()
    }

    /// 走一遍 PNG 的块，返回 IHDR 的 (w, h) 和 IDAT 的原始数据。
    fn png_chunks(png: &[u8]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let mut kinds = Vec::new();
        let mut ihdr = Vec::new();
        let mut idat = Vec::new();
        let mut i = 8;
        while i + 8 <= png.len() {
            let len = u32::from_be_bytes([png[i], png[i + 1], png[i + 2], png[i + 3]]) as usize;
            let kind = &png[i + 4..i + 8];
            let data = &png[i + 8..i + 8 + len];
            kinds.extend_from_slice(kind);
            match kind {
                b"IHDR" => ihdr.extend_from_slice(data),
                b"IDAT" => idat.extend_from_slice(data),
                _ => {}
            }
            i += 12 + len;
        }
        (kinds, ihdr, idat)
    }

    #[test]
    fn png_gray_round_trips() {
        let (w, h) = (7, 5);
        let gray = gray_page(w, h, 3);
        let png = png_gray(&gray, w, h);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        assert!(png.windows(4).any(|s| s == b"IEND"), "要有 IEND");
        let (kinds, ihdr, idat) = png_chunks(&png);
        assert!(kinds.windows(4).any(|s| s == b"IHDR"));
        let pw = u32::from_be_bytes([ihdr[0], ihdr[1], ihdr[2], ihdr[3]]);
        let ph = u32::from_be_bytes([ihdr[4], ihdr[5], ihdr[6], ihdr[7]]);
        assert_eq!((pw as usize, ph as usize), (w, h));
        assert_eq!((ihdr[8], ihdr[9]), (8, 0), "8 bit / 灰度");
        // 解回来：每行开头一个 filter 字节 0，后面就是原图
        let raw = miniz_oxide::inflate::decompress_to_vec_zlib(&idat).expect("zlib");
        assert_eq!(raw.len(), (w + 1) * h);
        for y in 0..h {
            assert_eq!(raw[y * (w + 1)], 0, "第 {y} 行 filter");
            assert_eq!(&raw[y * (w + 1) + 1..(y + 1) * (w + 1)], &gray[y * w..(y + 1) * w]);
        }
    }

    #[test]
    fn compose_takes_the_darker_pixel() {
        // 底图有字的地方是黑的，墨迹是白 → 取底图；反过来也一样
        let base = vec![255, 0, 255, 128];
        let ink = vec![255, 255, 0, 200];
        assert_eq!(compose(&base, &ink), vec![255, 0, 0, 128]);
        // 长度不一致就按短的来（调用方已经校验过等长）
        assert_eq!(compose(&[0, 255], &[255]), vec![0]);
    }

    #[test]
    fn png_gray_rejects_bad_size() {
        assert!(png_gray(&[], 4, 4).is_empty());
        assert!(png_gray(&[0; 10], 4, 4).is_empty());
        assert!(png_gray(&[0; 16], 0, 0).is_empty());
    }

    // ---------------------------------------------------------- 预览（屏幕共享）

    /// 2×2 只取均值；尺寸不是整数倍时向下取整；倍率/尺寸非法返回空。
    #[test]
    fn downscale_averages_blocks() {
        // 4×4：左上 2×2 全白（255），其余全黑 → 2×2 里只有 (0,0) 是白的
        let mut g = vec![0u8; 16];
        for i in [0, 1, 4, 5] {
            g[i] = 255;
        }
        let (d, dw, dh) = downscale(&g, 4, 4, 2);
        assert_eq!((dw, dh), (2, 2));
        assert_eq!(d, vec![255, 0, 0, 0]);
        // 一条 1 px 细线（255）摊到 2×2 里 → 63，不会整条消失
        let mut g = vec![0u8; 16];
        g[0] = 255;
        assert_eq!(downscale(&g, 4, 4, 2).0[0], 63);
        // 5×3 按 2 取 → 2×1（右边一列、下面一行丢掉）
        let (d, dw, dh) = downscale(&[9u8; 15], 5, 3, 2);
        assert_eq!((dw, dh), (2, 1));
        assert_eq!(d, vec![9, 9]);
        // 非法
        assert!(downscale(&[], 4, 4, 2).0.is_empty());
        assert!(downscale(&[0; 16], 4, 4, 0).0.is_empty());
        assert!(downscale(&[0; 16], 1, 16, 2).0.is_empty(), "宽比倍率还小");
    }

    /// 预览图 = 降采样尺寸的合法灰度 PNG，解回来和 `downscale` 逐字节相等。
    #[test]
    fn png_preview_is_downscaled_and_round_trips() {
        let (w, h) = (8, 6);
        let gray = gray_page(w, h, 5);
        let (small, dw, dh) = downscale(&gray, w, h, PREVIEW_DIV);
        assert_eq!((dw, dh), (4, 3));
        let png = png_preview(&small, dw, dh);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
        let (_, ihdr, idat) = png_chunks(&png);
        let pw = u32::from_be_bytes([ihdr[0], ihdr[1], ihdr[2], ihdr[3]]) as usize;
        let ph = u32::from_be_bytes([ihdr[4], ihdr[5], ihdr[6], ihdr[7]]) as usize;
        assert_eq!((pw, ph), (dw, dh), "预览图得是降采样后的尺寸");
        assert_eq!((ihdr[8], ihdr[9]), (8, 0), "8 bit / 灰度");
        let raw = miniz_oxide::inflate::decompress_to_vec_zlib(&idat).expect("zlib");
        assert_eq!(raw.len(), (dw + 1) * dh);
        for y in 0..dh {
            assert_eq!(raw[y * (dw + 1)], 0, "第 {y} 行 filter");
            assert_eq!(&raw[y * (dw + 1) + 1..(y + 1) * (dw + 1)], &small[y * dw..(y + 1) * dw]);
        }
        // 尺寸给错 → 空
        assert!(png_preview(&small, 0, dh).is_empty());
        assert!(png_preview(&small[..3], dw, dh).is_empty());
    }

    /// 白页的预览必须压得足够小：512×512 缩到 256×256 后应该只有几百字节。
    /// （防回归：万一有人把整页原图喂进 `png_preview`，这里会炸。）
    #[test]
    fn blank_preview_stays_tiny() {
        let (w, h) = (512, 512);
        let white = vec![255u8; w * h];
        let (small, dw, dh) = downscale(&white, w, h, PREVIEW_DIV);
        let png = png_preview(&small, dw, dh);
        assert!(png.len() < 2048, "白页预览 {} B，太大了", png.len());
        // 同一个白页用全分辨率编，字节数至少是它的 3 倍（尺寸就大 4 倍）
        assert!(png_gray(&white, w, h).len() > png.len());
    }

    #[test]
    fn pdf_gray_has_pages_and_valid_xref() {
        let a = gray_page(8, 6, 1);
        let b = gray_page(8, 6, 9);
        let pdf = pdf_gray(&[(a, 8, 6), (b, 8, 6)]);
        assert_eq!(&pdf[..8], b"%PDF-1.4");
        assert!(pdf.ends_with(b"%%EOF\n"));
        let text = String::from_utf8_lossy(&pdf).into_owned();
        assert!(text.contains("/Count 2"), "两页");
        assert!(text.contains("/Filter /FlateDecode"));
        assert!(text.contains("/ColorSpace /DeviceGray"));
        // startxref 指向 "xref" 的位置，且每个偏移正好落在 "N 0 obj" 上
        let pos = text.find("startxref").expect("startxref");
        let num: usize = text[pos + 9..]
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(&pdf[num..num + 4], b"xref");
        let xref = text.find("xref\n0 ").expect("xref 段");
        let mut lines = text[xref..].lines();
        assert_eq!(lines.next(), Some("xref"));
        assert_eq!(lines.next(), Some("0 9"), "条目 = 空闲项 + 8 个对象");
        assert_eq!(lines.next(), Some("0000000000 65535 f "), "空闲项");
        for i in 1..=8 {
            let line = lines.next().expect("交叉引用行");
            let off: usize = line[..10].parse().unwrap();
            let want = format!("{i} 0 obj");
            assert_eq!(&pdf[off..off + want.len()], want.as_bytes(), "第 {i} 个对象的偏移不对");
        }
    }

    #[test]
    fn pdf_gray_rejects_empty_or_bad_size() {
        assert!(pdf_gray(&[]).is_empty());
        assert!(pdf_gray(&[(vec![0; 4], 8, 6)]).is_empty());
    }

    #[test]
    fn attachments_switch_on_format() {
        let spec = MailSpec {
            to: vec!["a@b.com".into()],
            pages: PagePick::All,
            format: MailFormat::Pdf,
            subject: "S".into(),
            filename: "Notes".into(),
        };
        let pages: Vec<PageImage> = (1..=2)
            .map(|n| PageImage {
                number: n,
                gray: gray_page(4, 3, n as u8),
                w: 4,
                h: 3,
            })
            .collect();
        // PDF：一个多页附件
        let atts = attachments(&spec, "Doc", &pages);
        assert_eq!(atts.len(), 1);
        assert_eq!(atts[0].filename, "Notes.pdf");
        assert_eq!(atts[0].mime, "application/pdf");
        assert_eq!(&atts[0].data[..4], b"%PDF");
        // PNG：每页一张
        let mut png = spec.clone();
        png.format = MailFormat::Png;
        let atts = attachments(&png, "Doc", &pages);
        assert_eq!(atts.len(), 2);
        assert_eq!(atts[0].filename, "Notes-1.png");
        assert_eq!(atts[1].filename, "Notes-2.png");
        assert_eq!(atts[0].mime, "image/png");
        assert_eq!(&atts[0].data[..4], &[0x89, b'P', b'N', b'G']);
        // PNG 超过 5 页只发前 5 张（服务端上限）
        let many: Vec<PageImage> = (1..=7)
            .map(|n| PageImage {
                number: n,
                gray: gray_page(4, 3, n as u8),
                w: 4,
                h: 3,
            })
            .collect();
        assert_eq!(attachments(&png, "Doc", &many).len(), MAX_PNG_ATTACHMENTS);
        // SVG：没有渲染器
        let mut svg = spec.clone();
        svg.format = MailFormat::Svg;
        assert!(attachments(&svg, "Doc", &pages).is_empty());
    }

    #[test]
    fn layout_geometry_is_stable() {
        // 行/单选/段头的 y 是照官方截图量好的常量，挪动必须同步改测试
        assert_eq!(TO_Y, 170);
        assert_eq!(PAGES_Y, 290);
        assert_eq!(SEND_AS_Y, 470);
        assert!(SEND_AS_Y <= radio_cy(0) - 30, "段头不压单选");
        assert_eq!(SUBJECT_Y, 790);
        assert_eq!(FILE_Y, 910);
        assert_eq!(radio_cy(0), 540);
        assert!(radio_cy(2) + 40 <= SUBJECT_Y);
    }
}
