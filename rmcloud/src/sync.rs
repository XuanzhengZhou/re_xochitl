//! 同步六个接口：manifest/blob/commit/tree/thumb。C3。
//!
//! 对外契约以设备端 `src/net.rs` / `src/sync.rs` 为准：字段名、状态码、错误串
//! 一个字都不改。和 `docs/CLOUD.md` 有出入的地方（设备实际发什么）在注释里标了。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::body::to_bytes;
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::state::{self, App, EntryRow, Event};

/// 单个 blob 上限。设备自己 32 MB 就拒了（`src/sync.rs:22`），这里挡坏客户端。
pub const MAX_BLOB: u64 = 64 * 1024 * 1024;

/// commit 的 JSON 上限：整库清单可能上千条，给宽点。
const MAX_JSON: usize = 8 * 1024 * 1024;

// ---------------------------------------------------------------- 校验

/// 路径必须是相对路径，且不许空段、`.`、`..`。
/// 与设备端 `src/sync.rs::safe_rel` 保持一致（它会把畸形路径整条丢弃）。
pub fn safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('/')
        && !rel
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
}

fn safe_sha(sha: &str) -> bool {
    sha.len() == 64 && sha.bytes().all(|b| b.is_ascii_hexdigit())
}

// ---------------------------------------------------------------- 响应小工具

fn fail(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

fn unauthorized() -> Response {
    fail(StatusCode::UNAUTHORIZED, "unauthorized")
}

fn rev_changed(rev: u64) -> Response {
    (
        StatusCode::CONFLICT,
        Json(json!({ "error": "rev_changed", "rev": rev })),
    )
        .into_response()
}

fn authed(app: &App, headers: &HeaderMap) -> bool {
    app.authorized_any(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    )
}

// ---------------------------------------------------------------- manifest

#[derive(Serialize)]
struct ManifestEntry {
    path: String,
    sha256: String,
    size: u64,
    mtime: u64,
}

async fn manifest(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if !authed(&app, &headers) {
        return unauthorized();
    }
    let (rev, rows) = app.snapshot();
    let entries: Vec<ManifestEntry> = rows
        .into_iter()
        .map(|e| ManifestEntry {
            path: e.path,
            sha256: e.sha,
            size: e.size,
            mtime: e.mtime,
        })
        .collect();
    // 不做墓碑：设备端 `parse_manifest` 本来就把没有 sha256 的条目当"不存在"，
    // 对三路比较来说墓碑和"从未存在"等价。
    Json(json!({ "rev": rev, "entries": entries })).into_response()
}

// ---------------------------------------------------------------- blob

async fn blob_put(State(app): State<Arc<App>>, headers: HeaderMap, req: Request) -> Response {
    if !authed(&app, &headers) {
        return unauthorized();
    }
    let Some(want) = headers
        .get("x-sha256")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_ascii_lowercase())
    else {
        return fail(StatusCode::BAD_REQUEST, "missing_sha");
    };
    if !safe_sha(&want) {
        return fail(StatusCode::BAD_REQUEST, "bad_sha");
    }
    // 不用 `Bytes` 提取器：它的 2 MB 默认上限和 413 文案都不是我们要的。
    let body = match to_bytes(req.into_body(), MAX_BLOB as usize).await {
        Ok(b) => b,
        Err(_) => return fail(StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
    };
    let got = hex::encode(Sha256::digest(&body));
    if got != want {
        return fail(StatusCode::BAD_REQUEST, "sha_mismatch");
    }
    match store_blob(&app, &want, &body) {
        // 已存在也回 204：内容寻址天然幂等（`net.rs:398` 认 200/204）。
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            eprintln!("[sync] store blob {want}: {e}");
            fail(StatusCode::INTERNAL_SERVER_ERROR, "store_failed")
        }
    }
}

/// 临时文件 + rename：并发/断电都不会留下半截 blob（同目录保证 rename 原子）。
///
/// 收信模块也用它（附件/正文都先按 sha 进 blobs/）。
pub fn store_blob(app: &App, sha: &str, data: &[u8]) -> std::io::Result<()> {
    let dir = app.blobs();
    std::fs::create_dir_all(&dir)?;
    let dst = dir.join(sha);
    if !dst.is_file() {
        let tmp = dir.join(format!(".{sha}.{}.tmp", uniq()));
        std::fs::write(&tmp, data)?;
        if let Err(e) = std::fs::rename(&tmp, &dst) {
            let _ = std::fs::remove_file(&tmp);
            // 另一个并发请求刚好先 rename 成功：内容一样，算成功。
            if !dst.is_file() {
                return Err(e);
            }
        }
    }
    app.blob_insert(sha, data.len() as u64);
    Ok(())
}

