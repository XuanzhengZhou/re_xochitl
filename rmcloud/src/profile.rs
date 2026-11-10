//! 错题档案：`/api/v1/profile*`。C13。
//!
//! 用户要的三件事都在这一张表里：**有没有学会**（`status` + `learned_at`）、
//! **优先级**（`priority`）、**有没有重复**（归一化指纹 + `repeat_count`）。
//!
//! - 一次 scan = 一个 `dsh` 子进程（复用 [`crate::agent_api::collect_once`]，
//!   提示词要求模型只输出一个固定 schema 的 JSON）+ 落库。
//! - 重复检测不靠模型：`fingerprint = sha256(normalize(doc|page|text))`，
//!   归一化去掉空白/标点/大小写/全角差异，指纹相同就是同一道题。
//! - 又错一次 = `repeat_count+1`、`last_seen` 刷新、`priority` 升一档；
//!   之前标成 `learned` 的会**弹回 `open`**（又错了说明没真学会）。
//!
//! 表由 [`ensure_schema`] 在第一次访问时 `CREATE ... IF NOT EXISTS` 建出来
//! （`state.rs` 的 DDL 归存储层，这里自带、幂等，老库不用迁移脚本）。

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{Path as UrlPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
    Json, Router,
};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::agent_api::session_for;
use crate::mail::iso_utc;
use crate::state::App;

/// 喂给模型的"已有条目"上限（多了 prompt 太长，认 `repeat_of` 反而不准）。
const KNOWN: usize = 50;
/// prompt 里每条已有题目的截断（只给模型认亲，不要全文）。
const KNOWN_CHARS: usize = 80;
/// `GET /api/v1/profile` 的默认条数 / 上限。
const LIST_LIMIT: i64 = 50;
const LIST_MAX: i64 = 200;

/// 表结构。唯一键是归一化指纹（`fingerprint` 是 `problem` 的派生列）。
const DDL: &str = "CREATE TABLE IF NOT EXISTS problem(
    id INTEGER PRIMARY KEY,
    doc TEXT NOT NULL,
    page INTEGER NOT NULL,
    text TEXT NOT NULL,
    subject TEXT,
    difficulty INTEGER,
    status TEXT NOT NULL,
    priority TEXT NOT NULL,
    repeat_count INTEGER NOT NULL DEFAULT 1,
    first_seen TEXT NOT NULL,
    last_seen TEXT NOT NULL,
    learned_at TEXT,
    note TEXT,
    fingerprint TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS problem_fp ON problem(fingerprint);
CREATE INDEX IF NOT EXISTS problem_doc ON problem(doc);";

/// 全列（`SELECT` 和 `from_row` 必须同序）。
const COLS: &str = "id, doc, page, text, subject, difficulty, status, priority, \
                    repeat_count, first_seen, last_seen, learned_at, note, fingerprint";

/// 列表排序：优先级 高→低，再看最近一次出错（近→远），最后 id 兜底保证确定性。
const ORDER: &str = "ORDER BY CASE priority WHEN 'high' THEN 0 WHEN 'normal' THEN 1 \
                     WHEN 'low' THEN 2 ELSE 3 END, last_seen DESC, id DESC";

/// `POST /api/v1/profile/scan` 的请求体。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ScanReq {
    /// 笔记 uuid。
    #[serde(default)]
    pub doc: String,
    #[serde(default)]
    pub page: i64,
    /// 整页渲染图的 blob sha（可选，给模型 `read_image` 用）。
    #[serde(default)]
    pub image_blob: Option<String>,
    /// 用户附带的提示（可选），不参与指纹。
    #[serde(default)]
    pub text: String,
}

/// 模型输出的一条题目。字段全可选：它经常漏字段，漏了就用默认值，不能让整次 scan 失败。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Item {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub difficulty: Option<i64>,
    /// 已有条目的 id：`7` / `"7"` / `""` 都见过。
    #[serde(default)]
    pub repeat_of: Option<Value>,
    #[serde(default)]
    pub priority: Option<String>,
}

