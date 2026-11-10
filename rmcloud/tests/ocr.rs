//! `POST /api/v1/ocr` 集成测试。
//!
//! 不真跑 dsh：用缓存、`/bin/true`、不存在的路径，以及一个假 dsh 脚本验证
//! 参数/环境/结果缓存。

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
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
    let d = std::env::temp_dir().join(format!("rmcloud-ocr-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    Tmp(d)
}

fn app(dir: &Tmp, dsh: &str, dsh_env: &str) -> Arc<App> {
    let cfg = Config {
        bind: "127.0.0.1:0".into(),
        state: dir.0.clone(),
        token: "t0ken".into(),
        dsh: dsh.into(),
        dsh_env: dsh_env.into(),
        smtp: None,
        ..Default::default()
    };
    Arc::new(App::open(cfg).unwrap())
}

/// 放一张图进 blobs/，返回 64 位十六进制 sha。
fn put_blob(dir: &Tmp, tag: &str, data: &[u8]) -> String {
    let mut sha: String = tag.bytes().map(|b| format!("{b:02x}")).collect();
    sha.truncate(64);
    while sha.len() < 64 {
        sha.push('0');
    }
    std::fs::write(dir.0.join("blobs").join(&sha), data).unwrap();
    sha
}

async fn post(app: &Arc<App>, body: Value, auth: bool) -> (StatusCode, Value) {
    let mut b = Request::builder()
        .method("POST")
        .uri("/api/v1/ocr")
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

fn png() -> &'static [u8] {
    b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR fake"
}

fn bmp() -> &'static [u8] {
    b"BM\x36\x00\x00\x00 fake"
}

#[tokio::test]
async fn ocr_requires_token() {
    let dir = tmp("auth");
    let app = app(&dir, "/nonexistent/dsh", "/nonexistent/env");
    let (st, v) = post(&app, json!({"image_blob": "a".repeat(64)}), false).await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(v["error"], "unauthorized");
}

#[tokio::test]
async fn ocr_bad_blob_name_is_400() {
    let dir = tmp("trav");
    let app = app(&dir, "/nonexistent/dsh", "/nonexistent/env");
    for blob in ["../../etc/passwd".to_string(), "xyz".to_string()] {
        let (st, v) = post(&app, json!({"image_blob": blob}), true).await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "bad_blob");
    }
}

#[tokio::test]
async fn ocr_missing_blob_is_404() {
    let dir = tmp("noblob");
    let app = app(&dir, "/nonexistent/dsh", "/nonexistent/env");
    let (st, v) = post(&app, json!({"image_blob": "b".repeat(64)}), true).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(v["error"], "no_blob");
}

#[tokio::test]
async fn ocr_rejects_non_image_bytes() {
    let dir = tmp("badimg");
    let app = app(&dir, "/nonexistent/dsh", "/nonexistent/env");
    let sha = put_blob(&dir, "txt", b"hello, not an image");
    let (st, v) = post(&app, json!({"image_blob": sha}), true).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(v["error"], "bad_image");
}

#[tokio::test]
async fn ocr_cache_hit_skips_dsh() {
    let dir = tmp("cache");
    let app = app(&dir, "/nonexistent/dsh", "/nonexistent/env");
    let sha = put_blob(&dir, "png", png());
    std::fs::write(
        dir.0.join("cache").join(format!("ocr-{sha}.txt")),
        "cached line one\ncached line two",
    )
    .unwrap();
    let (st, v) = post(&app, json!({"image_blob": sha, "mode": "handwriting", "lang": "en"}), true).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["text"], "cached line one\ncached line two");
    assert_eq!(v["lines"], json!([]));
    assert_eq!(v["engine"], "dsh-read");
}

#[tokio::test]
async fn ocr_bmp_is_accepted() {
    let dir = tmp("bmp");
    let app = app(&dir, "/nonexistent/dsh", "/nonexistent/env");
    let sha = put_blob(&dir, "bmp", bmp());
    let (st, _v) = post(&app, json!({"image_blob": sha}), true).await;
    // 走到了跑 dsh 那一步（工具不存在 → 502），说明 BMP 没被 422 拦。
    assert_eq!(st, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn ocr_missing_tool_is_502() {
    let dir = tmp("dsh");
    let app = app(&dir, "/nonexistent/dsh", "/nonexistent/env");
    let sha = put_blob(&dir, "png", png());
    let (st, v) = post(&app, json!({"image_blob": sha}), true).await;
    assert_eq!(st, StatusCode::BAD_GATEWAY);
    assert_eq!(v["error"], "ocr failed");
    assert!(v["detail"].as_str().unwrap().contains("cannot run"));
    // 图已经拷进 cache（方便人工复现）
    assert!(dir.0.join("cache").join(format!("ocr-{sha}.png")).exists());
}

#[tokio::test]
async fn ocr_empty_result_is_502() {
    let dir = tmp("true");
    // `/usr/bin/true` 在 macOS 和 Linux 都有；它退出 0 但没输出。
    let app = app(&dir, "/usr/bin/true", "/nonexistent/env");
    let sha = put_blob(&dir, "png", png());
    let (st, v) = post(&app, json!({"image_blob": sha}), true).await;
    assert_eq!(st, StatusCode::BAD_GATEWAY);
    assert_eq!(v["error"], "ocr failed");
    assert_eq!(v["detail"], "empty result");
    // 空结果不许写缓存（否则设备永远拿到空文本）
    assert!(!dir.0.join("cache").join(format!("ocr-{sha}.txt")).exists());
}

#[cfg(unix)]
#[tokio::test]
async fn ocr_runs_dsh_with_env_and_caches_text() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tmp("script");
    let script = dir.0.join("fake-dsh.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\n\
         printf '%s\\n' \"$DEEPSEEK_API_KEY\"\n\
         printf '%s/%s\\n' \"$1\" \"$2\"\n\
         printf '%s\\n' \"$3\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let envf = dir.0.join("web.env");
    std::fs::write(&envf, "# dsh env\nDEEPSEEK_API_KEY=sk-test-not-real\n").unwrap();

    let app = app(
        &dir,
        script.to_str().unwrap(),
        envf.to_str().unwrap(),
    );
    let sha = put_blob(&dir, "png", png());
    let (st, v) = post(&app, json!({"image_blob": sha, "mode": "handwriting", "lang": "en"}), true).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let text = v["text"].as_str().unwrap();
    assert!(text.starts_with("sk-test-not-real"), "{text}");
    assert!(text.contains("--profile/headless"), "{text}");
    assert!(text.contains("read_image"), "{text}");

    // 结果落了缓存，内容和回给设备的一致。
    let cached = std::fs::read_to_string(dir.0.join("cache").join(format!("ocr-{sha}.txt"))).unwrap();
    assert_eq!(cached, text);

    // 第二次：脚本删了也照样命中缓存。
    std::fs::remove_file(&script).unwrap();
    let (st, v2) = post(&app, json!({"image_blob": sha}), true).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v2["text"], text);
}
