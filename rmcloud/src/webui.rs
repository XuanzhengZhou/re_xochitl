//! Web UI 外壳（C16）：`GET /`、`/app.js`、`/app.css`，三个文件都 `include_str!` 进二进制。
//!
//! 这里**只发常量字节，不碰文件系统**：没有静态目录映射，所以 `/../Cargo.toml`、
//! `/app.js/../../etc/passwd` 这类路径只会落到 404（`tests/webui.rs` 有回归）。
//!
//! 鉴权不在这一层：外壳里没有任何数据，设备/笔记/聊天全部走已有 API
//! （`/api/v1/tree`、`/api/v1/manifest`、`/api/v1/blob`、`/api/v1/thumb`、
//! `/api/v1/tool/render`、`/api/v1/agent/ask`），那些 handler 的 Bearer 校验原样保留。
//! token 只存在浏览器 `localStorage`，由 `app.js` 放进 `Authorization` 头，
//! 因此不会出现在 HTML、URL、浏览器历史和服务器访问日志里。

use std::sync::Arc;

use axum::http::{header, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;

use crate::state::App;

const INDEX: &str = include_str!("../web/index.html");
const APP_JS: &str = include_str!("../web/app.js");
const APP_CSS: &str = include_str!("../web/app.css");

/// 所有静态响应共用的头：类型固定 + 禁嗅探 + 禁 referrer + 禁 iframe。
///
/// CSP 只放行同源（脚本/样式/接口）和 `blob:` 图片——页图是 `fetch` 下来再用
/// `URL.createObjectURL` 挂上去的（`<img src>` 带不了 Authorization 头）。
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
                   img-src 'self' blob: data:; connect-src 'self'; object-src 'none'; \
                   base-uri 'none'; frame-ancestors 'none'";

fn asset(body: &'static str, content_type: &'static str) -> Response {
    (
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (header::CACHE_CONTROL, HeaderValue::from_static("no-cache")),
            (
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(CSP),
            ),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
            (header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY")),
            (
                header::REFERRER_POLICY,
                HeaderValue::from_static("no-referrer"),
            ),
        ],
        body,
    )
        .into_response()
}

async fn index() -> Response {
    asset(INDEX, "text/html; charset=utf-8")
}

async fn app_js() -> Response {
    asset(APP_JS, "text/javascript; charset=utf-8")
}

async fn app_css() -> Response {
    asset(APP_CSS, "text/css; charset=utf-8")
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/", get(index))
        .route("/app.js", get(app_js))
        .route("/app.css", get(app_css))
}
