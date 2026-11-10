//! 收信：IMAP 拉 163 收件箱 → 库 `inbox/`；主题 `!` 开头的信当指令交给 agent 并回信。C19。
//!
//! 脏活（IMAP 登录、163 的 `ID` 命令、MIME/附件解码）全在 `scripts/imap_pull.py`
//! （只用 python3 标准库）。这里只做：跑脚本 → 读 JSON → 附件/正文按 sha 进 `blobs/`
//! → 落 `mirror/inbox/` → 记 entry → 一轮只 `bump_rev()` 一次。
//!
//! - 幂等：脚本自己维护 UID 游标（`<state>/inbox-state.json`），Rust 侧再用
//!   meta `inbox.uid.<uid>` 兜一层，同一个 UID 绝不处理两次。
//! - 脚本路径可用 `INBOX_SCRIPT` 覆盖（测试塞假脚本）。
//! - 日志走 stderr，`journalctl --user -u rmcloud` 能直接看。

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::state::{self, App, Event};

/// 拉信间隔。
pub const INTERVAL: Duration = Duration::from_secs(60);
/// 一次拉信的上限（脚本自己也有 20 封/次，这里防它卡死）。
const PULL_TIMEOUT: Duration = Duration::from_secs(180);
/// 163 IMAP 固定地址（SSL）。收信和发信共用 `cfg.smtp` 的账号/授权码。
const IMAP_HOST: &str = "imap.163.com";
const IMAP_PORT: u16 = 993;
/// 脚本路径覆盖（测试用；生产不设就是编译期仓库里的 `cloud/scripts/imap_pull.py`）。
const SCRIPT_ENV: &str = "INBOX_SCRIPT";
const DEFAULT_SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/imap_pull.py");
/// 附件兜底上限（脚本那边已经挡过 20 MB）。
const MAX_ATTACHMENT: u64 = 20 * 1024 * 1024;

const LAST_UID: &str = "inbox.last_uid";
const LAST_RUN: &str = "inbox.last_run";
const LAST_ERROR: &str = "inbox.last_error";

/// 起后台任务：每 60 s 拉一次。没配 `smtp` 就什么都不做（收信也用这个账号）。
pub fn spawn(app: Arc<App>) {
    if app.cfg.smtp.is_none() {
        eprintln!("[inbox] cfg.smtp 未配置，收信不启动");
        return;
    }
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await; // 第一次立刻跑（重启后不用干等 60 s）
            match run_once(&app).await {
                Ok(0) => {}
                Ok(n) => eprintln!("[inbox] 本轮处理 {n} 封"),
                Err(e) => eprintln!("[inbox] 本轮失败：{e}"),
            }
        }
    });
}

