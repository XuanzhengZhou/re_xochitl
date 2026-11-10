//! 成本记账 + `GET /api/v1/usage`。C7。
//!
//! 计价常量是 DeepSeek 官方价（**美元 / 百万 token**，2026-11 的价格，要改就改这里）：
//! - 输入缓存未命中 `IN_MISS = 0.27`
//! - 输入缓存命中 `IN_HIT = 0.07`
//! - 输出 `OUT = 1.10`
//!
//! `record` 只拿到总输入 token，分不清命中/未命中，所以**一律按 miss 价算**（保守）。

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::App;

/// 输入，缓存未命中（美元/百万 token）。
pub const IN_MISS: f64 = 0.27;
/// 输入，缓存命中（美元/百万 token）。`record` 拿不到命中信息，暂不使用。
pub const IN_HIT: f64 = 0.07;
/// 输出（美元/百万 token）。
pub const OUT: f64 = 1.10;

const PER_M: f64 = 1_000_000.0;

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/api/v1/usage", get(usage))
}

/// 记一次调用。`mode` 建议 `ask|select|live|review|organize|ocr`。
/// 发给别人调：`usage::record(&app, "ask", Some(&sid), in_tok, out_tok, ms)`。
pub fn record(app: &App, mode: &str, session: Option<&str>, in_tok: u64, out_tok: u64, ms: u64) {
    let ts = now();
    let c = cost(in_tok, out_tok);
    let _ = app.db().execute(
        "INSERT INTO usage(ts, mode, session, in_tok, out_tok, cost, ms)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            ts,
            mode,
            session,
            in_tok as i64,
            out_tok as i64,
            c,
            ms as i64
        ],
    );
}

/// 一次调用的费用（美元）。输入按 miss 价。
pub fn cost(in_tok: u64, out_tok: u64) -> f64 {
    (in_tok as f64 * IN_MISS + out_tok as f64 * OUT) / PER_M
}

#[derive(Debug, Default, Deserialize)]
pub struct UsageQ {
    #[serde(default)]
    days: Option<String>,
}

async fn usage(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<UsageQ>,
) -> Response {
    if !app.authorized_any(bearer(&headers)) {
        return fail(StatusCode::UNAUTHORIZED, "unauthorized", None);
    }
    let days = q
        .days
        .as_deref()
        .and_then(|s| s.parse::<i64>().ok())
        .unwrap_or(7)
        .clamp(1, 365);
    let since = now() - days * 86_400;

    let db = app.db();
    let total = match db.query_row(
        "SELECT COALESCE(SUM(in_tok), 0), COALESCE(SUM(out_tok), 0),
                COALESCE(SUM(cost), 0.0), COUNT(*)
         FROM usage WHERE ts >= ?1",
        [since],
        |r| {
            Ok(json!({
                "in": r.get::<_, i64>(0)?,
                "out": r.get::<_, i64>(1)?,
                "cost": round(r.get::<_, f64>(2)?),
                "calls": r.get::<_, i64>(3)?,
            }))
        },
    ) {
        Ok(v) => v,
        Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, "db", Some(e.to_string())),
    };

    let by_day = match db.prepare(
        "SELECT date(ts, 'unixepoch', 'localtime') AS day,
                COALESCE(SUM(in_tok), 0), COALESCE(SUM(out_tok), 0),
                COALESCE(SUM(cost), 0.0), COUNT(*)
         FROM usage WHERE ts >= ?1
         GROUP BY day ORDER BY day",
    ) {
        Ok(mut st) => match st.query_map([since], |r| {
            Ok(json!({
                "day": r.get::<_, String>(0)?,
                "in": r.get::<_, i64>(1)?,
                "out": r.get::<_, i64>(2)?,
                "cost": round(r.get::<_, f64>(3)?),
                "calls": r.get::<_, i64>(4)?,
            }))
        }) {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect::<Vec<Value>>(),
            Err(e) => {
                return fail(StatusCode::INTERNAL_SERVER_ERROR, "db", Some(e.to_string()))
            }
        },
        Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, "db", Some(e.to_string())),
    };

    let by_mode = match db.prepare(
        "SELECT mode, COALESCE(SUM(in_tok), 0) AS in_tok,
                COALESCE(SUM(out_tok), 0) AS out_tok,
                COALESCE(SUM(cost), 0.0) AS cost, COUNT(*) AS calls
         FROM usage WHERE ts >= ?1
         GROUP BY mode ORDER BY cost DESC, mode",
    ) {
        Ok(mut st) => match st.query_map([since], |r| {
            Ok(json!({
                "mode": r.get::<_, String>(0)?,
                "in": r.get::<_, i64>(1)?,
                "out": r.get::<_, i64>(2)?,
                "cost": round(r.get::<_, f64>(3)?),
                "calls": r.get::<_, i64>(4)?,
            }))
        }) {
            Ok(rows) => rows.filter_map(|r| r.ok()).collect::<Vec<Value>>(),
            Err(e) => {
                return fail(StatusCode::INTERNAL_SERVER_ERROR, "db", Some(e.to_string()))
            }
        },
        Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, "db", Some(e.to_string())),
    };

    Json(json!({
        "total": total,
        "by_day": by_day,
        "by_mode": by_mode,
    }))
    .into_response()
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 金额留 8 位（单 token 就是 2.7e-7 美元，6 位会把小数抹平）。
fn round(x: f64) -> f64 {
    (x * 1e8).round() / 1e8
}

fn bearer(h: &HeaderMap) -> Option<&str> {
    h.get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
}

fn fail(status: StatusCode, error: &str, detail: Option<String>) -> Response {
    let mut body = json!({"error": error});
    if let Some(d) = detail {
        if let Some(o) = body.as_object_mut() {
            o.insert("detail".into(), json!(d));
        }
    }
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-12, "{a} != {b}");
    }

    #[test]
    fn cost_zero() {
        assert_eq!(cost(0, 0), 0.0);
    }

    #[test]
    fn cost_one_million_each() {
        close(cost(1_000_000, 0), IN_MISS); // 0.27
        close(cost(0, 1_000_000), OUT); // 1.10
        close(cost(1_000_000, 1_000_000), 1.37);
    }

    #[test]
    fn cost_one_token() {
        close(cost(1, 0), 0.000_000_27);
        close(cost(0, 1), 0.000_001_10);
        close(cost(1, 1), 0.000_001_37);
    }

    #[test]
    fn cost_is_linear() {
        close(cost(3, 7), (3.0 * IN_MISS + 7.0 * OUT) / PER_M);
        close(cost(2_000, 4_000), cost(1_000, 2_000) * 2.0);
    }

    #[test]
    fn hit_price_is_cheaper() {
        assert!(IN_HIT < IN_MISS);
    }

    #[test]
    fn round_keeps_eight_digits() {
        assert_eq!(round(0.1 + 0.2), 0.3);
        assert_eq!(round(1.234_567_89), 1.234_567_89);
        assert_eq!(round(0.000_000_27), 0.000_000_27);
    }
}
