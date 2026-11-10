//! agent 网关的「图物化」集成测试：真跑 handler + 假 dsh（把收到的参数落盘）。
//!
//! 覆盖：整页图 + 裁图两个**绝对路径**都真实存在、无 rect 只给一张、
//! `crop_blob` 优先且与整页同 sha 时去重、blob 缺失/坏 PNG 时降级成
//! 「图不可用」但请求仍然成功、缩图后整页 ≤ 1600 而裁图保留原分辨率。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::Body;
use axum::http::Request;
use rmcloud::png::{self, Color, Image};
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
    let dir = std::env::temp_dir().join(format!("rmcloud-img-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

/// 假 dsh 通过 env 文件拿到落盘路径（顺便把 `cfg.dsh_env` 的注入也验了）。
fn app_with_capture() -> (Arc<App>, PathBuf) {
    let dir = temp_state();
    let capture = dir.join("prompt.txt");
    let env = dir.join("dsh.env");
    std::fs::write(&env, format!("FAKE_DSH_PROMPT={}\n", capture.display())).expect("写 env");
    let cfg = Config {
        state: dir.join("state"),
        token: "test-token".into(),
        dsh: fixture("fake-dsh-capture.sh"),
        dsh_env: env.to_string_lossy().into_owned(),
        ..Config::default()
    };
    (Arc::new(App::open(cfg).expect("App::open")), capture)
}

/// 造一张「白纸 + 稀疏笔迹」的灰度页。
fn page(w: u32, h: u32) -> Image {
    let mut pixels = vec![255u8; (w * h) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            if (x * 7 + y * 11) % 251 < 3 {
                pixels[y * w as usize + x] = 0;
            }
        }
    }
    Image {
        width: w,
        height: h,
        color: Color::Gray,
        pixels,
    }
}

fn store(app: &App, sha: &str, bytes: &[u8]) {
    rmcloud::sync::store_blob(app, sha, bytes).expect("存 blob");
}

fn sha_of(tag: u8) -> String {
    format!("{tag:02x}").repeat(32)
}

async fn ask(app: Arc<App>, body: Value) -> (u16, String) {
    let request = Request::builder()
        .method("POST")
        .uri("/api/v1/agent/ask")
        .header("content-type", "application/json")
        .header("authorization", "Bearer test-token")
        .body(Body::from(serde_json::to_vec(&body).expect("序列化")))
        .expect("构造请求");
    let response = app_router(app).oneshot(request).await.expect("handler");
    let status = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("读响应体");
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn prompt_of(capture: &Path) -> String {
    std::fs::read_to_string(capture).unwrap_or_else(|e| panic!("假 dsh 没收到 prompt：{e}"))
}

/// prompt 里所有以 `/` 开头、`.png` 结尾的绝对路径（按出现顺序）。
fn png_paths(prompt: &str) -> Vec<String> {
    prompt
        .split(|c: char| c.is_whitespace() || "，。、（）()：:,".contains(c))
        .filter(|t| t.starts_with('/') && t.ends_with(".png"))
        .map(str::to_string)
        .collect()
}

fn cache_pngs(app: &App) -> Vec<PathBuf> {
    let dir = app.cfg.state.join("cache/ask");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "png"))
        .collect();
    out.sort();
    out
}

/// 用我们自己的解码器量尺寸（顺便验证落盘的图是真 PNG）。
fn dims_of(path: &str) -> (u32, u32) {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("图不在 {path}：{e}"));
    let img = png::decode(&bytes).unwrap_or_else(|e| panic!("{path} 解不开：{e}"));
    (img.width, img.height)
}

// ---------------------------------------------------------------- 正常路径