impl Item {
    /// `repeat_of` 解析成 id；空串/非数字/缺字段都是 `None`。
    pub fn repeat_of_id(&self) -> Option<i64> {
        let v = self.repeat_of.as_ref()?;
        match v {
            Value::String(s) => s.trim().parse().ok(),
            other => other.as_i64(),
        }
    }
}

/// 一行档案。
#[derive(Debug, Clone)]
pub struct Problem {
    pub id: i64,
    pub doc: String,
    pub page: i64,
    pub text: String,
    pub subject: Option<String>,
    pub difficulty: Option<i64>,
    pub status: String,
    pub priority: String,
    pub repeat_count: i64,
    pub first_seen: String,
    pub last_seen: String,
    pub learned_at: Option<String>,
    pub note: Option<String>,
    pub fingerprint: String,
}

impl Problem {
    fn from_row(r: &Row<'_>) -> rusqlite::Result<Problem> {
        Ok(Problem {
            id: r.get(0)?,
            doc: r.get(1)?,
            page: r.get(2)?,
            text: r.get(3)?,
            subject: r.get(4)?,
            difficulty: r.get(5)?,
            status: r.get(6)?,
            priority: r.get(7)?,
            repeat_count: r.get(8)?,
            first_seen: r.get(9)?,
            last_seen: r.get(10)?,
            learned_at: r.get(11)?,
            note: r.get(12)?,
            fingerprint: r.get(13)?,
        })
    }

    /// 出网的 JSON（`fingerprint` 也带上，排查重复时能直接比对）。
    pub fn json(&self) -> Value {
        json!({
            "id": self.id,
            "doc": self.doc,
            "page": self.page,
            "text": self.text,
            "subject": self.subject,
            "difficulty": self.difficulty,
            "status": self.status,
            "priority": self.priority,
            "repeat_count": self.repeat_count,
            "first_seen": self.first_seen,
            "last_seen": self.last_seen,
            "learned_at": self.learned_at,
            "note": self.note,
            "fingerprint": self.fingerprint,
        })
    }
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/v1/profile/scan", post(scan))
        .route("/api/v1/profile", get(list))
        .route("/api/v1/profile/{id}/learned", post(learned))
        .route("/api/v1/profile/{id}", delete(remove))
}

// ---------------------------------------------------------------- 归一化 / 指纹

/// 手指纹的归一化：全角转半角、去所有空白、去常见中英文标点、转小写。
///
/// 目标是"语义相同、写法不同"落到同一个串：`求 x 的值：` / `求X的值:` /
/// `求ｘ的值。` 都归一化成 `求x的值`。
pub fn normalize(s: &str) -> String {
    s.chars()
        .map(fold_width)
        .filter(|c| !c.is_whitespace() && !is_punct(*c))
        .flat_map(char::to_lowercase)
        .collect()
}

/// 全角 ASCII（`！`..`～`）→ 半角，全角空格 → 半角空格；其余原样。
fn fold_width(c: char) -> char {
    match c {
        '\u{3000}' => ' ',
        '\u{FF01}'..='\u{FF5E}' => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        _ => c,
    }
}

/// 常见中英文标点。ASCII 之外的按 Unicode 块来：通用标点、CJK 标点、全角残留。
fn is_punct(c: char) -> bool {
    if c.is_ascii_punctuation() {
        return true;
    }
    matches!(
        c as u32,
        0x00A1 | 0x00A6..=0x00A7 | 0x00AB | 0x00B6..=0x00B7 | 0x00BB | 0x00BF
        | 0x2010..=0x205E // 破折号（—）、弯引号（“”）、省略号（…）、项目符号（•）
        | 0x2212 | 0x2236 // − ∶
        | 0x3000..=0x303F // CJK 标点：、。《》「」『』【】〔〕
        | 0xFE10..=0xFE19 | 0xFE30..=0xFE6B // 竖排标点 / CJK 兼容形式
        | 0xFF5F..=0xFF65 | 0xFFE0..=0xFFE6 // 全角括号、￥ 等
    )
}

