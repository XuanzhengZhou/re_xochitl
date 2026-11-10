//! 发信：`POST /api/v1/mail`。C15。
//!
//! 设备端 `src/net.rs::send_mail_multi` 是契约权威：附件先 PUT 成 blob，这里只收引用。
//! 提交交给服务器上现成的 `curl`（不引 SMTP 库）；MIME 全部自己拼，不引编码库。

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;

use crate::config::Smtp;
use crate::state::App;

/// 设备侧超时 60 s（`net.rs:497`），服务端不拖过它。
const TIMEOUT: Duration = Duration::from_secs(60);
/// `docs/CLOUD.md` §3.2(8)：最多 5 个附件、合计 ≤ 20 MB。
const MAX_ATTS: usize = 5;
const MAX_BYTES: u64 = 20 * 1024 * 1024;
/// MIME 内容按 76 列折行（RFC 2045）。
const LINE: usize = 76;
/// curl 路径可换（测试用；生产不设就是 PATH 里的 `curl`）。
const CURL_ENV: &str = "RMCLOUD_CURL";

/// 设备请求体，逐字照 `net.rs:493`。
#[derive(Debug, Deserialize)]
pub struct MailReq {
    #[serde(default)]
    pub to: Vec<String>,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub attachments: Vec<AttRef>,
}

#[derive(Debug, Deserialize)]
pub struct AttRef {
    #[serde(default)]
    pub blob: String,
    #[serde(default)]
    pub filename: String,
    #[serde(default)]
    pub mime: String,
}

/// 一个附件（已从 blob 读出来）。
pub struct Part {
    pub filename: String,
    pub mime: String,
    pub data: Vec<u8>,
}

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/api/v1/mail", post(mail))
}

async fn mail(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(req): Json<MailReq>,
) -> Response {
    if !app.authorized_any(bearer(&headers)) {
        return fail(StatusCode::UNAUTHORIZED, "unauthorized", None);
    }
    let Some(smtp) = app.cfg.smtp.clone() else {
        return fail(StatusCode::SERVICE_UNAVAILABLE, "mail not configured", None);
    };
    if req.to.is_empty() || !req.to.iter().all(|a| valid_addr(a)) {
        return fail(StatusCode::BAD_REQUEST, "bad_address", None);
    }
    let parts = match load_parts(&app, &req.attachments).await {
        Ok(p) => p,
        Err(r) => return r,
    };

    match send(&smtp, &app.cfg.state, &req.to, &req.subject, &req.body, &parts).await {
        Ok(id) => Json(json!({ "ok": true, "id": id })).into_response(),
        Err(SendError::Io(e)) => fail(StatusCode::INTERNAL_SERVER_ERROR, "io", Some(e)),
        Err(SendError::Curl(detail)) => fail(StatusCode::BAD_GATEWAY, "smtp failed", Some(detail)),
    }
}

/// 发信失败：`Io` 是本地落 `.eml` 失败（500），`Curl` 是 curl 起不来/超时/非 0（502）。
#[derive(Debug)]
pub enum SendError {
    Io(String),
    Curl(String),
}

/// 真正发一封（拼 MIME → 落临时 `.eml` → 跑 curl），返回 Message-ID。
///
/// `state` 是状态目录（`.eml` 落在 `<state>/cache/`，发完就删）。地址合法性由调用方先验。
/// `POST /api/v1/mail` 和收信模块的回信都走这里。
pub async fn send(
    smtp: &Smtp,
    state: &Path,
    to: &[String],
    subject: &str,
    body: &str,
    parts: &[Part],
) -> Result<String, SendError> {
    let id = msg_id();
    let date = rfc2822_date(unix_now());
    let raw = mime(&smtp.user, to, subject, body, parts, &id, &date);

    // `.eml` 落在 cache 里（`/srv/rm` 是 0700）；发完就删。
    let eml = state
        .join("cache")
        .join(format!("mail-{}.eml", boundary_token(&id)));
    if let Err(e) = tokio::fs::write(&eml, &raw).await {
        return Err(SendError::Io(e.to_string()));
    }

    let bin = std::env::var(CURL_ENV).unwrap_or_else(|_| "curl".into());
    let mut cmd = tokio::process::Command::new(&bin);
    cmd.arg("-sS")
        .arg("--url")
        .arg(format!("smtps://{}:{}", smtp.host, smtp.port))
        .arg("--user")
        .arg(format!("{}:{}", smtp.user, smtp.pass))
        .arg("--mail-from")
        .arg(&smtp.user);
    for rcpt in to {
        cmd.arg("--mail-rcpt").arg(rcpt);
    }
    cmd.arg("--upload-file")
        .arg(&eml)
        .stdin(Stdio::null())
        .kill_on_drop(true);

    let out = tokio::time::timeout(TIMEOUT, cmd.output()).await;
    let _ = tokio::fs::remove_file(&eml).await;
    match out {
        Err(_) => Err(SendError::Curl("curl timeout (60s)".into())),
        Ok(Err(e)) => Err(SendError::Curl(format!("cannot run {bin}: {e}"))),
        Ok(Ok(o)) if o.status.success() => Ok(id),
        Ok(Ok(o)) => Err(SendError::Curl(tail(
            &String::from_utf8_lossy(&o.stderr),
            2048,
        ))),
    }
}

