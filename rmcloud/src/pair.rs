//! 配对：一次性 8 位码 → token。C2。
//!
//! 码由 `rmcloud pair` 生成（`main.rs` 直接调 [`new_code`]），打印给用户，
//! 设备端在 Pairing code 页输入；5 分钟有效、用一次即作废。这是**唯一**
//! 不校验 token 的写接口。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::state::App;

/// 配对码有效期：5 分钟。
const TTL_MS: u64 = 300_000;

/// 8 位码的字母表：去掉 0/O/1/I（手抄、口述时最容易认错）。
const ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

/// 生成一次性配对码（5 分钟有效），打印给用户，设备端输入它换 token。
pub fn new_code(cfg: &Config) -> anyhow::Result<String> {
    // 服务可能没在跑：自己开库、自己建表。
    let db = crate::state::open_db(&cfg.state)?;
    let code = gen_code();
    let exp = now_ms() + TTL_MS;
    let upsert = "INSERT INTO meta(k,v) VALUES(?1,?2)
                  ON CONFLICT(k) DO UPDATE SET v=excluded.v";
    db.execute(upsert, rusqlite::params!["pair_code", &code])?;
    db.execute(upsert, rusqlite::params!["pair_exp", &exp.to_string()])?;
    Ok(code)
}

#[derive(Deserialize)]
struct PairBody {
    #[serde(default)]
    code: String,
    #[serde(default)]
    device_name: String,
}

async fn pair(State(app): State<Arc<App>>, Json(body): Json<PairBody>) -> Response {
    let code = body.code.trim().to_ascii_uppercase();
    if code.is_empty() {
        return fail(StatusCode::UNAUTHORIZED, "bad_code");
    }

    let stored = app.meta_get("pair_code").unwrap_or_default();
    let exp = app
        .meta_get("pair_exp")
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let now = now_ms();
    if stored.is_empty() || code != stored || now >= exp {
        // 过期的码顺手清掉，别让它一直躺在库里。
        if !stored.is_empty() && now >= exp {
            app.meta_del("pair_code");
            app.meta_del("pair_exp");
        }
        return fail(StatusCode::UNAUTHORIZED, "bad_code");
    }

    // 先原子地占用码：两个请求同时拿同一个码，只有一个能过这关。
    if !app.meta_take("pair_code", &stored) {
        return fail(StatusCode::UNAUTHORIZED, "bad_code");
    }
    app.meta_del("pair_exp");

    let token = random_hex(32);
    let device_id = format!("d-{}", random_hex(4));
    // 配置写不进去就别往下走（码已作废，重新 `rmcloud pair` 生成一个即可）。
    let mut cfg = app.cfg.clone();
    cfg.token = token.clone();
    if let Err(e) = cfg.save() {
        eprintln!("[pair] save config: {e}");
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "save_config");
    }
    app.meta_set("token", &token);
    app.meta_set("device_id", &device_id);
    app.meta_set("device_name", &body.device_name);
    app.meta_set("paired_at", &now.to_string());

    (
        StatusCode::OK,
        Json(json!({
            "token": token,
            "device_id": device_id,
            "server_time": now,
        })),
    )
        .into_response()
}

fn fail(code: StatusCode, msg: &str) -> Response {
    (code, Json(json!({ "error": msg }))).into_response()
}

fn gen_code() -> String {
    random_bytes(8)
        .into_iter()
        .map(|b| ALPHABET[b as usize % ALPHABET.len()] as char)
        .collect()
}

fn random_hex(n: usize) -> String {
    hex::encode(random_bytes(n))
}

/// `/dev/urandom` 取随机字节；没有的话（不该发生）用时间 + pid 反复 sha256 凑。
fn random_bytes(n: usize) -> Vec<u8> {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let mut b = vec![0u8; n];
        if f.read_exact(&mut b).is_ok() {
            return b;
        }
    }
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut seed = format!(
        "{}-{}-{}",
        now_ms(),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let d = Sha256::digest(seed.as_bytes());
        out.extend_from_slice(&d);
        seed = hex::encode(&d);
    }
    out.truncate(n);
    out
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/api/v1/pair", post(pair))
}