/// 拉一次、处理一次。返回成功处理的邮件数；失败返回中文原因（同时记进 meta）。
pub async fn run_once(app: &App) -> Result<usize, String> {
    if app.cfg.smtp.is_none() {
        return Err("cfg.smtp 未配置".to_string());
    }
    let outdir = std::env::temp_dir().join(format!("rm-inbox-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&outdir);
    let result = tick(app, &outdir).await;
    let _ = std::fs::remove_dir_all(&outdir);
    result
}

async fn tick(app: &App, outdir: &Path) -> Result<usize, String> {
    let mails = match pull(app, outdir).await {
        Ok(mails) => mails,
        Err(e) => {
            app.meta_set(LAST_RUN, &now_iso());
            app.meta_set(LAST_ERROR, &e);
            return Err(e);
        }
    };

    let mut puts: Vec<crate::sync::Put> = Vec::new();
    let mut handled = 0usize;
    let mut last_uid = app
        .meta_get(LAST_UID)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    let mut first_error: Option<String> = None;

    for mail in &mails {
        last_uid = last_uid.max(mail.uid);
        if app.meta_get(&done_key(mail.uid)).is_some() {
            continue; // Rust 侧兜底：同一个 UID 只处理一次
        }
        match handle(app, mail, &mut puts).await {
            Ok(()) => {
                app.meta_set(&done_key(mail.uid), &now_iso());
                handled += 1;
            }
            Err(e) => {
                eprintln!("[inbox] UID {} 处理失败：{e}", mail.uid);
                first_error.get_or_insert(e);
            }
        }
    }

    app.meta_set(LAST_UID, &last_uid.to_string());
    app.meta_set(LAST_RUN, &now_iso());
    if !puts.is_empty() {
        if let Err(e) = commit(app, &puts) {
            eprintln!("[inbox] {e}");
            app.meta_set(LAST_ERROR, &e);
            return Err(e);
        }
    }
    match first_error {
        Some(e) => {
            app.meta_set(LAST_ERROR, &e);
            Err(e)
        }
        None => {
            app.meta_del(LAST_ERROR);
            Ok(handled)
        }
    }
}

// ---------------------------------------------------------------- 一封信

/// 一封邮件（`imap_pull.py` 的 stdout 数组元素）。
#[derive(Debug, Deserialize)]
pub struct Mail {
    pub uid: u64,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub subject: String,
    #[serde(default)]
    pub date: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub attachments: Vec<Attachment>,
}

#[derive(Debug, Deserialize)]
pub struct Attachment {
    #[serde(default)]
    pub filename: String,
    pub path: String,
    #[serde(default)]
    pub size: u64,
}

async fn handle(app: &App, mail: &Mail, puts: &mut Vec<crate::sync::Put>) -> Result<(), String> {
    let date = date_prefix(&mail.date);

    // 1) 附件：读出来按 sha 进 blobs/，再镜像到 inbox/<日期>-<文件名>。
    for att in &mail.attachments {
        let data = tokio::fs::read(&att.path)
            .await
            .map_err(|e| format!("读附件 {} 失败：{e}", att.path))?;
        if data.len() as u64 > MAX_ATTACHMENT {
            eprintln!(
                "[inbox] 附件 {} 超过 20 MB，跳过",
                crate::mail::safe_filename(&att.filename)
            );
            continue;
        }
        let name = safe_name(&att.filename);
        let rel = unique_rel(app, &format!("{date}-{name}"));
        puts.push(store(app, &rel, &data)?);
    }

    // 2) 正文：一行头三行 + 空行 + 正文，进库当普通笔记。
    if !mail.text.trim().is_empty() {
        let note = note_text(mail);
        let rel = unique_rel(app, &format!("{date}-{}.txt", note_title(&mail.subject)));
        puts.push(store(app, &rel, note.as_bytes())?);
    }

    // 3) `!` 指令：交给 agent，结果（或错误）回信给原发件人。
    if let Some(command) = command_of(&mail.subject) {
        let answer = if command.is_empty() {
            "指令是空的：主题里 `!` 后面没有内容。".to_string()
        } else {
            match crate::agent_api::ask_once(app, &command).await {
                Ok(answer) => answer,
                Err(e) => format!("指令执行失败：{e}"),
            }
        };
        if let Err(e) = reply(app, mail, &answer).await {
            eprintln!("[inbox] UID {} 回信失败：{e}", mail.uid);
        }
    }
    Ok(())
}

/// 正文笔记的内容：头三行 + 空行 + 正文（UTF-8）。
fn note_text(mail: &Mail) -> String {
    format!(
        "From: {}\nSubject: {}\nDate: {}\n\n{}\n",
        mail.from.trim(),
        mail.subject.trim(),
        mail.date.trim(),
        mail.text.trim_end()
    )
}

/// 主题是否是指令；是就返回去掉 `!` 和首尾空白后的指令（可能是空串）。
fn command_of(subject: &str) -> Option<String> {
    let trimmed = subject.trim();
    trimmed
        .strip_prefix('!')
        .map(|rest| rest.trim().to_string())
}

/// 回信：`Re: <原标题>`，正文是 agent 的回答（或错误串）。
async fn reply(app: &App, mail: &Mail, body: &str) -> Result<(), String> {
    let smtp = app
        .cfg
        .smtp
        .as_ref()
        .ok_or_else(|| "cfg.smtp 未配置".to_string())?;
    let to = mail.from.trim();
    if !crate::mail::valid_addr(to) {
        return Err(format!("发件人地址不可用：{to:?}"));
    }
    let subject = format!("Re: {}", mail.subject.trim());
    crate::mail::send(smtp, &app.cfg.state, &[to.to_string()], &subject, body, &[])
        .await
        .map(|_| ())
        .map_err(|e| match e {
            crate::mail::SendError::Io(m) => format!("写 .eml 失败：{m}"),
            crate::mail::SendError::Curl(m) => format!("curl 失败：{m}"),
        })
}

// ---------------------------------------------------------------- 落盘/记账

/// 一份数据进 blobs/ + 落 mirror，返回要写进 entry 表的记录。
fn store(app: &App, rel: &str, data: &[u8]) -> Result<crate::sync::Put, String> {
    if !crate::sync::safe_rel(rel) {
        return Err(format!("非法路径：{rel:?}"));
    }
    let sha = hex::encode(Sha256::digest(data));
    crate::sync::store_blob(app, &sha, data).map_err(|e| format!("写 blob 失败（{rel}）：{e}"))?;
    write_atomic(&app.mirror().join(rel), data)
        .map_err(|e| format!("写 mirror 失败（{rel}）：{e}"))?;
    Ok(crate::sync::Put {
        path: rel.to_string(),
        sha,
        size: data.len() as u64,
        mtime: now_ms(),
    })
}

/// 同目录临时文件 + rename（断电不留半截）。
fn write_atomic(dst: &Path, data: &[u8]) -> std::io::Result<()> {
    let parent = dst
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".inbox-{}.tmp", crate::sync::uniq()));
    std::fs::write(&tmp, data)?;
    if let Err(e) = std::fs::rename(&tmp, dst) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// 一次拉取的所有条目在一个事务里进 entry 表，rev 只涨 1。
fn commit(app: &App, puts: &[crate::sync::Put]) -> Result<u64, String> {
    let mut db = app.db();
    let current = state::rev_of(&db);
    let next = crate::sync::apply_entries(&mut db, current, puts, &[])
        .map_err(|e| format!("写 entry 失败：{e}"))?;
    drop(db);
    let _ = app.events.send(Event::Rev(next));
    Ok(next)
}

// ---------------------------------------------------------------- 拉信

async fn pull(app: &App, outdir: &Path) -> Result<Vec<Mail>, String> {
    let Some(smtp) = app.cfg.smtp.as_ref() else {
        return Err("cfg.smtp 未配置".to_string());
    };
    let script = std::env::var(SCRIPT_ENV).unwrap_or_else(|_| DEFAULT_SCRIPT.to_string());
    let state_file = app.cfg.state.join("inbox-state.json");

    let mut cmd = tokio::process::Command::new("python3");
    cmd.arg(&script)
        .arg("--host")
        .arg(IMAP_HOST)
        .arg("--port")
        .arg(IMAP_PORT.to_string())
        .arg("--user")
        .arg(&smtp.user)
        .arg("--pass")
        .arg(&smtp.pass)
        .arg("--state")
        .arg(&state_file)
        .arg("--outdir")
        .arg(outdir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let out = match tokio::time::timeout(PULL_TIMEOUT, cmd.output()).await {
        Err(_) => return Err(format!("imap_pull.py 超时（{} 秒）", PULL_TIMEOUT.as_secs())),
        Ok(Err(e)) => return Err(format!("跑不了 python3 {script}：{e}")),
        Ok(Ok(out)) => out,
    };
    if !out.status.success() {
        let code = out
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        return Err(format!(
            "imap_pull.py 退出码 {code}：{}",
            tail(&String::from_utf8_lossy(&out.stderr), 800)
        ));
    }
    serde_json::from_slice::<Vec<Mail>>(&out.stdout)
        .map_err(|e| format!("imap_pull.py 输出不是 JSON：{e}"))
}

// ---------------------------------------------------------------- 取名/时间

/// 附件名复用 `mail::safe_filename`（basename、去引号/控制字符），再限长。
fn safe_name(raw: &str) -> String {
    crate::mail::safe_filename(raw).chars().take(80).collect()
}

/// 正文笔记的文件名：去掉主题开头的 `!`，空主题给「无主题」。
fn note_title(subject: &str) -> String {
    let title = subject.trim().trim_start_matches('!').trim();
    if title.is_empty() {
        return "无主题".to_string();
    }
    let name: String = crate::mail::safe_filename(title).chars().take(80).collect();
    if name.is_empty() {
        "无主题".to_string()
    } else {
        name
    }
}

/// `<YYYY-MM-DD>-...` 里的日期：取邮件 `Date` 前 10 位，畸形的用今天。
fn date_prefix(date: &str) -> String {
    let d = date.trim();
    let b = d.as_bytes();
    let looks_like_date = b.len() >= 10
        && d.is_char_boundary(10)
        && b[4] == b'-'
        && b[7] == b'-'
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[8..10].iter().all(u8::is_ascii_digit);
    if looks_like_date {
        d[..10].to_string()
    } else {
        crate::mail::iso_utc(now_secs())[..10].to_string()
    }
}

/// 同名文件已存在（不同邮件带了同名附件）时加序号，别互相覆盖。
fn unique_rel(app: &App, name: &str) -> String {
    let first = format!("inbox/{name}");
    if !app.mirror().join(&first).exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 2..1000 {
        let candidate = format!("inbox/{stem}-{n}{ext}");
        if !app.mirror().join(&candidate).exists() {
            return candidate;
        }
    }
    format!("inbox/{stem}-{}", crate::sync::uniq())
}

fn done_key(uid: u64) -> String {
    format!("inbox.uid.{uid}")
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn now_iso() -> String {
    crate::mail::iso_utc(now_secs())
}

/// 取字符串末尾最多 `max` 字节（按字符边界对齐）。
fn tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.trim().to_string();
    }
    let mut i = s.len() - max;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    s[i..].trim().to_string()
}

// ---------------------------------------------------------------- 排障接口

async fn state_route(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    if !app.authorized_any(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    ) {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "unauthorized" })),
        )
            .into_response();
    }
    let last_uid = app
        .meta_get(LAST_UID)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    Json(json!({
        "last_uid": last_uid,
        "last_run": app.meta_get(LAST_RUN),
        "last_error": app.meta_get(LAST_ERROR),
    }))
    .into_response()
}