/// 唯一键：`sha256(normalize(doc + "|" + page + "|" + text))`，小写 hex。
///
/// `normalize` 会把 `|` 当标点去掉，所以这就是"归一化后的整串"的哈希。
pub fn fingerprint(doc: &str, page: i64, text: &str) -> String {
    let raw = format!("{doc}|{page}|{text}");
    let mut h = Sha256::new();
    h.update(normalize(&raw).as_bytes());
    hex::encode(h.finalize())
}

// ---------------------------------------------------------------- 解析

/// 从模型输出里捞出 JSON 对象：第一个 `{` 到最后一个 `}`，前后废话丢掉。
pub fn extract_json(raw: &str) -> Option<&str> {
    let start = raw.find('{')?;
    let end = raw.rfind('}')?;
    (end > start).then(|| &raw[start..=end])
}

/// 解析 `{"problems":[...]}`。容忍前后废话、`problems` 缺失/为 null；
/// 不是 JSON 或者 `problems` 不是数组就返回错误串（调用方回 502），绝不 panic。
pub fn parse_problems(raw: &str) -> Result<Vec<Item>, String> {
    /// 只认 `problems`，别的字段（模型爱多写 `note`/`answer`）直接忽略。
    #[derive(Deserialize)]
    struct Out {
        #[serde(default)]
        problems: Option<Vec<Item>>,
    }
    let js = extract_json(raw).ok_or_else(|| "输出里没有 JSON 对象".to_string())?;
    let out: Out = serde_json::from_str(js).map_err(|e| format!("JSON 解析失败：{e}"))?;
    Ok(out.problems.unwrap_or_default())
}

// ---------------------------------------------------------------- 提示词

/// 固定 JSON schema 的扫描提示词。
///
/// `known` 是同一篇笔记里已有的条目（最近的在前），模型只在里面挑 `repeat_of`；
/// 真正的重复判定仍由 [`fingerprint`] 决定。
pub fn scan_prompt(req: &ScanReq, known: &[Problem]) -> String {
    let mut p = String::new();
    p.push_str(
        "你在 rM2 手写笔记的错题档案里干活：把这一页的错题登记下来——是哪道题、\
         什么学科、多难、多急、是不是又错了一遍。\n",
    );
    p.push_str(&format!(
        "笔记 doc = {}，第 {} 页。\n",
        req.doc.trim(),
        req.page
    ));
    if let Some(sha) = req
        .image_blob
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        p.push_str(&format!(
            "整页渲染图：PNG blob sha = {sha}（服务器 blobs/ 下；看不清可以用 read_image 读）。\n"
        ));
    }
    let hint = req.text.trim();
    if !hint.is_empty() {
        p.push_str(&format!("用户提示：{hint}\n"));
    }

    p.push_str("\n已经登记过的条目（id | 学科 | 题目）：\n");
    if known.is_empty() {
        p.push_str("（空）\n");
    } else {
        for k in known {
            p.push_str(&format!(
                "{} | {} | {}\n",
                k.id,
                k.subject.as_deref().unwrap_or("未知"),
                clip(&k.text, KNOWN_CHARS)
            ));
        }
    }

    // 成本实测（grade 模式同款改动）：模型默认会把推理过程也流出来，输出 token
    // 是 JSON 本身的 20 倍。明说"别输出思考"之后 grade 的 out 从 12.6k 掉到 4.7k。
    p.push_str("\n只输出一个 JSON 对象，前后不要任何解释、不要 markdown 代码围栏。\n");
    p.push_str("推理只在你心里做，**不要输出思考过程**；第一个字符必须是 `{`、最后一个必须是 `}`。\n");
    p.push_str(
        r#"{"problems":[{"text":"题目原文（从手写里转述或摘出，保留关键条件，不写解答）","subject":"数学","difficulty":3,"repeat_of":"<上面已有条目里最像的那条的 id，没有就空字符串>","priority":"high"}]}"#,
    );
    p.push('\n');
    p.push_str("字段要求：\n");
    p.push_str("- text：题目原文；看不清就只写看清的部分。\n");
    p.push_str("- subject：学科，如 数学 / 物理 / 化学 / 语文 / 英语。\n");
    p.push_str("- difficulty：1 到 5 的整数（1 最简单、5 最难）。\n");
    p.push_str("- repeat_of：和上面某条是同一道题就填它的 id（只填数字），不是就填空字符串。\n");
    p.push_str("- priority：high / normal / low，按“快考试 / 还没掌握”定。\n");
    p.push_str("这一页没有错题就输出 {\"problems\":[]}。\n");
    p
}