async fn blob_get(
    State(app): State<Arc<App>>,
    UrlPath(sha): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    if !authed(&app, &headers) {
        return unauthorized();
    }
    let sha = sha.to_ascii_lowercase();
    if !safe_sha(&sha) {
        return fail(StatusCode::NOT_FOUND, "no_blob");
    }
    match std::fs::read(app.blobs().join(&sha)) {
        Ok(bytes) => {
            let mut res = (StatusCode::OK, bytes).into_response();
            res.headers_mut().insert(
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            );
            res
        }
        Err(_) => fail(StatusCode::NOT_FOUND, "no_blob"),
    }
}

// ---------------------------------------------------------------- commit

#[derive(Deserialize)]
struct CommitReq {
    base_rev: u64,
    #[serde(default)]
    puts: Vec<PutReq>,
    #[serde(default)]
    dels: Vec<DelReq>,
}

#[derive(Deserialize)]
struct PutReq {
    path: String,
    /// 设备发 `sha256`（`net.rs:417` 的 `Put`），契约文档写 `sha`；两个都收。
    #[serde(default, alias = "sha")]
    sha256: String,
    /// 设备不发 size（它只有 sha256+mtime），缺了就从刚上传的 blob 量。
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    mtime: u64,
}

/// `dels` 设备发对象 `{path, mtime}`（`src/sync.rs:165`），契约写字符串数组；两个都收。
#[derive(Deserialize)]
#[serde(untagged)]
enum DelReq {
    Path(String),
    Obj { path: String },
}

/// 一条待写进 entry 表的记录（`apply_entries` 的入参）。收信模块复用。
pub struct Put {
    pub path: String,
    pub sha: String,
    pub size: u64,
    pub mtime: u64,
}

async fn commit(State(app): State<Arc<App>>, headers: HeaderMap, req: Request) -> Response {
    if !authed(&app, &headers) {
        return unauthorized();
    }
    let body = match to_bytes(req.into_body(), MAX_JSON).await {
        Ok(b) => b,
        Err(_) => return fail(StatusCode::PAYLOAD_TOO_LARGE, "too_large"),
    };
    let Ok(req) = serde_json::from_slice::<CommitReq>(&body) else {
        return fail(StatusCode::BAD_REQUEST, "bad_json");
    };

    let mut dels = Vec::with_capacity(req.dels.len());
    for d in &req.dels {
        let path = match d {
            DelReq::Path(p) => p,
            DelReq::Obj { path } => path,
        };
        if !safe_rel(path) {
            return fail(StatusCode::BAD_REQUEST, "bad_path");
        }
        dels.push(path.clone());
    }

    let mut puts = Vec::with_capacity(req.puts.len());
    for p in &req.puts {
        if !safe_rel(&p.path) {
            return fail(StatusCode::BAD_REQUEST, "bad_path");
        }
        let sha = p.sha256.trim().to_ascii_lowercase();
        if !safe_sha(&sha) {
            return fail(StatusCode::BAD_REQUEST, "bad_sha");
        }
        // blob 在就量真实大小（设备没发 size）；不在也不能不记账，用客户端给的/0。
        let size = match std::fs::metadata(app.blobs().join(&sha)) {
            Ok(md) => md.len(),
            Err(_) => p.size.unwrap_or(0),
        };
        puts.push(Put {
            path: p.path.clone(),
            sha,
            size,
            mtime: p.mtime,
        });
    }

    let current = {
        let db = app.db();
        state::rev_of(&db)
    };
    if current != req.base_rev {
        return rev_changed(current);
    }
    if puts.is_empty() && dels.is_empty() {
        // 设备端只有 puts/dels 非空才 commit（`src/sync.rs:191`），这里只是兜底：
        // 空提交不该白涨 rev。
        return Json(json!({ "rev": current, "conflicts": [] })).into_response();
    }

    // 先把 blob 拷成 mirror 里的临时文件；rev 闸门过了再 rename 到位。
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (i, p) in puts.iter().enumerate() {
        let src = app.blobs().join(&p.sha);
        if !src.is_file() {
            // 清单指向空 blob，设备下载时会 404 并重试；先记账，别丢记录。
            eprintln!("[sync] commit: blob {} missing for {}", p.sha, p.path);
            continue;
        }
        let dst = app.mirror().join(&p.path);
        let Some(parent) = dst.parent() else {
            cleanup(&staged);
            return fail(StatusCode::BAD_REQUEST, "bad_path");
        };
        if let Err(e) = std::fs::create_dir_all(parent) {
            cleanup(&staged);
            eprintln!("[sync] mkdir {}: {e}", parent.display());
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "mirror_failed");
        }
        let tmp = app.mirror().join(format!(".commit-{}-{i}.tmp", uniq()));
        if let Err(e) = std::fs::copy(&src, &tmp) {
            let _ = std::fs::remove_file(&tmp);
            cleanup(&staged);
            eprintln!("[sync] stage {}: {e}", p.path);
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "mirror_failed");
        }
        staged.push((tmp, dst));
    }

    // rev 闸门 + 落盘 + 记账在同一把库锁里：别的 commit 插不进来。
    let mut db = app.db();
    let current = state::rev_of(&db);
    if current != req.base_rev {
        drop(db);
        cleanup(&staged);
        return rev_changed(current);
    }
    for (tmp, dst) in &staged {
        if let Err(e) = std::fs::rename(tmp, dst) {
            drop(db);
            cleanup(&staged);
            eprintln!("[sync] rename into mirror: {e}");
            return fail(StatusCode::INTERNAL_SERVER_ERROR, "mirror_failed");
        }
    }
    for p in &dels {
        match std::fs::remove_file(app.mirror().join(p)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("[sync] remove mirror {p}: {e}"),
        }
    }
    let next = current + 1;
    if let Err(e) = apply_entries(&mut db, current, &puts, &dels) {
        drop(db);
        eprintln!("[sync] commit db: {e}");
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "commit_failed");
    }
    drop(db);
    let _ = app.events.send(Event::Rev(next));

    Json(json!({ "rev": next, "conflicts": [] })).into_response()
}

