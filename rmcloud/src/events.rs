//! 事件通道：设备订阅 `GET /api/v1/events`，服务器把「有新版本」和「设备命令」推下去。
//!
//! 设备端（`src/cloud/events.rs`）收 `rev` 就去 `sync_once`，收 `cmd` 就执行只读操作，
//! 把结果 POST 回 `/api/v1/device/result`，这里按 id 唤醒等着的那次 `call_device`。

#![allow(dead_code, unused_imports)]

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::stream::poll_fn;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::state::{App, Event};

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers.get("authorization").and_then(|v| v.to_str().ok())
}

/// SSE：把广播通道翻成事件流。设备重连后靠 `rev` 自己补同步。
async fn events(State(app): State<Arc<App>>, headers: HeaderMap) -> impl IntoResponse {
    if !app.authorized_any(bearer(&headers)) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }
    let mut rx = app.events.subscribe();
    let (tx, mut out) = mpsc::channel::<Result<SseEvent, Infallible>>(8);
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(Event::Rev(rev)) => {
                    let ev = SseEvent::default()
                        .event("rev")
                        .data(json!({"rev": rev}).to_string());
                    if tx.send(Ok(ev)).await.is_err() {
                        break;
                    }
                }
                Ok(Event::Cmd(cmd)) => {
                    let ev = SseEvent::default().event("cmd").data(cmd.to_string());
                    if tx.send(Ok(ev)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }
    });
    let stream = poll_fn(move |cx| out.poll_recv(cx));
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(20)).text("hb"))
        .into_response()
}

#[derive(Debug, Deserialize)]
struct DeviceResult {
    id: String,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    result: Value,
    #[serde(default)]
    error: String,
}

/// 设备把命令结果送回来。
async fn device_result(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<DeviceResult>,
) -> impl IntoResponse {
    if !app.authorized_any(bearer(&headers)) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "unauthorized"})),
        )
            .into_response();
    }
    let payload = if body.ok {
        body.result
    } else {
        json!({"error": if body.error.is_empty() { "device error" } else { &body.error }})
    };
    let waiter = app
        .pend
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&body.id);
    match waiter {
        Some(tx) => {
            let _ = tx.send(payload);
            (StatusCode::OK, Json(json!({"ok": true}))).into_response()
        }
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "unknown or expired command id"})),
        )
            .into_response(),
    }
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/v1/events", get(events))
        .route("/api/v1/device/result", post(device_result))
}
