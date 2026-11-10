//! 错题档案测试：不依赖服务器、不真调 dsh（假 dsh 脚本打印 NDJSON）。
//!
//! 覆盖：指纹归一化（空白/标点/大小写/全角）、重复检测与优先级升级、learned 幂等、
//! learned 后再错弹回 open、`repeat_of` 指认、JSON 解析容错（废话/缺字段/空数组/非法）、
//! 列表过滤排序、鉴权、成本记账（mode=profile）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use rmcloud::profile::{self, Problem, ScanReq};
use rmcloud::{app_router, App, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

const TOKEN: &str = "test-token";
const DOC: &str = "12345678-90ab-cdef-1234-567890abcdef";

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    assert!(path.is_file(), "缺 fixture：{}", path.display());
    path.to_string_lossy().into_owned()
}

/// 每个测试一个独立状态目录（`App::open` 会建 blobs/mirror/cache + sqlite）。
fn temp_state() -> PathBuf {
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("rmcloud-profile-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

fn app_with_dsh(dsh: &str) -> Arc<App> {
    let dir = temp_state();
    let cfg = Config {
        bind: "127.0.0.1:0".into(),
        local_bind: "127.0.0.1:0".into(),
        state: dir.join("state"),
        token: TOKEN.into(),
        dsh: dsh.into(),
        dsh_env: dir.join("dsh.env").to_string_lossy().into_owned(),
        smtp: None,
        inbox: false,
    };
    Arc::new(App::open(cfg).expect("App::open"))
}

fn fake_app() -> Arc<App> {
    app_with_dsh(&fixture("fake-dsh-profile.sh"))
}

/// 打真 handler（`oneshot`），返回 (状态码, JSON)。
async fn call(
    app: Arc<App>,
    method: &str,
    uri: &str,
    body: Option<Value>,
    token: Option<&str>,
) -> (u16, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let request = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&v).expect("序列化")))
            .expect("构造请求"),
        None => builder.body(Body::empty()).expect("构造请求"),
    };
    let response = app_router(app).oneshot(request).await.expect("handler");
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("读响应体");
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| json!({ "raw": String::from_utf8_lossy(&bytes) }));
    (status, value)
}

fn scan_body(hint: &str) -> Value {
    json!({ "doc": DOC, "page": 3, "image_blob": "a".repeat(64), "text": hint })
}

async fn scan(app: Arc<App>, hint: &str) -> (u16, Value) {
    call(
        app,
        "POST",
        "/api/v1/profile/scan",
        Some(scan_body(hint)),
        Some(TOKEN),
    )
    .await
}

async fn scan_ok(app: Arc<App>, hint: &str) -> Value {
    let (status, v) = scan(app, hint).await;
    assert_eq!(status, 200, "scan 失败：{v}");
    v
}

async fn list(app: Arc<App>, query: &str) -> (u16, Value) {
    call(app, "GET", &format!("/api/v1/profile{query}"), None, Some(TOKEN)).await
}

/// 断言用的最小行（只填 prompt 会读到的几个字段）。
fn known(id: i64, subject: Option<&str>, text: &str) -> Problem {
    Problem {
        id,
        doc: DOC.into(),
        page: 3,
        text: text.into(),
        subject: subject.map(str::to_string),
        difficulty: Some(3),
        status: "open".into(),
        priority: "normal".into(),
        repeat_count: 1,
        first_seen: "2026-01-01T00:00:00Z".into(),
        last_seen: "2026-01-01T00:00:00Z".into(),
        learned_at: None,
        note: None,
        fingerprint: "0".repeat(64),
    }
}

// ------------------------------ 指纹 / 归一化 ------------------------------

