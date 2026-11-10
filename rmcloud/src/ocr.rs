//! 转文字：`POST /api/v1/ocr`。C14。
//!
//! 不跑 OCR 模型：把图交给 `dsh --profile headless`，让它用 `read_image` 读
//! （`docs/CLOUD.md` §3.5）。环境照服务器上的 `ask-ink.sh` 抄：
//! PATH 里要有 node，key 从 `cfg.dsh_env` 那个 env 文件注入。

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;

use crate::state::App;

/// 设备等 300 s（`net.rs`），服务器把结果写缓存，重试直接命中。
///
/// **实测（2026-11-09）**：一整页手写（1404×1588，中文连笔 + 英文 + C++ + 两个
/// 积分式）转写要 **115 秒** —— 原来 120 s 的上限几乎是踩着线过，稍大一点的页
/// 必然超时。所以两头都放宽到 300 s。
const TIMEOUT: Duration = Duration::from_secs(300);
/// 串行跑 dsh。不要用 `App.dsh_lock`——那是 agent 网关的锁。
static OCR_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 设备请求体：只有三个字段（`net.rs:452`）。
#[derive(Debug, Deserialize)]
pub struct OcrReq {
    #[serde(default)]
    pub image_blob: String,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub lang: String,
}

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/api/v1/ocr", post(ocr))
}

async fn ocr(State(app): State<Arc<App>>, headers: HeaderMap, Json(req): Json<OcrReq>) -> Response {
    if !app.authorized_any(bearer(&headers)) {
        return fail(StatusCode::UNAUTHORIZED, "unauthorized", None);
    }
    let sha = req.image_blob.to_ascii_lowercase();
    if !is_sha(&sha) {
        return fail(StatusCode::BAD_REQUEST, "bad_blob", None);
    }
    let src = app.blobs().join(&sha);
    let data = match tokio::fs::read(&src).await {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return fail(StatusCode::NOT_FOUND, "no_blob", None)
        }
        Err(e) => {
            return fail(
                StatusCode::INTERNAL_SERVER_ERROR,
                "io",
                Some(e.to_string()),
            )
        }
    };
    let Some(ext) = image_ext(&data) else {
        return fail(
            StatusCode::UNPROCESSABLE_ENTITY,
            "bad_image",
            Some("expect BMP or PNG".into()),
        );
    };

    let cache = app.cfg.state.join("cache");
    let txt = cache.join(format!("ocr-{sha}.txt"));
    if let Ok(s) = tokio::fs::read_to_string(&txt).await {
        return ok(s);
    }
    // 拷一份到 cache 再交给 dsh（blobs 目录别让外部工具碰）。
    let img = cache.join(format!("ocr-{sha}.{ext}"));
    if let Err(e) = tokio::fs::write(&img, &data).await {
        return fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            "io",
            Some(e.to_string()),
        );
    }

    let _guard = OCR_LOCK.lock().await;
    // 排队期间可能已经有别的请求跑完了同一页。
    if let Ok(s) = tokio::fs::read_to_string(&txt).await {
        return ok(s);
    }
    match run_dsh(&app, &img).await {
        Ok(text) if !text.is_empty() => {
            let _ = tokio::fs::write(&txt, &text).await;
            ok(text)
        }
        Ok(_) => fail(
            StatusCode::BAD_GATEWAY,
            "ocr failed",
            Some("empty result".into()),
        ),
        Err(detail) => fail(StatusCode::BAD_GATEWAY, "ocr failed", Some(detail)),
    }
}

/// 跑一次 dsh，返回去围栏/去首尾空白后的正文。
async fn run_dsh(app: &App, img: &Path) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(&app.cfg.dsh);
    cmd.arg("--profile")
        .arg("headless")
        .arg(prompt(&img.display().to_string()));
    cmd.env("PATH", search_path(&app.cfg.dsh));
    for (k, v) in env_file(&app.cfg.dsh_env) {
        cmd.env(k, v);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    match tokio::time::timeout(TIMEOUT, cmd.output()).await {
        Err(_) => Err("dsh timeout (300s)".into()),
        Ok(Err(e)) => Err(format!("cannot run {}: {e}", app.cfg.dsh)),
        Ok(Ok(o)) if !o.status.success() => Err(format!(
            "dsh exit {}: {}",
            o.status.code().map(|c| c.to_string()).unwrap_or_default(),
            tail(&String::from_utf8_lossy(&o.stderr), 500)
        )),
        Ok(Ok(o)) => Ok(strip_fences(&String::from_utf8_lossy(&o.stdout))),
    }
}

