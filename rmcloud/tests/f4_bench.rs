//! 临时：量服务器侧 blob 落盘/取回（跑完删）。`cargo test --release --test f4_bench -- --nocapture`

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use rmcloud::{app_router, App, Config};
use sha2::{Digest, Sha256};
use tower::Service;

const TOKEN: &str = "tok-bench";

fn ctx(tag: &str) -> (Arc<App>, axum::Router, PathBuf) {
    let dir = std::env::temp_dir().join(format!("rmcloud-f4-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let cfg = Config {
        state: dir.clone(),
        token: TOKEN.into(),
        ..Default::default()
    };
    let app = Arc::new(App::open(cfg).expect("open app"));
    let router = app_router(app.clone());
    (app, router, dir)
}

async fn shot(router: &mut axum::Router, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let mut r = router.clone();
    std::future::poll_fn(|cx| {
        <axum::Router as Service<Request<Body>>>::poll_ready(&mut r, cx)
    })
    .await
    .expect("ready");
    let res = <axum::Router as Service<Request<Body>>>::call(&mut r, req)
        .await
        .expect("call");
    let st = res.status();
    let body = axum::body::to_bytes(res.into_body(), 64 << 20)
        .await
        .unwrap()
        .to_vec();
    (st, body)
}

#[tokio::test(flavor = "multi_thread")]
async fn f4_server_blob_timing() {
    let (app, mut router, dir) = ctx("blob");
    for (tag, size) in [("10 KB", 10 * 1024), ("110 KB", 110 * 1024), ("420 KB", 420 * 1024)] {
        let data = vec![7u8; size];
        let sha = hex::encode(Sha256::digest(&data));
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/blob")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header("x-sha256", &sha)
            .body(Body::from(data.clone()))
            .unwrap();
        let t = Instant::now();
        let (st, _) = shot(&mut router, req).await;
        let first = t.elapsed();
        // 第二次：同一个 sha，store_blob 只做 sqlite 更新
        let req = Request::builder()
            .method("POST")
            .uri("/api/v1/blob")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .header("x-sha256", &sha)
            .body(Body::from(data.clone()))
            .unwrap();
        let t = Instant::now();
        let (_st2, _) = shot(&mut router, req).await;
        let again = t.elapsed();
        // 取回（浏览器那边 GET /api/v1/blob/<sha>）
        let req = Request::builder()
            .method("GET")
            .uri(format!("/api/v1/blob/{sha}"))
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap();
        let t = Instant::now();
        let (st3, body) = shot(&mut router, req).await;
        let get = t.elapsed();
        println!(
            "PUT {tag}: {st} 首次 {:>7.2} ms（含 sha256+落盘+sqlite），重复 {:>7.2} ms；\
             GET {:>7.2} ms {st3} → {} B",
            first.as_secs_f64() * 1e3,
            again.as_secs_f64() * 1e3,
            get.as_secs_f64() * 1e3,
            body.len()
        );
    }
    let _ = std::fs::remove_dir_all(dir);
    drop(app);
}
