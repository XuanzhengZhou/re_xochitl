//! 进程内共享状态。所有 handler 拿 `State<Arc<App>>`。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use rusqlite::Connection;
use serde_json::Value;
use tokio::sync::{broadcast, oneshot};

use crate::config::Config;

/// 推给设备的事件（`GET /api/v1/events` 的 SSE 流）。
#[derive(Clone, Debug)]
pub enum Event {
    /// 库有新的 rev，设备该 `sync_once` 了。
    Rev(u64),
    /// 设备命令（§3.3）：{id, op, ...}。
    Cmd(Value),
}

pub struct App {
    pub cfg: Config,
    pub db: Mutex<Connection>,
    pub events: broadcast::Sender<Event>,
    /// dsh 一次只跑一个（rM2 一次只问一个，串行最省心）。
    pub dsh_lock: tokio::sync::Mutex<()>,
    /// 设备命令的等待表：id → 回包的 oneshot。
    pub pend: Mutex<HashMap<String, oneshot::Sender<Value>>>,
}

impl App {
    pub fn open(cfg: Config) -> anyhow::Result<App> {
        for sub in ["blobs", "mirror", "cache", "cache/tex"] {
            std::fs::create_dir_all(cfg.state.join(sub))?;
        }
        let db = Connection::open(cfg.state.join("rmcloud.sqlite"))?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS blob(sha TEXT PRIMARY KEY, size INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS entry(path TEXT PRIMARY KEY, sha TEXT NOT NULL,
                 size INTEGER NOT NULL, mtime INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS usage(ts INTEGER NOT NULL, mode TEXT NOT NULL,
                 session TEXT, in_tok INTEGER NOT NULL, out_tok INTEGER NOT NULL,
                 cost REAL NOT NULL, ms INTEGER NOT NULL);",
        )?;
        let (events, _) = broadcast::channel(64);
        Ok(App {
            cfg,
            db: Mutex::new(db),
            events,
            dsh_lock: tokio::sync::Mutex::new(()),
            pend: Mutex::new(HashMap::new()),
        })
    }

    pub fn db(&self) -> MutexGuard<'_, Connection> {
        self.db.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn blobs(&self) -> PathBuf {
        self.cfg.state.join("blobs")
    }

    pub fn mirror(&self) -> PathBuf {
        self.cfg.state.join("mirror")
    }

    /// 当前 rev（没有就是 0）。
    pub fn rev(&self) -> u64 {
        self.db()
            .query_row("SELECT v FROM meta WHERE k='rev'", [], |r| r.get::<_, String>(0))
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    }

    pub fn bump_rev(&self) -> u64 {
        let next = self.rev() + 1;
        let _ = self.db().execute(
            "INSERT INTO meta(k,v) VALUES('rev',?1)
             ON CONFLICT(k) DO UPDATE SET v=excluded.v",
            [next.to_string()],
        );
        let _ = self.events.send(Event::Rev(next));
        next
    }

    /// 校验设备 token（`/api/v1/health` 和 `/pair` 除外）。
    pub fn authorized(&self, header: Option<&str>) -> bool {
        let Some(h) = header else { return false };
        let Some(got) = h.strip_prefix("Bearer ") else { return false };
        !self.cfg.token.is_empty() && got == self.cfg.token
    }
}

impl App {
    /// 让设备做一件只读的事（`render` / `strokes` / `search`），等它回结果。
    /// 下发走 `Event::Cmd`（SSE），回收走 `POST /api/v1/device/result`。
    pub async fn call_device(&self, op: &str, args: Value) -> anyhow::Result<Value> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(1);
        let id = format!(
            "c{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        );
        let (tx, rx) = oneshot::channel();
        self.pend
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.clone(), tx);
        let mut cmd = args;
        if let Value::Object(m) = &mut cmd {
            m.insert("id".into(), Value::String(id.clone()));
            m.insert("op".into(), Value::String(op.to_string()));
        } else {
            cmd = serde_json::json!({"id": id, "op": op, "args": cmd});
        }
        if self.events.send(Event::Cmd(cmd)).is_err() {
            self.pend
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&id);
            anyhow::bail!("no device connected");
        }
        let secs = op_timeout_secs(op);
        match tokio::time::timeout(std::time::Duration::from_secs(secs), rx).await {
            Ok(Ok(v)) => {
                if v.get("error").is_some() {
                    anyhow::bail!("device error: {v}");
                }
                Ok(v)
            }
            Ok(Err(_)) => anyhow::bail!("device channel closed"),
            Err(_) => {
                self.pend
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&id);
                // 设备在深挂起时 WiFi/USB 一起掉，事件发出去也没人收 ——
                // 这时等满超时纯属干等，文案要让调用方能提示"设备在睡觉"。
                anyhow::bail!("device timeout ({secs}s)")
            }
        }
    }
}

/// 每个设备命令的等待时间。`render` 是整页光栅化 + PNG 编码 + 上传，8 s 够；
/// `search` 要全库扫 `.metadata` 和逐页文字层，大库十几秒很正常 —— 8 s 会
/// 无辜超时（实测过）。按 op 分开给。
fn op_timeout_secs(op: &str) -> u64 {
    match op {
        "search" => 25,
        "strokes" => 12,
        _ => 8,
    }
}

// ---------------------------------------------------------------- 库表访问

/// 一条库记录（manifest / tree 用）。
#[derive(Clone, Debug)]
pub struct EntryRow {
    pub path: String,
    pub sha: String,
    pub size: u64,
    pub mtime: u64,
}