#[tokio::test]
async fn select_gives_two_existing_absolute_images() {
    let (app, capture) = app_with_capture();
    let sha = sha_of(0xab);
    store(&app, &sha, &png::encode(&page(702, 936)));

    let body = json!({
        "mode": "select",
        "doc": "12345678-90ab-cdef-1234-567890abcdef",
        "page": 3,
        "rect": [100.0, 200.0, 400.0, 600.0],
        "image_blob": sha,
        "text": "这题选什么？",
    });
    let (status, sse) = ask(app.clone(), body).await;
    assert_eq!(status, 200, "sse:\n{sse}");
    assert!(sse.contains("event: done"), "sse:\n{sse}");

    let prompt = prompt_of(&capture);
    let paths = png_paths(&prompt);
    assert_eq!(paths.len(), 2, "prompt 里应有两个图路径：\n{prompt}");
    for path in &paths {
        assert!(Path::new(path).is_absolute(), "不是绝对路径：{path}");
        assert!(Path::new(path).is_file(), "prompt 指的图不存在：{path}");
    }
    // 整页 702×936（长边没超 1600，原样落盘）
    assert_eq!(dims_of(&paths[0]), (702, 936));
    // 设备 rect 是 1404×1872 坐标，图是它的一半 → 裁 (50,100)-(200,300)
    assert_eq!(dims_of(&paths[1]), (150, 200));
    assert!(paths[1].contains("-rect-100-200-400-600.png"), "{}", paths[1]);

    // prompt：明确指令 + 只看一次 + 选区在整页图里的像素范围 + 两套坐标
    assert!(prompt.contains("先用 read_image 看这张整页渲染图"), "{prompt}");
    assert!(prompt.contains("整页图只看一次 read_image"), "{prompt}");
    assert!(prompt.contains("选区细节图（原始分辨率的局部裁片"), "{prompt}");
    assert!(prompt.contains("分辨率：整页图 702×936 像素，对应设备坐标 1404×1872"), "{prompt}");
    assert!(prompt.contains("像素 x∈[50, 200), y∈[100, 300)"), "{prompt}");
    assert!(prompt.contains("设备坐标 x∈[100, 400), y∈[200, 600)"), "{prompt}");
    // 原有约束不许被新段落挤掉
    assert!(prompt.contains("$...$"), "{prompt}");
    assert!(prompt.contains("不要用 markdown 代码围栏"), "{prompt}");
    assert!(prompt.contains("只答选区问题但要能读全页"), "{prompt}");
}

#[tokio::test]
async fn no_rect_gives_only_the_page() {
    let (app, capture) = app_with_capture();
    let sha = sha_of(0xcd);
    store(&app, &sha, &png::encode(&page(600, 800)));

    let body = json!({
        "mode": "live",
        "doc": "abcdef12-0000-0000-0000-000000000000",
        "page": 1,
        "image_blob": sha,
        "text": "这道题怎么想？",
    });
    let (status, sse) = ask(app.clone(), body).await;
    assert_eq!(status, 200, "sse:\n{sse}");

    let prompt = prompt_of(&capture);
    let paths = png_paths(&prompt);
    assert_eq!(paths.len(), 1, "无 rect 只该给整页图：\n{prompt}");
    assert_eq!(dims_of(&paths[0]), (600, 800));
    assert!(prompt.contains("整页图即选区"), "{prompt}");
    assert!(!prompt.contains("-rect-"), "{prompt}");
}

#[tokio::test]
async fn page_is_downscaled_while_crop_keeps_source_resolution() {
    let (app, capture) = app_with_capture();
    let sha = sha_of(0x11);
    store(&app, &sha, &png::encode(&page(1404, 1872)));

    let body = json!({
        "mode": "select",
        "doc": "12345678-90ab-cdef-1234-567890abcdef",
        "page": 3,
        "rect": [100.0, 200.0, 400.0, 600.0],
        "image_blob": sha,
    });
    let (status, _) = ask(app.clone(), body).await;
    assert_eq!(status, 200);

    let prompt = prompt_of(&capture);
    let paths = png_paths(&prompt);
    assert_eq!(paths.len(), 2, "\n{prompt}");
    // 整页 1404×1872 → 长边缩到 1600 → 1200×1600
    assert_eq!(dims_of(&paths[0]), (1200, 1600));
    // 裁图从**原图**裁，保留 1:1 分辨率
    assert_eq!(dims_of(&paths[1]), (300, 400));
    assert!(prompt.contains("分辨率：整页图 1200×1600 像素"), "{prompt}");
    assert!(prompt.contains("像素 x∈[85, 342), y∈[171, 513)"), "{prompt}");
}