#[test]
fn fingerprint_ignores_whitespace_punct_case_and_width() {
    let base = profile::fingerprint(DOC, 3, "求 x 的值：3x+1=0");
    let same = [
        "求x的值:3x+1=0",
        "  求X的值：3X+1=0  ",
        "求\tx\n的值；3x+1=0。",
        "求ｘ的值：３ｘ＋１＝０",
        "求 x 的值……3x+1=0！",
    ];
    for s in same {
        assert_eq!(profile::fingerprint(DOC, 3, s), base, "写法不同但语义相同：{s}");
    }

    // 字、页、doc 任一不同都必须换指纹
    assert_ne!(profile::fingerprint(DOC, 3, "求 y 的值：3x+1=0"), base);
    assert_ne!(profile::fingerprint(DOC, 4, "求 x 的值：3x+1=0"), base);
    assert_ne!(
        profile::fingerprint("00000000-0000-0000-0000-000000000000", 3, "求 x 的值：3x+1=0"),
        base
    );
    // 小写 hex，长度固定
    assert_eq!(base.len(), 64);
    assert!(base.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
}

#[test]
fn normalize_strips_punct_and_folds_width() {
    // 标点（含全角）全去掉：按约定连数学符号也不算"字"，所以 ３＋２ → 32。
    assert_eq!(profile::normalize(" 求 X 的值，３＋２ 。"), "求x的值32");
    assert_eq!(profile::normalize("ＡＢＣ"), "abc");
    assert_eq!(profile::normalize("（一）【二】《三》"), "一二三");
    assert_eq!(profile::normalize("“引号”—破折号…省略号"), "引号破折号省略号");
    assert_eq!(profile::normalize(""), "");
}

#[test]
fn priority_upgrade_saturates() {
    assert_eq!(profile::upgrade("low"), "normal");
    assert_eq!(profile::upgrade("normal"), "high");
    assert_eq!(profile::upgrade("high"), "high");
    assert_eq!(profile::norm_priority(Some(" HIGH ")), "high");
    assert_eq!(profile::norm_priority(Some("low")), "low");
    assert_eq!(profile::norm_priority(Some("urgent")), "normal");
    assert_eq!(profile::norm_priority(None), "normal");
}

// ------------------------------ JSON 解析容错 ------------------------------

#[test]
fn parse_tolerates_chatter_around_json() {
    let raw = "好的，我先看看这页：\n```json\n{\"problems\":[{\"text\":\"1+1=?\"}]}\n```\n以上。";
    let items = profile::parse_problems(raw).expect("应能解析");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].text, "1+1=?");
}

