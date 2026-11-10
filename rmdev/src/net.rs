//! 自建服务器（home-server）客户端：同步 / 转文字 / 发邮件 / 更新检查。
//!
//! 官方这几项都走 Connect 云（要订阅）。我们换成自己家里一台服务器，
//! 接口契约写在仓库根的 `SERVER.md`。这里只做**客户端**：
//!
//! - 一个极简 HTTP/1.1 客户端（只用 `std::net`，不引第三方 crate；明文 HTTP，
//!   内网或 Tailscale 里跑，TLS 由反代/隧道负责）
//! - 内容寻址的 blob 上传/下载（sha256 自己算，见 [`sha256_hex`]）
//! - 配置持久化：`/home/root/.config/rmdev/server.json`
//!
//! 失败**一律返回 `Err(String)` 且信息可读**——UI 会把它显示成英文 toast。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::Duration;

// ---------------------------------------------------------------- 配置

/// 服务器配置。`url` 为空 = 没配对。
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Config {
    /// 形如 `http://192.168.1.10:8080`（不带尾斜杠）
    pub url: String,
    pub token: String,
    pub device_id: String,
    /// 上次同步到的库版本
    pub last_rev: u64,
    /// "Send by email" 的默认收件人（发过一次就记住）
    #[serde(default)]
    pub mail_to: String,
    /// SOCKS5 代理，形如 `socks5://127.0.0.1:1055`；空 = 直连。
    #[serde(default)]
    pub proxy: String,
}

fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("RMDEV_SERVER_CONFIG") {
        return PathBuf::from(p);
    }
    PathBuf::from("/home/root/.config/rmdev/server.json")
}

impl Config {
    pub fn load() -> Config {
        Self::load_at(&config_path())
    }

    pub fn save(&self) -> std::io::Result<()> {
        self.save_at(&config_path())
    }

    /// 从指定路径读（测试用，避开全局路径竞争）。
    pub fn load_at(p: &std::path::Path) -> Config {
        let Ok(s) = std::fs::read_to_string(p) else {
            return Config::default();
        };
        serde_json::from_str(&s).unwrap_or_default()
    }

    pub fn save_at(&self, p: &std::path::Path) -> std::io::Result<()> {
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let s = serde_json::to_string_pretty(self).unwrap_or_default();
        std::fs::write(p, s)
    }

    pub fn configured(&self) -> bool {
        !self.url.is_empty() && !self.token.is_empty()
    }

    /// 去掉尾斜杠，防止拼出 `//api/v1/...`
    pub fn base(&self) -> &str {
        self.url.trim_end_matches('/')
    }
}

// ---------------------------------------------------------------- HTTP

/// 一次 HTTP 响应。
pub struct Resp {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    /// 把服务器返回的 `{"error":"..."}` 抠出来；没有就返回状态码。
    pub fn error(&self) -> String {
        let t = self.text();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
            if let Some(e) = v.get("error").and_then(|x| x.as_str()) {
                return e.to_string();
            }
        }
        format!("HTTP {}", self.status)
    }
}

/// 拆 `http://host:port/path`。只支持明文 http（TLS 交给隧道/反代）。
fn split_url(url: &str) -> Result<(String, u16, String), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("only http:// is supported: {url}"))?;
    let (hostport, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => {
            let port: u16 = p.parse().map_err(|_| format!("bad port in {url}"))?;
            (h.to_string(), port)
        }
        None => (hostport.to_string(), 80),
    };
    if host.is_empty() {
        return Err(format!("bad url {url}"));
    }
    Ok((host, port, path.to_string()))
}

/// 拆 `socks5://host:port`（只支持无认证的 socks5，不支持 socks5h 远端解析）。
fn split_proxy(proxy: &str) -> Result<(String, u16), String> {
    let rest = proxy
        .trim()
        .strip_prefix("socks5://")
        .ok_or_else(|| format!("only socks5:// proxy is supported: {proxy}"))?;
    let hostport = rest.trim_end_matches('/');
    let (host, port) = hostport
        .rsplit_once(':')
        .ok_or_else(|| format!("proxy needs host:port: {proxy}"))?;
    let port: u16 = port.parse().map_err(|_| format!("bad proxy port in {proxy}"))?;
    if host.is_empty() {
        return Err(format!("bad proxy {proxy}"));
    }
    Ok((host.to_string(), port))
}

