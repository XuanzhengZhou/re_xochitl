//! 同步六接口的集成测试：直接打 `app_router`，不起真端口、不依赖网络。
//!
//! tower 在本仓库里没有开 `util` feature（`Cargo.toml` 不能改），所以用
//! `tower::Service` 的 `poll_ready` + `call` 手搓一个 oneshot，语义和
//! `ServiceExt::oneshot` 一样。

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use rmcloud::{app_router, App, Config};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::Service;

const TOKEN: &str = "tok-test";

struct Ctx {
    app: Arc<App>,
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
        let dir = std::env::temp_dir().join(format!("rmcloud-sync-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let cfg = Config {
            state: dir.clone(),
            token: TOKEN.into(),
            ..Default::default()
        };
        let app = Arc::new(App::open(cfg).expect("open app"));
        let router = app_router(Arc::clone(&app));
        Ctx { app, router, dir }
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
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024 * 1024)
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

fn json_req(uri: &str, token: Option<&str>, v: &Value) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(uri)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::from(v.to_string())).unwrap()
}

fn blob_req(data: &[u8], sha: &str, token: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri("/api/v1/blob")
        .header("x-sha256", sha)
        .header(header::CONTENT_TYPE, "application/octet-stream");
    if let Some(t) = token {
        b = b.header(header::AUTHORIZATION, format!("Bearer {t}"));
    }
    b.body(Body::from(data.to_vec())).unwrap()
}