/// 写 entry 表 + rev，一个事务。调用方已确认 rev 闸门。
///
/// 收信模块把一次拉取的所有条目攒起来调一次（rev 只涨 1）。
pub fn apply_entries(
    db: &mut rusqlite::Connection,
    current: u64,
    puts: &[Put],
    dels: &[String],
) -> rusqlite::Result<u64> {
    let next = current + 1;
    let tx = db.transaction()?;
    for p in puts {
        tx.execute(
            "INSERT INTO entry(path,sha,size,mtime) VALUES(?1,?2,?3,?4)
             ON CONFLICT(path) DO UPDATE SET
                 sha=excluded.sha, size=excluded.size, mtime=excluded.mtime",
            rusqlite::params![p.path, p.sha, p.size as i64, p.mtime as i64],
        )?;
    }
    for p in dels {
        tx.execute("DELETE FROM entry WHERE path=?1", [p])?;
    }
    tx.execute(
        "INSERT INTO meta(k,v) VALUES('rev',?1)
         ON CONFLICT(k) DO UPDATE SET v=excluded.v",
        [next.to_string()],
    )?;
    tx.commit()?;
    Ok(next)
}

fn cleanup(staged: &[(PathBuf, PathBuf)]) {
    for (tmp, _) in staged {
        let _ = std::fs::remove_file(tmp);
    }
}

// ---------------------------------------------------------------- tree / thumb

#[derive(Serialize)]
struct TreeItem {
    uuid: String,
    title: String,
    #[serde(rename = "type")]
    kind: String,
    pages: u64,
    mtime: u64,
}

async fn tree(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if !authed(&app, &headers) {
        return unauthorized();
    }
    let mirror = app.mirror();
    let entries: Vec<EntryRow> = app.entries();
    let mut items = Vec::new();
    let Ok(rd) = std::fs::read_dir(&mirror) else {
        return Json(json!({ "items": [] })).into_response();
    };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        let Some(uuid) = name.strip_suffix(".metadata") else {
            continue;
        };
        if !safe_rel(uuid) {
            continue;
        }
        let meta = std::fs::read(ent.path())
            .ok()
            .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            .unwrap_or(Value::Null);
        let title = meta
            .get("visibleName")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or(uuid)
            .to_string();
        let kind = meta
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("DocumentType")
            .to_string();
        let mtime = entries
            .iter()
            .find(|e| e.path == name)
            .map(|e| e.mtime)
            .unwrap_or_else(|| file_mtime_ms(&ent.path()));
        items.push(TreeItem {
            uuid: uuid.to_string(),
            title,
            kind,
            pages: pages_of(&mirror, uuid),
            mtime,
        });
    }
    items.sort_by(|a, b| a.uuid.cmp(&b.uuid));
    Json(json!({ "items": items })).into_response()
}