pub fn routes() -> Router<Arc<App>> {
    Router::new().route("/api/v1/inbox/state", get(state_route))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_prefix_takes_ymd_or_falls_back_to_today() {
        assert_eq!(date_prefix("2026-11-09T10:00:00+08:00"), "2026-11-09");
        assert_eq!(date_prefix("  2026-11-09  "), "2026-11-09");
        // 畸形 → 今天（10 位 ASCII，形态同 YYYY-MM-DD）
        let fallback = date_prefix("not a date");
        assert_eq!(fallback.len(), 10, "{fallback}");
        assert_eq!(&fallback[4..5], "-");
        assert_eq!(&fallback[7..8], "-");
        // 多字节字符不会 panic
        assert_eq!(date_prefix("日期").len(), 10);
    }

    #[test]
    fn command_of_strips_bang_only_at_start() {
        assert_eq!(command_of("!整理这页"), Some("整理这页".into()));
        assert_eq!(command_of("  !  summarise  "), Some("summarise".into()));
        assert_eq!(command_of("!"), Some(String::new()));
        assert_eq!(command_of("普通主题"), None);
        assert_eq!(command_of("主题里有个 ! 号"), None);
    }

    #[test]
    fn note_title_drops_bang_and_falls_back() {
        assert_eq!(note_title("!查课表"), "查课表");
        assert_eq!(note_title(""), "无主题");
        assert_eq!(note_title("!!"), "无主题");
        // 路径分隔符不能带进文件名
        assert_eq!(note_title("a/b"), "b");
    }

    #[test]
    fn safe_name_keeps_basename_only() {
        assert_eq!(safe_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_name(""), "attachment");
        assert_eq!(safe_name("讲义.pdf"), "讲义.pdf");
    }

    #[test]
    fn note_text_has_three_header_lines_then_body() {
        let mail = Mail {
            uid: 1,
            from: " a@b.c ".into(),
            subject: " 题目 ".into(),
            date: "2026-11-09T10:00:00+08:00".into(),
            text: "第一行\n第二行\n\n".into(),
            attachments: Vec::new(),
        };
        let got = note_text(&mail);
        assert_eq!(
            got,
            "From: a@b.c\nSubject: 题目\nDate: 2026-11-09T10:00:00+08:00\n\n第一行\n第二行\n"
        );
    }

    #[test]
    fn tail_is_utf8_safe() {
        let s = "一二三四五";
        assert!(s.ends_with(&tail(s, 5)));
        assert_eq!(tail("  abc  ", 10), "abc");
    }
}
