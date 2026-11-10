//! LaTeX → 8 位灰度 PNG，带缓存。C6。
//!
//! `GET /api/v1/tex?f=<公式片段>&scale=2`（要 `Authorization: Bearer <token>`）
//!
//! 管线：`m.tex`（standalone）→ `pdflatex` → 中间图 → `convert` 出灰度 PNG。
//! 中间图优先 `pdftoppm`（poppler，最稳）；没有就 `dvisvgm --pdf` 出 SVG 再让
//! convert 栅格化。缓存落 `<state>/cache/tex/<sha256(f|scale)>.png`，命中直接读文件。
//!
//! 失败分三档：工具不在 → 503 `tex toolchain missing`；编译/转换出错 → 400
//! `{"error": "<最后 20 行输出>"}`；超时 → 504（子进程 kill 掉，临时目录随之删）。

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tokio::time::timeout;

use crate::state::App;

/// 单条外部命令的上限。pdflatex 冷启动 + 字体加载几百 ms，20 s 足够。
const LIMIT: Duration = Duration::from_secs(20);
/// 单张 PNG 宽度上限：设备屏 1404 px，别把 4000 px 宽的图塞过去。
const MAX_W: u32 = 1200;
/// 默认 2 倍宽，供 274 ppi 屏直接用。
const DEFAULT_SCALE: u32 = 2;
/// 临时目录/文件名的序号，防同进程并发撞名。
static SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Deserialize)]
struct TexQuery {
    f: Option<String>,
    scale: Option<String>,
}

/// 缓存 key：`sha256(f + "|" + scale)`。scale 变了图就不同，必须进 key。
pub fn cache_key(f: &str, scale: u32) -> String {
    let mut h = Sha256::new();
    h.update(format!("{f}|{scale}").as_bytes());
    hex::encode(h.finalize())
}

/// 这些环境自己就是 display math，套 `$` 会报 "Bad math environment delimiter"。
const DISPLAY_ENVS: [&str; 11] = [
    "align", "align*", "equation", "equation*", "gather", "gather*", "multline", "multline*",
    "eqnarray", "eqnarray*", "displaymath",
];

/// 公式片段 → 文档正文：裸公式和自带 `$...$` 的都给包成 `$\displaystyle ...$`；
/// display 环境（`align*` 等）整段照用不套 `$`；`pmatrix`/`aligned`/`cases` 这类
/// 内层环境必须包进 `$...$` 才能编译。
pub fn wrap(src: &str) -> String {
    let s = src.trim();
    if let Some(env) = s.strip_prefix("\\begin{").and_then(|r| r.split('}').next()) {
        if DISPLAY_ENVS.contains(&env) {
            return s.to_string();
        }
    }
    let inner = s.trim_start_matches('$').trim_end_matches('$').trim();
    format!("$\\displaystyle {inner}$")
}

fn document(body: &str) -> String {
    // `tikz` / `pgfplots` 让模型能画**图**（示意图、函数曲线），设备那边走"图 → 墨迹"
    // 同一条路；不加这两个包的话 `\begin{tikzpicture}` 直接编译失败。
    format!(
        "\\documentclass[border=2pt]{{standalone}}\n\
         \\usepackage{{amsmath,amssymb}}\n\
         \\usepackage{{tikz}}\n\
         \\usepackage{{pgfplots}}\n\
         \\pgfplotsset{{compat=1.18}}\n\
         \\usetikzlibrary{{arrows.meta,positioning,calc,shapes.geometric,patterns}}\n\
         \\begin{{document}}\n\
         {{\\large {body}}}\n\
         \\end{{document}}\n"
    )
}

async fn tex(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<TexQuery>) -> Response {
    let auth = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    if !app.authorized_any(auth) {
        return err(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    let Some(f) = q.f.filter(|s| !s.trim().is_empty()) else {
        return err(StatusCode::BAD_REQUEST, "missing f");
    };
    // 1..=4：太小看不清，太大出巨图（1200 px 上限会兜住，但别让它白算）。
    let scale = q
        .scale
        .as_deref()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(DEFAULT_SCALE)
        .clamp(1, 4);
    match render(&app, &f, scale).await {
        Ok(png) => png_response(png),
        Err(fail) => fail.response(),
    }
}

/// 先看缓存，没有就跑管线并把结果落盘。
async fn render(app: &App, f: &str, scale: u32) -> Result<Vec<u8>, Fail> {
    let dir = app.cfg.state.join("cache/tex");
    let path = dir.join(format!("{}.png", cache_key(f, scale)));
    if let Ok(png) = tokio::fs::read(&path).await {
        return Ok(png);
    }
    let png = build(f, scale).await?;
    // 先写临时名再 rename：并发或崩溃都不会留半个 PNG 当缓存。
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".tmp-{}-{seq}", std::process::id()));
    if tokio::fs::write(&tmp, &png).await.is_ok() {
        let _ = tokio::fs::rename(&tmp, &path).await;
    }
    Ok(png)
}

