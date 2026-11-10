//! `tex` 模块集成测试：鉴权 / 缺工具链 / 缓存 key / 包裹逻辑 / 有工具链时端到端。
//!
//! 开发机（macOS）不一定有 TeX，所以端到端用例先探测 `pdflatex`，没有就跳过。

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use axum::Router;
use rmcloud::{app_router, App, Config};
use tower::ServiceExt;

const TOKEN: &str = "test-token";

/// 测试里要串行：503 用例会临时改 `PATH`（进程全局），并行会互相干扰。
static LOCK: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 每个用例一个临时 state 目录 + 一个配好 token 的 `App`，Drop 时删干净。
struct Env {
    app: Arc<App>,
    dir: PathBuf,
}

impl Env {
    fn new() -> Env {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "rmcloud-tex-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config {
            state: dir.clone(),
            token: TOKEN.to_string(),
            ..Config::default()
        };
        let app = Arc::new(App::open(cfg).expect("App::open"));
        Env { app, dir }
    }

    fn router(&self) -> Router {
        app_router(self.app.clone())
    }

    fn cache(&self) -> PathBuf {
        self.dir.join("cache/tex")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// 打一次 `oneshot`，回（状态码、响应头、响应体）。
async fn call(router: Router, uri: &str, token: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut req = Request::builder().uri(uri);
    if let Some(t) = token {
        req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    let resp = router
        .oneshot(req.body(Body::empty()).unwrap())
        .await
        .expect("oneshot");
    let status = resp.status();
    let headers = resp.headers().clone();
    let body = axum::body::to_bytes(resp.into_body(), 8 << 20)
        .await
        .expect("body")
        .to_vec();
    (status, headers, body)
}

/// 把公式塞进 query：只留 unreserved 字符，其余按 `%XX` 编码（`+` 必须是 `%2B`）。
fn q(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// 端到端要的工具齐不齐。
fn toolchain() -> bool {
    have("pdflatex") && have("convert") && (have("pdftoppm") || have("dvisvgm"))
}

fn have(prog: &str) -> bool {
    std::process::Command::new(prog)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// 临时把 `PATH` 指向不存在的地方，Drop 时还原（用例 panic 也不污染后面的用例）。
struct BarePath(Option<OsString>);

impl BarePath {
    fn set() -> BarePath {
        let old = std::env::var_os("PATH");
        std::env::set_var("PATH", "/nonexistent-rmcloud-tex-tools");
        BarePath(old)
    }
}

impl Drop for BarePath {
    fn drop(&mut self) {
        match self.0.take() {
            Some(p) => std::env::set_var("PATH", p),
            None => std::env::remove_var("PATH"),
        }
    }
}

#[test]
fn cache_key_stable_and_scale_sensitive() {
    let _g = serial();
    let a = rmcloud::tex::cache_key("\\frac{1}{2}", 2);
    let b = rmcloud::tex::cache_key("\\frac{1}{2}", 2);
    assert_eq!(a, b, "同一输入必须同 key（命中缓存的前提）");
    assert_eq!(a.len(), 64, "sha256 是 64 位 hex");
    assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));

    let c = rmcloud::tex::cache_key("\\frac{1}{2}", 3);
    assert_ne!(a, c, "scale 变了图也变，key 必须不同");

    let d = rmcloud::tex::cache_key("\\frac{1}{3}", 2);
    assert_ne!(a, d, "公式变了 key 必须不同");
}

#[test]
fn wrap_three_forms() {
    let _g = serial();
    // 裸公式：补上 $...$ + \displaystyle
    assert_eq!(
        rmcloud::tex::wrap("\\frac{1}{2}"),
        "$\\displaystyle \\frac{1}{2}$"
    );
    // 自带 $...$：不再套一层
    assert_eq!(
        rmcloud::tex::wrap("$x^2+\\sqrt{y}$"),
        "$\\displaystyle x^2+\\sqrt{y}$"
    );
    // 已经是 display 环境（align* 之类）：原样用，绝不包 $
    assert_eq!(
        rmcloud::tex::wrap("\\begin{align*}a&=b\\end{align*}"),
        "\\begin{align*}a&=b\\end{align*}"
    );
    // 内层环境（pmatrix/aligned/cases）：必须包进 $，否则编译不过
    assert_eq!(
        rmcloud::tex::wrap("\\begin{pmatrix}1&2\\\\3&4\\end{pmatrix}"),
        "$\\displaystyle \\begin{pmatrix}1&2\\\\3&4\\end{pmatrix}$"
    );
    // 前后空白不算内容
    assert_eq!(rmcloud::tex::wrap("  $$ z $$  "), "$\\displaystyle z$");
}

#[tokio::test]
async fn no_token_is_401() {
    let _g = serial();
    let env = Env::new();
    let uri = format!("/api/v1/tex?f={}&scale=2", q("x^2"));
    let (status, _, _) = call(env.router(), &uri, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _, _) = call(env.router(), &uri, Some("wrong-token")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn missing_f_is_400() {
    let _g = serial();
    let env = Env::new();
    let (status, _, body) = call(env.router(), "/api/v1/tex?scale=2", Some(TOKEN)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(String::from_utf8_lossy(&body).contains("missing f"));
}

#[tokio::test]
async fn no_toolchain_is_503() {
    let _g = serial();
    let env = Env::new();
    let uri = format!("/api/v1/tex?f={}&scale=2", q("\\frac{1}{2}"));
    let _bare = BarePath::set();
    let (status, headers, body) = call(env.router(), &uri, Some(TOKEN)).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{}", String::from_utf8_lossy(&body));
    assert_eq!(
        headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
    assert_eq!(v["error"], "tex toolchain missing");
}

#[tokio::test]
async fn bad_latex_is_400() {
    let _g = serial();
    if !have("pdflatex") {
        eprintln!("skip: 本机没有 pdflatex");
        return;
    }
    let env = Env::new();
    // 少一个右花括号，pdflatex 必然报错。
    let uri = format!("/api/v1/tex?f={}&scale=2", q("\\frac{1}{2"));
    let (status, _, body) = call(env.router(), &uri, Some(TOKEN)).await;
    let text = String::from_utf8_lossy(&body);
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    let v: serde_json::Value = serde_json::from_slice(&body).expect("json");
    let msg = v["error"].as_str().expect("error 字段");
    assert!(!msg.is_empty(), "400 要带 pdflatex 的日志");
    assert!(msg.lines().count() <= 20, "只回最后 20 行");
    assert_eq!(
        env.cache().read_dir().expect("cache dir").count(),
        0,
        "编译失败不该落缓存"
    );
}

#[tokio::test]
async fn math_env_fragment_ok() {
    let _g = serial();
    if !toolchain() {
        eprintln!("skip: 本机没有 tex 工具链（pdflatex/convert）");
        return;
    }
    let env = Env::new();
    // 内层环境：wrap 会补 `$...$`，不然 pdflatex 直接 "Missing $ inserted"。
    let uri = format!("/api/v1/tex?f={}&scale=2", q("\\begin{pmatrix}1&2\\\\3&4\\end{pmatrix}"));
    let (status, _, body) = call(env.router(), &uri, Some(TOKEN)).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(&body[..8], b"\x89PNG\r\n\x1a\n");
}

#[tokio::test]
async fn render_png_cached() {
    let _g = serial();
    if !toolchain() {
        eprintln!("skip: 本机没有 tex 工具链（pdflatex/convert）");
        return;
    }
    let env = Env::new();
    let uri = format!("/api/v1/tex?f={}&scale=2", q("x^2+\\sqrt{y}"));

    let t0 = std::time::Instant::now();
    let (status, headers, body) = call(env.router(), &uri, Some(TOKEN)).await;
    let render_ms = t0.elapsed();
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));

    // 头部约定
    assert_eq!(
        headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()),
        Some("image/png")
    );
    assert_eq!(
        headers.get(header::CACHE_CONTROL).and_then(|v| v.to_str().ok()),
        Some("public, max-age=604800")
    );
    let cl: usize = headers
        .get(header::CONTENT_LENGTH)
        .expect("必须带 Content-Length")
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(cl, body.len(), "Content-Length 和 body 不等长");

    // PNG 头：签名 + IHDR + 8 位灰度 + 宽高 > 0
    assert_eq!(&body[..8], b"\x89PNG\r\n\x1a\n", "不是 PNG 签名");
    assert_eq!(&body[12..16], b"IHDR");
    let w = u32::from_be_bytes(body[16..20].try_into().unwrap());
    let h = u32::from_be_bytes(body[20..24].try_into().unwrap());
    assert!(w > 0 && h > 0, "IHDR 宽高要 > 0，实际 {w}x{h}");
    assert_eq!(body[24], 8, "要 8 位色深");
    assert_eq!(body[25], 0, "要灰度（color type 0）");
    assert!(w <= 1200, "宽度上限 1200 px，实际 {w}");

    // 缓存落盘 + 命中直接吐文件
    let key = rmcloud::tex::cache_key("x^2+\\sqrt{y}", 2);
    let cached = env.cache().join(format!("{key}.png"));
    assert!(cached.is_file(), "缓存文件没落盘：{}", cached.display());
    assert_eq!(std::fs::read(&cached).unwrap(), body, "缓存内容和响应不一致");

    let t1 = std::time::Instant::now();
    let (status, _, again) = call(env.router(), &uri, Some(TOKEN)).await;
    let hit_ms = t1.elapsed();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again, body, "缓存命中要回同一张图");
    assert!(
        hit_ms < render_ms,
        "缓存命中（{hit_ms:?}）应快于首次渲染（{render_ms:?}）"
    );
    eprintln!("首次渲染 {render_ms:?}，缓存命中 {hit_ms:?}，{w}x{h}");
}