/// SOCKS5 无认证握手 + CONNECT，把这个连接变成到 `host:port` 的隧道。
fn socks5_connect(sock: &mut TcpStream, host: &str, port: u16) -> Result<(), String> {
    // 方法协商：VER=5, NMETHODS=1, METHODS=[0x00 无认证]
    sock.write_all(&[0x05, 0x01, 0x00])
        .map_err(|e| format!("socks5 hello: {e}"))?;
    let mut hello = [0u8; 2];
    sock.read_exact(&mut hello)
        .map_err(|e| format!("socks5 hello reply: {e}"))?;
    if hello[0] != 0x05 {
        return Err(format!("socks5 bad version 0x{:02x}", hello[0]));
    }
    if hello[1] != 0x00 {
        return Err(format!("socks5 unsupported auth method 0x{:02x}", hello[1]));
    }

    // CONNECT：VER=5, CMD=1, RSV=0, ATYP + DST.ADDR, DST.PORT。
    // IPv4 走 ATYP=1；域名/其他走 ATYP=3（域名交给代理解析）。
    let mut req = vec![0x05u8, 0x01, 0x00];
    match host.parse::<std::net::Ipv4Addr>() {
        Ok(ip) => {
            req.push(0x01);
            req.extend_from_slice(&ip.octets());
        }
        Err(_) => {
            let name = host.as_bytes();
            let n = u8::try_from(name.len()).map_err(|_| format!("socks5 host too long: {host}"))?;
            req.push(0x03);
            req.push(n);
            req.extend_from_slice(name);
        }
    }
    req.extend_from_slice(&port.to_be_bytes());
    sock.write_all(&req)
        .map_err(|e| format!("socks5 connect: {e}"))?;

    // 回复：VER REP RSV ATYP BND.ADDR BND.PORT
    let mut rep = [0u8; 4];
    sock.read_exact(&mut rep)
        .map_err(|e| format!("socks5 connect reply: {e}"))?;
    if rep[0] != 0x05 {
        return Err(format!("socks5 bad reply version 0x{:02x}", rep[0]));
    }
    if rep[1] != 0x00 {
        return Err(format!("socks5 connect refused: REP=0x{:02x}", rep[1]));
    }
    // BND.ADDR 长度随 ATYP 变，读到就丢（1=IPv4，3=域名，4=IPv6）
    let skip = match rep[3] {
        0x01 => 4 + 2,
        0x04 => 16 + 2,
        0x03 => {
            let mut n = [0u8; 1];
            sock.read_exact(&mut n)
                .map_err(|e| format!("socks5 bind addr: {e}"))?;
            n[0] as usize + 2
        }
        a => return Err(format!("socks5 bad reply ATYP 0x{a:02x}")),
    };
    let mut skipbuf = vec![0u8; skip];
    sock.read_exact(&mut skipbuf)
        .map_err(|e| format!("socks5 bind addr: {e}"))?;
    Ok(())
}

/// 建一条到 `host:port` 的连接；`proxy` 非空就先过 SOCKS5。
fn connect(host: &str, port: u16, timeout: Duration, proxy: &str) -> Result<TcpStream, String> {
    if proxy.trim().is_empty() {
        let addr = format!("{host}:{port}");
        return addr
            .to_socket_addrs_first()
            .and_then(|sa| TcpStream::connect_timeout(&sa, timeout).ok())
            .ok_or_else(|| format!("cannot connect to {addr}"));
    }
    let (phost, pport) = split_proxy(proxy)?;
    let paddr = format!("{phost}:{pport}");
    let mut sock = paddr
        .to_socket_addrs_first()
        .and_then(|sa| TcpStream::connect_timeout(&sa, timeout).ok())
        .ok_or_else(|| format!("cannot connect to proxy {paddr}"))?;
    // 握手也得有超时，否则代理一挂就卡死
    let _ = sock.set_read_timeout(Some(timeout));
    let _ = sock.set_write_timeout(Some(timeout));
    eprintln!("[net] 经 SOCKS5 代理 {paddr} → {host}:{port}");
    socks5_connect(&mut sock, host, port)?;
    Ok(sock)
}