/// pdflatex → 中间图 → convert。临时目录靠 [`Tmp`] 的 Drop 删掉（超时/报错也一样）。
async fn build(f: &str, scale: u32) -> Result<Vec<u8>, Fail> {
    let tmp = Tmp::new()?;
    let dir = tmp.path();
    tokio::fs::write(dir.join("m.tex"), document(&wrap(f)))
        .await
        .map_err(Fail::Io)?;

    let out = run("pdflatex", &["-interaction=nonstopmode", "-halt-on-error", "m.tex"], dir).await?;
    if !out.status.success() {
        return Err(Fail::Failed(out));
    }

    // 中间图：pdftoppm 优先；没有就 dvisvgm 出 SVG，让 convert 顺手栅格化。
    let from_svg = match run("pdftoppm", &["-png", "-r", "300", "-singlefile", "m.pdf", "m"], dir).await {
        Ok(out) if out.status.success() => false,
        Ok(out) => return Err(Fail::Failed(out)),
        Err(Fail::Missing(_)) => {
            let out = run(
                "dvisvgm",
                &["--pdf", "--no-fonts", "--font-format=woff", "m.pdf", "-o", "m.svg"],
                dir,
            )
            .await?;
            if !out.status.success() {
                return Err(Fail::Failed(out));
            }
            true
        }
        Err(e) => return Err(e),
    };

    let mut args: Vec<String> = Vec::new();
    if from_svg {
        // 读 SVG 时按 300 dpi 栅格化，和 pdftoppm -r 300 对齐。
        args.push("-density".into());
        args.push("300".into());
    }
    for a in ["-background", "white", "-alpha", "remove", "-colorspace", "Gray", "-depth", "8"] {
        args.push(a.into());
    }
    args.push("-resize".into());
    args.push(format!("{}%", scale * 100));
    args.push("-resize".into());
    args.push(format!("{MAX_W}x>")); // `>` = 只缩不放
    args.push(if from_svg { "m.svg" } else { "m.png" }.into());
    args.push("out.png".into());

    let out = run("convert", &args, dir).await?;
    if !out.status.success() {
        return Err(Fail::Failed(out));
    }
    let png = tokio::fs::read(dir.join("out.png")).await.map_err(Fail::Io)?;
    if !png.starts_with(&[0x89, b'P', b'N', b'G']) {
        return Err(Fail::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "convert 没产出 PNG",
        )));
    }
    Ok(png)
}

/// 跑一个外部命令：`cwd` 里跑，带超时；子进程随 [`Command::kill_on_drop`] 一起死。
async fn run<S: AsRef<OsStr>>(prog: &str, args: &[S], cwd: &Path) -> Result<Output, Fail> {
    let mut cmd = Command::new(prog);
    cmd.args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Fail::Missing(prog.into()))
        }
        Err(e) => return Err(Fail::Io(e)),
    };
    match timeout(LIMIT, child.wait_with_output()).await {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(e)) => Err(Fail::Io(e)),
        Err(_) => Err(Fail::Timeout(prog.into())),
    }
}

/// 管线失败。
enum Fail {
    /// 可执行文件不在（`PATH` 里找不到）。
    Missing(String),
    /// 超过 `LIMIT` 没结束，子进程已经 kill 掉。
    Timeout(String),
    /// 进程跑完了但退出码非 0，`Output` 里是要回给设备的日志。
    Failed(Output),
    Io(std::io::Error),
}

impl Fail {
    fn response(self) -> Response {
        match self {
            Fail::Missing(prog) => {
                eprintln!("[tex] 找不到可执行文件：{prog}");
                err(StatusCode::SERVICE_UNAVAILABLE, "tex toolchain missing")
            }
            Fail::Timeout(prog) => err(StatusCode::GATEWAY_TIMEOUT, &format!("{prog} timeout")),
            Fail::Failed(out) => err(StatusCode::BAD_REQUEST, &tail(&out)),
            Fail::Io(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    }
}

/// pdflatex 最后 20 行输出（stdout 为主，空则用 stderr），给设备上的人看。
fn tail(out: &Output) -> String {
    let raw = if out.stdout.is_empty() {
        String::from_utf8_lossy(&out.stderr).into_owned()
    } else {
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let lines: Vec<&str> = raw.lines().collect();
    if lines.is_empty() {
        return format!("{}", out.status);
    }
    lines[lines.len().saturating_sub(20)..].join("\n")
}

fn err(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

fn png_response(png: Vec<u8>) -> Response {
    let len = png.len().to_string();
    (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "image/png".to_string()),
            (header::CACHE_CONTROL, "public, max-age=604800".to_string()),
            (header::CONTENT_LENGTH, len),
        ],
        png,
    )
        .into_response()
}

/// 一次性的临时目录，离开作用域就删。
struct Tmp(PathBuf);

impl Tmp {
    fn new() -> Result<Tmp, Fail> {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!("rmcloud-tex-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&p).map_err(Fail::Io)?;
        Ok(Tmp(p))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/api/v1/tex", get(tex))
}
