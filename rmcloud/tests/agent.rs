//! `agent_api` 测试：不依赖服务器、不真调 dsh（假 dsh 脚本打印 NDJSON）。
//!
//! 覆盖：session 派生、四种 mode 的 prompt 关键措辞、NDJSON → `Parsed`、
//! 真 handler 的 SSE 流（token/done/error）、unknown session 重试、忙时 503、鉴权。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use rmcloud::agent_api::{self, AskReq, Images, Parsed};
use rmcloud::{app_router, App, Config};
use serde_json::{json, Value};
use tower::ServiceExt;

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
    let dir = std::env::temp_dir().join(format!("rmcloud-agent-{}-{n}", std::process::id()));
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
        token: "test-token".into(),
        dsh: dsh.into(),
        dsh_env: dir.join("dsh.env").to_string_lossy().into_owned(),
        smtp: None,
        inbox: false,
    };
    Arc::new(App::open(cfg).expect("App::open"))
}

fn ask_json(mode: &str) -> Value {
    json!({
        "mode": mode,
        "doc": "12345678-90ab-cdef-1234-567890abcdef",
        "page": 3,
        "rect": [10.0, 20.0, 300.0, 400.0],
        "image_blob": "deadbeef",
        "text": "这题选什么？",
    })
}

fn req(mode: &str) -> AskReq {
    serde_json::from_value(ask_json(mode)).expect("AskReq 反序列化")
}

/// 打真 handler（`oneshot`），读完整个 SSE 响应体。
async fn ask(app: Arc<App>, body: Value, token: Option<&str>) -> (u16, String) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/api/v1/agent/ask")
        .header("content-type", "application/json");
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let request = builder
        .body(Body::from(serde_json::to_vec(&body).expect("序列化")))
        .expect("构造请求");
    let response = app_router(app).oneshot(request).await.expect("handler");
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("读响应体");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

// ------------------------------ 纯函数 ------------------------------

#[test]
fn session_is_derived_from_doc_prefix() {
    assert_eq!(
        agent_api::session_for("12345678-90ab-cdef-1234-567890abcdef"),
        "session-rm-12345678"
    );
    // 大小写规整、短横线去掉
    assert_eq!(
        agent_api::session_for("ABCDEF12-0000-0000-0000-000000000000"),
        "session-rm-abcdef12"
    );
    // 不够 8 位就有几位用几位；空 doc 给固定兜底
    assert_eq!(agent_api::session_for("abc"), "session-rm-abc");
    assert_eq!(agent_api::session_for(""), "session-rm-00000000");
}

#[test]
fn select_prompt_has_rect_and_scope() {
    let prompt = agent_api::build_prompt(&req("select"), &Images::default());
    assert!(prompt.contains("[10, 20, 300, 400]"), "{prompt}");
    assert!(prompt.contains("只答选区问题但要能读全页"), "{prompt}");
    assert!(prompt.contains("这题选什么？"), "{prompt}");
}

#[test]
fn live_prompt_wants_short_spoken_chinese() {
    let prompt = agent_api::build_prompt(&req("live"), &Images::default());
    assert!(prompt.contains("短答、口语、中文"), "{prompt}");
}

#[test]
fn review_prompt_wants_per_question_grading() {
    let prompt = agent_api::build_prompt(&req("review"), &Images::default());
    assert!(prompt.contains("逐题批改"), "{prompt}");
}

#[test]
fn organize_prompt_wants_structure() {
    let prompt = agent_api::build_prompt(&req("organize"), &Images::default());
    assert!(prompt.contains("结构化要点"), "{prompt}");
}

#[test]
fn every_mode_requires_math_delimiters_and_no_code_fence() {
    for mode in ["select", "live", "review", "organize"] {
        let prompt = agent_api::build_prompt(&req(mode), &Images::default());
        assert!(prompt.contains("$...$"), "{mode} 缺数学定界符：{prompt}");
        assert!(prompt.contains("不要用 markdown 代码围栏"), "{mode}");
        assert!(!prompt.contains("```"), "{mode} 的 prompt 里出现了代码围栏");
        // 真机实测：模型会先来一句 "I'll start by looking at the full page
        // render..."，复用 session 之后这习惯会被历史固化，所以硬约束压在
        // prompt **最末尾**（位置越靠后权重越大）。
        assert!(prompt.contains("输出要求（覆盖上面所有冲突的措辞）"), "{mode}");
        assert!(prompt.contains("第一句就是给用户的答案"), "{mode}");
        if mode == "live" {
            // 实时文字带是纯文本流，`$...$` 会原样显示成美元符号
            assert!(prompt.contains("数学一律用 Unicode 直接写"), "{prompt}");
            assert!(!prompt.contains("所有数学必须用 $...$"), "{mode}");
        } else {
            assert!(prompt.contains("所有数学必须用 $...$ 包起来"), "{mode}");
        }
    }
}