/// 页数：优先数 `<uuid>/*.rm`；PDF/EPUB 没有 `.rm`，退回 `.content` 的页数。
fn pages_of(mirror: &Path, uuid: &str) -> u64 {
    let mut n = 0u64;
    if let Ok(rd) = std::fs::read_dir(mirror.join(uuid)) {
        n = rd
            .flatten()
            .filter(|e| e.path().extension().map(|x| x == "rm").unwrap_or(false))
            .count() as u64;
    }
    if n > 0 {
        return n;
    }
    let Ok(bytes) = std::fs::read(mirror.join(format!("{uuid}.content"))) else {
        return 0;
    };
    let Ok(v) = serde_json::from_slice::<Value>(&bytes) else {
        return 0;
    };
    if let Some(n) = v.get("pageCount").and_then(|x| x.as_u64()) {
        return n;
    }
    v.get("cPages")
        .and_then(|c| c.get("pages"))
        .and_then(|p| p.as_array())
        .map(|a| a.len() as u64)
        .unwrap_or(0)
}

async fn thumb(
    State(app): State<Arc<App>>,
    UrlPath((uuid, page)): UrlPath<(String, String)>,
    headers: HeaderMap,
) -> Response {
    if !authed(&app, &headers) {
        return unauthorized();
    }
    // 设备写出来的缩略图是 **`<uuid>.thumbnails/<页 id>.jpg`**（280×374 JPEG，实测），
    // 而 Web UI 拿的是**页序号**。所以要先从 `.content` 的 `cPages.pages[i].id`
    // 把页序号换成页 id，再按 id 试 `.jpg` / `.png` 两种后缀。
    //
    // 老实现只认 `<页序号>.png` —— 永远 404，Web UI 里就是一片灰块（真机验过）。
    let page = page.strip_suffix(".png").unwrap_or(&page).to_string();
    if page.is_empty() || !safe_rel(&uuid) || !safe_rel(&page) {
        return fail(StatusCode::BAD_REQUEST, "bad_path");
    }
    let dir = app.mirror().join(format!("{uuid}.thumbnails"));
    // 先按页 id 直接找（调用方可传 id），再按序号换成 id
    let mut cands: Vec<String> = Vec::new();
    if !page.bytes().all(|b| b.is_ascii_digit()) {
        cands.push(page.clone());
    } else if let Ok(idx) = page.parse::<usize>() {
        if let Some(id) = page_id_at(&app, &uuid, idx) {
            cands.push(id);
        }
    }
    // 兜底：序号本身也可能就是文件名（老约定 / 手写的镜像）
    cands.push(page.clone());
    for name in cands {
        if !safe_rel(&name) {
            continue;
        }
        for (ext, mime) in [("jpg", "image/jpeg"), ("png", "image/png")] {
            if let Ok(bytes) = std::fs::read(dir.join(format!("{name}.{ext}"))) {
                let mut res = (StatusCode::OK, bytes).into_response();
                res.headers_mut()
                    .insert(header::CONTENT_TYPE, HeaderValue::from_static(mime));
                return res;
            }
        }
    }
    fail(StatusCode::NOT_FOUND, "no_thumb")
}

/// `.content` 里第 `idx` 页的页 id（和设备的 `Library::page_id_at` 同一套规则：
/// 跳过 `deleted: true` 的页）。读不到就 `None`。
fn page_id_at(app: &App, uuid: &str, idx: usize) -> Option<String> {
    let raw = std::fs::read_to_string(app.mirror().join(format!("{uuid}.content"))).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let mut seen = 0usize;
    for p in v.pointer("/cPages/pages")?.as_array()? {
        if p.get("deleted").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        let id = p.get("id")?.as_str()?;
        if seen == idx {
            return Some(id.to_string());
        }
        seen += 1;
    }
    None
}

// ---------------------------------------------------------------- 杂项

/// 临时文件名后缀：纳秒 + 进程内自增，够用且不用引 rand/uuid。
pub fn uniq() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{t}-{n}")
}

fn file_mtime_ms(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/v1/manifest", get(manifest))
        .route("/api/v1/blob", post(blob_put))
        .route("/api/v1/blob/{sha}", get(blob_get))
        .route("/api/v1/commit", post(commit))
        .route("/api/v1/tree", get(tree))
        .route("/api/v1/thumb/{uuid}/{page}", get(thumb))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_rel_rejects_traversal_and_empty_segments() {
        for bad in [
            "../x",
            "/etc/passwd",
            "",
            "a//b",
            "./a",
            "a/./b",
            "a/..",
            "..",
        ] {
            assert!(!safe_rel(bad), "{bad:?}");
        }
        assert!(safe_rel("a.metadata"));
        assert!(safe_rel("u1/p1.rm"));
    }

    #[test]
    fn safe_sha_needs_64_hex() {
        assert!(safe_sha(&"a".repeat(64)));
        assert!(safe_sha(&"0123456789abcdef".repeat(4)));
        assert!(!safe_sha("deadbeef"));
        assert!(!safe_sha(&"g".repeat(64)));
        assert!(!safe_sha(&"a".repeat(63)));
    }
}
