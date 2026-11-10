//! `rmcloud-tool`：给 agent 的 bash 用的窄工具（只读设备命令的出口）。
//!
//! ```text
//! rmcloud-tool render <uuid> <page> [-o out.png]   # 整页 PNG，打印落盘路径
//! rmcloud-tool strokes <uuid> <page>               # 笔迹几何摘要（JSON）
//! rmcloud-tool search "<query>"                    # 库内搜索（JSON）
//! rmcloud-tool profile [--all|--learned] [--limit N]  # 错题档案（表格）
//! ```
//!
//! token 从 `/etc/rmcloud.json`（`RMCLOUD_CONFIG` 可覆盖）读；请求
//! `127.0.0.1:8787`（本机回环，不走 Tailscale），所以 rmcloud 必须同时监听回环。
//! 不引入 HTTP 客户端依赖：这里手写一个够用的 HTTP/1.1 POST。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{bail, Context, Result};

/// 本机回环，固定端口（见 `docs/CLOUD.md`）。
const URL: &str = "127.0.0.1:8787";

fn main() {
    if let Err(e) = run() {
        eprintln!("rmcloud-tool: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("render") => render(&args[1..]),
        Some("strokes") => page_op(&args[1..], "strokes"),
        Some("search") => search(&args[1..]),
        Some("profile") => profile_list(&args[1..]),
        Some("-h" | "--help" | "help") => {
            print_usage();
            Ok(())
        }
        Some(other) => {
            print_usage();
            bail!("未知子命令：{other}")
        }
        None => {
            print_usage();
            bail!("缺少子命令");
        }
    }
}

/// `render <uuid> <page> [-o out.png]`
fn render(args: &[String]) -> Result<()> {
    let (uuid, page) = uuid_page(args, "render <uuid> <page> [-o out.png]")?;
    let mut out = None;
    let mut i = 2;
    while i < args.len() {
        match args[i].as_str() {
            "-o" | "--out" => {
                out = Some(
                    args.get(i + 1)
                        .cloned()
                        .context("-o 后面要跟输出文件路径")?,
                );
                i += 2;
            }
            other => bail!("render 不认识的参数：{other}"),
        }
    }
    let out = out.unwrap_or_else(|| format!("/tmp/rmcloud-render-{uuid}-{page}.png"));

    let (status, body) = post("/api/v1/tool/render", &serde_json::json!({
        "uuid": uuid,
        "page": page,
    }))?;
    if status != 200 {
        bail!("HTTP {status}: {}", String::from_utf8_lossy(&body).trim());
    }
    std::fs::write(&out, &body).with_context(|| format!("写不了 {out}"))?;
    println!("{out}");
    Ok(())
}

/// `strokes <uuid> <page>` / 将来别的按页 op
fn page_op(args: &[String], op: &str) -> Result<()> {
    let (uuid, page) = uuid_page(args, &format!("{op} <uuid> <page>"))?;
    let (status, body) = post(&format!("/api/v1/tool/{op}"), &serde_json::json!({
        "uuid": uuid,
        "page": page,
    }))?;
    print_json(status, &body)
}

/// `search "<query>"`
fn search(args: &[String]) -> Result<()> {
    let query = args.join(" ");
    if query.trim().is_empty() {
        bail!("search 需要一个查询串");
    }
    let (status, body) = post("/api/v1/tool/search", &serde_json::json!({
        "q": query.trim(),
    }))?;
    print_json(status, &body)
}

/// `profile [--all | --learned] [--limit N]`：打错题档案（默认只看没学会的）。
fn profile_list(args: &[String]) -> Result<()> {
    let mut status = "open";
    let mut limit = 50u32;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--all" => {
                status = "all";
                i += 1;
            }
            "--learned" => {
                status = "learned";
                i += 1;
            }
            "--limit" => {
                limit = args
                    .get(i + 1)
                    .context("--limit 后面要跟数字")?
                    .parse()
                    .context("--limit 必须是数字")?;
                i += 2;
            }
            other => bail!("profile 不认识的参数：{other}"),
        }
    }

    let (st, body) = get(&format!("/api/v1/profile?status={status}&limit={limit}"))?;
    let text = String::from_utf8_lossy(&body);
    if !(200..300).contains(&st) {
        bail!("HTTP {st}: {}", text.trim());
    }
    let v: serde_json::Value = serde_json::from_str(&text).context("响应不是 JSON")?;
    let rows = v
        .get("problems")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();

    println!(
        "{:<3} {:<6} {:<4} {:<7} {:<6} {}",
        "id", "优先级", "重复", "状态", "学科", "题目"
    );
    for p in &rows {
        println!(
            "{:<3} {:<6} ×{:<2} {:<7} {:<6} {}",
            p["id"].as_i64().unwrap_or(0),
            p["priority"].as_str().unwrap_or("-"),
            p["repeat_count"].as_i64().unwrap_or(0),
            p["status"].as_str().unwrap_or("-"),
            p["subject"].as_str().unwrap_or("-"),
            clip(p["text"].as_str().unwrap_or(""), 40),
        );
    }
    println!("共 {} 条（status={status}）", rows.len());
    Ok(())
}

/// 只留前 `n` 个字（按字符切，不会切坏 UTF-8）。
fn clip(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().nth(n).is_some() {
        out.push('…');
    }
    out
}

