//! USB Web 文件管理：插上 USB 线后，浏览器打开 `http://10.11.99.1` 就能
//! 浏览 / 上传 / 下载 / 删除库里的文件。
//!
//! 官方这块在 `res_0686a15.qml` 的 `USB web interface` 里，文案是
//! `Open the address: http://10.11.99.1 in a web browser.`；这是官方唯一的
//! 「往设备塞文件」通道，我们照原样做一个极简版。
//!
//! # 为什么手写
//!
//! 和 `src/net.rs` 一样：**零依赖**，只用 `std::net` + `std::thread`，明文 HTTP/1.1，
//! 每个连接一个线程。一个浏览器点一下就是几 KB 的请求，没必要引框架。
//!
//! # 只做四面
//!
//! | 路由 | 作用 |
//! |---|---|
//! | `GET /` | 目录列表（`?dir=` 子目录） |
//! | `GET /f/<path>` | 下载 |
//! | `POST /u?dir=` | multipart 上传到当前目录（同名文件按官方语义**直接覆盖**） |
//! | `POST /d` | 删除（表单带 `confirm=1`，页面上还有一次 `confirm()`） |
//! | `GET /s.css` | 样式表 |
//!
//! 上传**原样落文件**，不生成 `.metadata`、不动 `.rm` 的字节语义——官方 Web 界面
//! 也是直接往库里丢文件，设备下次扫描（或官方 UI 打开）时自己认。
//! 和官方的差别：官方 Web 界面只收 PDF/EPUB、会顺手写 `.metadata`；我们是个
//! 文件浏览器，连 `.metadata` 本身也能被覆盖（等于手工 `scp` 的风险）。
//!
//! # 安全
//!
//! 这是全设备唯一一个「外部浏览器能写文件」的入口，路径校验按最坏情况写：
//!
//! - 路径先按段切开再校验：`..`、绝对路径、以 `.` 开头的段（`.git`、`.thumbnails`）
//!   全部 403；
//! - 每一段落盘前 `canonicalize`，结果必须仍在库根内（挡符号链接逃逸）；
//! - 请求体只认 `Content-Length` 且有界：缺长度 411，超过 [`MAX_UPLOAD`] 413；
//! - 响应头拒绝 CRLF（文件名来自客户端），文件名里的非 ASCII 走 RFC 5987。

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 官方给的地址：USB gadget 网卡（`usb0`）的固定 IP + 80 端口。
pub const DEFAULT_ADDR: &str = "10.11.99.1:80";

/// 单次请求体上限。和自建服务器的 `POST /api/v1/blob` 取同一个数，方便对照。
pub const MAX_UPLOAD: usize = 64 * 1024 * 1024;

/// 请求头（到 `\r\n\r\n` 为止）的上限，防止一个不发空行的客户端把内存吃光。
const MAX_HEAD: usize = 16 * 1024;

/// 非阻塞 `accept` 空转时的睡眠。20 ms 对「页面上点一下」完全无感。
const IDLE_POLL: Duration = Duration::from_millis(20);

// ---------------------------------------------------------------- 生命周期

/// 起服务要的两样东西：库根 + 监听地址。
#[derive(Clone, Debug)]
pub struct Config {
    pub root: PathBuf,
    pub addr: SocketAddr,
}