#[test]
fn parse_text_line() {
    assert_eq!(
        agent_api::parse_event(r#"{"type":"text","text":"你好"}"#),
        Parsed::Text("你好".into())
    );
}

#[test]
fn parse_usage_line() {
    assert_eq!(
        agent_api::parse_event(
            r#"{"type":"status","phase":"step_end","step":1,"usage":{"inputTokens":8407,"outputTokens":2,"cacheReadTokens":5120}}"#
        ),
        Parsed::Usage {
            in_tok: 8407,
            out_tok: 2
        }
    );
}

#[test]
fn parse_error_line() {
    assert_eq!(
        agent_api::parse_event(
            r#"{"type":"status","phase":"turn_end","reason":{"kind":"error","message":"模型超时了"}}"#
        ),
        Parsed::Error("模型超时了".into())
    );
}

#[test]
fn parse_ignorable_lines() {
    // 正常结束、session、thinking、final、非 JSON —— 都不该出现在 SSE 里
    assert_eq!(
        agent_api::parse_event(
            r#"{"type":"status","phase":"turn_end","reason":{"kind":"completed"}}"#
        ),
        Parsed::Ignore
    );
    assert_eq!(
        agent_api::parse_event(r#"{"type":"session","sessionId":"session-x"}"#),
        Parsed::Ignore
    );
    assert_eq!(
        agent_api::parse_event(r#"{"type":"thinking","text":""}"#),
        Parsed::Ignore
    );
    assert_eq!(
        agent_api::parse_event(r#"{"type":"final","text":"你好"}"#),
        Parsed::Ignore
    );
    assert_eq!(agent_api::parse_event("not json at all"), Parsed::Ignore);
    assert_eq!(agent_api::parse_event(""), Parsed::Ignore);
}

// ------------------------------ 集成（假 dsh） ------------------------------

#[tokio::test]
async fn ask_streams_tokens_then_done() {
    let app = app_with_dsh(&fixture("fake-dsh.sh"));
    let (status, body) = ask(app, ask_json("select"), Some("test-token")).await;

    assert_eq!(status, 200, "body:\n{body}");
    assert!(body.contains("event: token"), "body:\n{body}");
    assert!(body.contains(r#"{"t":"你好"}"#), "body:\n{body}");
    assert!(body.contains("event: done"), "body:\n{body}");
    assert!(body.contains(r#""in":100"#), "body:\n{body}");
    assert!(body.contains(r#""out":7"#), "body:\n{body}");
    assert!(
        body.contains(r#""session":"session-rm-12345678""#),
        "body:\n{body}"
    );
    assert!(!body.contains("event: error"), "body:\n{body}");
}

#[tokio::test]
async fn unknown_session_is_retried_without_session_id() {
    let app = app_with_dsh(&fixture("fake-dsh-retry.sh"));
    let (status, body) = ask(app, ask_json("live"), Some("test-token")).await;

    assert_eq!(status, 200, "body:\n{body}");
    assert!(!body.contains("event: error"), "body:\n{body}");
    assert!(body.contains("重试成功"), "body:\n{body}");
    assert!(body.contains("event: done"), "body:\n{body}");
    assert!(body.contains(r#""out":3"#), "body:\n{body}");
}

#[tokio::test]
async fn explicit_session_is_not_swapped_silently() {
    let app = app_with_dsh(&fixture("fake-dsh-retry.sh"));
    let mut body = ask_json("live");
    body["session"] = json!("session-from-device");
    let (status, body) = ask(app, body, Some("test-token")).await;

    assert_eq!(status, 200, "body:\n{body}");
    assert!(body.contains("event: error"), "body:\n{body}");
    assert!(body.contains("unknown session"), "body:\n{body}");
    assert!(!body.contains("event: done"), "body:\n{body}");
}

#[tokio::test]
async fn turn_end_error_becomes_sse_error() {
    let app = app_with_dsh(&fixture("fake-dsh-fail.sh"));
    let mut body = ask_json("review");
    body["session"] = json!("session-fixed");
    let (status, body) = ask(app, body, Some("test-token")).await;

    assert_eq!(status, 200, "body:\n{body}");
    assert!(body.contains("event: token"), "body:\n{body}");
    assert!(body.contains("event: error"), "body:\n{body}");
    assert!(body.contains("模型超时了"), "body:\n{body}");
    assert!(!body.contains("event: done"), "body:\n{body}");
}

#[tokio::test]
async fn busy_agent_returns_503() {
    let app = app_with_dsh(&fixture("fake-dsh.sh"));
    let _held = app.dsh_lock.try_lock().expect("锁应是空闲的");
    let (status, body) = ask(app.clone(), ask_json("live"), Some("test-token")).await;

    assert_eq!(status, 503, "body:\n{body}");
    assert!(body.contains("agent busy"), "body:\n{body}");
}

#[tokio::test]
async fn ask_requires_token() {
    let app = app_with_dsh(&fixture("fake-dsh.sh"));
    let (status, body) = ask(app, ask_json("live"), None).await;

    assert_eq!(status, 401, "body:\n{body}");
    assert!(body.contains("unauthorized"), "body:\n{body}");
}

#[tokio::test]
async fn tool_route_requires_token_and_known_op() {
    let app = app_with_dsh(&fixture("fake-dsh.sh"));

    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/tool/strokes")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .expect("构造请求");
    let response = app_router(app.clone()).oneshot(request).await.expect("handler");
    assert_eq!(response.status().as_u16(), 401);

    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/tool/nope")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test-token")
        .body(Body::from("{}"))
        .expect("构造请求");
    let response = app_router(app).oneshot(request).await.expect("handler");
    assert_eq!(response.status().as_u16(), 404);
}
