//! rmcloud 启动器。模块在 `lib.rs`，这里只做配置、配对子命令、监听。

use std::sync::Arc;

use rmcloud::{app_router, pair, App, Config};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = Config::load()?;
    // `rmcloud pair`：生成一次性配对码就退出（设备端 `Pairing code` 页输入它）。
    if std::env::args().nth(1).as_deref() == Some("pair") {
        println!("{}", pair::new_code(&cfg)?);
        return Ok(());
    }
    let mut addrs = vec![cfg.bind.clone()];
    if !cfg.local_bind.is_empty() && cfg.local_bind != cfg.bind {
        addrs.push(cfg.local_bind.clone());
    }
    let inbox = cfg.inbox;
    let app = Arc::new(App::open(cfg)?);
    if inbox {
        rmcloud::inbox::spawn(app.clone());
    } else {
        eprintln!("[inbox] 收信轮询已关闭（配置 inbox=false）");
    }
    let mut tasks = Vec::new();
    for a in addrs {
        let router = app_router(app.clone());
        let listener = tokio::net::TcpListener::bind(&a).await?;
        eprintln!("[rmcloud] listening on http://{a}");
        tasks.push(tokio::spawn(async move { axum::serve(listener, router).await }));
    }
    for t in tasks {
        let _ = t.await?;
    }
    Ok(())
}