/// 发一个请求（地址走 [`Config::load`] 里的 `proxy`）。
pub fn request(
    method: &str,
    url: &str,
    token: &str,
    content_type: Option<&str>,
    body: &[u8],
    timeout: Duration,
) -> Result<Resp, String> {
    // 每次现读配置：改完 server.json / 配对后立刻生效
    let proxy = Config::load().proxy;
    request_with(method, url, token, content_type, body, timeout, &proxy)
}

/// 同 [`request`]，但代理用参数显式传入（测试用，免去抢全局 `RMDEV_SERVER_CONFIG`）。
/// 打开一条**流式**请求：返回的 reader 停在响应头之后，调用方按行读（SSE/NDJSON）。
///
/// `path` 是相对服务器的路径（`/api/v1/events`），token 和代理自动从 `Config` 取。
/// 只支持 `200`；其它状态码读完整个响应再报错（错误里带 body）。
pub fn open_stream(
    method: &str,
    path: &str,
    body: &[u8],
    timeout: Duration,
) -> Result<std::io::BufReader<std::net::TcpStream>, String> {
    use std::io::{Read, Write};
    let cfg = Config::load();
    if !cfg.configured() {
        return Err("not paired".into());
    }
    let (host, port, _) = split_url(&format!("{}{}", cfg.base(), path))?;
    let mut sock = connect(&host, port, timeout, &cfg.proxy)?;
    let _ = sock.set_read_timeout(Some(timeout));
    let _ = sock.set_write_timeout(Some(timeout));
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\
         Accept: text/event-stream\r\nContent-Length: {}\r\n\
         Authorization: Bearer {}\r\n",
        body.len(),
        cfg.token
    );
    if !body.is_empty() {
        head.push_str("Content-Type: application/json\r\n");
    }
    head.push_str("\r\n");
    sock.write_all(head.as_bytes())
        .and_then(|_| sock.write_all(body))
        .map_err(|e| format!("send failed: {e}"))?;

    // 逐字节读响应头（几十字节，不会慢），剩下的留给调用方流式读。
    let mut hdr = Vec::with_capacity(256);
    let mut b = [0u8; 1];
    while hdr.len() < 8192 {
        match sock.read(&mut b) {
            Ok(0) => break,
            Ok(_) => {
                hdr.push(b[0]);
                if hdr.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Err(e) => return Err(format!("read failed: {e}")),
        }
    }
    let text = String::from_utf8_lossy(&hdr).to_string();
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    if status != 200 {
        let mut rest = Vec::new();
        let _ = sock.read_to_end(&mut rest);
        return Err(format!(
            "HTTP {status}: {}",
            String::from_utf8_lossy(&rest).trim()
        ));
    }
    Ok(std::io::BufReader::new(sock))
}