/// 跑着的服务。丢掉它不会停服务（线程还在），要显式 [`Handle::stop`]。
pub struct Handle {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl Handle {
    /// 实际监听的地址（`Config.addr` 的端口传 0 时，这里是内核选的那个）。
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// 优雅退出：置标志 → 连一次自己（把可能已经就绪的 `accept` 叫醒）→ join。
    ///
    /// 监听套接字是 `set_nonblocking` 的，`accept` 不会永久阻塞，所以「连一次自己」
    /// 严格来说不是必需的；但留着能把内核里已排队的那个连接收掉，退出更快。
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect_timeout(&self.addr, Duration::from_millis(200));
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// 起服务。绑不上（地址不在本机 / 端口被占）就返回 `Err`，调用方只记日志、不 panic。
pub fn start(cfg: Config) -> Result<Handle, String> {
    // 库根只解析一次：之后所有请求都拿这个**已经 canonicalize 过**的绝对路径比对。
    let root =
        fs::canonicalize(&cfg.root).map_err(|e| format!("library root {}: {e}", cfg.root.display()))?;
    let listener = TcpListener::bind(cfg.addr).map_err(|e| format!("bind {}: {e}", cfg.addr))?;
    let addr = listener
        .local_addr()
        .map_err(|e| format!("local_addr: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("set_nonblocking: {e}"))?;

    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let join = std::thread::Builder::new()
        .name("webui".into())
        .spawn(move || accept_loop(listener, root, flag))
        .map_err(|e| format!("spawn webui: {e}"))?;

    Ok(Handle {
        addr,
        stop,
        join: Some(join),
    })
}

fn accept_loop(listener: TcpListener, root: PathBuf, stop: Arc<AtomicBool>) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let root = root.clone();
                // 一个慢客户端（半截请求）不能把别人的页面堵住，所以每连接一个线程。
                std::thread::spawn(move || {
                    let _ = serve(stream, &root);
                });
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(IDLE_POLL);
            }
            // fd 用光之类的瞬时错误：别空转烧 CPU。
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

fn serve(mut s: TcpStream, root: &Path) -> std::io::Result<()> {
    // 一个浏览器连接最多占 10 s / 30 s：断线或半截请求不会攒成僵尸线程。
    let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(30)));
    let resp = match read_request(&mut s) {
        Ok(req) => route(root, &req),
        Err(code) => Response::text(code, reason(code)),
    };
    write_response(&mut s, resp)
}

// ---------------------------------------------------------------- 请求

/// 一个已经读完的请求。`path` / `query` 只在 `target` 上做**路径段**解码，
/// 表单字段等到各自解析时再解码（`+` 在路径里是加号，在表单里是空格）。
#[derive(Debug)]
struct Request {
    method: String,
    /// 解码后的路径部分（`/f/a%20b.pdf` → `/f/a b.pdf`）
    path: String,
    /// `?` 之后、未解码的部分
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn read_request(s: &mut TcpStream) -> Result<Request, u16> {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    let mut tmp = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = find(&buf, b"\r\n\r\n") {
            break i;
        }
        if buf.len() > MAX_HEAD {
            return Err(431);
        }
        let n = s.read(&mut tmp).map_err(|_| 400u16)?;
        if n == 0 {
            return Err(400);
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let mut req = parse_head(&buf[..head_end])?;
    let want = body_len(&req.method, req.header("content-length"))?;
    let mut body: Vec<u8> = buf[head_end + 4..].to_vec();
    while body.len() < want {
        let n = s.read(&mut tmp).map_err(|_| 400u16)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    if body.len() < want {
        return Err(400);
    }
    // 一个连接只处理一个请求，多读进来的字节直接扔掉。
    body.truncate(want);
    req.body = body;
    Ok(req)
}

/// 请求行 + 头。`head` 不含结尾的 `\r\n\r\n`。
fn parse_head(head: &[u8]) -> Result<Request, u16> {
    let text = std::str::from_utf8(head).map_err(|_| 400u16)?;
    let mut lines = text.split("\r\n");
    let reqline = lines.next().ok_or(400u16)?;
    let mut it = reqline.split_whitespace();
    let method = it.next().ok_or(400u16)?.to_string();
    let target = it.next().ok_or(400u16)?.to_string();
    // 只认真 HTTP/1.x：HTTP/2 前置帧、HTTP/0.9 都不该走到这儿。
    if !it.next().unwrap_or("").starts_with("HTTP/1.") {
        return Err(400);
    }
    if !target.starts_with('/') {
        // 挡住 absolute-form（代理）和 `*`（OPTIONS）：我们只服务源站。
        return Err(400);
    }

    let (raw_path, raw_query) = match target.split_once('?') {
        Some((p, q)) => (p, q),
        None => (target.as_str(), ""),
    };
    let path = percent_decode(raw_path, false).ok_or(400u16)?;
    if path.contains('\0') {
        return Err(400);
    }

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once(':') else {
            return Err(400);
        };
        headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
    }

    Ok(Request {
        method,
        path,
        query: raw_query.to_string(),
        headers,
        body: Vec::new(),
    })
}

/// 请求体长度：只认有界 `Content-Length`。POST 没写 → 411，超上限 → 413。
fn body_len(method: &str, content_length: Option<&str>) -> Result<usize, u16> {
    match content_length {
        Some(v) => {
            let n: usize = v.trim().parse().map_err(|_| 400u16)?;
            if n > MAX_UPLOAD {
                return Err(413);
            }
            Ok(n)
        }
        // 有体的方法必须自己声明长度：拒绝 chunked（我们不解析）。
        None if method == "POST" || method == "PUT" || method == "PATCH" => Err(411),
        None => Ok(0),
    }
}

// ---------------------------------------------------------------- 编解码

/// `%XX` 解码。
///
/// `plus` = 表单语义（`+` → 空格）。路径里 **不** 转：文件名本来就可以叫 `a+b.pdf`。
/// `%` 后面不是两位十六进制、或解出来不是合法 UTF-8，一律 `None`（→ 400）。
fn percent_decode(s: &str, plus: bool) -> Option<String> {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hi = hex_val(*b.get(i + 1)?)?;
                let lo = hex_val(*b.get(i + 2)?)?;
                out.push(hi * 16 + lo);
                i += 3;
            }
            b'+' if plus => {
                out.push(b' ');
                i += 1;
            }
            // 控制字符不进路径：挡掉 %0A 一类被日志/终端误解的值。
            c if c < 0x20 || c == 0x7f => return None,
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    // 解码后也要复查：`%00` / `%0A` 这类转义解出来同样是控制字符。
    if out.iter().any(|&c| c < 0x20 || c == 0x7f) {
        return None;
    }
    String::from_utf8(out).ok()
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// 百分号编码。`keep` 里的字符原样保留，其余一律 `%XX`（UTF-8 逐字节）。
fn percent_encode(s: &str, keep: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &c in s.as_bytes() {
        if c.is_ascii_alphanumeric() || keep.as_bytes().contains(&c) {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

/// 路径/文件名 → URL 里那一段。
fn url_of(s: &str) -> String {
    percent_encode(s, "-_.~/")
}

/// HTML 转义：文件名的每一个字节都要过一遍，否则 `a<b>.txt` 能把页面写坏。
fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// `application/x-www-form-urlencoded` 里取一个字段（query 和 POST 体共用）。
fn form_field(src: &str, key: &str) -> Option<String> {
    for pair in src.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if percent_decode(k, true).as_deref() == Some(key) {
            return percent_decode(v, true);
        }
    }
    None
}

/// 请求头/响应头里能安全出现的文件名（防 CRLF 注入）。
fn header_safe(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_graphic() || c == ' ' { c } else { '_' })
        .collect()
}

// ---------------------------------------------------------------- 路径

/// 相对路径 → 段列表。这是**唯一**的路径入口，所有越界检查都在这里。
fn segments(rel: &str) -> Result<Vec<String>, u16> {
    // 绝对路径直接拒：`/etc/passwd` 不能在库根下被解释成 `etc/passwd`。
    if rel.starts_with('/') {
        return Err(403);
    }
    let mut out = Vec::new();
    for seg in rel.split('/') {
        if seg.is_empty() || seg == "." {
            continue;
        }
        if seg == ".." {
            return Err(403);
        }
        // 隐藏段：`.git`、`.thumbnails`、`.ssh` 之类一律不给看也不给写。
        if seg.starts_with('.') {
            return Err(403);
        }
        if seg.len() > 255 {
            return Err(400);
        }
        out.push(seg.to_string());
    }
    Ok(out)
}

/// 把段拼到库根下，再 canonicalize 一次确认没跑出去（挡软链逃逸）。
///
/// `root` 必须已经是 canonicalize 过的绝对路径（[`start`] 保证）。
fn resolve(root: &Path, rel: &str) -> Result<PathBuf, u16> {
    let segs = segments(rel)?;
    let mut p = root.to_path_buf();
    for s in &segs {
        p.push(s);
    }
    let canon = fs::canonicalize(&p).map_err(|_| 404u16)?;
    if !canon.starts_with(root) {
        return Err(403);
    }
    Ok(canon)
}

/// 上传目标：父目录必须存在且在根内，文件名刚由 [`safe_name`] 校验过。
fn resolve_new(root: &Path, dir_rel: &str, name: &str) -> Result<PathBuf, u16> {
    let dir = resolve(root, dir_rel)?;
    if !dir.is_dir() {
        return Err(404);
    }
    let target = dir.join(name);
    // 目标已存在时必须是普通文件：软链指向哪儿都不能跟（`fs::write` 会顺着写出去）。
    if let Ok(md) = fs::symlink_metadata(&target) {
        if md.file_type().is_symlink() || md.is_dir() {
            return Err(403);
        }
    }
    Ok(target)
}

/// 客户端给的文件名：只要最后一段，挡掉 `..`、隐藏文件、控制字符。
fn safe_name(raw: &str) -> Option<String> {
    // 浏览器不会带目录，但历史上 IE/某些客户端会发 `C:\fakepath\a.pdf`。
    let base = raw.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if base.is_empty() || base == "." || base == ".." || base.starts_with('.') {
        return None;
    }
    if base.len() > 255 {
        return None;
    }
    if base.chars().any(|c| c.is_control()) {
        return None;
    }
    Some(base.to_string())
}

/// 拼一个子项的相对路径（给链接/表单用，统一成 `/` 分隔）。
fn join_rel(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

// ---------------------------------------------------------------- multipart

struct Part {
    /// 表单字段名（上传表单里是 `file`）
    name: String,
    /// 有 filename 才是文件；普通字段是 `None`
    filename: Option<String>,
    data: Vec<u8>,
}

/// 从 `Content-Type: multipart/form-data; boundary=xxx` 里抠 boundary。
fn boundary_of(content_type: &str) -> Option<String> {
    for piece in content_type.split(';') {
        let Some((k, v)) = piece.split_once('=') else {
            continue;
        };
        if k.trim().eq_ignore_ascii_case("boundary") {
            let v = v.trim().trim_matches('"');
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// 解析 multipart 体。
///
/// 只处理浏览器表单发得出来的那种：`--boundary` 分段、头到 `\r\n\r\n`、
/// 下一段以 `\r\n--boundary` 开始。畸形输入尽量「能拿多少拿多少」，
/// 一个 part 解析不出来就跳过，不 panic。
fn parse_multipart(body: &[u8], boundary: &str) -> Vec<Part> {
    let delim = format!("--{boundary}").into_bytes();
    let tail = format!("\r\n--{boundary}").into_bytes();
    let mut out = Vec::new();
    let mut pos = 0usize;
    while let Some(i) = find(&body[pos..], &delim) {
        let after = pos + i + delim.len();
        // 收尾的 `--boundary--`
        if body[after..].starts_with(b"--") {
            break;
        }
        let start = if body[after..].starts_with(b"\r\n") {
            after + 2
        } else {
            after
        };
        let Some(h) = find(&body[start..], b"\r\n\r\n") else {
            break;
        };
        let head = String::from_utf8_lossy(&body[start..start + h]).into_owned();
        let data_start = start + h + 4;
        match find(&body[data_start..], &tail) {
            Some(j) => {
                out.push(part_of(&head, &body[data_start..data_start + j]));
                pos = data_start + j + 2;
            }
            // 最后一段没有尾分隔符：剩下的全算它的，收工。
            None => {
                out.push(part_of(&head, &body[data_start..]));
                break;
            }
        }
    }
    out
}

fn part_of(head: &str, data: &[u8]) -> Part {
    let mut name = String::new();
    let mut filename = None;
    for line in head.split("\r\n") {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        if !k.trim().eq_ignore_ascii_case("content-disposition") {
            continue;
        }
        name = param_of(v, "name").unwrap_or_default();
        filename = param_of(v, "filename");
    }
    Part {
        name,
        filename,
        data: data.to_vec(),
    }
}

/// `form-data; name="file"; filename="a.txt"` 里取一个参数。
///
/// 按 `;` 切：文件名里真带 `;` 的（极少）会被切坏——上传会被拒（400），不会写错。
fn param_of(header: &str, key: &str) -> Option<String> {
    for piece in header.split(';') {
        let Some((k, v)) = piece.split_once('=') else {
            continue;
        };
        if k.trim().eq_ignore_ascii_case(key) {
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
}

// ---------------------------------------------------------------- 路由

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
    /// 额外响应头，每行自己带 `\r\n`。
    extra: String,
}

impl Response {
    fn text(status: u16, body: impl Into<String>) -> Response {
        Response {
            status,
            content_type: "text/plain; charset=utf-8",
            body: body.into().into_bytes(),
            extra: String::new(),
        }
    }

    fn html(body: String) -> Response {
        Response {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.into_bytes(),
            extra: String::new(),
        }
    }

    fn bytes(content_type: &'static str, body: Vec<u8>, extra: String) -> Response {
        Response {
            status: 200,
            content_type,
            body,
            extra,
        }
    }

    fn redirect(location: &str) -> Response {
        Response {
            status: 303,
            content_type: "text/plain; charset=utf-8",
            body: Vec::new(),
            extra: format!("Location: {}\r\n", header_safe(location)),
        }
    }
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        204 => "No Content",
        303 => "See Other",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        _ => "Error",
    }
}

fn route(root: &Path, req: &Request) -> Response {
    if req.method != "GET" && req.method != "POST" {
        return Response::text(405, reason(405));
    }
    match req.path.as_str() {
        "/" if req.method == "GET" => list(root, &req.query),
        "/s.css" if req.method == "GET" => Response::bytes(
            "text/css; charset=utf-8",
            CSS.as_bytes().to_vec(),
            String::new(),
        ),
        "/favicon.ico" => Response {
            status: 204,
            content_type: "image/x-icon",
            body: Vec::new(),
            extra: String::new(),
        },
        "/u" if req.method == "POST" => upload(root, req),
        "/d" if req.method == "POST" => delete(root, req),
        p if req.method == "GET" && p.starts_with("/f/") => download(root, &p[3..]),
        _ => Response::text(404, reason(404)),
    }
}

fn write_response(s: &mut TcpStream, r: Response) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n\
         Connection: close\r\n{}\r\n",
        r.status,
        reason(r.status),
        r.content_type,
        r.body.len(),
        r.extra
    );
    s.write_all(head.as_bytes())?;
    s.write_all(&r.body)?;
    s.flush()
}

// ---------------------------------------------------------------- 三个动作

fn list(root: &Path, query: &str) -> Response {
    let dir_rel = form_field(query, "dir").unwrap_or_default();
    let dir_rel = dir_rel.trim_matches('/').to_string();
    let dir = match resolve(root, &dir_rel) {
        Ok(p) => p,
        Err(code) => return Response::text(code, reason(code)),
    };
    if !dir.is_dir() {
        return Response::text(404, reason(404));
    }
    let Ok(rd) = fs::read_dir(&dir) else {
        return Response::text(500, reason(500));
    };
    let mut dirs: Vec<String> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for e in rd.flatten() {
        let Ok(name) = e.file_name().into_string() else {
            // 非 UTF-8 名字没法安全地编进链接，直接不显示。
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        match e.file_type() {
            Ok(ft) if ft.is_dir() => dirs.push(name),
            Ok(ft) if ft.is_file() => files.push(name),
            // 软链不列：点进去也只会被 `resolve` 拒掉，徒增困惑。
            _ => {}
        }
    }
    dirs.sort_by_key(|s| s.to_lowercase());
    files.sort_by_key(|s| s.to_lowercase());
    Response::html(list_page(&dir_rel, &dirs, &files))
}

fn download(root: &Path, raw: &str) -> Response {
    // 这里的路径已经过 `percent_decode(plus=false)`（见 `parse_head`）。
    let path = match resolve(root, raw) {
        Ok(p) => p,
        Err(code) => return Response::text(code, reason(code)),
    };
    if !path.is_file() {
        return Response::text(404, reason(404));
    }
    let data = match fs::read(&path) {
        Ok(d) => d,
        Err(_) => return Response::text(500, reason(500)),
    };
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    // 中文文件名：给一个 ASCII 回退 + RFC 5987 的 UTF-8 版本，主流浏览器认后者。
    let extra = format!(
        "Content-Disposition: attachment; filename=\"{}\"; filename*=UTF-8''{}\r\n",
        header_safe(&name),
        percent_encode(&name, "!#$&+-.^_`|~")
    );
    Response::bytes(mime_of(&name), data, extra)
}

fn upload(root: &Path, req: &Request) -> Response {
    let dir_rel = form_field(&req.query, "dir")
        .unwrap_or_default()
        .trim_matches('/')
        .to_string();
    let Some(boundary) = req.header("content-type").and_then(boundary_of) else {
        return Response::text(400, reason(400));
    };
    let parts = parse_multipart(&req.body, &boundary);
    let Some(part) = parts
        .into_iter()
        .find(|p| p.name == "file" && p.filename.is_some())
    else {
        return Response::text(400, reason(400));
    };
    let Some(name) = part.filename.as_deref().and_then(safe_name) else {
        return Response::text(400, reason(400));
    };
    let target = match resolve_new(root, &dir_rel, &name) {
        Ok(p) => p,
        Err(code) => return Response::text(code, reason(code)),
    };
    // 官方就是直接覆盖同名文件，备份留给同步服务做。
    if fs::write(&target, &part.data).is_err() {
        return Response::text(500, reason(500));
    }
    eprintln!("[webui] 上传 {}（{} 字节）", name, part.data.len());
    Response::redirect(&format!("/?dir={}", url_of(&dir_rel)))
}

fn delete(root: &Path, req: &Request) -> Response {
    let body = String::from_utf8_lossy(&req.body);
    let Some(rel) = form_field(&body, "path") else {
        return Response::text(400, reason(400));
    };
    // 页面上的 `confirm()` 是给用户的，这里是给「不是页面点的」请求的：
    // 没有显式确认，一律不动手。
    if form_field(&body, "confirm").as_deref() != Some("1") {
        return Response::text(400, reason(400));
    }
    let rel = rel.trim_matches('/').to_string();
    if rel.is_empty() {
        return Response::text(403, reason(403));
    }
    let path = match resolve(root, &rel) {
        Ok(p) => p,
        Err(code) => return Response::text(code, reason(code)),
    };
    if path == root {
        return Response::text(403, reason(403));
    }
    let ok = if path.is_dir() {
        fs::remove_dir_all(&path).is_ok()
    } else {
        fs::remove_file(&path).is_ok()
    };
    if !ok {
        return Response::text(500, reason(500));
    }
    eprintln!("[webui] 删除 {rel}");
    let parent = rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
    Response::redirect(&format!("/?dir={}", url_of(parent)))
}

fn mime_of(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("").to_lowercase().as_str() {
        "pdf" => "application/pdf",
        "epub" => "application/epub+zip",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "txt" | "md" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

// ---------------------------------------------------------------- 页面

fn list_page(dir_rel: &str, dirs: &[String], files: &[String]) -> String {
    let title = "reMarkable";
    let crumb = if dir_rel.is_empty() {
        "&nbsp;".to_string()
    } else {
        html_escape(&format!("/{dir_rel}"))
    };
    // 上一级：`a/b` → `a`，`a` → 根。根目录不画 `..`。
    let mut rows = String::new();
    if !dir_rel.is_empty() {
        let parent = dir_rel.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
        rows.push_str(&format!(
            "<tr><td class=\"name\"><a href=\"/?dir={}\">..</a></td><td class=\"act\"></td></tr>",
            url_of(parent)
        ));
    }
    for d in dirs {
        let rel = join_rel(dir_rel, d);
        rows.push_str(&format!(
            "<tr><td class=\"name\"><a href=\"/?dir={href}\">{name}/</a></td><td class=\"act\"></td></tr>",
            href = url_of(&rel),
            name = html_escape(d)
        ));
    }
    for f in files {
        let rel = join_rel(dir_rel, f);
        let href = url_of(&rel);
        rows.push_str(&format!(
            "<tr><td class=\"name\"><a href=\"/f/{href}\">{name}</a></td>\
             <td class=\"act\"><a class=\"dl\" href=\"/f/{href}\">Download</a>\
             <form method=\"post\" action=\"/d\" onsubmit=\"return confirm('Delete')\">\
             <input type=\"hidden\" name=\"path\" value=\"{rel}\">\
             <input type=\"hidden\" name=\"confirm\" value=\"1\">\
             <button type=\"submit\">Delete</button></form></td></tr>",
            href = href,
            name = html_escape(f),
            rel = html_escape(&rel)
        ));
    }
    if rows.is_empty() {
        rows.push_str("<tr><td class=\"name empty\" colspan=\"2\">&nbsp;</td></tr>");
    }

    format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{title}</title><link rel=\"stylesheet\" href=\"/s.css\"></head>\n\
         <body><header><h1><a href=\"/\">{title}</a></h1><p class=\"path\">{crumb}</p></header>\n\
         <main><form class=\"up\" method=\"post\" action=\"/u?dir={dir}\" \
         enctype=\"multipart/form-data\"><input type=\"file\" name=\"file\" required>\
         <button type=\"submit\">Upload</button></form>\n\
         <table><thead><tr><th>Name</th><th></th></tr></thead><tbody>\n{rows}\
         </tbody></table></main></body></html>\n",
        title = title,
        crumb = crumb,
        dir = url_of(dir_rel),
        rows = rows
    )
}

const CSS: &str = "\
:root{color-scheme:light}\
*{box-sizing:border-box}\
body{margin:0;background:#fff;color:#111;\
font:16px/1.45 -apple-system,BlinkMacSystemFont,\"Segoe UI\",Roboto,sans-serif}\
header{background:#000;color:#fff;padding:16px 22px}\
header h1{margin:0;font-size:20px;font-weight:600}\
header h1 a{color:#fff;text-decoration:none}\
header .path{margin:4px 0 0;font-size:13px;color:#bbb;word-break:break-all}\
main{max-width:920px;margin:0 auto;padding:18px 22px 64px}\
.up{display:flex;gap:10px;align-items:center;padding:0 0 16px;\
border-bottom:1px solid #e6e6e6}\
button{font:inherit;font-size:14px;padding:6px 14px;border:1px solid #111;\
background:#fff;color:#111;border-radius:6px;cursor:pointer}\
button:hover{background:#111;color:#fff}\
table{width:100%;border-collapse:collapse}\
th{text-align:left;font-size:11px;letter-spacing:.08em;text-transform:uppercase;\
color:#777;padding:14px 6px 6px;font-weight:600}\
td{padding:11px 6px;border-bottom:1px solid #eee;vertical-align:middle}\
td.name a{color:#111;text-decoration:none;word-break:break-all}\
td.name a:hover{text-decoration:underline}\
td.act{text-align:right;white-space:nowrap}\
td.act form{display:inline;margin:0 0 0 12px}\
.dl{font-size:14px;color:#111}\
.empty{padding:28px 6px}\
";

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;
    use std::time::Duration as StdDuration;

    const T: StdDuration = StdDuration::from_secs(5);

    // -------------------------------------------------------- 夹具

    /// 每个测试一个独立目录（并发跑也不会打架）。
    fn temp_root(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "rmdev-webui-{}-{}-{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        fs::canonicalize(&p).unwrap()
    }

    /// 直接用 `net::request` 发请求，走真实的 socket（顺带验证响应格式）。
    fn http(
        method: &str,
        addr: SocketAddr,
        path: &str,
        ct: Option<&str>,
        body: &[u8],
    ) -> crate::net::Resp {
        let url = format!("http://{addr}{path}");
        crate::net::request(method, &url, "", ct, body, T).expect("request")
    }

    fn multipart(boundary: &str, filename: &str, data: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; \
                 filename=\"{filename}\"\r\nContent-Type: application/octet-stream\r\n\r\n"
            )
            .as_bytes(),
        );
        v.extend_from_slice(data);
        v.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        v
    }

    // -------------------------------------------------------- 请求解析

    #[test]
    fn parse_head_reads_request_line_and_query() {
        let req = parse_head(
            b"GET /f/a%20b.pdf?dir=x%2Fy&z=1 HTTP/1.1\r\nHost: 10.11.99.1\r\nContent-Length: 0",
        )
        .unwrap();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/f/a b.pdf");
        assert_eq!(req.query, "dir=x%2Fy&z=1");
        assert_eq!(req.header("host"), Some("10.11.99.1"));
    }

    #[test]
    fn parse_head_rejects_bad_input() {
        // 非 HTTP/1.x
        assert_eq!(parse_head(b"GET / HTTP/2.0").unwrap_err(), 400);
        // absolute-form（代理）
        assert_eq!(
            parse_head(b"GET http://x/y HTTP/1.1").unwrap_err(),
            400
        );
        // 没有版本号
        assert_eq!(parse_head(b"GET /").unwrap_err(), 400);
        // 坏头
        assert_eq!(parse_head(b"GET / HTTP/1.1\r\nnokey").unwrap_err(), 400);
        // 坏 %转义
        assert_eq!(parse_head(b"GET /f/%zz HTTP/1.1").unwrap_err(), 400);
    }

    #[test]
    fn body_len_enforces_bounds() {
        assert_eq!(body_len("GET", None), Ok(0));
        assert_eq!(body_len("POST", None), Err(411));
        assert_eq!(body_len("POST", Some("abc")), Err(400));
        assert_eq!(body_len("POST", Some("12")), Ok(12));
        assert_eq!(
            body_len("POST", Some(&(MAX_UPLOAD + 1).to_string())),
            Err(413)
        );
        assert_eq!(body_len("POST", Some(&MAX_UPLOAD.to_string())), Ok(MAX_UPLOAD));
    }

    // -------------------------------------------------------- 编解码

    #[test]
    fn percent_decode_handles_escapes_and_plus() {
        assert_eq!(percent_decode("a%20b", false).unwrap(), "a b");
        assert_eq!(percent_decode("a+b", true).unwrap(), "a b");
        assert_eq!(percent_decode("a+b", false).unwrap(), "a+b");
        // 中文：`新.txt` 的 UTF-8 字节
        assert_eq!(percent_decode("%E6%96%B0.txt", false).unwrap(), "新.txt");
        assert_eq!(percent_decode("%zz", false), None);
        assert_eq!(percent_decode("%E6%96", false), None, "截断的 UTF-8");
        assert_eq!(percent_decode("%00", false), None, "NUL 不能进路径");
    }

    #[test]
    fn percent_encode_roundtrips_a_chinese_name() {
        let enc = url_of("新 文件.pdf");
        assert!(!enc.contains(' '), "{enc}");
        assert_eq!(percent_decode(&enc, false).unwrap(), "新 文件.pdf");
    }

    #[test]
    fn html_escape_neutralizes_tags_and_quotes() {
        assert_eq!(
            html_escape("<img src=x onerror='a'>"),
            "&lt;img src=x onerror=&#39;a&#39;&gt;"
        );
        assert_eq!(html_escape("a&b\"c"), "a&amp;b&quot;c");
        assert_eq!(html_escape("新.txt"), "新.txt");
    }

    // -------------------------------------------------------- 越界

    #[test]
    fn segments_reject_escape_attempts() {
        assert_eq!(segments("../etc/passwd").unwrap_err(), 403);
        assert_eq!(segments("/etc/passwd").unwrap_err(), 403);
        assert_eq!(segments("a/../../b").unwrap_err(), 403);
        assert_eq!(segments(".git/config").unwrap_err(), 403);
        assert_eq!(segments(".thumbnails/x.png").unwrap_err(), 403);
        assert_eq!(segments("a/./b").unwrap(), vec!["a", "b"]);
        assert_eq!(segments("a//b/").unwrap(), vec!["a", "b"]);
    }

    #[test]
    #[cfg(unix)]
    fn resolve_blocks_symlink_escape() {
        let root = temp_root("symlink");
        let outside = temp_root("outside");
        fs::write(outside.join("secret.txt"), b"secret").unwrap();
        // 库根里放一个指向外面的软链（模拟用户/同步服务留下的）
        std::os::unix::fs::symlink(&outside, root.join("leak")).unwrap();
        assert_eq!(resolve(&root, "leak/secret.txt").unwrap_err(), 403);
        assert_eq!(resolve(&root, "leak").unwrap_err(), 403);
    }

    #[test]
    fn resolve_keeps_paths_inside_the_root() {
        let root = temp_root("inside");
        fs::create_dir_all(root.join("docs")).unwrap();
        fs::write(root.join("docs/a.txt"), b"x").unwrap();
        assert_eq!(resolve(&root, "docs/a.txt").unwrap(), root.join("docs/a.txt"));
        assert_eq!(resolve(&root, "missing.txt").unwrap_err(), 404);
        assert_eq!(resolve(&root, "docs/../a.txt").unwrap_err(), 403);
    }

    #[test]
    fn safe_name_strips_client_paths() {
        assert_eq!(safe_name("a.txt").as_deref(), Some("a.txt"));
        assert_eq!(safe_name("C:\\fakepath\\a.txt").as_deref(), Some("a.txt"));
        assert_eq!(safe_name("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(safe_name(".."), None);
        assert_eq!(safe_name("."), None);
        assert_eq!(safe_name(""), None);
        assert_eq!(safe_name(".bashrc"), None);
        assert_eq!(safe_name("a\nb"), None);
    }

    // -------------------------------------------------------- multipart

    #[test]
    fn multipart_parses_file_part() {
        let body = multipart("----x", "notes.txt", b"hello");
        let parts = parse_multipart(&body, "----x");
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].name, "file");
        assert_eq!(parts[0].filename.as_deref(), Some("notes.txt"));
        assert_eq!(parts[0].data, b"hello");
    }

    #[test]
    fn multipart_parses_two_parts_and_keeps_crlf_in_data() {
        let mut body = Vec::new();
        body.extend_from_slice(
            b"------b\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\nv1\r\n",
        );
        body.extend_from_slice(b"------b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.bin\"\r\n\r\n");
        body.extend_from_slice(b"line1\r\nline2");
        body.extend_from_slice(b"\r\n------b--\r\n");
        let parts = parse_multipart(&body, "----b");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].name, "note");
        assert_eq!(parts[0].data, b"v1");
        assert_eq!(parts[1].filename.as_deref(), Some("a.bin"));
        assert_eq!(parts[1].data, b"line1\r\nline2");
    }

    #[test]
    fn multipart_boundary_is_extracted() {
        assert_eq!(
            boundary_of("multipart/form-data; boundary=----abc").as_deref(),
            Some("----abc")
        );
        assert_eq!(
            boundary_of("multipart/form-data; boundary=\"q1\"").as_deref(),
            Some("q1")
        );
        assert_eq!(boundary_of("multipart/form-data"), None);
    }

    // -------------------------------------------------------- 页面

    #[test]
    fn list_page_escapes_names_and_links() {
        let page = list_page(
            "",
            &["<dir>".to_string()],
            &["a<b>&.txt".to_string(), "新 文件.pdf".to_string()],
        );
        assert!(page.contains("&lt;dir&gt;/"), "{page}");
        assert!(page.contains("a&lt;b&gt;&amp;.txt"), "{page}");
        assert!(!page.contains("<b>&"), "文件名不能原样进 HTML：{page}");
        // 链接里的中文/空格必须编码
        assert!(page.contains("%E6%96%B0%20%E6%96%87%E4%BB%B6.pdf"), "{page}");
        assert!(page.contains(">Upload<"));
        assert!(page.contains(">Download<"));
        assert!(page.contains(">Delete<"));
    }

    // -------------------------------------------------------- 端到端

    #[test]
    fn end_to_end_list_upload_download_delete() {
        let root = temp_root("e2e");
        fs::write(root.join("hello.txt"), b"hi").unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();

        let h = start(Config {
            root: root.clone(),
            addr: "127.0.0.1:0".parse().unwrap(),
        })
        .unwrap();
        let a = h.addr();

        // 目录列表
        let r = http("GET", a, "/", None, b"");
        assert_eq!(r.status, 200);
        let html = r.text();
        assert!(html.contains("hello.txt") && html.contains("sub/"), "{html}");

        // 下载
        let r = http("GET", a, "/f/hello.txt", None, b"");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"hi");

        // 上传（中文 + 空格文件名，落到子目录）
        let body = multipart("----b", "新 文件.txt", b"new-bytes");
        let r = http(
            "POST",
            a,
            "/u?dir=sub",
            Some("multipart/form-data; boundary=----b"),
            &body,
        );
        assert_eq!(r.status, 303, "{}", r.text());
        assert_eq!(fs::read(root.join("sub/新 文件.txt")).unwrap(), b"new-bytes");

        // 下载回来（链接里的编码要能解回去）
        let r = http("GET", a, "/f/sub/%E6%96%B0%20%E6%96%87%E4%BB%B6.txt", None, b"");
        assert_eq!(r.status, 200);
        assert_eq!(r.body, b"new-bytes");

        // 删除：没有 confirm 不动手
        let r = http(
            "POST",
            a,
            "/d",
            Some("application/x-www-form-urlencoded"),
            b"path=hello.txt",
        );
        assert_eq!(r.status, 400);
        assert!(root.join("hello.txt").exists());

        // 带确认才删
        let r = http(
            "POST",
            a,
            "/d",
            Some("application/x-www-form-urlencoded"),
            b"path=hello.txt&confirm=1",
        );
        assert_eq!(r.status, 303);
        assert!(!root.join("hello.txt").exists());

        h.stop();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn server_rejects_traversal_and_oversized_requests() {
        let root = temp_root("guard");
        let h = start(Config {
            root: root.clone(),
            addr: "127.0.0.1:0".parse().unwrap(),
        })
        .unwrap();
        let a = h.addr();

        // `%2e%2e` 解码后是 `..`
        let r = http("GET", a, "/f/%2e%2e/%2e%2e/etc/passwd", None, b"");
        assert_eq!(r.status, 403, "{}", r.text());

        // 绝对路径（`//` 被路由吃掉，用 `%2F` 解码成真 `/`）
        let r = http("GET", a, "/f/%2Fetc%2Fpasswd", None, b"");
        assert_eq!(r.status, 403, "{}", r.text());

        // `.git`
        let r = http("GET", a, "/f/.git/config", None, b"");
        assert_eq!(r.status, 403, "{}", r.text());

        // 目录穿越到列表
        let r = http("GET", a, "/?dir=..%2F..%2Fetc", None, b"");
        assert_eq!(r.status, 403, "{}", r.text());

        // 超过 64 MB 的上传：直接 413，不读 body
        let r = http(
            "POST",
            a,
            "/u",
            Some("multipart/form-data; boundary=x"),
            &[],
        );
        // 上面这个没有 Content-Length 的 body 是 0，能进到 multipart 校验 → 400
        assert_eq!(r.status, 400, "{}", r.text());

        h.stop();
        let _ = fs::remove_dir_all(&root);
    }

    /// 413：`net::request` 一定会带 `Content-Length`，所以自己连一次拿裸 socket。
    #[test]
    fn oversized_upload_is_413() {
        let root = temp_root("big");
        let h = start(Config {
            root: root.clone(),
            addr: "127.0.0.1:0".parse().unwrap(),
        })
        .unwrap();
        let mut s = TcpStream::connect(h.addr()).unwrap();
        let head = format!(
            "POST /u HTTP/1.1\r\nHost: x\r\nContent-Type: multipart/form-data; boundary=x\r\n\
             Content-Length: {}\r\n\r\n",
            MAX_UPLOAD + 1
        );
        s.write_all(head.as_bytes()).unwrap();
        let mut raw = Vec::new();
        let _ = s.read_to_end(&mut raw);
        let code = String::from_utf8_lossy(&raw);
        assert!(code.starts_with("HTTP/1.1 413"), "{code}");
        h.stop();
        let _ = fs::remove_dir_all(&root);
    }
}