#[test]
fn parse_partial_fields_use_defaults() {
    let items = profile::parse_problems(r#"{"problems":[{"text":"只有 text"}]}"#).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].text, "只有 text");
    assert!(items[0].subject.is_none());
    assert!(items[0].difficulty.is_none());
    assert!(items[0].priority.is_none());
    assert_eq!(items[0].repeat_of_id(), None);
}

#[test]
fn parse_repeat_of_accepts_string_and_number() {
    let a = profile::parse_problems(r#"{"problems":[{"text":"x","repeat_of":"7"}]}"#).unwrap();
    assert_eq!(a[0].repeat_of_id(), Some(7));
    let b = profile::parse_problems(r#"{"problems":[{"text":"x","repeat_of":12}]}"#).unwrap();
    assert_eq!(b[0].repeat_of_id(), Some(12));
    let c = profile::parse_problems(r#"{"problems":[{"text":"x","repeat_of":""}]}"#).unwrap();
    assert_eq!(c[0].repeat_of_id(), None);
    let d = profile::parse_problems(r#"{"problems":[{"text":"x","repeat_of":"第 3 题"}]}"#).unwrap();
    assert_eq!(d[0].repeat_of_id(), None);
}

#[test]
fn parse_empty_or_missing_problems_is_empty() {
    for raw in [
        r#"{"problems":[]}"#,
        r#"{"problems":null}"#,
        "{}",
        "废话 {\"problems\":[]} 废话",
    ] {
        assert!(profile::parse_problems(raw).unwrap().is_empty(), "{raw}");
    }
}

#[test]
fn parse_invalid_json_is_error_and_never_panics() {
    for raw in [
        "",
        "没有 JSON",
        "{\"problems\":[}",
        "}{",
        "{\"problems\": 3}",
        "{\"problems\":[{\"text\":}]}",
    ] {
        assert!(profile::parse_problems(raw).is_err(), "{raw} 应该报错");
    }
    assert_eq!(profile::extract_json("{}"), Some("{}"));
    assert_eq!(profile::extract_json("前 {a:1} 后"), Some("{a:1}"));
    assert_eq!(profile::extract_json("前 {a:1 后"), None);
    assert_eq!(profile::extract_json("}{"), None);
}

// ------------------------------ 提示词 ------------------------------

#[test]
fn prompt_asks_for_the_fixed_schema_and_lists_known_entries() {
    let req = ScanReq {
        doc: DOC.into(),
        page: 7,
        image_blob: Some("deadbeef".into()),
        text: "只登记今天的".into(),
    };
    let p = profile::scan_prompt(&req, &[known(4, Some("数学"), "旧题：求导")]);
    // 固定 schema 的五个字段都在
    for needle in [
        "\"problems\"",
        "\"text\"",
        "\"subject\"",
        "\"difficulty\"",
        "\"repeat_of\"",
        "\"priority\":\"high\"",
    ] {
        assert!(p.contains(needle), "prompt 缺 {needle}：\n{p}");
    }
    assert!(p.contains("只输出一个 JSON 对象"), "{p}");
    assert!(p.contains("不要 markdown 代码围栏"), "{p}");
    assert!(p.contains("4 | 数学 | 旧题：求导"), "已有条目没喂进去：\n{p}");
    assert!(p.contains("只登记今天的"), "{p}");
    assert!(p.contains("deadbeef"), "{p}");
    assert!(p.contains("第 7 页"), "{p}");
    assert!(p.contains("{\"problems\":[]}"), "空页的兜底输出没写：\n{p}");

    // 没有已有条目时明确说"（空）"
    let p = profile::scan_prompt(&req, &[]);
    assert!(p.contains("（空）"), "{p}");
}

// ------------------------------ 落库：重复 / 升级 ------------------------------

#[tokio::test]
async fn scan_twice_marks_repeat_and_upgrades_priority() {
    let app = fake_app();

    let v = scan_ok(app.clone(), "").await;
    assert_eq!(v["added"], 1);
    assert_eq!(v["repeated"], 0);
    let p = &v["problems"][0];
    assert_eq!(p["id"], 1);
    assert_eq!(p["repeat_count"], 1);
    assert_eq!(p["status"], "open");
    assert_eq!(p["priority"], "low");
    assert_eq!(p["subject"], "数学");
    assert_eq!(p["difficulty"], 3);
    assert_eq!(p["action"], "added");
    assert!(p["fingerprint"].as_str().expect("指纹").len() == 64);
    assert_eq!(p["first_seen"], p["last_seen"]);

    // 第二次同一道题：不新增，count+1，优先级升一档
    let v = scan_ok(app.clone(), "").await;
    assert_eq!(v["added"], 0);
    assert_eq!(v["repeated"], 1);
    let p = &v["problems"][0];
    assert_eq!(p["id"], 1);
    assert_eq!(p["repeat_count"], 2);
    assert_eq!(p["priority"], "normal");
    assert_eq!(p["action"], "repeated");

    // 第三次：high 封顶
    let v = scan_ok(app.clone(), "").await;
    assert_eq!(v["added"], 0);
    assert_eq!(v["repeated"], 1);
    assert_eq!(v["problems"][0]["repeat_count"], 3);
    assert_eq!(v["problems"][0]["priority"], "high");

    // 同一道题永远只有一行
    let (status, v) = list(app, "?status=all").await;
    assert_eq!(status, 200);
    assert_eq!(v["count"], 1);
}

#[tokio::test]
async fn repeat_of_bumps_the_named_entry_too() {
    let app = fake_app();
    scan_ok(app.clone(), "").await; // id 1

    // 模型说这道新题和 1 号是同一道 → 1 号也记一次重复
    let v = scan_ok(app.clone(), "场景:指认").await;
    assert_eq!(v["added"], 1, "{v}");
    assert_eq!(v["repeated"], 1, "{v}");
    let new = &v["problems"][0];
    assert_eq!(new["id"], 2);
    assert_eq!(new["repeat_count"], 1);

    let (_, v) = list(app, "?status=all").await;
    let rows = v["problems"].as_array().expect("列表");
    assert_eq!(rows.len(), 2);
    let one = rows.iter().find(|p| p["id"] == 1).expect("id 1");
    assert_eq!(one["repeat_count"], 2);
}

#[tokio::test]
async fn different_text_stays_a_separate_problem() {
    let app = fake_app();
    scan_ok(app.clone(), "").await;
    let v = scan_ok(app.clone(), "场景:高甲").await;
    assert_eq!(v["added"], 1);
    assert_eq!(v["repeated"], 0);
    let (_, v) = list(app, "?status=all").await;
    assert_eq!(v["count"], 2);
}

// ------------------------------ 学会 / 复学 ------------------------------

#[tokio::test]
async fn learned_is_idempotent() {
    let app = fake_app();
    scan_ok(app.clone(), "").await;

    let (status, v) = call(
        app.clone(),
        "POST",
        "/api/v1/profile/1/learned",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["problem"]["status"], "learned");
    let at = v["problem"]["learned_at"].clone();
    assert!(at.is_string(), "learned_at 要写上去：{v}");

    // 幂等：再调一次还是 200，learned_at 不刷新
    let (status, v) = call(
        app.clone(),
        "POST",
        "/api/v1/profile/1/learned",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["problem"]["status"], "learned");
    assert_eq!(v["problem"]["learned_at"], at);
}

#[tokio::test]
async fn wrong_again_after_learned_reopens() {
    let app = fake_app();
    scan_ok(app.clone(), "").await;
    call(
        app.clone(),
        "POST",
        "/api/v1/profile/1/learned",
        None,
        Some(TOKEN),
    )
    .await;

    // 又错了一遍 → 说明没真学会，弹回 open
    let v = scan_ok(app.clone(), "").await;
    assert_eq!(v["repeated"], 1);
    let p = &v["problems"][0];
    assert_eq!(p["status"], "open");
    assert!(p["learned_at"].is_null(), "{v}");
    assert_eq!(p["repeat_count"], 2);
    assert_eq!(p["priority"], "normal");

    let (_, v) = list(app, "?status=learned").await;
    assert_eq!(v["count"], 0);
}

#[tokio::test]
async fn learned_and_delete_unknown_id_are_404() {
    let app = fake_app();
    for (method, uri) in [
        ("POST", "/api/v1/profile/99/learned"),
        ("DELETE", "/api/v1/profile/99"),
        ("DELETE", "/api/v1/profile/abc"),
        ("POST", "/api/v1/profile/abc/learned"),
    ] {
        let (status, v) = call(app.clone(), method, uri, None, Some(TOKEN)).await;
        assert_eq!(status, 404, "{method} {uri} → {v}");
        assert_eq!(v["error"], "not found");
    }
}

#[tokio::test]
async fn delete_removes_one_problem() {
    let app = fake_app();
    scan_ok(app.clone(), "").await;
    scan_ok(app.clone(), "场景:高甲").await;

    let (status, v) = call(
        app.clone(),
        "DELETE",
        "/api/v1/profile/1",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(status, 200, "{v}");
    assert_eq!(v["deleted"], 1);

    let (_, v) = list(app.clone(), "?status=all").await;
    assert_eq!(v["count"], 1);
    assert_eq!(v["problems"][0]["id"], 2);

    // 再删同一条 → 404
    let (status, _) = call(
        app.clone(),
        "DELETE",
        "/api/v1/profile/1",
        None,
        Some(TOKEN),
    )
    .await;
    assert_eq!(status, 404);
}

// ------------------------------ 模型输出的坏情况 ------------------------------

#[tokio::test]
async fn empty_problems_array_adds_nothing() {
    let app = fake_app();
    let v = scan_ok(app.clone(), "场景:空").await;
    assert_eq!(v["added"], 0);
    assert_eq!(v["repeated"], 0);
    assert!(v["problems"].as_array().expect("数组").is_empty());
    let (_, v) = list(app, "?status=all").await;
    assert_eq!(v["count"], 0);
}

#[tokio::test]
async fn chatter_and_missing_fields_are_accepted() {
    let app = fake_app();
    let v = scan_ok(app.clone(), "场景:废话").await;
    assert_eq!(v["added"], 1, "{v}");
    let p = &v["problems"][0];
    assert_eq!(p["text"], "求 3x+1 的定义域");
    assert!(p["subject"].is_null(), "{v}");
    assert!(p["difficulty"].is_null(), "{v}");
    assert_eq!(p["priority"], "normal"); // 缺 priority 落到 normal
    assert_eq!(p["status"], "open");
}

#[tokio::test]
async fn invalid_json_is_502_and_writes_nothing() {
    let app = fake_app();
    for hint in ["场景:坏", "场景:无"] {
        let (status, v) = scan(app.clone(), hint).await;
        assert_eq!(status, 502, "{hint} → {v}");
        assert_eq!(v["error"], "parse failed", "{v}");
    }
    let (_, v) = list(app, "?status=all").await;
    assert_eq!(v["count"], 0);
}

#[tokio::test]
async fn dsh_failure_is_502_and_writes_nothing() {
    let app = fake_app();
    let (status, v) = scan(app.clone(), "场景:失败").await;
    assert_eq!(status, 502, "{v}");
    assert_eq!(v["error"], "scan failed");
    assert!(
        v["detail"].as_str().unwrap_or("").contains("模型超时了"),
        "{v}"
    );
    let (_, v) = list(app, "?status=all").await;
    assert_eq!(v["count"], 0);
}

#[tokio::test]
async fn scan_requires_doc() {
    let app = fake_app();
    let (status, v) = call(
        app,
        "POST",
        "/api/v1/profile/scan",
        Some(json!({"page": 3, "text": ""})),
        Some(TOKEN),
    )
    .await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"], "bad_doc");
}

// ------------------------------ 列表：过滤 / 排序 / 条数 ------------------------------

#[tokio::test]
async fn list_filters_sorts_and_limits() {
    let app = fake_app();
    scan_ok(app.clone(), "场景:高甲").await; // id 1, high
    scan_ok(app.clone(), "场景:低").await; // id 2, low
    scan_ok(app.clone(), "场景:高乙").await; // id 3, high

    // 人为改 last_seen：让"最新"和"优先级"给出相反的答案，才能验证排序真的按优先级开头。
    for (id, ts) in [
        (1, "2099-01-01T00:00:00Z"),
        (2, "2099-12-31T00:00:00Z"), // 最新，但 low
        (3, "2020-01-01T00:00:00Z"), // 最旧，但 high
    ] {
        app.db()
            .execute(
                "UPDATE problem SET last_seen=?1 WHERE id=?2",
                rusqlite::params![ts, id],
            )
            .expect("改 last_seen");
    }

    let (status, v) = list(app.clone(), "?status=open").await;
    assert_eq!(status, 200, "{v}");
    let ids: Vec<i64> = v["problems"]
        .as_array()
        .expect("列表")
        .iter()
        .map(|p| p["id"].as_i64().expect("id"))
        .collect();
    assert_eq!(ids, vec![1, 3, 2], "high(近→远) 然后 low：{v}");

    // 默认 status=open
    let (status, v) = list(app.clone(), "").await;
    assert_eq!(status, 200);
    assert_eq!(v["count"], 3);

    // limit
    let (_, v) = list(app.clone(), "?status=open&limit=1").await;
    assert_eq!(v["count"], 1);
    assert_eq!(v["problems"][0]["id"], 1);
    // limit 是脏数据就退回默认值，不报错
    let (status, v) = list(app.clone(), "?status=open&limit=abc").await;
    assert_eq!(status, 200);
    assert_eq!(v["count"], 3);

    // 学会一条后 open/learned 各自过滤
    call(
        app.clone(),
        "POST",
        "/api/v1/profile/2/learned",
        None,
        Some(TOKEN),
    )
    .await;
    let (_, v) = list(app.clone(), "?status=open").await;
    assert_eq!(v["count"], 2);
    let (_, v) = list(app.clone(), "?status=learned").await;
    assert_eq!(v["count"], 1);
    assert_eq!(v["problems"][0]["id"], 2);
    let (_, v) = list(app.clone(), "?status=all").await;
    assert_eq!(v["count"], 3);

    // 未知 status 明确报错
    let (status, v) = list(app, "?status=weird").await;
    assert_eq!(status, 400, "{v}");
    assert_eq!(v["error"], "bad_status");
}

// ------------------------------ 鉴权 / 成本 ------------------------------

#[tokio::test]
async fn every_profile_route_requires_token() {
    let app = fake_app();
    for (method, uri, body) in [
        ("POST", "/api/v1/profile/scan", Some(scan_body(""))),
        ("GET", "/api/v1/profile", None),
        ("POST", "/api/v1/profile/1/learned", None),
        ("DELETE", "/api/v1/profile/1", None),
    ] {
        let (status, v) = call(app.clone(), method, uri, body.clone(), None).await;
        assert_eq!(status, 401, "{method} {uri} 无 token → {v}");
        assert_eq!(v["error"], "unauthorized");
        // 错 token 一样 401（body 照发，别让 Json 提取器的 415 抢在鉴权前面）
        let (status, _) = call(app.clone(), method, uri, body, Some("wrong-token")).await;
        assert_eq!(status, 401, "{method} {uri} 错 token");
    }
}

#[tokio::test]
async fn scan_records_usage_with_profile_mode() {
    let app = fake_app();
    scan_ok(app.clone(), "").await;

    let (mode, in_tok, out_tok, session): (String, i64, i64, String) = app
        .db()
        .query_row(
            "SELECT mode, in_tok, out_tok, session FROM usage",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .expect("usage 行");
    assert_eq!(mode, "profile");
    assert_eq!(in_tok, 120);
    assert_eq!(out_tok, 30);
    assert_eq!(session, "session-rm-12345678");

    // 第二次 scan 也要记一笔（每次 scan 都记账，不绕过）
    scan_ok(app.clone(), "").await;
    let calls: i64 = app
        .db()
        .query_row("SELECT COUNT(*) FROM usage WHERE mode='profile'", [], |r| r.get(0))
        .expect("计数");
    assert_eq!(calls, 2);
}