fn request_with(
    method: &str,
    url: &str,
    token: &str,
    content_type: Option<&str>,
    body: &[u8],
    timeout: Duration,
    proxy: &str,
) -> Result<Resp, String> {
    let (host, port, path) = split_url(url)?;
    let mut sock = connect(&host, port, timeout, proxy)?;
    let _ = sock.set_read_timeout(Some(timeout));
    let _ = sock.set_write_timeout(Some(timeout));

    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\
         Accept: */*\r\nContent-Length: {}\r\n",
        body.len()
    );
    if !token.is_empty() {
        head.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    if let Some(ct) = content_type {
        head.push_str(&format!("Content-Type: {ct}\r\n"));
    }
    head.push_str("\r\n");
    sock.write_all(head.as_bytes())
        .and_then(|_| sock.write_all(body))
        .map_err(|e| format!("send failed: {e}"))?;

    let mut raw = Vec::new();
    sock.read_to_end(&mut raw)
        .map_err(|e| format!("read failed: {e}"))?;
    parse_response(&raw)
}

/// 把 `addr` 解析成第一个 socket 地址（`ToSocketAddrs` 的取首元素版）。
trait FirstAddr {
    fn to_socket_addrs_first(&self) -> Option<std::net::SocketAddr>;
}
impl FirstAddr for String {
    fn to_socket_addrs_first(&self) -> Option<std::net::SocketAddr> {
        use std::net::ToSocketAddrs;
        self.as_str().to_socket_addrs().ok().and_then(|mut it| it.next())
    }
}

/// 解析 HTTP/1.1 响应。只处理 `Content-Length` / 读到 EOF 两种。
pub fn parse_response(raw: &[u8]) -> Result<Resp, String> {
    let sep = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("malformed response (no header end)")?;
    let head = String::from_utf8_lossy(&raw[..sep]);
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("bad status line: {status_line}"))?;
    let mut len: Option<usize> = None;
    let mut chunked = false;
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { continue };
        let k = k.trim().to_ascii_lowercase();
        let v = v.trim();
        if k == "content-length" {
            len = v.parse().ok();
        } else if k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked") {
            chunked = true;
        }
    }
    let mut body = raw[sep + 4..].to_vec();
    if chunked {
        body = dechunk(&body);
    } else if let Some(n) = len {
        if body.len() > n {
            body.truncate(n);
        }
    }
    Ok(Resp { status, body })
}

/// `Transfer-Encoding: chunked` 解码。
fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    loop {
        let Some(eol) = b.windows(2).position(|w| w == b"\r\n") else { break };
        let size_str = String::from_utf8_lossy(&b[..eol]);
        let Ok(n) = usize::from_str_radix(size_str.trim(), 16) else { break };
        if n == 0 {
            break;
        }
        let start = eol + 2;
        if start + n > b.len() {
            out.extend_from_slice(&b[start..]);
            break;
        }
        out.extend_from_slice(&b[start..start + n]);
        b = &b[start + n..];
        if b.starts_with(b"\r\n") {
            b = &b[2..];
        }
    }
    out
}

// ---------------------------------------------------------------- sha256

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// 标准 sha256，输出小写十六进制。服务器用同一个值寻址 blob。
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bit_len = (data.len() as u64) * 8;
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for chunk in msg.chunks(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let (mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh) =
            (h[0], h[1], h[2], h[3], h[4], h[5], h[6], h[7]);
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (i, v) in [a, b, c, d, e, f, g, hh].iter().enumerate() {
            h[i] = h[i].wrapping_add(*v);
        }
    }
    h.iter().map(|x| format!("{x:08x}")).collect()
}

// ---------------------------------------------------------------- API

const T: Duration = Duration::from_secs(20);

/// 配对：服务器返回 device_id + token，我们存下来。
pub fn pair(url: &str, code: &str, device_name: &str) -> Result<Config, String> {
    pair_at(&config_path(), url, code, device_name)
}

/// 配对到指定配置文件（测试用）。
pub fn pair_at(
    path: &std::path::Path,
    url: &str,
    code: &str,
    device_name: &str,
) -> Result<Config, String> {
    let body = serde_json::json!({
        "code": code.trim(),
        "device_name": device_name,
        "protocol": 1,
    })
    .to_string();
    let r = request("POST", &format!("{}/api/v1/pair", url.trim_end_matches('/')), "", Some("application/json"), body.as_bytes(), T)?;
    if r.status != 200 {
        return Err(r.error());
    }
    let v: serde_json::Value =
        serde_json::from_str(&r.text()).map_err(|e| format!("bad json: {e}"))?;
    let cfg = Config {
        url: url.trim_end_matches('/').to_string(),
        token: v.get("token").and_then(|x| x.as_str()).unwrap_or_default().to_string(),
        device_id: v
            .get("device_id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
        last_rev: 0,
        mail_to: String::new(),
        // 配对请求本身往往就要走代理，别在这里把它清掉
        proxy: Config::load_at(path).proxy,
    };
    if cfg.token.is_empty() {
        return Err("server did not return a token".into());
    }
    cfg.save_at(path).map_err(|e| format!("save config: {e}"))?;
    Ok(cfg)
}

/// 健康检查（不需要 token）。
pub fn health(url: &str) -> Result<bool, String> {
    let r = request("GET", &format!("{}/api/v1/health", url.trim_end_matches('/')), "", None, b"", Duration::from_secs(5))?;
    Ok(r.status == 200)
}

/// 服务器侧的库清单。
pub fn manifest(cfg: &Config) -> Result<serde_json::Value, String> {
    let r = request("GET", &format!("{}/api/v1/manifest", cfg.base()), &cfg.token, None, b"", T)?;
    if r.status != 200 {
        return Err(r.error());
    }
    serde_json::from_str(&r.text()).map_err(|e| format!("bad json: {e}"))
}

/// 传一个 blob（内容寻址，已存在服务器直接 204）。
pub fn put_blob(cfg: &Config, data: &[u8]) -> Result<String, String> {
    let sha = sha256_hex(data);
    let url = format!("{}/api/v1/blob", cfg.base());
    // 需要额外头 X-Sha256：先用一个变体
    let (host, port, path) = split_url(&url)?;
    let mut sock = connect(&host, port, T, &cfg.proxy)?;
    let _ = sock.set_read_timeout(Some(T));
    let _ = sock.set_write_timeout(Some(T));
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\
         Authorization: Bearer {}\r\nX-Sha256: {sha}\r\n\
         Content-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
        cfg.token,
        data.len()
    );
    sock.write_all(head.as_bytes())
        .and_then(|_| sock.write_all(data))
        .map_err(|e| format!("send failed: {e}"))?;
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw)
        .map_err(|e| format!("read failed: {e}"))?;
    let r = parse_response(&raw)?;
    if r.status != 204 && r.status != 200 {
        return Err(r.error());
    }
    Ok(sha)
}

/// 取一个 blob。
pub fn get_blob(cfg: &Config, sha: &str) -> Result<Vec<u8>, String> {
    let r = request("GET", &format!("{}/api/v1/blob/{sha}", cfg.base()), &cfg.token, None, b"", T)?;
    if r.status != 200 {
        return Err(r.error());
    }
    Ok(r.body)
}

/// 一次 commit 里的一个 put。
#[derive(Clone, Debug, serde::Serialize)]
pub struct Put {
    pub path: String,
    pub sha256: String,
    pub mtime: u64,
}

/// 提交改动。返回新 rev 和冲突列表。
pub fn commit(
    cfg: &Config,
    base_rev: u64,
    puts: &[Put],
    dels: &[serde_json::Value],
) -> Result<(u64, Vec<String>), String> {
    let body = serde_json::json!({
        "base_rev": base_rev,
        "puts": puts,
        "dels": dels,
    })
    .to_string();
    let r = request("POST", &format!("{}/api/v1/commit", cfg.base()), &cfg.token, Some("application/json"), body.as_bytes(), T)?;
    if r.status != 200 {
        return Err(r.error());
    }
    let v: serde_json::Value =
        serde_json::from_str(&r.text()).map_err(|e| format!("bad json: {e}"))?;
    let rev = v.get("rev").and_then(|x| x.as_u64()).unwrap_or(base_rev);
    let conflicts = v
        .get("conflicts")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default();
    Ok((rev, conflicts))
}

/// 手写/印刷体转文字。`image` 是页面 PNG，先上传再引用。
pub fn ocr(cfg: &Config, image: &[u8], mode: &str, lang: &str) -> Result<String, String> {
    let sha = put_blob(cfg, image)?;
    let body = serde_json::json!({
        "image_blob": sha, "mode": mode, "lang": lang,
    })
    .to_string();
    let r = request("POST", &format!("{}/api/v1/ocr", cfg.base()), &cfg.token, Some("application/json"), body.as_bytes(), Duration::from_secs(300))?;
    if r.status != 200 {
        return Err(r.error());
    }
    let v: serde_json::Value =
        serde_json::from_str(&r.text()).map_err(|e| format!("bad json: {e}"))?;
    Ok(v.get("text").and_then(|x| x.as_str()).unwrap_or_default().to_string())
}

/// 发邮件（附件走 blob）。
pub fn send_mail(
    cfg: &Config,
    to: &[String],
    subject: &str,
    body_text: &str,
    attachment: Option<(&[u8], &str, &str)>,
) -> Result<(), String> {
    let mut list: Vec<(&[u8], String, &str)> = Vec::new();
    if let Some((data, name, mime)) = attachment {
        list.push((data, name.to_string(), mime));
    }
    send_mail_multi(cfg, to, subject, body_text, &list)
}

/// 多附件发信（P2：PNG 多页要发多张）。请求体字段与 `send_mail` 完全一致。
pub fn send_mail_multi(
    cfg: &Config,
    to: &[String],
    subject: &str,
    body_text: &str,
    attachments: &[(&[u8], String, &str)],
) -> Result<(), String> {
    let mut atts = Vec::with_capacity(attachments.len());
    for (data, name, mime) in attachments {
        let sha = put_blob(cfg, data)?;
        atts.push(serde_json::json!({"blob": sha, "filename": name, "mime": mime}));
    }
    let body = serde_json::json!({
        "to": to, "subject": subject, "body": body_text, "attachments": atts,
    })
    .to_string();
    let r = request("POST", &format!("{}/api/v1/mail", cfg.base()), &cfg.token, Some("application/json"), body.as_bytes(), Duration::from_secs(60))?;
    if r.status != 200 {
        return Err(r.error());
    }
    Ok(())
}

/// 当前设备 IP（给"屏幕共享"提示用；拿不到就 10.11.99.1）。
pub fn device_ip() -> String {
    let Ok(out) = std::process::Command::new("ip")
        .args(["-4", "-o", "addr", "show", "dev", "wlan0"])
        .output()
    else {
        return "10.11.99.1".into();
    };
    let s = String::from_utf8_lossy(&out.stdout);
    s.split("inet ")
        .nth(1)
        .and_then(|x| x.split('/').next())
        .map(|x| x.trim().to_string())
        .unwrap_or_else(|| "10.11.99.1".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        // 跨块边界
        let long = vec![b'a'; 1000];
        assert_eq!(sha256_hex(&long).len(), 64);
        assert_eq!(
            sha256_hex(b"The quick brown fox jumps over the lazy dog"),
            "d7a8fbb307d7809469ca9abcb0082e4f8d5651e46d3cdb762d02d0bf37c9e592"
        );
    }

    #[test]
    fn split_url_handles_ports_and_paths() {
        assert_eq!(
            split_url("http://1.2.3.4:8080").unwrap(),
            ("1.2.3.4".into(), 8080, "/".into())
        );
        assert_eq!(
            split_url("http://nas.local/api/v1/health").unwrap(),
            ("nas.local".into(), 80, "/api/v1/health".into())
        );
        assert!(split_url("https://x/").is_err());
        assert!(split_url("http://1.2.3.4:notaport/").is_err());
    }

    #[test]
    fn parse_response_reads_body_and_status() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhelloEXTRA";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.text(), "hello");
    }

    #[test]
    fn parse_response_dechunks() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n2\r\n !\r\n0\r\n\r\n";
        let r = parse_response(raw).unwrap();
        assert_eq!(r.text(), "hello !");
    }

    #[test]
    fn error_prefers_json_error_field() {
        let r = Resp {
            status: 401,
            body: br#"{"error":"bad_code"}"#.to_vec(),
        };
        assert_eq!(r.error(), "bad_code");
        let r = Resp { status: 500, body: b"oops".to_vec() };
        assert_eq!(r.error(), "HTTP 500");
    }

    #[test]
    fn config_roundtrip() {
        let dir = std::env::temp_dir().join(format!("rmdev-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("server.json");
        let c = Config {
            url: "http://192.168.1.9:8080/".into(),
            token: "abc".into(),
            device_id: "d-1".into(),
            last_rev: 7,
            mail_to: "me@example.com".into(),
            ..Default::default()
        };
        c.save_at(&p).unwrap();
        let back = Config::load_at(&p);
        assert_eq!(back.token, "abc");
        assert_eq!(back.last_rev, 7);
        assert!(back.configured());
        assert_eq!(back.base(), "http://192.168.1.9:8080");
        // 老 server.json 没有 proxy 字段也要能读（serde default = 直连）
        std::fs::write(
            &p,
            r#"{"url":"http://1.2.3.4:8787","token":"t","device_id":"d","last_rev":3,"mail_to":""}"#,
        )
        .unwrap();
        let old = Config::load_at(&p);
        assert_eq!(old.proxy, "");
        assert!(old.configured());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 起一个只会说 HTTP 的假服务器，验证真实收发的字节。
    fn mock_server(reply: &'static str) -> (u16, std::thread::JoinHandle<String>) {
        use std::net::TcpListener;
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            // 读到 header 结束 + 声明的 body
            loop {
                let n = s.read(&mut tmp).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..i]).to_ascii_lowercase();
                    let want: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    if buf.len() >= i + 4 + want {
                        break;
                    }
                }
            }
            s.write_all(reply.as_bytes()).unwrap();
            String::from_utf8_lossy(&buf).into_owned()
        });
        (port, h)
    }

    #[test]
    fn request_writes_headers_and_body() {
        let (port, h) = mock_server("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
        let url = format!("http://127.0.0.1:{port}/api/v1/commit");
        let r = request("POST", &url, "tok123", Some("application/json"), b"{\"a\":1}", T).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.text(), "ok");
        let seen = h.join().unwrap();
        let low = seen.to_ascii_lowercase();
        assert!(low.starts_with("post /api/v1/commit http/1.1"), "{seen}");
        assert!(low.contains("authorization: bearer tok123"), "{seen}");
        assert!(low.contains("content-type: application/json"), "{seen}");
        assert!(seen.ends_with("{\"a\":1}"), "{seen}");
    }

    #[test]
    fn pair_saves_config_from_response() {
        let (port, h) = mock_server(
            "HTTP/1.1 200 OK\r\nContent-Length: 45\r\n\r\n{\"device_id\":\"d-9\",\"token\":\"deadbeef\"}",
        );
        let dir = std::env::temp_dir().join(format!("rmdev-pair-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("server.json");
        let cfg = pair_at(&p, &format!("http://127.0.0.1:{port}"), "12345678", "rM2").unwrap();
        assert_eq!(cfg.token, "deadbeef");
        assert_eq!(Config::load_at(&p).device_id, "d-9");
        let seen = h.join().unwrap();
        assert!(seen.contains("\"code\":\"12345678\""), "{seen}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 假 SOCKS5 代理：走完握手 + CONNECT 后，在**同一个连接**上收 HTTP 再回一段固定响应。
    /// 返回它实际收到的 `(问候, CONNECT 请求, HTTP 请求)` 字节。
    /// `rep` 是 CONNECT 回复里的 REP（0 = 成功）。
    fn socks_server(rep: u8) -> (u16, std::thread::JoinHandle<(Vec<u8>, Vec<u8>, String)>) {
        use std::net::TcpListener;
        let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = l.local_addr().unwrap().port();
        let h = std::thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            // 01 方法协商：必须正好是 05 01 00
            let mut hello = [0u8; 3];
            s.read_exact(&mut hello).unwrap();
            s.write_all(&[0x05, 0x00]).unwrap();
            // 02 CONNECT：先读 4 字节头（VER CMD RSV ATYP），再按 ATYP 读地址 + 端口
            let mut head = [0u8; 4];
            s.read_exact(&mut head).unwrap();
            let mut req = head.to_vec();
            let rest = match head[3] {
                0x01 => 4 + 2,
                0x03 => {
                    let mut n = [0u8; 1];
                    s.read_exact(&mut n).unwrap();
                    req.push(n[0]);
                    n[0] as usize + 2
                }
                a => panic!("unexpected ATYP {a:#04x}"),
            };
            let mut tail = vec![0u8; rest];
            s.read_exact(&mut tail).unwrap();
            req.extend_from_slice(&tail);
            // 03 回 CONNECT 结果（绑定地址随便填，客户端会跳过）
            let reply = [0x05, rep, 0x00, 0x01, 127, 0, 0, 1, 0x1f, 0x90];
            if rep != 0 {
                // 客户端读到 REP!=0 就收工，写不出去也不算错
                let _ = s.write_all(&reply);
                return (hello.to_vec(), req, String::new());
            }
            s.write_all(&reply).unwrap();
            // 04 隧道通了，照旧收 HTTP（header 结束 + Content-Length）
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            loop {
                let n = s.read(&mut tmp).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..i]).to_ascii_lowercase();
                    let want: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    if buf.len() >= i + 4 + want {
                        break;
                    }
                }
            }
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nsocks")
                .unwrap();
            (hello.to_vec(), req, String::from_utf8_lossy(&buf).into_owned())
        });
        (port, h)
    }

    #[test]
    fn request_tunnels_http_through_socks5() {
        let (port, h) = socks_server(0x00);
        let proxy = format!("socks5://127.0.0.1:{port}");
        let r = request_with(
            "GET",
            "http://127.0.0.1:8787/api/v1/health",
            "tok",
            None,
            b"",
            T,
            &proxy,
        )
        .unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.text(), "socks");
        let (hello, connect, http) = h.join().unwrap();
        assert_eq!(hello, vec![0x05, 0x01, 0x00]);
        // IPv4 目标 → ATYP=1 + 4 字节 + 2 字节端口
        assert_eq!(
            connect,
            vec![
                0x05,
                0x01,
                0x00,
                0x01,
                127,
                0,
                0,
                1,
                (8787u16 >> 8) as u8,
                8787u16 as u8
            ]
        );
        // 握手之后必须是同一个连接上跑的明文 HTTP
        assert!(http.starts_with("GET /api/v1/health HTTP/1.1"), "{http}");
    }

    #[test]
    fn socks5_sends_domain_targets_as_atyp_3() {
        let (port, h) = socks_server(0x00);
        let proxy = format!("socks5://127.0.0.1:{port}");
        let r = request_with("GET", "http://nas.local:8787/x", "", None, b"", T, &proxy).unwrap();
        assert_eq!(r.status, 200);
        let (_hello, connect, _http) = h.join().unwrap();
        let mut want = vec![0x05, 0x01, 0x00, 0x03, 9];
        want.extend_from_slice(b"nas.local");
        want.extend_from_slice(&8787u16.to_be_bytes());
        assert_eq!(connect, want);
    }

    #[test]
    fn socks5_rep_error_fails_request() {
        let (port, h) = socks_server(0x05);
        let proxy = format!("socks5://127.0.0.1:{port}");
        let err = match request_with("GET", "http://127.0.0.1:8787/x", "", None, b"", T, &proxy) {
            Ok(_) => panic!("REP=0x05 必须失败"),
            Err(e) => e,
        };
        assert!(err.contains("socks5"), "{err}");
        assert!(err.contains("0x05"), "{err}");
        let _ = h.join();
    }

    #[test]
    fn split_proxy_requires_socks5_host_port() {
        assert_eq!(
            split_proxy("socks5://127.0.0.1:1055").unwrap(),
            ("127.0.0.1".into(), 1055)
        );
        assert!(split_proxy("http://127.0.0.1:1055").is_err());
        assert!(split_proxy("socks5://127.0.0.1").is_err());
        assert!(split_proxy("socks5://:1055").is_err());
    }
}