/// 固定提示词（拿不到确切参数，就不把 mode/lang 传进去——识别要确定性）。
pub fn prompt(path: &str) -> String {
    format!(
        "这是一页手写笔记的图片：{path}\n\
         请用 read_image 读它，逐行转成纯文本。\n\
         只输出正文：不要解释、不要 Markdown 代码块围栏、不要臆造看不清的字。\n\
         公式写成 LaTeX 行内形式，表格用空格对齐。"
    )
}

/// 去掉 ``` 围栏和首尾空行。dsh 偶尔会自己加围栏。
pub fn strip_fences(s: &str) -> String {
    let mut lines: Vec<&str> = s.trim().lines().collect();
    if lines
        .first()
        .map(|l| l.trim_start().starts_with("```"))
        .unwrap_or(false)
    {
        lines.remove(0);
    }
    if lines.last().map(|l| l.trim() == "```").unwrap_or(false) {
        lines.pop();
    }
    while lines.first().map(|l| l.trim().is_empty()).unwrap_or(false) {
        lines.remove(0);
    }
    while lines.last().map(|l| l.trim().is_empty()).unwrap_or(false) {
        lines.pop();
    }
    lines.join("\n")
}

/// 只认 BMP 和 PNG 的 magic（设备现在发 BMP，以后发 PNG）。
fn image_ext(d: &[u8]) -> Option<&'static str> {
    if d.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if d.starts_with(b"BM") {
        Some("bmp")
    } else {
        None
    }
}

/// `dsh` 所在目录（nvm 的 node bin）放进 PATH 最前面。
fn search_path(dsh: &str) -> String {
    let cur = std::env::var("PATH").unwrap_or_default();
    match Path::new(dsh).parent() {
        Some(dir) if !dir.as_os_str().is_empty() => {
            format!("{}:{}", dir.display(), cur)
        }
        _ => cur,
    }
}

/// 读 env 文件（`KEY=VALUE` 每行一条，`#` 注释，兼容 `export ` 前缀）。
fn env_file(path: &str) -> Vec<(String, String)> {
    let Ok(s) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    s.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            let l = l.strip_prefix("export ").unwrap_or(l);
            let (k, v) = l.split_once('=')?;
            let k = k.trim();
            if k.is_empty() {
                return None;
            }
            let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
            Some((k.to_string(), v.to_string()))
        })
        .collect()
}

fn is_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn bearer(h: &HeaderMap) -> Option<&str> {
    h.get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
}

fn ok(text: String) -> Response {
    Json(json!({"text": text, "lines": [], "engine": "dsh-read"})).into_response()
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

/// 取字符串末尾最多 `max` 字节（按字符边界对齐）。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_names_path_and_read_image() {
        let p = prompt("/srv/rm/cache/ocr-abc.png");
        assert!(p.contains("read_image"));
        assert!(p.contains("/srv/rm/cache/ocr-abc.png"));
        assert!(p.contains("逐行转成纯文本"));
        assert!(p.contains("不要解释"));
        assert!(p.contains("围栏"));
    }

    #[test]
    fn strip_plain_text() {
        assert_eq!(strip_fences("  line one\nline two \n"), "line one\nline two");
        assert_eq!(strip_fences(""), "");
    }

    #[test]
    fn strip_fenced_block() {
        assert_eq!(strip_fences("```\nline one\nline two\n```"), "line one\nline two");
        assert_eq!(strip_fences("```text\nabc\n```\n"), "abc");
        assert_eq!(strip_fences("```markdown\n\nabc\n\n```"), "abc");
    }

    #[test]
    fn strip_keeps_inner_blank_lines() {
        assert_eq!(strip_fences("a\n\nb\n"), "a\n\nb");
        assert_eq!(strip_fences("\n\nc\n\n\n"), "c");
    }

    #[test]
    fn image_magic() {
        assert_eq!(image_ext(b"BM\x00\x00"), Some("bmp"));
        assert_eq!(image_ext(b"\x89PNG\r\n\x1a\n\x00"), Some("png"));
        assert_eq!(image_ext(b"%PDF"), None);
        assert_eq!(image_ext(b""), None);
    }

    #[test]
    fn env_file_parses_kv() {
        let dir = std::env::temp_dir().join(format!("rmcloud-env-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("web.env");
        std::fs::write(
            &p,
            "# comment\nDEEPSEEK_API_KEY=sk-test\n\nexport FOO=\"bar baz\"\n",
        )
        .unwrap();
        let kv = env_file(&p.to_string_lossy());
        assert_eq!(
            kv,
            vec![
                ("DEEPSEEK_API_KEY".to_string(), "sk-test".to_string()),
                ("FOO".to_string(), "bar baz".to_string()),
            ]
        );
        assert!(env_file("/nonexistent/env").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