/// 读附件。顺序有意：先查数量再读盘，别为了 6 个附件读 100 MB。
async fn load_parts(app: &App, refs: &[AttRef]) -> Result<Vec<Part>, Response> {
    if refs.len() > MAX_ATTS {
        return Err(fail(StatusCode::PAYLOAD_TOO_LARGE, "too_large", None));
    }
    let mut parts = Vec::with_capacity(refs.len());
    let mut total = 0u64;
    for a in refs {
        let blob = a.blob.to_ascii_lowercase();
        if !is_sha(&blob) {
            return Err(fail(StatusCode::BAD_REQUEST, "bad_blob", None));
        }
        let data = match tokio::fs::read(app.blobs().join(&blob)).await {
            Ok(d) => d,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(fail(StatusCode::NOT_FOUND, "no_blob", None))
            }
            Err(e) => {
                return Err(fail(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "io",
                    Some(e.to_string()),
                ))
            }
        };
        total += data.len() as u64;
        if total > MAX_BYTES {
            return Err(fail(StatusCode::PAYLOAD_TOO_LARGE, "too_large", None));
        }
        parts.push(Part {
            filename: safe_filename(&a.filename),
            mime: safe_mime(&a.mime),
            data,
        });
    }
    Ok(parts)
}

/// 拼整封 MIME。`id`/`date` 由调用方给，便于单测。
pub fn mime(
    from: &str,
    to: &[String],
    subject: &str,
    body: &str,
    atts: &[Part],
    id: &str,
    date: &str,
) -> Vec<u8> {
    let boundary = format!("----rmcloud-{}", boundary_token(id));
    let mut m = String::new();
    m.push_str(&format!("Date: {date}\r\n"));
    m.push_str(&format!("From: {}\r\n", header_safe(from)));
    m.push_str(&format!("To: {}\r\n", to.join(", ")));
    m.push_str(&format!("Subject: {}\r\n", encode_header(subject)));
    m.push_str(&format!("Message-ID: {id}\r\n"));
    m.push_str("MIME-Version: 1.0\r\n");
    m.push_str(&format!(
        "Content-Type: multipart/mixed; boundary=\"{boundary}\"\r\n"
    ));
    m.push_str("\r\n");

    // 正文：固定 base64 + 76 列折行（中文不会变乱码，也不受 8bit 限制）。
    m.push_str(&format!("--{boundary}\r\n"));
    m.push_str("Content-Type: text/plain; charset=utf-8\r\n");
    m.push_str("Content-Transfer-Encoding: base64\r\n");
    m.push_str("\r\n");
    m.push_str(&fold(&b64(body.as_bytes())));
    m.push_str("\r\n");

    for a in atts {
        m.push_str(&format!("--{boundary}\r\n"));
        m.push_str(&format!("Content-Type: {}; name=\"{}\"\r\n", a.mime, a.filename));
        m.push_str("Content-Transfer-Encoding: base64\r\n");
        m.push_str(&format!(
            "Content-Disposition: attachment; filename=\"{}\"\r\n",
            a.filename
        ));
        m.push_str("\r\n");
        m.push_str(&fold(&b64(&a.data)));
        m.push_str("\r\n");
    }
    m.push_str(&format!("--{boundary}--\r\n"));
    m.into_bytes()
}

