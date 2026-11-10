//! 配对接口的集成测试：直接打 `app_router`，不起真端口、不依赖网络。
//!
//! `Config::save()` 走 `RMCLOUD_CONFIG`，而这个环境变量是进程级的，所以本文件
//! 里的测试用一把静态锁串行，每个测试用自己的临时配置文件。
//!
//! tower 在本仓库里没有开 `util` feature（`Cargo.toml` 不能改），所以用
//! `tower::Service` 的 `poll_ready` + `call` 手搓 oneshot。

use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use rmcloud::{app_router, App, Config};
use serde_json::{json, Value};
use tower::Service;

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct Ctx {
    dir: PathBuf,
    cfg_path: PathBuf,
    cfg: Config,
    app: Arc<App>,
    router: axum::Router,
    /// 守住进程级环境变量，直到测试结束。
    _guard: MutexGuard<'static, ()>,
}

impl Drop for Ctx {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

impl Ctx {
    fn new(tag: &str) -> Ctx {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("rmcloud-pair-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("make temp dir");
        let cfg_path = dir.join("rmcloud.json");
        std::env::set_var("RMCLOUD_CONFIG", &cfg_path);
        let cfg = Config {
            state: dir.join("state"),
            token: String::new(),
            ..Default::default()
        };
        let app = Arc::new(App::open(cfg.clone()).expect("open app"));
        let router = app_router(Arc::clone(&app));
        Ctx {
            dir,
            cfg_path,
            cfg,
            app,
            router,
            _guard: guard,
        }
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

fn pair_req(code: &str, device_name: &str) -> Request<Body> {
    // 设备端真实请求（`net.rs:326`）：多一个 protocol 字段，服务端忽略即可。
    let v = json!({"code": code, "device_name": device_name, "protocol": 1});
    Request::builder()
        .method("POST")
        .uri("/api/v1/pair")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(v.to_string()))
        .unwrap()
}

fn get_req(uri: &str, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("GET").uri(uri);
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::empty()).unwrap()
}

fn parse(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or(Value::Null)
}

fn err_of(body: &[u8]) -> String {
    parse(body)["error"].as_str().unwrap_or_default().to_string()
}

// ---------------------------------------------------------------- 测试

#[tokio::test]
async fn code_exchanges_for_token_and_token_works_immediately() {
    let cx = Ctx::new("flow");
    let code = rmcloud::pair::new_code(&cx.cfg).expect("new code");

    // 8 位、全在大写字母数字表里、不含 0/O/1/I。
    assert_eq!(code.len(), 8, "{code}");
    for c in code.chars() {
        assert!(c.is_ascii_uppercase() || c.is_ascii_digit(), "{code}");
        assert!(!"0O1I".contains(c), "{code}");
    }

    // 错码先打一发：不该消耗真码。
    let (status, _, body) = send(&cx, pair_req("00000000", "reMarkable 2")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(err_of(&body), "bad_code");

    // 真码 → token。
    let (status, _, body) = send(&cx, pair_req(&code, "reMarkable 2")).await;
    assert_eq!(status, StatusCode::OK, "{}", err_of(&body));
    let v = parse(&body);
    let token = v["token"].as_str().expect("token").to_string();
    assert_eq!(token.len(), 64, "{token}");
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()), "{token}");
    assert!(v["device_id"].as_str().unwrap_or_default().starts_with("d-"));

    // token 立刻能用：服务没重启，`App::cfg.token` 还是空的，靠 meta 生效。
    assert!(cx.app.cfg.token.is_empty());
    let (status, _, body) = send(&cx, get_req("/api/v1/manifest", Some(&token))).await;
    assert_eq!(status, StatusCode::OK, "{}", err_of(&body));
    assert_eq!(parse(&body)["rev"], 0);

    // 写回配置（重启后仍有效）。
    let saved: Config =
        serde_json::from_str(&std::fs::read_to_string(&cx.cfg_path).expect("config file")).unwrap();
    assert_eq!(saved.token, token);

    // 码用一次就作废，库里也不留。
    assert!(cx.app.meta_get("pair_code").is_none());
    assert!(cx.app.meta_get("pair_exp").is_none());
    let (status, _, body) = send(&cx, pair_req(&code, "again")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(err_of(&body), "bad_code");
}

#[tokio::test]
async fn expired_or_missing_code_is_401() {
    let cx = Ctx::new("expired");

    // 从没生成过码。
    let (status, _, body) = send(&cx, pair_req("ABCDEFGH", "rM2")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(err_of(&body), "bad_code");

    // 生成了但已过期。
    let code = rmcloud::pair::new_code(&cx.cfg).expect("new code");
    cx.app.meta_set("pair_exp", "1"); // 1970 年
    let (status, _, body) = send(&cx, pair_req(&code, "rM2")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(err_of(&body), "bad_code");
    // 过期的码顺手清掉。
    assert!(cx.app.meta_get("pair_code").is_none());
}

#[tokio::test]
async fn newest_code_replaces_older_one() {
    let cx = Ctx::new("replace");
    let first = rmcloud::pair::new_code(&cx.cfg).expect("first");
    let second = rmcloud::pair::new_code(&cx.cfg).expect("second");
    assert_ne!(first, second);
    let (status, _, _) = send(&cx, pair_req(&first, "rM2")).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _, _) = send(&cx, pair_req(&second, "rM2")).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn sync_without_token_is_401() {
    let cx = Ctx::new("notoken");
    for req in [
        get_req("/api/v1/manifest", None),
        get_req("/api/v1/manifest", Some("deadbeef")),
        get_req("/api/v1/tree", None),
    ] {
        let (status, _, body) = send(&cx, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(err_of(&body), "unauthorized");
    }
    // 库还是空的，没有别的副作用。
    assert!(cx.app.entries().is_empty());
}
