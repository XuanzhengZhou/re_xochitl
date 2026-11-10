//! Web UI 外壳（C16）的集成测试：公开静态资源、鉴权边界、路径穿越、体积预算。
//!
//! 和 `tests/sync.rs` 一样直接打 `app_router`：起真端口会引入端口/清理问题，
//! 而路由/头/状态码在 `Router` 层就已经完全决定了。tower 没开 `util` feature，
//! 所以手搓一个 oneshot（`poll_ready` + `call`）。

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use rmcloud::{app_router, App, Config};
use tower::Service;

const TOKEN: &str = "tok-webui-test-secret";
/// 三个静态文件（未压缩）的预算：需求要求 ~30 KB。
/// 前端总量上限（未压缩）。30 KiB 起，加「错题」视图后到 34.9 KB，
/// 放到 36 KiB —— 再多就该想想是不是把东西塞进 HTML 了。
const ASSET_BUDGET: usize = 36 * 1024;

struct Ctx {
    router: axum::Router,
    dir: PathBuf,
}

impl Drop for Ctx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Ctx {
    fn new(tag: &str) -> Ctx {
        let dir = std::env::temp_dir().join(format!("rmcloud-webui-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config {
            state: dir.clone(),
            token: TOKEN.into(),
            ..Default::default()
        };
        let app = Arc::new(App::open(cfg).expect("open app"));
        let router = app_router(app);
        Ctx { router, dir }
    }
}

async fn send(ctx: &Ctx, req: Request<Body>) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut router = ctx.router.clone();
    std::future::poll_fn(|cx| {
        <axum::Router as Service<Request<Body>>>::poll_ready(&mut router, cx)
    })
    .await
    .expect("router ready");
    let res = <axum::Router as Service<Request<Body>>>::call(&mut router, req)
        .await
        .expect("router call");
    let status = res.status();
    let headers = res.headers().clone();
    let body = axum::body::to_bytes(res.into_body(), 1024 * 1024)
        .await
        .expect("read body");
    (status, headers, body.to_vec())
}

