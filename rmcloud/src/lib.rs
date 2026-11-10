//! rmcloud 库：模块装配 + 路由。`main.rs` 只负责启动，测试直接 `app_router()`。

pub mod agent_api;
pub mod ai;
pub mod config;
pub mod events;
pub mod inbox;
pub mod latex;
pub mod mail;
pub mod ocr;
pub mod pair;
pub mod png;
pub mod profile;
pub mod state;
pub mod sync;
pub mod tex;
pub mod usage;
pub mod webui;

use std::sync::Arc;

use axum::{routing::get, Json, Router};
use serde_json::json;

pub use crate::config::Config;
pub use crate::state::{App, Event};

pub async fn health() -> Json<serde_json::Value> {
    Json(json!({"ok": true, "service": "rmcloud", "version": env!("CARGO_PKG_VERSION")}))
}

pub fn app_router(app: Arc<App>) -> Router {
    Router::new()
        .route("/api/v1/health", get(health))
        .merge(pair::routes())
        .merge(sync::routes())
        .merge(events::routes())
        .merge(agent_api::routes())
        .merge(tex::routes())
        .merge(mail::routes())
        .merge(inbox::routes())
        .merge(ocr::routes())
        .merge(usage::routes())
        .merge(profile::routes())
        .merge(webui::routes())
        .with_state(app)
}