/// 标准 base64（带 `=` 补齐），不折行。
pub fn b64(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for c in data.chunks(3) {
        let n = ((c[0] as u32) << 16)
            | ((*c.get(1).unwrap_or(&0) as u32) << 8)
            | (*c.get(2).unwrap_or(&0) as u32);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// 按 76 列折成 CRLF 行（b64 全 ASCII，字符数就是字节数）。
fn fold(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / LINE + 2);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && i % LINE == 0 {
            out.push_str("\r\n");
        }
        out.push(c);
    }
    out
}

/// 非 ASCII 标题用 RFC 2047 编码字，纯 ASCII 原样。
fn encode_header(s: &str) -> String {
    let clean = header_safe(s);
    if clean.is_ascii() {
        clean
    } else {
        format!("=?UTF-8?B?{}?=", b64(clean.as_bytes()))
    }
}

/// 头字段里绝不能出现 CR/LF（防头注入），控制字符一并去掉。
fn header_safe(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '\r' && *c != '\n' && !c.is_control())
        .collect()
}

/// 附件名取 basename（`/`、`\` 都不留），并去掉引号/控制字符。
pub fn safe_filename(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("").trim();
    let s: String = base
        .chars()
        .map(|c| if c == '"' || c.is_control() { '_' } else { c })
        .collect();
    if s.is_empty() {
        "attachment".to_string()
    } else {
        s
    }
}

/// `mime` 只允许一个裸类型，别让头里多出参数/换行。
fn safe_mime(m: &str) -> String {
    let t = m.split(';').next().unwrap_or("").trim();
    if t.is_empty() || !t.contains('/') {
        "application/octet-stream".into()
    } else {
        t.chars()
            .filter(|c| c.is_ascii() && !c.is_ascii_control() && *c != '"')
            .collect()
    }
}

pub fn valid_addr(a: &str) -> bool {
    let a = a.trim();
    if a.is_empty() || a.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    if a.contains(['<', '>', ',', ';', '"']) {
        return false;
    }
    match a.split_once('@') {
        Some((l, d)) => !l.is_empty() && !d.is_empty(),
        None => false,
    }
}

fn is_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn bearer(h: &HeaderMap) -> Option<&str> {
    h.get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 一封一个 Message-ID：`<秒.纳秒.pid.序号@rmcloud.local>`。也当 `.eml` 文件名。
fn msg_id() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!(
        "<{}.{}.{}.{}@rmcloud.local>",
        now.as_secs(),
        now.subsec_nanos(),
        std::process::id(),
        n
    )
}

