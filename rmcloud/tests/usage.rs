//! `GET /api/v1/usage` 集成测试 + `usage::record` 落库。
//!
//! 聚合用直接 `INSERT` 的假数据验证，不跑真 dsh。

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rmcloud::usage;
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
    let d = std::env::temp_dir().join(format!("rmcloud-usage-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    Tmp(d)
}

fn app(dir: &Tmp) -> Arc<App> {
    let cfg = Config {
        bind: "127.0.0.1:0".into(),
        state: dir.0.clone(),
        token: "t0ken".into(),
        dsh: "/nonexistent/dsh".into(),
        dsh_env: dir.0.join("no.env").to_string_lossy().into_owned(),
        smtp: None,
        ..Default::default()
    };
    Arc::new(App::open(cfg).unwrap())
}

async fn get(app: &Arc<App>, uri: &str, auth: bool) -> (StatusCode, Value) {
    let mut b = Request::builder().method("GET").uri(uri);
    if auth {
        b = b.header("authorization", "Bearer t0ken");
    }
    let req = b.body(Body::empty()).unwrap();
    let res = app_router(app.clone()).oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn insert(app: &App, ts: i64, mode: &str, in_tok: i64, out_tok: i64, c: f64, ms: i64) {
    app.db()
        .execute(
            "INSERT INTO usage(ts, mode, session, in_tok, out_tok, cost, ms)
             VALUES(?1, ?2, NULL, ?3, ?4, ?5, ?6)",
            rusqlite::params![ts, mode, in_tok, out_tok, c, ms],
        )
        .unwrap();
}

/// 服务端 SQL 里的“本地日”（和 handler 用同一个表达式，验证分桶而不是时区）。
fn local_day(app: &App, ts: i64) -> String {
    app.db()
        .query_row("SELECT date(?1, 'unixepoch', 'localtime')", [ts], |r| {
            r.get::<_, String>(0)
        })
        .unwrap()
}

#[tokio::test]
async fn usage_requires_token() {
    let dir = tmp("auth");
    let app = app(&dir);
    let (st, v) = get(&app, "/api/v1/usage?days=7", false).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(v["error"], "unauthorized");
}

#[tokio::test]
async fn usage_empty_is_zeros() {
    let dir = tmp("empty");
    let app = app(&dir);
    let (st, v) = get(&app, "/api/v1/usage", true).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["total"], json!({"in": 0, "out": 0, "cost": 0.0, "calls": 0}));
    assert_eq!(v["by_day"], json!([]));
    assert_eq!(v["by_mode"], json!([]));
}

#[tokio::test]
async fn record_writes_a_row() {
    let dir = tmp("record");
    let app = app(&dir);
    usage::record(&app, "ask", Some("s1"), 1234, 567, 890);

    let (session, in_tok, out_tok, c, ms): (String, i64, i64, f64, i64) = app
        .db()
        .query_row(
            "SELECT session, in_tok, out_tok, cost, ms FROM usage",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap();
    assert_eq!(session, "s1");
    assert_eq!((in_tok, out_tok, ms), (1234, 567, 890));
    assert!((c - usage::cost(1234, 567)).abs() < 1e-12);

    let (st, v) = get(&app, "/api/v1/usage?days=7", true).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["total"]["in"], 1234);
    assert_eq!(v["total"]["out"], 567);
    assert_eq!(v["total"]["calls"], 1);
    let want = (1234.0 * 0.27 + 567.0 * 1.10) / 1e6;
    assert!((v["total"]["cost"].as_f64().unwrap() - want).abs() < 1e-9);

    // 没有 session 的调用：session 列写 NULL，不报错。
    usage::record(&app, "ocr", None, 10, 0, 5);
    let (st, v) = get(&app, "/api/v1/usage", true).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["total"]["calls"], 2);
}

#[tokio::test]
async fn usage_aggregates_by_day_and_mode() {
    let dir = tmp("agg");
    let app = app(&dir);
    let t = now();
    insert(&app, t, "ask", 1_000, 500, usage::cost(1_000, 500), 1_200);
    insert(&app, t, "ocr", 2_000, 0, usage::cost(2_000, 0), 30);
    // 窗口外（30 天前）
    insert(
        &app,
        t - 30 * 86_400,
        "ask",
        9_999,
        9_999,
        usage::cost(9_999, 9_999),
        1,
    );

    let (st, v) = get(&app, "/api/v1/usage?days=7", true).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["total"]["in"], 3_000);
    assert_eq!(v["total"]["out"], 500);
    assert_eq!(v["total"]["calls"], 2);
    let want = usage::cost(3_000, 500);
    assert!((v["total"]["cost"].as_f64().unwrap() - want).abs() < 1e-9);

    // by_day：只有今天一桶
    let days = v["by_day"].as_array().unwrap();
    assert_eq!(days.len(), 1, "{days:?}");
    assert_eq!(days[0]["day"], local_day(&app, t));
    assert_eq!(days[0]["in"], 3_000);
    assert_eq!(days[0]["calls"], 2);

    // by_mode：按花费降序（ask 0.00082 > ocr 0.00054）
    let modes = v["by_mode"].as_array().unwrap();
    assert_eq!(modes.len(), 2, "{modes:?}");
    assert_eq!(modes[0]["mode"], "ask");
    assert_eq!(modes[0]["in"], 1_000);
    assert_eq!(modes[0]["calls"], 1);
    assert_eq!(modes[1]["mode"], "ocr");
    assert_eq!(modes[1]["out"], 0);

    // 窗口放大到 365 天 → 30 天前那条也算进来
    let (st, v) = get(&app, "/api/v1/usage?days=365", true).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["total"]["calls"], 3);
    assert_eq!(v["total"]["in"], 12_999);
}

#[tokio::test]
async fn usage_days_param_is_forgiving() {
    let dir = tmp("days");
    let app = app(&dir);
    let t = now();
    insert(&app, t, "ask", 100, 0, usage::cost(100, 0), 1);

    // 非法/缺省 → 7 天
    for uri in ["/api/v1/usage?days=abc", "/api/v1/usage?days=", "/api/v1/usage"] {
        let (st, v) = get(&app, uri, true).await;
        assert_eq!(st, StatusCode::OK, "{uri}");
        assert_eq!(v["total"]["calls"], 1, "{uri}");
    }
    // 0 天 clamp 到 1 天，今天的还在
    let (st, v) = get(&app, "/api/v1/usage?days=0", true).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["total"]["calls"], 1);
    // 超大值不让它溢出
    let (st, _v) = get(&app, "/api/v1/usage?days=999999", true).await;
    assert_eq!(st, StatusCode::OK);
}
