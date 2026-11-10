//! `POST /api/v1/mail` 集成测试：鉴权、配置检查、附件缺失、curl 成功/失败。
//!
//! 不连真 SMTP：靠 `RMCLOUD_CURL` 把 curl 换成 `/usr/bin/true`（成功）或
//! `/usr/bin/false`（非 0 退出）。

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rmcloud::config::Smtp;
use rmcloud::{app_router, App, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

struct Tmp(PathBuf);

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmp(tag: &str) -> Tmp {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("rmcloud-mail-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    Tmp(d)
}

fn app(dir: &Tmp, smtp: Option<Smtp>) -> Arc<App> {
    let cfg = Config {
        bind: "127.0.0.1:0".into(),
        state: dir.0.clone(),
        token: "t0ken".into(),
        dsh: "/nonexistent/dsh".into(),
        dsh_env: dir.0.join("no.env").to_string_lossy().into_owned(),
        smtp,
        ..Default::default()
    };
    Arc::new(App::open(cfg).unwrap())
}

fn smtp() -> Option<Smtp> {
    Some(Smtp {
        host: "smtp.example.com".into(),
        port: 465,
        user: "me@example.com".into(),
        // 假密码，测试里不许出现真授权码。
        pass: "not-a-real-pass".into(),
    })
}

async fn post(app: &Arc<App>, body: Value, auth: bool) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method("POST")
        .uri("/api/v1/mail")
        .header("content-type", "application/json");
    if auth {
        b = b.header("authorization", "Bearer t0ken");
    }
    let req = b.body(Body::from(body.to_string())).unwrap();
    let res = app_router(app.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// 往 blobs/ 里放一个附件，返回 64 位十六进制的“sha”。
fn put_blob(dir: &Tmp, tag: &str, data: &[u8]) -> String {
    let mut sha: String = tag.bytes().map(|b| format!("{b:02x}")).collect();
    sha.truncate(64);
    while sha.len() < 64 {
        sha.push('0');
    }
    std::fs::write(dir.0.join("blobs").join(&sha), data).unwrap();
    sha
}

#[tokio::test]
async fn mail_requires_token() {
    let dir = tmp("auth");
    let app = app(&dir, smtp());
    let (st, v) = post(&app, json!({"to": ["a@b.c"]}), false).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(v["error"], "unauthorized");
}

#[tokio::test]
async fn mail_not_configured() {
    let dir = tmp("noconf");
    let app = app(&dir, None);
    let (st, v) = post(&app, json!({"to": ["a@b.c"], "subject": "s", "body": "b"}), true).await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(v["error"], "mail not configured");
}

#[tokio::test]
async fn mail_rejects_bad_addresses() {
    let dir = tmp("addr");
    let app = app(&dir, smtp());
    for body in [json!({"to": []}), json!({"to": ["nope"]}), json!({"to": ["a@"]})] {
        let (st, v) = post(&app, body, true).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "bad_address");
    }
}

#[tokio::test]
async fn mail_missing_attachment_blob_is_404() {
    let dir = tmp("noblob");
    let app = app(&dir, smtp());
    let body = json!({
        "to": ["a@b.c"], "subject": "s", "body": "b",
        "attachments": [{"blob": "f".repeat(64), "filename": "x.pdf", "mime": "application/pdf"}],
    });
    let (st, v) = post(&app, body, true).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(v["error"], "no_blob");
}

#[tokio::test]
async fn mail_traversal_blob_is_400() {
    let dir = tmp("trav");
    let app = app(&dir, smtp());
    let body = json!({
        "to": ["a@b.c"],
        "attachments": [{"blob": "../../etc/passwd", "filename": "x", "mime": "text/plain"}],
    });
    let (st, v) = post(&app, body, true).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(v["error"], "bad_blob");
}

#[tokio::test]
async fn mail_sends_via_curl_and_reports_curl_failure() {
    let dir = tmp("curl");
    let app = app(&dir, smtp());
    let att = put_blob(&dir, "att", b"%PDF-1.4 fake");
    let body = json!({
        "to": ["a@b.c", "d@e.f"],
        "subject": "测试",
        "body": "第一行\n第二行",
        "attachments": [{"blob": att, "filename": "../../evil.pdf", "mime": "application/pdf"}],
    });

    // 成功：curl 换成 true（参数一律不管，只看退出码）。
    std::env::set_var("RMCLOUD_CURL", "/usr/bin/true");
    let (st, v) = post(&app, body.clone(), true).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["ok"], true);
    let id = v["id"].as_str().unwrap();
    assert!(id.starts_with('<') && id.ends_with('>'), "{id}");
    // `.eml` 发完就删，cache 里不许留。
    let leftovers: Vec<_> = std::fs::read_dir(dir.0.join("cache"))
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "eml").unwrap_or(false))
        .collect();
    assert!(leftovers.is_empty(), "leftover eml: {leftovers:?}");

    // 失败：curl 非 0 → 502 + detail。
    std::env::set_var("RMCLOUD_CURL", "/usr/bin/false");
    let (st, v) = post(&app, body.clone(), true).await;
    assert_eq!(st, StatusCode::BAD_GATEWAY);
    assert_eq!(v["error"], "smtp failed");
    assert!(v.get("detail").is_some());

    // 工具不存在 → 也是 502（不能 500，设备只认 error 串）。
    std::env::set_var("RMCLOUD_CURL", "/nonexistent/curl");
    let (st, v) = post(&app, body, true).await;
    assert_eq!(st, StatusCode::BAD_GATEWAY);
    assert_eq!(v["error"], "smtp failed");
    assert!(v["detail"].as_str().unwrap().contains("cannot run"));
}