fn uuid_page(args: &[String], usage: &str) -> Result<(String, u32)> {
    let uuid = args
        .first()
        .cloned()
        .with_context(|| format!("用法：rmcloud-tool {usage}"))?;
    let page = args
        .get(1)
        .with_context(|| format!("用法：rmcloud-tool {usage}"))?
        .parse::<u32>()
        .with_context(|| format!("page 必须是数字：{}", args[1]))?;
    Ok((uuid, page))
}

fn print_json(status: u16, body: &[u8]) -> Result<()> {
    let text = String::from_utf8_lossy(body);
    if !(200..300).contains(&status) {
        bail!("HTTP {status}: {}", text.trim());
    }
    println!("{}", text.trim());
    Ok(())
}

/// 发一个 JSON POST，返回 (状态码, 响应体字节)。
fn post(path: &str, body: &serde_json::Value) -> Result<(u16, Vec<u8>)> {
    let payload = serde_json::to_vec(body).context("请求体序列化失败")?;
    request("POST", path, Some(&payload))
}

/// 发一个 GET（错题档案列表用），返回 (状态码, 响应体字节)。
fn get(path: &str) -> Result<(u16, Vec<u8>)> {
    request("GET", path, None)
}

/// 手写一个 HTTP/1.1 请求：`payload` 有值才带 JSON body。
fn request(method: &str, path: &str, payload: Option<&[u8]>) -> Result<(u16, Vec<u8>)> {
    let cfg = load_config()?;
    if cfg.token.is_empty() {
        bail!(
            "{} 里没有 token：先跑 `rmcloud pair` 并把码输入设备",
            config_path().display()
        );
    }
    let mut stream = TcpStream::connect(URL)
        .with_context(|| format!("连不上 {URL}（rmcloud 没起，或者只绑了 Tailscale 地址）"))?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;

    let body_headers = match payload {
        Some(p) => format!("Content-Type: application/json\r\nContent-Length: {}\r\n", p.len()),
        None => String::new(),
    };
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {URL}\r\nAuthorization: Bearer {}\r\n\
         {body_headers}Connection: close\r\n\r\n",
        cfg.token,
    );
    stream.write_all(head.as_bytes())?;
    if let Some(payload) = payload {
        stream.write_all(payload)?;
    }
    stream.flush()?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).context("读响应失败")?;
    parse_response(&raw)
}

/// 解析 HTTP/1.1 响应（`Connection: close`，所以读到 EOF；顺手支持 chunked）。
fn parse_response(raw: &[u8]) -> Result<(u16, Vec<u8>)> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .context("HTTP 响应缺少头尾分隔")?;
    let head = std::str::from_utf8(&raw[..split]).context("响应头不是 UTF-8")?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().context("空响应")?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .context("状态行里没有状态码")?
        .parse()
        .context("状态码不是数字")?;

    let mut content_length = None;
    let mut chunked = false;
    for line in lines {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "content-length" => content_length = value.trim().parse::<usize>().ok(),
            "transfer-encoding" if value.to_ascii_lowercase().contains("chunked") => {
                chunked = true
            }
            _ => {}
        }
    }

    let rest = &raw[split + 4..];
    let body = if chunked {
        dechunk(rest)?
    } else {
        rest.to_vec()
    };
    let body = match content_length {
        Some(n) if n <= body.len() => body[..n].to_vec(),
        _ => body,
    };
    Ok((status, body))
}

fn dechunk(mut body: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let Some(end) = body.windows(2).position(|w| w == b"\r\n") else {
            bail!("chunked 响应被截断");
        };
        let line = std::str::from_utf8(&body[..end]).context("chunk 头不是 UTF-8")?;
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16)
            .context("chunk 大小不是十六进制")?;
        body = &body[end + 2..];
        if size == 0 {
            break;
        }
        if body.len() < size + 2 {
            bail!("chunked 响应被截断");
        }
        out.extend_from_slice(&body[..size]);
        body = &body[size + 2..];
    }
    Ok(out)
}

/// 配置路径：`RMCLOUD_CONFIG` > `/etc/rmcloud.json` > `$HOME/rmcloud.json`。
///
/// 第三种是**用户态部署**用的（`systemctl --user` + `~/rmcloud.json`，本机就是
/// 这样跑的）；不给这条兜底，工具在服务器上会一直报
/// `读不到 /etc/rmcloud.json`。
fn config_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("RMCLOUD_CONFIG") {
        if !p.is_empty() {
            return p.into();
        }
    }
    let etc = std::path::PathBuf::from("/etc/rmcloud.json");
    if etc.exists() {
        return etc;
    }
    if let Some(home) = std::env::var_os("HOME") {
        let p = std::path::PathBuf::from(home).join("rmcloud.json");
        if p.exists() {
            return p;
        }
    }
    etc
}

/// 只读配置（不走 `Config::load`，那个会 `save` 回文件，工具没权限也不该写）。
fn load_config() -> Result<rmcloud::Config> {
    let path = config_path();
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("读不到 {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} 不是合法 JSON", path.display()))
}

fn print_usage() {
    println!(
        "用法：
  rmcloud-tool render <uuid> <page> [-o out.png]   整页 PNG，打印落盘路径
  rmcloud-tool strokes <uuid> <page>               笔迹几何摘要（JSON）
  rmcloud-tool search \"<query>\"                    库内搜索（JSON）
  rmcloud-tool profile [--all|--learned] [--limit N] 错题档案（表格，默认只看没学会的）

token 读 RMCLOUD_CONFIG > /etc/rmcloud.json > ~/rmcloud.json，请求 {URL}。"
    );
}