/// Message-ID → boundary/文件名可用的 token（只留字母数字和 `.-`）。
fn boundary_token(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// RFC 2822 日期（UTC）。不带本地时区，免得依赖 TZ 环境。
fn rfc2822_date(secs: i64) -> String {
    const WD: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MON: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let wd = WD[days.rem_euclid(7) as usize];
    format!(
        "{wd}, {d:02} {} {y} {:02}:{:02}:{:02} +0000",
        MON[m as usize - 1],
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// RFC 3339（UTC）：`2026-11-09T10:00:00Z`。收信状态里的 `last_run` 用它。
pub fn iso_utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// 自 1970-01-01 起的天数 → (年, 月, 日)。Howard Hinnant 的 civil_from_days。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 取字符串末尾最多 `max` 字节（按字符边界对齐，不会把 UTF-8 切坏）。
fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut i = s.len() - max;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    s[i..].to_string()
}

fn fail(status: StatusCode, error: &str, detail: Option<String>) -> Response {
    let mut body = json!({"error": error});
    if let Some(d) = detail {
        if let Some(o) = body.as_object_mut() {
            o.insert("detail".into(), json!(d));
        }
    }
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64_empty() {
        assert_eq!(b64(b""), "");
    }

    #[test]
    fn b64_one_byte() {
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(&[0]), "AA==");
        assert_eq!(b64(&[0xff]), "/w==");
    }

    #[test]
    fn b64_three_bytes() {
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"abc"), "YWJj");
        assert_eq!(b64(&[0, 0, 0]), "AAAA");
    }

    #[test]
    fn b64_chinese_utf8() {
        // "你好" = e4 bd a0 e5 a5 bd
        assert_eq!(b64("你好".as_bytes()), "5L2g5aW9");
        assert_eq!(b64("中文".as_bytes()), "5Lit5paH");
    }

    #[test]
    fn b64_wraps_at_76() {
        let data = vec![b'a'; 100];
        let folded = fold(&b64(&data));
        assert_eq!(b64(&data).len(), 136);
        for line in folded.split("\r\n") {
            assert!(line.len() <= LINE, "line too long: {}", line.len());
        }
        assert_eq!(folded.split("\r\n").count(), 2); // 76 + 60
    }

    #[test]
    fn date_is_rfc2822() {
        assert_eq!(rfc2822_date(0), "Thu, 01 Jan 1970 00:00:00 +0000");
        assert_eq!(
            rfc2822_date(1_700_000_000),
            "Tue, 14 Nov 2023 22:13:20 +0000"
        );
        assert_eq!(
            rfc2822_date(1_700_000_000 + 86_400),
            "Wed, 15 Nov 2023 22:13:20 +0000"
        );
    }

    #[test]
    fn filename_drops_path_separators() {
        assert_eq!(safe_filename("../../etc/passwd"), "passwd");
        assert_eq!(safe_filename("a\\b\\c.pdf"), "c.pdf");
        assert_eq!(safe_filename("page-3.pdf"), "page-3.pdf");
        assert_eq!(safe_filename(""), "attachment");
        assert_eq!(safe_filename("x\"y.pdf"), "x_y.pdf");
    }

    #[test]
    fn mime_has_headers_parts_and_crlf() {
        let att = Part {
            filename: "page-3.pdf".into(),
            mime: "application/pdf".into(),
            data: b"%PDF-1.4".to_vec(),
        };
        let raw = mime(
            "me@example.com",
            &["to@example.com".to_string()],
            "测试笔记",
            "第一行\n第二行",
            &[att],
            "<123.456@rmcloud.local>",
            "Sun, 09 Nov 2026 10:00:00 +0000",
        );
        let s = String::from_utf8(raw).unwrap();
        assert!(s.starts_with("Date: Sun, 09 Nov 2026 10:00:00 +0000\r\n"));
        assert!(s.contains("From: me@example.com\r\n"));
        assert!(s.contains("To: to@example.com\r\n"));
        assert!(s.contains("Message-ID: <123.456@rmcloud.local>\r\n"));
        assert!(s.contains("MIME-Version: 1.0\r\n"));
        assert!(s.contains("Content-Type: multipart/mixed; boundary=\"----rmcloud-"));
        assert!(s.contains("Content-Type: text/plain; charset=utf-8\r\n"));
        assert!(s.contains("Content-Transfer-Encoding: base64\r\n"));
        // 正文 base64（"第一行\n第二行"）
        assert!(s.contains(&b64("第一行\n第二行".as_bytes())));
        // 附件
        assert!(s.contains("Content-Disposition: attachment; filename=\"page-3.pdf\"\r\n"));
        assert!(s.contains(&b64(b"%PDF-1.4")));
        assert!(s.trim_end().ends_with("--"));
        // 只有 CRLF，没有裸 LF（SMTP DATA 不能收裸 LF）
        let b = s.as_bytes();
        for (i, c) in b.iter().enumerate() {
            if *c == b'\n' {
                assert!(i > 0 && b[i - 1] == b'\r', "bare LF at {i}");
            }
        }
    }

    #[test]
    fn mime_encodes_non_ascii_subject_only() {
        assert_eq!(encode_header("Notes"), "Notes");
        assert_eq!(encode_header("你好"), "=?UTF-8?B?5L2g5aW9?=");
        assert_eq!(encode_header("a\r\nBcc: x"), "aBcc: x");
    }

    #[test]
    fn addr_and_sha_checks() {
        assert!(valid_addr("a@b.c"));
        assert!(!valid_addr(""));
        assert!(!valid_addr("a@"));
        assert!(!valid_addr("a b@c"));
        assert!(!valid_addr("a@b.c\r\nBcc: x"));
        assert!(is_sha(&"a".repeat(64)));
        assert!(!is_sha("../etc/passwd"));
        assert!(!is_sha(&"a".repeat(63)));
    }

    #[test]
    fn tail_is_utf8_safe() {
        let s = "一二三四五";
        let t = tail(s, 5); // 从中间切也不 panic
        assert!(s.ends_with(&t));
        assert_eq!(tail("abc", 10), "abc");
    }
}
