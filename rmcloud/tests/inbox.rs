//! 收信模块集成测试：假 `imap_pull.py`（固定 JSON）、假 dsh、假 curl 顶替一切外部依赖。
//!
//! 不连 163、不发真信、不调真模型。覆盖：附件/正文进库 + rev 只涨 1、同 JSON 跑两次
//! 幂等、`!` 主题走 agent 并回信、脚本失败不 panic 且 `last_error` 有内容。

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use rmcloud::config::Smtp;
use rmcloud::{app_router, inbox, App, Config};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

/// `INBOX_SCRIPT` / `RMCLOUD_CURL` 是进程级环境变量，测试必须串行改。
static ENV: Mutex<()> = Mutex::new(());

fn env_lock() -> MutexGuard<'static, ()> {
    ENV.lock().unwrap_or_else(|e| e.into_inner())
}

struct Tmp(PathBuf);

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tmp(tag: &str) -> Tmp {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let d = std::env::temp_dir().join(format!("rmcloud-inbox-{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    Tmp(d)
}

fn fixture(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    assert!(path.is_file(), "缺 fixture：{}", path.display());
    path.to_string_lossy().into_owned()
}

fn smtp() -> Option<Smtp> {
    Some(Smtp {
        host: "smtp.example.com".into(),
        port: 465,
        user: "me@example.com".into(),
        // 假授权码，测试里不许出现真的。
        pass: "not-a-real-pass".into(),
    })
}

fn app(dir: &Tmp, smtp: Option<Smtp>, dsh: &str) -> Arc<App> {
    let cfg = Config {
        bind: "127.0.0.1:0".into(),
        state: dir.0.join("state"),
        token: "t0ken".into(),
        dsh: dsh.into(),
        dsh_env: dir.0.join("no.env").to_string_lossy().into_owned(),
        smtp,
        ..Default::default()
    };
    Arc::new(App::open(cfg).unwrap())
}

/// 假 `imap_pull.py`：把同目录的 `payload.json` 原样吐到 stdout。
/// （真的脚本也是 `python3 <script> ...`，这里只换内容。）
fn fake_script(dir: &Path, payload: &Value) -> PathBuf {
    let script = dir.join("imap_pull.py");
    std::fs::write(
        &script,
        "import pathlib, sys\n\
         sys.stdout.write(pathlib.Path(__file__).with_name('payload.json').read_text(encoding='utf-8'))\n",
    )
    .unwrap();
    std::fs::write(dir.join("payload.json"), serde_json::to_vec(payload).unwrap()).unwrap();
    script
}

/// 假 curl：把 `--upload-file` 的那封 .eml 抄出来，别的（smtps、授权码）一概不管。
fn fake_curl(dir: &Path) -> PathBuf {
    let script = dir.join("fake-curl.sh");
    std::fs::write(
        &script,
        "#!/bin/sh\n\
         d=$(dirname \"$0\")\n\
         prev=\"\"\n\
         for a in \"$@\"; do\n\
           if [ \"$prev\" = \"--upload-file\" ]; then cp \"$a\" \"$d/sent.eml\"; fi\n\
           prev=\"$a\"\n\
         done\n\
         exit 0\n",
    )
    .unwrap();
    chmod_exec(&script);
    script
}

fn chmod_exec(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let mut perm = std::fs::metadata(path).unwrap().permissions();
    perm.set_mode(0o755);
    std::fs::set_permissions(path, perm).unwrap();
}

fn state_dir(dir: &Tmp) -> PathBuf {
    dir.0.join("state")
}

fn inbox_dir(dir: &Tmp) -> PathBuf {
    state_dir(dir).join("mirror").join("inbox")
}

/// 附件丢在临时目录里，路径写进 JSON（真脚本也是把附件写到 --outdir 再报绝对路径）。
fn attachment(dir: &Tmp, name: &str, data: &[u8]) -> Value {
    let path = dir.0.join(name);
    std::fs::write(&path, data).unwrap();
    json!({"filename": name, "path": path.to_string_lossy(), "size": data.len()})
}

fn one_mail(uid: u64, subject: &str, text: &str, attachments: Value) -> Value {
    json!([{
        "uid": uid,
        "from": "phone@example.com",
        "subject": subject,
        "date": "2026-11-09T10:00:00+08:00",
        "text": text,
        "attachments": attachments,
    }])
}

async fn run(app: &Arc<App>) -> Result<usize, String> {
    inbox::run_once(app).await
}

// ---------------------------------------------------------------- 1. 进库

#[tokio::test]
async fn attachment_and_body_land_in_library() {
    let _env = env_lock();
    let dir = tmp("land");
    let app = app(&dir, smtp(), "/nonexistent/dsh");

    let pdf = b"%PDF-1.4 fake page".to_vec();
    let payload = one_mail(
        101,
        "扫描件",
        "第一行\n第二行",
        json!([attachment(&dir, "page.pdf", &pdf)]),
    );
    let script = fake_script(&dir.0, &payload);
    std::env::set_var("INBOX_SCRIPT", &script);

    let handled = run(&app).await.expect("收信应成功");
    assert_eq!(handled, 1);

    // 附件：原名 + 邮件日期前缀
    let att_path = inbox_dir(&dir).join("2026-11-09-page.pdf");
    assert!(att_path.is_file(), "缺附件：{}", att_path.display());
    assert_eq!(std::fs::read(&att_path).unwrap(), pdf);

    // 正文：头三行 + 空行 + 正文
    let note_path = inbox_dir(&dir).join("2026-11-09-扫描件.txt");
    let note = std::fs::read_to_string(&note_path).expect("缺正文笔记");
    assert!(
        note.starts_with(
            "From: phone@example.com\nSubject: 扫描件\nDate: 2026-11-09T10:00:00+08:00\n\n"
        ),
        "{note:?}"
    );
    assert!(note.ends_with("第一行\n第二行\n"), "{note:?}");

    // entry：两条，sha 对得上，blob 也在
    let entries = app.entries();
    assert_eq!(entries.len(), 2, "{entries:?}");
    let want_pdf = hex::encode(Sha256::digest(&pdf));
    let pdf_entry = entries
        .iter()
        .find(|e| e.path == "inbox/2026-11-09-page.pdf")
        .expect("entry 里缺附件");
    assert_eq!(pdf_entry.sha, want_pdf);
    assert_eq!(pdf_entry.size, pdf.len() as u64);
    assert!(state_dir(&dir).join("blobs").join(&want_pdf).is_file());

    let note_sha = hex::encode(Sha256::digest(note.as_bytes()));
    let note_entry = entries
        .iter()
        .find(|e| e.path == "inbox/2026-11-09-扫描件.txt")
        .expect("entry 里缺正文");
    assert_eq!(note_entry.sha, note_sha);
    assert!(state_dir(&dir).join("blobs").join(&note_sha).is_file());

    // 一轮只涨 1（不是每个文件一次）
    assert_eq!(app.rev(), 1);
    assert_eq!(app.meta_get("inbox.last_uid"), Some("101".into()));
    assert_eq!(app.meta_get("inbox.last_error"), None);
    assert!(app.meta_get("inbox.last_run").is_some());
}

// ---------------------------------------------------------------- 2. 幂等

#[tokio::test]
async fn same_payload_twice_is_idempotent() {
    let _env = env_lock();
    let dir = tmp("idem");
    let app = app(&dir, smtp(), "/nonexistent/dsh");

    let payload = one_mail(
        102,
        "重复",
        "只该存一次",
        json!([attachment(&dir, "x.pdf", b"same bytes")]),
    );
    let script = fake_script(&dir.0, &payload);
    std::env::set_var("INBOX_SCRIPT", &script);

    assert_eq!(run(&app).await.unwrap(), 1);
    let rev = app.rev();
    let entries = app.entries().len();
    let note = std::fs::read(inbox_dir(&dir).join("2026-11-09-重复.txt")).unwrap();

    // 同一份 JSON 再来一遍：UID 已记账，什么都不做。
    assert_eq!(run(&app).await.unwrap(), 0, "第二次不该重复处理");
    assert_eq!(app.rev(), rev, "第二次不该涨 rev");
    assert_eq!(app.entries().len(), entries);
    assert_eq!(
        std::fs::read(inbox_dir(&dir).join("2026-11-09-重复.txt")).unwrap(),
        note
    );

    // mirror/inbox 里就两个文件，没有 `-2` 之类的副本
    let names: Vec<String> = std::fs::read_dir(inbox_dir(&dir))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
    assert!(!names.iter().any(|n| n.contains("-2.")), "{names:?}");
}

// ---------------------------------------------------------------- 3. `!` 指令

#[tokio::test]
async fn bang_subject_runs_agent_and_mails_the_answer_back() {
    let _env = env_lock();
    let dir = tmp("bang");
    let app = app(&dir, smtp(), &fixture("fake-dsh.sh"));

    let script = fake_script(&dir.0, &one_mail(201, "!summarize", "", json!([])));
    std::env::set_var("INBOX_SCRIPT", &script);
    let curl = fake_curl(&dir.0);
    std::env::set_var("RMCLOUD_CURL", &curl);

    let handled = run(&app).await.expect("收信应成功");
    assert_eq!(handled, 1);

    // 假 curl 抄出来的那封信
    let sent = std::fs::read_to_string(dir.0.join("sent.eml")).expect("没发出回信");
    assert!(sent.contains("To: phone@example.com\r\n"), "{sent}");
    assert!(sent.contains("Subject: Re: !summarize\r\n"), "{sent}");
    // fake-dsh 的 text 事件是「你好」，正文 base64 应出现
    assert!(sent.contains("5L2g5aW9"), "回信正文不是 agent 的回答：{sent}");

    // 指令信没正文，就不该硬写一条空笔记
    assert!(!inbox_dir(&dir).exists(), "空正文不该落笔记");

    // 回信失败/成功都不影响 UID 记账
    assert!(app.meta_get("inbox.uid.201").is_some());
    std::env::remove_var("RMCLOUD_CURL");
}

#[tokio::test]
async fn agent_failure_is_mailed_back_not_swallowed() {
    let _env = env_lock();
    let dir = tmp("bangfail");
    // 假 dsh 不存在 → ask_once 返回错误串
    let app = app(&dir, smtp(), "/nonexistent/dsh");

    let script = fake_script(&dir.0, &one_mail(202, "!do it", "", json!([])));
    std::env::set_var("INBOX_SCRIPT", &script);
    let curl = fake_curl(&dir.0);
    std::env::set_var("RMCLOUD_CURL", &curl);

    assert_eq!(run(&app).await.unwrap(), 1);
    let sent = std::fs::read_to_string(dir.0.join("sent.eml")).expect("失败也要回信");
    assert!(sent.contains("Subject: Re: !do it\r\n"), "{sent}");
    // `指令执行失败：...` 的 base64
    let body = sent
        .split("\r\n\r\n")
        .nth(1)
        .expect("回信没有正文段")
        .to_string();
    assert!(!body.trim().is_empty(), "{sent}");
    std::env::remove_var("RMCLOUD_CURL");
}

// ---------------------------------------------------------------- 4. 脚本失败

#[tokio::test]
async fn script_failure_records_last_error_without_panic() {
    let _env = env_lock();
    let dir = tmp("boom");
    let app = app(&dir, smtp(), "/nonexistent/dsh");

    let script = dir.0.join("broken.py");
    std::fs::write(&script, "import sys\nsys.stderr.write('boom: 登录失败\\n')\nsys.exit(1)\n")
        .unwrap();
    std::env::set_var("INBOX_SCRIPT", &script);

    let err = run(&app).await.expect_err("脚本退出 1 应返回 Err");
    assert!(err.contains("退出码 1"), "{err}");
    assert!(err.contains("boom"), "{err}");

    let recorded = app.meta_get("inbox.last_error").expect("last_error 必须有内容");
    assert!(recorded.contains("boom"), "{recorded}");
    assert_eq!(app.rev(), 0, "失败不该动库");
    assert!(app.meta_get("inbox.last_run").is_some());
}

// ---------------------------------------------------------------- 5. 排障接口

#[tokio::test]
async fn inbox_state_route_needs_token_and_reports_fields() {
    let _env = env_lock();
    let dir = tmp("route");
    let app = app(&dir, smtp(), "/nonexistent/dsh");
    app.meta_set("inbox.last_uid", "42");
    app.meta_set("inbox.last_error", "上次挂了");

    let request = Request::builder()
        .uri("/api/v1/inbox/state")
        .body(Body::empty())
        .unwrap();
    let res = app_router(app.clone()).oneshot(request).await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);

    let request = Request::builder()
        .uri("/api/v1/inbox/state")
        .header("authorization", "Bearer t0ken")
        .body(Body::empty())
        .unwrap();
    let res = app_router(app).oneshot(request).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), 1 << 20).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["last_uid"], 42);
    assert_eq!(body["last_error"], "上次挂了");
    assert!(body["last_run"].is_null());
}