fn sha(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

fn parse(body: &[u8]) -> Value {
    serde_json::from_slice(body).unwrap_or(Value::Null)
}

fn err_of(body: &[u8]) -> String {
    parse(body)["error"].as_str().unwrap_or_default().to_string()
}

async fn upload(ctx: &Ctx, data: &[u8]) -> String {
    let s = sha(data);
    let (status, _, body) = send(ctx, blob_req(data, &s, Some(TOKEN))).await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{}", err_of(&body));
    s
}

// ---------------------------------------------------------------- 测试

#[tokio::test]
async fn blob_manifest_and_idempotent_reupload() {
    let cx = Ctx::new("manifest");
    let data = b"hello rM2".to_vec();
    let s = upload(&cx, &data).await;

    // 只传 blob 还没进库：清单里什么都不该有。
    let (status, _, body) = send(&cx, get_req("/api/v1/manifest", Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(parse(&body)["rev"], 0);
    assert!(parse(&body)["entries"].as_array().unwrap().is_empty());

    // 契约写 `sha`+`size`（设备实际发 `sha256`、不发 size，另一个测试覆盖）。
    let (status, _, body) = send(
        &cx,
        json_req(
            "/api/v1/commit",
            Some(TOKEN),
            &json!({
                "base_rev": 0,
                "puts": [{
                    "path": "a.metadata",
                    "sha": s,
                    "size": data.len(),
                    "mtime": 1_700_000_000_000u64,
                }],
                "dels": [],
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", err_of(&body));
    assert_eq!(parse(&body)["rev"], 1);

    let (status, _, body) = send(&cx, get_req("/api/v1/manifest", Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse(&body);
    assert_eq!(v["rev"], 1);
    let entries = v["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["path"], "a.metadata");
    assert_eq!(entries[0]["sha256"], s.as_str());
    assert_eq!(entries[0]["size"], data.len());
    assert_eq!(entries[0]["mtime"], 1_700_000_000_000u64);

    // 幂等：同一个 sha 再传一次还是 204。
    let (status, _, _) = send(&cx, blob_req(&data, &s, Some(TOKEN))).await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // 取回来字节一致。
    let (status, headers, got) = send(
        &cx,
        get_req(&format!("/api/v1/blob/{s}"), Some(TOKEN)),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        headers[header::CONTENT_TYPE].to_str().unwrap(),
        "application/octet-stream"
    );
    assert_eq!(got, data);

    // 没有的 sha → 404。
    let missing = sha(b"nope");
    let (status, _, body) = send(
        &cx,
        get_req(&format!("/api/v1/blob/{missing}"), Some(TOKEN)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(err_of(&body), "no_blob");
}

#[tokio::test]
async fn stale_base_rev_gets_409_rev_changed() {
    let cx = Ctx::new("rev");
    let s = upload(&cx, b"v1").await;
    let commit = json!({
        "base_rev": 0,
        "puts": [{"path": "a.metadata", "sha256": s, "mtime": 10u64}],
        "dels": [],
    });
    let (status, _, body) = send(&cx, json_req("/api/v1/commit", Some(TOKEN), &commit)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(parse(&body)["rev"], 1);
    assert_eq!(parse(&body)["conflicts"], json!([]));

    // 同一个 base_rev 再来一次：必须 409，且 error 字面含 rev_changed（设备端 contains）。
    let (status, _, body) = send(&cx, json_req("/api/v1/commit", Some(TOKEN), &commit)).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let v = parse(&body);
    assert!(
        v["error"].as_str().unwrap_or_default().contains("rev_changed"),
        "{v}"
    );
    assert_eq!(v["rev"], 1);

    // rev 没被第二次提交推动。
    let (_, _, body) = send(&cx, get_req("/api/v1/manifest", Some(TOKEN))).await;
    assert_eq!(parse(&body)["rev"], 1);
}

#[tokio::test]
async fn commit_writes_mirror_and_dels_remove_it() {
    let cx = Ctx::new("mirror");
    let meta = b"{\"visibleName\":\"note\"}";
    let meta_sha = upload(&cx, meta).await;
    let page = b"ink-bytes";
    let page_sha = upload(&cx, page).await;
    let gone = b"bye";
    let gone_sha = upload(&cx, gone).await;

    // 设备真实形状：puts 带 sha256、不带 size；dels 是 {path, mtime} 对象。
    let (status, _, body) = send(
        &cx,
        json_req(
            "/api/v1/commit",
            Some(TOKEN),
            &json!({
                "base_rev": 0,
                "puts": [
                    {"path": "u1.metadata", "sha256": meta_sha, "mtime": 21u64},
                    {"path": "u1/p1.rm", "sha256": page_sha, "mtime": 22u64},
                    {"path": "gone.metadata", "sha256": gone_sha, "mtime": 23u64},
                ],
                "dels": [],
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", err_of(&body));

    let mirror = cx.app.mirror();
    assert_eq!(std::fs::read(mirror.join("u1.metadata")).unwrap(), meta);
    // 嵌套目录自动建出来。
    assert_eq!(std::fs::read(mirror.join("u1/p1.rm")).unwrap(), page);
    assert_eq!(std::fs::read(mirror.join("gone.metadata")).unwrap(), gone);

    // size 设备没发，服务器从 blob 量出来。
    let (_, _, body) = send(&cx, get_req("/api/v1/manifest", Some(TOKEN))).await;
    let entries = parse(&body)["entries"].as_array().unwrap().clone();
    let p1 = entries.iter().find(|e| e["path"] == "u1/p1.rm").unwrap();
    assert_eq!(p1["size"], page.len());

    // 删一个（字符串形式也收），rev 前进到 2。
    let (status, _, body) = send(
        &cx,
        json_req(
            "/api/v1/commit",
            Some(TOKEN),
            &json!({
                "base_rev": 1,
                "puts": [],
                "dels": [{"path": "gone.metadata", "mtime": 24u64}],
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", err_of(&body));
    assert_eq!(parse(&body)["rev"], 2);
    assert!(!mirror.join("gone.metadata").exists());

    let (_, _, body) = send(&cx, get_req("/api/v1/manifest", Some(TOKEN))).await;
    let entries = parse(&body)["entries"].as_array().unwrap().clone();
    assert!(entries.iter().all(|e| e["path"] != "gone.metadata"));

    // 字符串形式的 del 也能删。
    let (status, _, _) = send(
        &cx,
        json_req(
            "/api/v1/commit",
            Some(TOKEN),
            &json!({"base_rev": 2, "puts": [], "dels": ["u1.metadata"]}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!mirror.join("u1.metadata").exists());
}

#[tokio::test]
async fn safe_rel_rejects_escapes_and_empty_segments() {
    for bad in [
        "../x",
        "/etc/passwd",
        "",
        "a//b",
        "./a",
        "a/./b",
        "a/..",
        "..",
        "a/../b",
    ] {
        assert!(!rmcloud::sync::safe_rel(bad), "should reject {bad:?}");
    }
    for ok in ["a", "a/b.rm", "u1.thumbnails/p1.png", "a.b/c-d_e.rm"] {
        assert!(rmcloud::sync::safe_rel(ok), "should accept {ok:?}");
    }
}

#[tokio::test]
async fn blob_sha_mismatch_is_400_and_not_stored() {
    let cx = Ctx::new("mismatch");
    let data = b"payload";
    let wrong = sha(b"other");
    let (status, _, body) = send(&cx, blob_req(data, &wrong, Some(TOKEN))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(err_of(&body), "sha_mismatch");
    assert!(!cx.app.blobs().join(&wrong).exists());

    // 正确的还能传。
    let s = upload(&cx, data).await;
    assert!(cx.app.blobs().join(&s).is_file());
    assert_eq!(std::fs::read(cx.app.blobs().join(&s)).unwrap(), data);
}

#[tokio::test]
async fn every_sync_endpoint_requires_token() {
    let cx = Ctx::new("auth");
    // 配置里是有 token 的，但没带/带错都该 401。
    for req in [
        get_req("/api/v1/manifest", None),
        get_req("/api/v1/manifest", Some("wrong")),
        get_req(&format!("/api/v1/blob/{}", sha(b"x")), None),
        get_req("/api/v1/tree", Some("wrong")),
        get_req("/api/v1/thumb/u1/p1", None),
        blob_req(b"x", &sha(b"x"), None),
        json_req("/api/v1/commit", Some("wrong"), &json!({"base_rev":0,"puts":[],"dels":[]})),
    ] {
        let (status, _, body) = send(&cx, req).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{}", err_of(&body));
    }
}

#[tokio::test]
async fn commit_rejects_unsafe_paths() {
    let cx = Ctx::new("badpath");
    let s = upload(&cx, b"x").await;
    for path in ["../x", "/etc/passwd", "a/../../b"] {
        let (status, _, body) = send(
            &cx,
            json_req(
                "/api/v1/commit",
                Some(TOKEN),
                &json!({"base_rev": 0, "puts": [{"path": path, "sha256": s}], "dels": []}),
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
        assert_eq!(err_of(&body), "bad_path");
    }
    // 恶意路径没写进去，mirror 里也不该有东西。
    assert!(cx.app.entries().is_empty());
    assert_eq!(cx.app.rev(), 0);
}

#[tokio::test]
async fn tree_reads_metadata_and_thumb_serves_png() {
    let cx = Ctx::new("tree");
    let uuid = "a1b2c3";
    let metadata = br#"{"visibleName":"Meeting notes","type":"DocumentType"}"#;
    let png = b"\x89PNG\r\n\x1a\nfake-thumb";
    let rm = b"stroke-data";
    let meta_sha = upload(&cx, metadata).await;
    let png_sha = upload(&cx, png).await;
    let rm_sha = upload(&cx, rm).await;

    let (status, _, body) = send(
        &cx,
        json_req(
            "/api/v1/commit",
            Some(TOKEN),
            &json!({
                "base_rev": 0,
                "puts": [
                    {"path": format!("{uuid}.metadata"), "sha256": meta_sha, "mtime": 1000u64},
                    {"path": format!("{uuid}.thumbnails/p1.png"), "sha256": png_sha, "mtime": 1001u64},
                    {"path": format!("{uuid}/p1.rm"), "sha256": rm_sha, "mtime": 1002u64},
                ],
                "dels": [],
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", err_of(&body));

    let (status, _, body) = send(&cx, get_req("/api/v1/tree", Some(TOKEN))).await;
    assert_eq!(status, StatusCode::OK);
    let v = parse(&body);
    let items = v["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["uuid"], uuid);
    assert_eq!(items[0]["title"], "Meeting notes");
    assert_eq!(items[0]["type"], "DocumentType");
    assert_eq!(items[0]["pages"], 1);
    assert_eq!(items[0]["mtime"], 1000);

    // 没 visibleName 时退回 uuid。
    let no_name = br#"{"type":"DocumentType"}"#;
    let sha2 = upload(&cx, no_name).await;
    let (status, _, _) = send(
        &cx,
        json_req(
            "/api/v1/commit",
            Some(TOKEN),
            &json!({
                "base_rev": 1,
                "puts": [{"path": "plain.metadata", "sha256": sha2, "mtime": 1u64}],
                "dels": [],
            }),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, _, body) = send(&cx, get_req("/api/v1/tree", Some(TOKEN))).await;
    let items = parse(&body)["items"].as_array().unwrap().clone();
    let plain = items.iter().find(|i| i["uuid"] == "plain").unwrap();
    assert_eq!(plain["title"], "plain");

    // 缩略图：裸 page 和带 .png 后缀都要能取。
    for uri in [
        format!("/api/v1/thumb/{uuid}/p1"),
        format!("/api/v1/thumb/{uuid}/p1.png"),
    ] {
        let (status, headers, got) = send(&cx, get_req(&uri, Some(TOKEN))).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(headers[header::CONTENT_TYPE].to_str().unwrap(), "image/png");
        assert_eq!(got, png);
    }

    // 缺页 404。
    let (status, _, body) = send(
        &cx,
        get_req(&format!("/api/v1/thumb/{uuid}/p9"), Some(TOKEN)),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(err_of(&body), "no_thumb");

    // 路径穿越不出去。
    let (status, _, _) = send(
        &cx,
        get_req("/api/v1/thumb/..%2F..%2Fetc/passwd", Some(TOKEN)),
    )
    .await;
    assert!(status == StatusCode::BAD_REQUEST || status == StatusCode::NOT_FOUND);
}

/// 设备写出来的缩略图是 `<uuid>.thumbnails/<页 id>.jpg`（实测 280×374 JPEG），
/// 而 Web UI 拿的是页序号。老实现只认 `<序号>.png` → 永远 404 → Web UI 一片灰块。
#[tokio::test]
async fn thumb_resolves_page_index_to_page_id_and_serves_jpeg() {
    let ctx = Ctx::new("thumb");
    let uuid = "d-thumb";
    let content = serde_json::json!({"cPages": {"pages": [
        {"id": "page-aaa", "deleted": true},
        {"id": "page-bbb"},
        {"id": "page-ccc"},
    ]}});
    let dir = ctx.app.mirror().join(format!("{uuid}.thumbnails"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        ctx.app.mirror().join(format!("{uuid}.content")),
        content.to_string(),
    )
    .unwrap();
    // 第 0 页被 deleted 跳过 → 序号 0 == page-bbb
    std::fs::write(dir.join("page-bbb.jpg"), b"JPEGDATA").unwrap();

    let (code, hdr, body) = send(&ctx, get_req(&format!("/api/v1/thumb/{uuid}/0"), Some(TOKEN))).await;
    assert_eq!(code, StatusCode::OK, "序号 0 应该落到 page-bbb");
    assert_eq!(
        hdr.get("content-type").unwrap(),
        "image/jpeg",
        "设备写的是 JPEG，不能谎报 PNG"
    );
    assert_eq!(&body[..], b"JPEGDATA");

    // 越界 → 404 而不是 panic
    let (code, _, _) = send(&ctx, get_req(&format!("/api/v1/thumb/{uuid}/9"), Some(TOKEN))).await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}