// ---------------------------------------------------------------- handler

async fn scan(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(req): Json<ScanReq>,
) -> Response {
    if !app.authorized_any(bearer(&headers)) {
        return fail(StatusCode::UNAUTHORIZED, "unauthorized", None);
    }
    let doc = req.doc.trim().to_string();
    if doc.is_empty() {
        return fail(StatusCode::BAD_REQUEST, "bad_doc", None);
    }
    let req = ScanReq { doc, ..req };
    if let Err(e) = ensure_schema(&app) {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "db", Some(e.to_string()));
    }
    let known = match known_for(&app, &req.doc) {
        Ok(v) => v,
        Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, "db", Some(e.to_string())),
    };

    let prompt = scan_prompt(&req, &known);
    // 同一篇笔记的 scan 落同一个 session，吃 dsh 的 prompt 缓存。
    let session = session_for(&req.doc);
    let raw = match crate::agent_api::collect_once(&app, &prompt, Some(&session), "profile").await {
        Ok(text) => text,
        Err(detail) => return fail(StatusCode::BAD_GATEWAY, "scan failed", Some(detail)),
    };
    let items = match parse_problems(&raw) {
        Ok(items) => items,
        Err(detail) => return fail(StatusCode::BAD_GATEWAY, "parse failed", Some(detail)),
    };

    let now = iso_utc(now_secs());
    match apply(&app, &req, &items, &now) {
        Ok((added, repeated, problems)) => Json(json!({
            "added": added,
            "repeated": repeated,
            "problems": problems,
        }))
        .into_response(),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, "db", Some(e.to_string())),
    }
}

/// `GET /api/v1/profile?status=open|learned|all&limit=50`
#[derive(Debug, Default, Deserialize)]
pub struct ListQ {
    #[serde(default)]
    pub status: Option<String>,
    /// 当字符串收：`limit=abc` 就退回默认值，别 400。
    #[serde(default)]
    pub limit: Option<String>,
}

async fn list(State(app): State<Arc<App>>, headers: HeaderMap, Query(q): Query<ListQ>) -> Response {
    if !app.authorized_any(bearer(&headers)) {
        return fail(StatusCode::UNAUTHORIZED, "unauthorized", None);
    }
    let status = q
        .status
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .unwrap_or_else(|| "open".to_string());
    let filter = match status.as_str() {
        "open" | "learned" => Some(status),
        "all" | "" => None,
        other => {
            return fail(
                StatusCode::BAD_REQUEST,
                "bad_status",
                Some(format!("status 只能是 open/learned/all，收到 {other}")),
            )
        }
    };
    let limit = q
        .limit
        .as_deref()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(LIST_LIMIT)
        .clamp(1, LIST_MAX);

    if let Err(e) = ensure_schema(&app) {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "db", Some(e.to_string()));
    }
    let db = app.db();
    let mut stmt = match db.prepare(&format!(
        "SELECT {COLS} FROM problem WHERE (?1 IS NULL OR status = ?1) {ORDER} LIMIT ?2"
    )) {
        Ok(s) => s,
        Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, "db", Some(e.to_string())),
    };
    let rows = match stmt.query_map(params![filter, limit], Problem::from_row) {
        Ok(rows) => rows,
        Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, "db", Some(e.to_string())),
    };
    let mut problems = Vec::new();
    for row in rows {
        match row {
            Ok(p) => problems.push(p.json()),
            Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, "db", Some(e.to_string())),
        }
    }
    let count = problems.len();
    Json(json!({ "problems": problems, "count": count })).into_response()
}