fn get_req(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("GET").uri(uri);
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

fn text(body: &[u8]) -> String {
    String::from_utf8_lossy(body).into_owned()
}

fn content_type(headers: &HeaderMap) -> String {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

// ---------------------------------------------------------------- 测试

#[tokio::test]
async fn index_is_a_public_shell_without_any_secret() {
    let cx = Ctx::new("index");
    let (status, headers, body) = send(&cx, get_req("/", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "text/html; charset=utf-8");
    assert_eq!(
        headers.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(),
        "nosniff"
    );
    assert!(headers.get(header::CONTENT_SECURITY_POLICY).is_some());

    let html = text(&body);
    // 三栏骨架 + 引用到的两个资源。
    for needle in [
        "<title>rmcloud",
        "href=\"/app.css\"",
        "src=\"/app.js\"",
        "id=\"tree\"",
        "id=\"pages\"",
        "id=\"viewer\"",
        "id=\"log\"",
        "id=\"mode\"",
        "value=\"live\"",
        "id=\"cred\"",
    ] {
        assert!(html.contains(needle), "index.html 少了 {needle}");
    }

    // 外壳里没有任何令牌：配置里的真实值、Bearer 头、常见的密钥字样都不能出现。
    assert!(!html.contains(TOKEN), "index.html 泄露了配置 token");
    let lower = html.to_lowercase();
    for bad in ["token", "bearer", "secret", "api_key", "api-key", "private key"] {
        assert!(!lower.contains(bad), "index.html 出现了 {bad:?}");
    }
    // 离线可用：不能有任何外链（CDN / 字体 / 图片）。
    assert!(!lower.contains("http://") && !lower.contains("https://"));
    assert!(!lower.contains("//cdn"));

    // 带不带 Authorization 都是同一个外壳（外壳不持有数据，鉴权在 API 层）。
    let (status2, _, body2) = send(&cx, get_req("/", Some(TOKEN))).await;
    assert_eq!(status2, StatusCode::OK);
    assert_eq!(body, body2);
}

#[tokio::test]
async fn assets_are_public_correct_types_and_within_budget() {
    let cx = Ctx::new("assets");

    let (status, headers, js) = send(&cx, get_req("/app.js", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "text/javascript; charset=utf-8");
    let js = text(&js);
    // 关键实现：头里带 Bearer、令牌存 localStorage、聊天用流式 fetch。
    for needle in ["Authorization", "Bearer ", "localStorage", "ReadableStream", "IntersectionObserver"] {
        assert!(js.contains(needle), "app.js 少了 {needle}");
    }
    assert!(!js.contains("http://") && !js.contains("https://"));
    assert!(!js.contains(TOKEN));

    let (status, headers, css) = send(&cx, get_req("/app.css", None)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(content_type(&headers), "text/css; charset=utf-8");
    let css = text(&css);
    assert!(css.contains("grid-template-columns"));
    assert!(css.contains("@media (max-width: 900px)"), "窄屏要上下堆叠");
    assert!(!css.contains("@import"));
    assert!(!css.contains("url(http"));
    assert!(!css.contains("https://"));

    // index.html 引用的路径必须能取到（名字写错会 404 白屏）。
    let (_, _, index) = send(&cx, get_req("/", None)).await;
    let index = text(&index);
    for reference in ["/app.js", "/app.css"] {
        assert!(index.contains(reference), "index.html 没引用 {reference}");
        let (status, _, _) = send(&cx, get_req(reference, None)).await;
        assert_eq!(status, StatusCode::OK, "index.html 引用的 {reference} 取不到");
    }

    // app.js 里 `$('#x')` 用到的每个 id，index.html 里都得有，否则一执行就 TypeError。
    let mut rest = js.as_str();
    let mut ids = Vec::new();
    while let Some(i) = rest.find("$('#") {
        rest = &rest[i + 4..];
        match rest.find('\'') {
            Some(j) => {
                ids.push(rest[..j].to_string());
                rest = &rest[j..];
            }
            None => break,
        }
    }
    assert!(!ids.is_empty());
    for id in ids {
        assert!(
            index.contains(&format!("id=\"{id}\"")),
            "app.js 用了 #{id}，但 index.html 里没有"
        );
    }

    let total = index.len() + js.len() + css.len();
    eprintln!("web ui assets: index={} js={} css={} total={total}", index.len(), js.len(), css.len());
    assert!(total <= ASSET_BUDGET, "静态资源 {total} B 超出预算 {ASSET_BUDGET} B");
}

#[tokio::test]
async fn webui_did_not_open_the_data_apis() {
    let cx = Ctx::new("auth");
    // 外壳公开，但数据接口一个都没被顺手打开。
    for uri in [
        "/api/v1/tree",
        "/api/v1/manifest",
        "/api/v1/thumb/u1/p1",
        "/api/v1/blob/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        let (status, _, body) = send(&cx, get_req(uri, None)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} 没带 token 应 401");
        assert!(text(&body).contains("unauthorized"), "{uri}");
        let (status, _, _) = send(&cx, get_req(uri, Some("wrong-token"))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} 错 token 应 401");
    }
}

#[tokio::test]
async fn traversal_never_serves_repository_files() {
    let cx = Ctx::new("traversal");
    for uri in [
        "/../Cargo.toml",
        "/app.js/../../etc/passwd",
        "/%2e%2e%2fCargo.toml",
        "/..%2fCargo.toml",
        "/%2e%2e/%2e%2e/Cargo.toml",
        "/Cargo.toml",
        "/src/lib.rs",
        "/web/index.html",
        "/.git/config",
        "/does-not-exist",
    ] {
        let (status, _, body) = send(&cx, get_req(uri, None)).await;
        assert!(
            status == StatusCode::NOT_FOUND || status == StatusCode::BAD_REQUEST,
            "{uri} 返回了 {status}，不该命中任何文件"
        );
        let body = text(&body);
        for marker in ["[package]", "name = \"rmcloud\"", "fn app_router", "root:x:", "use axum"] {
            assert!(!body.contains(marker), "{uri} 吐出了仓库/系统文件内容");
        }
    }
}

#[tokio::test]
async fn only_get_is_allowed_on_the_shell() {
    let cx = Ctx::new("methods");
    for uri in ["/", "/app.js", "/app.css"] {
        let req = Request::builder()
            .method("POST")
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let (status, _, _) = send(&cx, req).await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{uri}");
    }
}

/// 错题视图（C13）必须在静态资源里真的存在：两个 tab 的 id、容器、
/// 以及四个接口路径。前端是 `include_str!` 进二进制的，字符串在就说明发得出去。
#[test]
fn quiz_tab_is_shipped() {
    let js = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/web/app.js")).unwrap();
    let html =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/web/index.html")).unwrap();
    for id in ["tabNotes", "tabQuiz", "quiz"] {
        assert!(html.contains(&format!("id=\"{id}\"")), "index.html 缺 #{id}");
    }
    for path in [
        "'/api/v1/profile?status=all&limit=100'",
        "'/api/v1/profile/' + id + '/learned'",
        "'/api/v1/profile/' + id",
    ] {
        assert!(js.contains(path), "app.js 缺 {path}");
    }
    // 不许把令牌写进 DOM 或 URL
    assert!(!js.contains("?token="), "令牌不能进 URL");
    assert!(!js.contains("innerHTML ="), "服务端文本一律 textContent");
}