#[tokio::test]
async fn crop_blob_wins_over_server_side_crop() {
    let (app, capture) = app_with_capture();
    let page_sha = sha_of(0x22);
    let crop_sha = sha_of(0x33);
    store(&app, &page_sha, &png::encode(&page(702, 936)));
    store(&app, &crop_sha, &png::encode(&page(120, 160)));

    let body = json!({
        "mode": "select",
        "doc": "12345678-90ab-cdef-1234-567890abcdef",
        "page": 3,
        "rect": [100.0, 200.0, 400.0, 600.0],
        "image_blob": page_sha,
        "crop_blob": crop_sha,
    });
    let (status, _) = ask(app.clone(), body).await;
    assert_eq!(status, 200);

    let prompt = prompt_of(&capture);
    let paths = png_paths(&prompt);
    assert_eq!(paths.len(), 2, "\n{prompt}");
    assert!(paths[0].ends_with(&format!("{page_sha}.png")), "{}", paths[0]);
    assert!(paths[1].ends_with(&format!("{crop_sha}.png")), "{}", paths[1]);
    // 设备裁的尺寸原样用，服务器不该再自己裁
    assert_eq!(dims_of(&paths[1]), (120, 160));
    let files = cache_pngs(&app);
    assert_eq!(files.len(), 2, "多落了文件：{files:?}");
    assert!(
        files.iter().all(|f| !f.to_string_lossy().contains("-rect-")),
        "不该有服务器端裁图：{files:?}"
    );
    // 即便细节图来自设备，像素范围还是要写给模型
    assert!(prompt.contains("像素 x∈[50, 200), y∈[100, 300)"), "{prompt}");
}

#[tokio::test]
async fn crop_blob_equal_to_page_is_one_file_and_one_path() {
    let (app, capture) = app_with_capture();
    let sha = sha_of(0x44); // 整页模式：设备把 crop_blob 也设成整页

    store(&app, &sha, &png::encode(&page(600, 800)));
    let body = json!({
        "mode": "live",
        "doc": "abcdef12-0000-0000-0000-000000000000",
        "page": 1,
        "image_blob": sha,
        "crop_blob": sha,
        "text": "看看这页",
    });
    let (status, _) = ask(app.clone(), body).await;
    assert_eq!(status, 200);

    let prompt = prompt_of(&capture);
    let paths = png_paths(&prompt);
    assert_eq!(paths.len(), 1, "同一个 sha 只说一次：\n{prompt}");
    assert!(prompt.contains("整页图即选区"), "{prompt}");
    assert_eq!(cache_pngs(&app).len(), 1, "同一个 sha 不该落两份");
}

// ---------------------------------------------------------------- 降级

#[tokio::test]
async fn missing_blob_degrades_without_failing() {
    let (app, capture) = app_with_capture();
    let body = json!({
        "mode": "select",
        "doc": "12345678-90ab-cdef-1234-567890abcdef",
        "page": 3,
        "rect": [10.0, 20.0, 300.0, 400.0],
        "image_blob": sha_of(0x55),
        "text": "这题选什么？",
    });
    let (status, sse) = ask(app.clone(), body).await;
    assert_eq!(status, 200, "sse:\n{sse}");
    assert!(sse.contains("event: done"), "图挂了也不该让请求失败：\n{sse}");

    let prompt = prompt_of(&capture);
    assert!(prompt.contains("图不可用（blob 不存在："), "{prompt}");
    assert!(png_paths(&prompt).is_empty(), "{prompt}");
    // 文字部分照常
    assert!(prompt.contains("这题选什么？"), "{prompt}");
}

#[tokio::test]
async fn broken_png_degrades_without_failing() {
    let (app, capture) = app_with_capture();
    let sha = sha_of(0x66);
    store(&app, &sha, b"definitely not a png");
    let body = json!({
        "mode": "review",
        "doc": "12345678-90ab-cdef-1234-567890abcdef",
        "page": 2,
        "image_blob": sha,
    });
    let (status, sse) = ask(app.clone(), body).await;
    assert_eq!(status, 200, "sse:\n{sse}");
    assert!(sse.contains("event: done"), "{sse}");

    let prompt = prompt_of(&capture);
    assert!(prompt.contains("图不可用（PNG 解不开"), "{prompt}");
    assert!(png_paths(&prompt).is_empty(), "{prompt}");
}