/// `POST /api/v1/profile/{id}/learned`：标记学会。已经 learned 再调也是 200（幂等，
/// 不刷新 `learned_at`）。
async fn learned(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    if !app.authorized_any(bearer(&headers)) {
        return fail(StatusCode::UNAUTHORIZED, "unauthorized", None);
    }
    let Ok(id) = id.trim().parse::<i64>() else {
        return fail(StatusCode::NOT_FOUND, "not found", None);
    };
    if let Err(e) = ensure_schema(&app) {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "db", Some(e.to_string()));
    }
    let now = iso_utc(now_secs());
    let db = app.db();
    let changed = match db.execute(
        "UPDATE problem SET status='learned', learned_at=COALESCE(learned_at, ?2)
         WHERE id=?1",
        params![id, now],
    ) {
        Ok(n) => n,
        Err(e) => return fail(StatusCode::INTERNAL_SERVER_ERROR, "db", Some(e.to_string())),
    };
    if changed == 0 {
        return fail(StatusCode::NOT_FOUND, "not found", None);
    }
    match one(&db, id) {
        Ok(p) => Json(json!({ "problem": p.json() })).into_response(),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, "db", Some(e.to_string())),
    }
}

/// `DELETE /api/v1/profile/{id}`：删一条。删不到就 404。
async fn remove(
    State(app): State<Arc<App>>,
    UrlPath(id): UrlPath<String>,
    headers: HeaderMap,
) -> Response {
    if !app.authorized_any(bearer(&headers)) {
        return fail(StatusCode::UNAUTHORIZED, "unauthorized", None);
    }
    let Ok(id) = id.trim().parse::<i64>() else {
        return fail(StatusCode::NOT_FOUND, "not found", None);
    };
    if let Err(e) = ensure_schema(&app) {
        return fail(StatusCode::INTERNAL_SERVER_ERROR, "db", Some(e.to_string()));
    }
    match app.db().execute("DELETE FROM problem WHERE id=?1", [id]) {
        Ok(1) => Json(json!({ "ok": true, "deleted": 1 })).into_response(),
        Ok(_) => fail(StatusCode::NOT_FOUND, "not found", None),
        Err(e) => fail(StatusCode::INTERNAL_SERVER_ERROR, "db", Some(e.to_string())),
    }
}

// ---------------------------------------------------------------- 落库

/// 建表（幂等）。老库第一次访问就补上 `problem`，不需要迁移脚本。
fn ensure_schema(app: &App) -> rusqlite::Result<()> {
    app.db().execute_batch(DDL)
}