/// 打开（必要时建表）`<state>/rmcloud.sqlite`。
///
/// `App::open` 也做同样的事，但 `rmcloud pair` 在服务没跑时就要写配对码，
/// 所以给一个独立入口；两处 schema 必须一致（只加不改）。
pub fn open_db(state: &std::path::Path) -> anyhow::Result<Connection> {
    std::fs::create_dir_all(state)?;
    let db = Connection::open(state.join("rmcloud.sqlite"))?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta(k TEXT PRIMARY KEY, v TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS blob(sha TEXT PRIMARY KEY, size INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS entry(path TEXT PRIMARY KEY, sha TEXT NOT NULL,
             size INTEGER NOT NULL, mtime INTEGER NOT NULL);
         CREATE TABLE IF NOT EXISTS usage(ts INTEGER NOT NULL, mode TEXT NOT NULL,
             session TEXT, in_tok INTEGER NOT NULL, out_tok INTEGER NOT NULL,
             cost REAL NOT NULL, ms INTEGER NOT NULL);",
    )?;
    Ok(db)
}

/// 从**已加锁**的连接读 rev。`App::rev()` 会再 lock 一次，持锁/事务路径里用它。
pub fn rev_of(db: &Connection) -> u64 {
    db.query_row("SELECT v FROM meta WHERE k='rev'", [], |r| {
        r.get::<_, String>(0)
    })
    .ok()
    .and_then(|s| s.parse().ok())
    .unwrap_or(0)
}

/// 从**已加锁**的连接读全库条目，按 path 排序。
pub fn entries_of(db: &Connection) -> Vec<EntryRow> {
    let Ok(mut stmt) = db.prepare("SELECT path, sha, size, mtime FROM entry ORDER BY path") else {
        return Vec::new();
    };
    let rows = stmt.query_map([], |r| {
        Ok(EntryRow {
            path: r.get(0)?,
            sha: r.get(1)?,
            size: r.get::<_, i64>(2)? as u64,
            mtime: r.get::<_, i64>(3)? as u64,
        })
    });
    match rows {
        Ok(it) => it.flatten().collect(),
        Err(_) => Vec::new(),
    }
}

impl App {
    pub fn meta_get(&self, k: &str) -> Option<String> {
        self.db()
            .query_row("SELECT v FROM meta WHERE k=?1", [k], |r| r.get(0))
            .ok()
    }

    pub fn meta_set(&self, k: &str, v: &str) -> bool {
        self.db()
            .execute(
                "INSERT INTO meta(k,v) VALUES(?1,?2)
                 ON CONFLICT(k) DO UPDATE SET v=excluded.v",
                [k, v],
            )
            .is_ok()
    }

    pub fn meta_del(&self, k: &str) -> bool {
        self.db().execute("DELETE FROM meta WHERE k=?1", [k]).is_ok()
    }

    /// 一次性消费一个 meta 键：值等于 `v` 才删，并发下只有一个请求能拿到 `true`。
    pub fn meta_take(&self, k: &str, v: &str) -> bool {
        self.db()
            .execute("DELETE FROM meta WHERE k=?1 AND v=?2", [k, v])
            .map(|n| n == 1)
            .unwrap_or(false)
    }

    /// 全库条目，按 path 排序（manifest 顺序稳定，方便人看/测试）。
    pub fn entries(&self) -> Vec<EntryRow> {
        entries_of(&self.db())
    }

    /// 一次锁内取 `(rev, entries)`。manifest 必须用这个：分两次取的话，
    /// 另一台设备刚好 commit，会拿到"新 rev + 旧条目"，设备会误判成服务器删了文件。
    pub fn snapshot(&self) -> (u64, Vec<EntryRow>) {
        let db = self.db();
        (rev_of(&db), entries_of(&db))
    }

    /// 记一笔 blob（幂等；内容寻址的 sha 不会变）。
    pub fn blob_insert(&self, sha: &str, size: u64) {
        let _ = self.db().execute(
            "INSERT INTO blob(sha,size) VALUES(?1,?2)
             ON CONFLICT(sha) DO UPDATE SET size=excluded.size",
            rusqlite::params![sha, size as i64],
        );
    }

    /// 校验 Bearer token。
    ///
    /// **meta 里的 token 优先**：配对待在服务运行中完成时 `App::cfg` 是只读的，
    /// 靠 meta 才能立刻生效，重新配对也会让旧 token 马上失效。meta 里还没有
    /// token（老配置/测试直接给 `cfg.token`）时才回退到配置。
    pub fn authorized_any(&self, header: Option<&str>) -> bool {
        let Some(got) = header.and_then(|h| h.strip_prefix("Bearer ")) else {
            return false;
        };
        let got = got.trim();
        if got.is_empty() {
            return false;
        }
        match self.meta_get("token") {
            Some(t) if !t.is_empty() => t == got,
            _ => !self.cfg.token.is_empty() && self.cfg.token == got,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::op_timeout_secs;

    /// `search` 全库扫描实测会超过 8 s，别把它和 `render` 用一个超时。
    #[test]
    fn per_op_timeouts_differ() {
        assert_eq!(op_timeout_secs("render"), 8);
        assert_eq!(op_timeout_secs("search"), 25);
        assert_eq!(op_timeout_secs("strokes"), 12);
        // 未知 op 走默认，不 panic
        assert_eq!(op_timeout_secs("whatever"), 8);
        assert_eq!(op_timeout_secs(""), 8);
    }
}