#[tokio::test]
async fn bad_crop_blob_falls_back_to_server_side_crop() {
    let (app, capture) = app_with_capture();
    let sha = sha_of(0x77);
    store(&app, &sha, &png::encode(&page(702, 936)));
    let body = json!({
        "mode": "select",
        "doc": "12345678-90ab-cdef-1234-567890abcdef",
        "page": 3,
        "rect": [100.0, 200.0, 400.0, 600.0],
        "image_blob": sha,
        "crop_blob": "not-a-sha",
    });
    let (status, _) = ask(app.clone(), body).await;
    assert_eq!(status, 200);

    // 设备给的裁图用不了，就自己按 rect 裁——不能让整页图也跟着丢
    let prompt = prompt_of(&capture);
    let paths = png_paths(&prompt);
    assert_eq!(paths.len(), 2, "\n{prompt}");
    assert!(paths[1].contains("-rect-100-200-400-600.png"), "{}", paths[1]);
    assert_eq!(dims_of(&paths[1]), (150, 200));
    assert!(!prompt.contains("没有可用的细节图"), "{prompt}");
}

#[tokio::test]
async fn unusable_selection_degrades_to_the_page_only() {
    let (app, capture) = app_with_capture();
    let sha = sha_of(0x99);
    store(&app, &sha, &png::encode(&page(702, 936)));
    let body = json!({
        "mode": "select",
        "doc": "12345678-90ab-cdef-1234-567890abcdef",
        "page": 3,
        "rect": [0.0, 0.0, 4.0, 4.0], // 折算后 2×2，够不上 8×8
        "image_blob": sha,
    });
    let (status, sse) = ask(app.clone(), body).await;
    assert_eq!(status, 200, "sse:\n{sse}");
    assert!(sse.contains("event: done"), "{sse}");

    let prompt = prompt_of(&capture);
    let paths = png_paths(&prompt);
    assert_eq!(paths.len(), 1, "裁不出细节图时只给整页图：\n{prompt}");
    assert!(prompt.contains("没有可用的细节图（选区太小"), "{prompt}");
    assert!(prompt.contains("只能看整页图"), "{prompt}");
}

// ---------------------------------------------------------------- 幂等

#[tokio::test]
async fn cache_is_idempotent_and_leaves_no_tmp_files() {
    let (app, capture) = app_with_capture();
    let sha = sha_of(0x88);
    store(&app, &sha, &png::encode(&page(1404, 1872)));
    let body = json!({
        "mode": "select",
        "doc": "12345678-90ab-cdef-1234-567890abcdef",
        "page": 3,
        "rect": [100.0, 200.0, 400.0, 600.0],
        "image_blob": sha,
    });

    assert_eq!(ask(app.clone(), body.clone()).await.0, 200);
    let first: Vec<_> = cache_pngs(&app)
        .iter()
        .map(|p| {
            let m = std::fs::metadata(p).expect("metadata");
            (p.clone(), m.len(), m.modified().expect("mtime"))
        })
        .collect();
    assert_eq!(first.len(), 2);

    // 第二次（换个 prompt）不该重写缓存
    let (status, _) = ask(app.clone(), body).await;
    assert_eq!(status, 200);
    let second: Vec<_> = cache_pngs(&app)
        .iter()
        .map(|p| {
            let m = std::fs::metadata(p).expect("metadata");
            (p.clone(), m.len(), m.modified().expect("mtime"))
        })
        .collect();
    assert_eq!(first, second, "第二次请求重写了缓存");

    // 落盘走 tmp+rename，不该留临时文件
    let leftovers: Vec<_> = std::fs::read_dir(app.cfg.state.join("cache/ask"))
        .expect("读缓存目录")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "留了临时文件：{leftovers:?}");

    // 第二次的 prompt 仍然完整（缓存复用不能把 prompt 内容省掉）
    let prompt = prompt_of(&capture);
    assert!(prompt.matches("先用 read_image 看这张整页渲染图").count() >= 2);
    assert_eq!(png_paths(&prompt).len(), 4); // 两次请求 × 两个路径
}