/// 同一篇笔记里已有的条目（最近的在前，最多 [`KNOWN`] 条）。
fn known_for(app: &App, doc: &str) -> rusqlite::Result<Vec<Problem>> {
    let db = app.db();
    let mut stmt = db.prepare(&format!(
        "SELECT {COLS} FROM problem WHERE doc=?1 ORDER BY id DESC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(params![doc, KNOWN as i64], Problem::from_row)?;
    rows.collect()
}

fn one(db: &Connection, id: i64) -> rusqlite::Result<Problem> {
    db.query_row(
        &format!("SELECT {COLS} FROM problem WHERE id=?1"),
        [id],
        Problem::from_row,
    )
}

/// 落库，返回（新增数、重复数、受影响的条目 JSON）。
///
/// - 指纹不存在 → 插入，`repeat_count=1`；
/// - 指纹已存在 → `repeat_count+1`、`last_seen` 刷新、`priority` 升一档、
///   `learned` 弹回 `open`（`learned_at` 清空）；
/// - `repeat_of` 指向的**另一条**也记一次重复（指认的正是刚命中的那条就不重复记）。
fn apply(
    app: &App,
    req: &ScanReq,
    items: &[Item],
    now: &str,
) -> rusqlite::Result<(i64, i64, Vec<Value>)> {
    let mut db = app.db();
    let tx = db.transaction()?;
    let mut added = 0i64;
    let mut repeated = 0i64;
    let mut problems = Vec::new();

    for item in items {
        let text = item.text.trim();
        if text.is_empty() {
            // 没有题目原文就没法去重，也没法看，跳过（模型偶尔吐空条目）。
            continue;
        }
        let fp = fingerprint(&req.doc, req.page, text);
        let existing: Option<Problem> = tx
            .query_row(
                &format!("SELECT {COLS} FROM problem WHERE fingerprint=?1"),
                [&fp],
                Problem::from_row,
            )
            .optional()?;

        let (id, action) = match existing {
            Some(p) => {
                tx.execute(
                    "UPDATE problem SET repeat_count=repeat_count+1, last_seen=?2, priority=?3,
                            status='open', learned_at=NULL
                     WHERE id=?1",
                    params![p.id, now, upgrade(&p.priority)],
                )?;
                repeated += 1;
                (p.id, "repeated")
            }
            None => {
                tx.execute(
                    "INSERT INTO problem(doc, page, text, subject, difficulty, status, priority,
                                         repeat_count, first_seen, last_seen, learned_at, note,
                                         fingerprint)
                     VALUES(?1, ?2, ?3, ?4, ?5, 'open', ?6, 1, ?7, ?7, NULL, NULL, ?8)",
                    params![
                        req.doc,
                        req.page,
                        text,
                        clean_subject(item.subject.as_deref()),
                        difficulty(item.difficulty),
                        norm_priority(item.priority.as_deref()),
                        now,
                        fp,
                    ],
                )?;
                added += 1;
                (tx.last_insert_rowid(), "added")
            }
        };

        // 模型指认的"同一道题"：命中的是别的一条时也算它重复一次。
        if let Some(other) = item.repeat_of_id().filter(|other| *other != id) {
            let n = tx.execute(
                "UPDATE problem SET repeat_count=repeat_count+1, last_seen=?2 WHERE id=?1",
                params![other, now],
            )?;
            if n == 1 {
                repeated += 1;
            }
        }

        let mut v = one(&tx, id)?.json();
        if let Some(o) = v.as_object_mut() {
            o.insert("action".into(), json!(action));
        }
        problems.push(v);
    }

    tx.commit()?;
    Ok((added, repeated, problems))
}

/// 又错一次：优先级升一档（low → normal → high，high 封顶）。
pub fn upgrade(priority: &str) -> &'static str {
    match priority {
        "high" => "high",
        "low" => "normal",
        _ => "high",
    }
}

/// 模型给的优先级只认 high/low/normal，其它一律 normal。
pub fn norm_priority(s: Option<&str>) -> &'static str {
    match s.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("high") => "high",
        Some("low") => "low",
        _ => "normal",
    }
}

/// 难度收敛到 1..=5；没给就是 NULL。
fn difficulty(d: Option<i64>) -> Option<i64> {
    d.map(|d| d.clamp(1, 5))
}

/// 学科：去空白，空的当 NULL，最多留 32 个字（模型偶尔写一整句）。
fn clean_subject(s: Option<&str>) -> Option<String> {
    let s = s.map(str::trim).filter(|s| !s.is_empty())?;
    Some(s.chars().take(32).collect())
}

// ---------------------------------------------------------------- 小工具

/// 只留前 `n` 个字（按字符切，不会切坏 UTF-8）。
fn clip(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().nth(n).is_some() {
        out.push('…');
    }
    out
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
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
