//! AI 回答落成 PDF，并**直接发布进设备镜像**。S1（`docs/AI-REFS.md`）。
//!
//! 设备问一次，这里做四件事：
//!
//! 1. 把**父会话的记忆**读出来（`answers/<parent>/memory.md`，没有父就用源文档的
//!    `src/<doc>/memory.md`），拼进 prompt；
//! 2. 回答编译成 PDF（[`crate::latex`]）；
//! 3. **发布**：在 `mirror/` 里造出真正的 rM 条目 ——
//!    `AI answers/<标题>/aiN.pdf` + `.metadata/.content/.pagedata`，
//!    并把引用框写进 `<源文档>.airefs.json`；条目进 entry 表、rev+1、发
//!    `Event::Rev`，设备下一次 sync 就把文件和索引一起拉走。
//!    设备端**不需要**自己建文档、下载 blob、算页数。
//! 4. 写记忆：`answers/<ai_doc>/memory.md`（父记忆 + 本轮）和
//!    `src/<doc>/memory.md`（源文档累积）。
//!
//! **失败不发布**：LaTeX 编译两次都失败就返回 `Err`，`done` 事件里只带
//! `pdf_error`，镜像里不会出现半份文档。
//!
//! # 为什么发布放在服务器
//!
//! 「agent 处理完 → 跑脚本把文件传回设备并连上索引」这一步必须是**自动**的。
//! 放在 Rust 里比让模型自己调脚本可靠：模型只负责写回答，落地/记账/rev 全在
//! 一条确定性路径上。模型要迭代几轮工具调用都不影响这里（它结束时我们才发布）。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::agent_api::AskReq;
use crate::latex;
use crate::state::App;

/// prompt 里最多带多少字符的记忆。12k ≈ 6k token，够 5~8 轮问答。
const MEMORY_CHARS: usize = 12_000;
/// `memory.md` 里每轮回答截到这么多字符，防止一次超长回答把后面几轮挤掉。
const ROUND_CHARS: usize = 6_000;
/// 设备库根下的文件夹名（用户起的名字）。
pub const FOLDER_NAME: &str = "AI answers";
/// 引用索引的后缀：`<源文档 uuid>.airefs.json`。
pub const REFS_SUFFIX: &str = ".airefs.json";

/// `state/ai/answers/<ai>`。
pub fn answer_dir(state: &Path, ai: &str) -> PathBuf {
    state.join("ai").join("answers").join(safe(ai))
}

/// 源文档的记忆文件：`state/ai/src/<doc>/memory.md`。
pub fn src_memory_path(state: &Path, doc: &str) -> PathBuf {
    state.join("ai").join("src").join(safe(doc)).join("memory.md")
}

/// uuid 只留 ASCII 字母数字和 `-`，防止 `..` 之类跑出 state 目录。
fn safe(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(64)
        .collect()
}

/// 取尾部最多 `n` 个字符（UTF-8 安全）。
pub fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n {
        return s.to_string();
    }
    s.chars().skip(count - n).collect()
}

/// 读这一轮该带的记忆（有 parent 用父的，否则用源文档的）。读不到当空。
pub fn memory_for(state: &Path, req: &AskReq) -> String {
    let path = if !req.parent.trim().is_empty() {
        answer_dir(state, &req.parent).join("memory.md")
    } else {
        src_memory_path(state, &req.doc)
    };
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    tail_chars(&raw, MEMORY_CHARS)
}

/// 由种子生成稳定的 uuid（同一次发布会重算出同一个 id，幂等）。
///
/// 版本位写 4、variant 写 8，纯粹为了看起来像 rM 的 uuid；不参与任何校验。
pub fn stable_uuid(seed: &str) -> String {
    let h = Sha256::digest(seed.as_bytes());
    let x = hex::encode(h);
    format!(
        "{}-{}-4{}-8{}-{}",
        &x[..8],
        &x[8..12],
        &x[13..16],
        &x[17..20],
        &x[20..32]
    )
}

/// 一份回答的落盘结果。
pub struct Stored {
    /// PDF 的 sha256（设备也能用 blob 通道取，正常走 sync）。
    pub sha: String,
    pub pdf: PathBuf,
    /// 发布到镜像里的相对路径。
    pub published: Vec<String>,
}

/// 把这一轮问答存下来并发布。
pub async fn store(
    app: &App,
    req: &AskReq,
    answer: &str,
    memory: &str,
) -> Result<Stored, String> {
    let ai = safe(&req.ai_doc);
    if ai.is_empty() {
        return Err("缺少 ai_doc".into());
    }
    let dir = answer_dir(&app.cfg.state, &ai);
    let question = if req.text.trim().is_empty() {
        "（整页提问，没有文字）".to_string()
    } else {
        req.text.trim().to_string()
    };

    // ① 编译（正常档 → 降级档）。
    let tex = latex::document(&question, answer);
    let (pdf, degraded) = match latex::compile(&dir, &tex).await {
        Ok(p) => (p, false),
        Err(first) => {
            eprintln!("[ai] 正常档编译失败：{first}");
            let tex2 = latex::document_opt(&question, answer, false);
            match latex::compile(&dir, &tex2).await {
                Ok(p) => (p, true),
                Err(second) => {
                    return Err(format!("LaTeX 编译失败：{first}；降级也失败：{second}"))
                }
            }
        }
    };
    let _ = tokio::fs::rename(dir.join("a.tex"), dir.join("answer.tex")).await;
    let _ = tokio::fs::rename(&pdf, dir.join("answer.pdf")).await;
    let pdf = dir.join("answer.pdf");
    let bytes = tokio::fs::read(&pdf)
        .await
        .map_err(|e| format!("读 PDF 失败：{e}"))?;
    let sha = hex::encode(Sha256::digest(&bytes));

    // ② 发布进镜像（设备 sync 一次就全有了）。
    let pages = pdf_pages(&bytes).await;
    let title = src_title(app, &req.doc);
    let first = first_line(answer);
    let published = publish(app, req, &ai, &bytes, pages, &title, &first).await?;
    eprintln!(
        "[ai] {} → {} 页，已发布 {} 个文件（{} 字{}）",
        ai,
        pages,
        published.len(),
        answer.chars().count(),
        if degraded { "，降级排版" } else { "" }
    );

    // ③ 元数据 + 记忆（`memory/` 只在服务器上，给下一轮 prompt 用）。
    let meta = json!({
        "src": req.doc,
        "parent": req.parent,
        "n": req.n,
        "page": req.page,
        "rect": req.rect,
        "question": question,
        "ai": ai,
        "pdf": sha,
        "degraded": degraded,
        "published": published,
    });
    let _ = tokio::fs::write(
        dir.join("meta.json"),
        serde_json::to_vec_pretty(&meta).unwrap_or_default(),
    )
    .await;

    let round = format!(
        "\n## 第 {} 轮（{}{}）\n问：{}\n答：{}\n",
        req.n,
        ai,
        if req.parent.trim().is_empty() {
            String::new()
        } else {
            format!("，接在 {} 后", req.parent)
        },
        question,
        tail_chars(answer.trim(), ROUND_CHARS)
    );
    write_append(&dir.join("memory.md"), memory, &round).await;
    // 源文档的记忆是累积的：接上以前问过的，再追加本轮。
    let src_mem = src_memory_path(&app.cfg.state, &req.doc);
    let old = tokio::fs::read_to_string(&src_mem).await.unwrap_or_default();
    write_append(&src_mem, &old, &round).await;

    Ok(Stored {
        sha,
        pdf,
        published,
    })
}

/// 源文档标题（`<uuid>.metadata` 的 `visibleName`），读不到就用 `Document`。
fn src_title(app: &App, doc: &str) -> String {
    let p = app.mirror().join(format!("{}.metadata", safe(doc)));
    let v: Value = std::fs::read(&p)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null);
    v.get("visibleName")
        .and_then(|x| x.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Document".to_string())
}

/// 发布一组文件：进 blob + 写 mirror + 一个事务进 entry 表 + rev+1 + 通知。
///
/// 和收信模块（`inbox::store` / `inbox::commit`）同一套做法：**先落盘再记账**，
/// 记账失败最多多一次重传，不会出现"清单里有、盘上没有"。
async fn publish(
    app: &App,
    req: &AskReq,
    ai: &str,
    pdf: &[u8],
    pages: u32,
    title: &str,
    first_line: &str,
) -> Result<Vec<String>, String> {
    let ms = now_ms();
    let root = stable_uuid(FOLDER_NAME);
    let sub = stable_uuid(&format!("{FOLDER_NAME}/{title}"));
    let name = ai_name(app, &sub, req.n);
    let mut files: Vec<(String, Vec<u8>)> = Vec::new();

    // 两个文件夹（重写无害：内容一样、sha 一样）。
    files.push((format!("{root}.metadata"), folder_metadata(FOLDER_NAME, "", ms)));
    files.push((format!("{root}.content"), b"{}\n".to_vec()));
    files.push((format!("{sub}.metadata"), folder_metadata(title, &root, ms)));
    files.push((format!("{sub}.content"), b"{}\n".to_vec()));

    // 回答 PDF 本体 + 三件套。
    files.push((format!("{ai}.pdf"), pdf.to_vec()));
    files.push((format!("{ai}.metadata"), doc_metadata(&name, &sub, ms)));
    files.push((format!("{ai}.content"), doc_content(pages, pdf.len())));
    files.push((format!("{ai}.pagedata"), pagedata(pages)));

    // 引用索引：`<源文档>.airefs.json` 追加一条（设备画框/跳转的唯一依据）。
    let refs_rel = format!("{}{}", safe(&req.doc), REFS_SUFFIX);
    let old = std::fs::read(app.mirror().join(&refs_rel)).ok();
    let refs = append_ref(old.as_deref(), &sub, ai, req, ms, first_line);
    files.push((refs_rel, refs));

    let mut puts = Vec::new();
    let mut published = Vec::new();
    for (rel, data) in files {
        if !crate::sync::safe_rel(&rel) {
            return Err(format!("非法路径：{rel:?}"));
        }
        let sha = hex::encode(Sha256::digest(&data));
        crate::sync::store_blob(app, &sha, &data).map_err(|e| format!("写 blob 失败：{e}"))?;
        write_atomic(&app.mirror().join(&rel), &data)
            .map_err(|e| format!("写 mirror 失败（{rel}）：{e}"))?;
        puts.push(crate::sync::Put {
            path: rel.clone(),
            sha,
            size: data.len() as u64,
            mtime: ms,
        });
        published.push(rel);
    }

    let mut db = app.db();
    let current = crate::state::rev_of(&db);
    let next = crate::sync::apply_entries(&mut db, current, &puts, &[])
        .map_err(|e| format!("写 entry 失败：{e}"))?;
    drop(db);
    let _ = app.events.send(crate::state::Event::Rev(next));
    Ok(published)
}

/// 同一文件夹里已经叫 `aiN` 就往后加 ` (2)`、` (3)`。
fn ai_name(app: &App, parent: &str, n: u32) -> String {
    let base = format!("ai{n}");
    let mut used = std::collections::HashSet::new();
    if let Ok(rd) = std::fs::read_dir(app.mirror()) {
        for e in rd.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("metadata") {
                continue;
            }
            let Ok(b) = std::fs::read(&p) else { continue };
            let Ok(v) = serde_json::from_slice::<Value>(&b) else {
                continue;
            };
            if v.get("parent").and_then(|x| x.as_str()) != Some(parent) {
                continue;
            }
            if let Some(name) = v.get("visibleName").and_then(|x| x.as_str()) {
                used.insert(name.to_string());
            }
        }
    }
    if !used.contains(&base) {
        return base;
    }
    for k in 2..1000 {
        let cand = format!("{base} ({k})");
        if !used.contains(&cand) {
            return cand;
        }
    }
    format!("{base}-{}", &stable_uuid(&format!("{base}{}", now_ms()))[..8])
}

/// 文件夹的 `.metadata`。
fn folder_metadata(name: &str, parent: &str, ms: u64) -> Vec<u8> {
    let v = json!({
        "createdTime": ms.to_string(),
        "deleted": false,
        "lastModified": ms.to_string(),
        "metadatamodified": true,
        "modified": true,
        "parent": parent,
        "pinned": false,
        "synced": true,
        "type": "CollectionType",
        "version": 0,
        "visibleName": name,
    });
    serde_json::to_vec_pretty(&v).unwrap_or_default()
}

/// 回答 PDF 的 `.metadata`。
fn doc_metadata(name: &str, parent: &str, ms: u64) -> Vec<u8> {
    let v = json!({
        "createdTime": ms.to_string(),
        "deleted": false,
        "lastModified": ms.to_string(),
        "lastOpened": ms.to_string(),
        "lastOpenedPage": 0,
        "metadatamodified": false,
        "modified": false,
        "parent": parent,
        "pinned": false,
        "synced": true,
        "type": "DocumentType",
        "version": 1,
        "visibleName": name,
        // 设备靠这个字段认「这是 AI 回答 PDF」：在它上面提问 = 追问（parent 指自己）。
        "aiAnswer": true,
    });
    serde_json::to_vec_pretty(&v).unwrap_or_default()
}

/// PDF 的 `.content`。只给库/阅读器真正读的那几个，其余用官方默认值。
fn doc_content(pages: u32, size: usize) -> Vec<u8> {
    let v = json!({
        "coverPageNumber": -1,
        "customZoomCenterX": 0,
        "customZoomCenterY": 936,
        "customZoomOrientation": "portrait",
        "customZoomPageHeight": 1872,
        "customZoomPageWidth": 1404,
        "customZoomScale": 1,
        "documentMetadata": {},
        "extraMetadata": {},
        "fileType": "pdf",
        "fontName": "",
        "formatVersion": 2,
        "lineHeight": -1,
        "margins": 125,
        "orientation": "portrait",
        "pageCount": pages,
        "pageTags": [],
        "sizeInBytes": size.to_string(),
        "tags": [],
        "textScale": 1,
        "zoomMode": "bestFit",
    });
    serde_json::to_vec_pretty(&v).unwrap_or_default()
}

/// `.pagedata`：每页一行 `Blank`。
fn pagedata(pages: u32) -> Vec<u8> {
    let mut s = String::new();
    for _ in 0..pages.max(1) {
        s.push_str("Blank\n");
    }
    s.into_bytes()
}

/// 回答的"第一行"：去掉 Markdown 记号 / 空行，截到 120 字。
///
/// 设备那个引用列表一行只能放这么多；存整段会把 `<src>.airefs.json` 撑大，
/// 而它每次 sync 都要下载一遍。
fn first_line(answer: &str) -> String {
    for raw in answer.lines() {
        let mut l = raw.trim();
        if l.is_empty() || l.chars().all(|c| "#-=*_> ".contains(c)) {
            continue;
        }
        // 去掉常见的行首记号（标题井号、引用尖括号、短横列表）
        l = l.trim_start_matches(|c: char| "#>-+ ".contains(c)).trim();
        // `* 条目` 是列表；`**加粗**` 不是，别把星号吃掉
        if let Some(rest) = l.strip_prefix("* ") {
            l = rest.trim();
        }
        if l.is_empty() {
            continue;
        }
        let out: String = l.chars().take(120).collect();
        return if l.chars().count() > 120 { format!("{out}…") } else { out };
    }
    String::new()
}

/// 引用索引追加一条。老文件坏了就当空重建（丢几个框比整份不发布强）。
fn append_ref(
    old: Option<&[u8]>,
    folder: &str,
    ai: &str,
    req: &AskReq,
    ms: u64,
    first_line: &str,
) -> Vec<u8> {
    let mut v: Value = old
        .and_then(|b| serde_json::from_slice(b).ok())
        .filter(|v: &Value| v.get("items").map(|i| i.is_array()).unwrap_or(false))
        .unwrap_or_else(|| json!({"version": 1, "items": []}));
    v["version"] = json!(1);
    v["folder"] = json!(folder);
    // refs 里的 `page` 是**0 基**（设备按 `nb.page_index` 画框），
    // 而请求体的 `page` 是 1 基（提示词里说"第 N 页"）。
    let page0 = req.page.saturating_sub(1);
    let item = json!({
        "ai": ai,
        "page": page0,
        "rect": req.rect,
        "kind": if req.rect.is_some() { "rect" } else { "page" },
        "n": req.n,
        "pdf_page": 0,
        // 设备那个"这一页的 AI 引用"列表显示的就是这一行。回答里换行很多，
        // 存整段进去会把索引撑大，取第一行足够认出是哪一次回答。
        "title": first_line,
        "at": ms,
    });
    if let Some(arr) = v["items"].as_array_mut() {
        // 同一个 ai 重复发布（重试）就替换，不叠两条。
        arr.retain(|x| x.get("ai").and_then(|s| s.as_str()) != Some(ai));
        arr.push(item);
    }
    serde_json::to_vec_pretty(&v).unwrap_or_default()
}

/// PDF 页数（`pdfinfo`）。取不到按 1 页算——不能因为数不出页数就不发布。
async fn pdf_pages(bytes: &[u8]) -> u32 {
    let dir = std::env::temp_dir().join(format!("rmcloud-pdf-{}", crate::sync::uniq()));
    let _ = tokio::fs::create_dir_all(&dir).await;
    let p = dir.join("a.pdf");
    if tokio::fs::write(&p, bytes).await.is_ok() {
        if let Ok(o) = tokio::process::Command::new("pdfinfo").arg(&p).output().await {
            let s = String::from_utf8_lossy(&o.stdout);
            for line in s.lines() {
                if let Some(n) = line.strip_prefix("Pages:") {
                    if let Ok(n) = n.trim().parse::<u32>() {
                        let _ = tokio::fs::remove_dir_all(&dir).await;
                        return n.max(1);
                    }
                }
            }
        }
    }
    let _ = tokio::fs::remove_dir_all(&dir).await;
    1
}

/// 同目录临时文件 + rename（断电不留半截）。
fn write_atomic(dst: &Path, data: &[u8]) -> std::io::Result<()> {
    let parent = dst
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".ai-{}.tmp", crate::sync::uniq()));
    std::fs::write(&tmp, data)?;
    if let Err(e) = std::fs::rename(&tmp, dst) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `head` + `tail` 写进 `path`（`head` 是父记忆，可能为空）。
async fn write_append(path: &Path, head: &str, tail: &str) {
    if let Some(p) = path.parent() {
        let _ = tokio::fs::create_dir_all(p).await;
    }
    let mut s = String::new();
    if !head.trim().is_empty() {
        s.push_str(head.trim_end());
        s.push('\n');
    }
    s.push_str(tail);
    let _ = tokio::fs::write(path, s).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(page: u32, rect: Option<[f64; 4]>, n: u32) -> AskReq {
        AskReq {
            mode: "select".into(),
            doc: "doc1".into(),
            page,
            rect,
            image_blob: None,
            crop_blob: None,
            text: "问".into(),
            session: None,
            ai_doc: "ai-x".into(),
            parent: String::new(),
            n,
        }
    }

    #[test]
    fn tail_is_utf8_safe() {
        assert_eq!(tail_chars("中文abc", 4), "文abc");
        assert_eq!(tail_chars("短", 10), "短");
    }

    #[test]
    fn safe_keeps_uuid_shape() {
        assert_eq!(safe("../a/b"), "ab");
        assert_eq!(safe("1a2b-3c"), "1a2b-3c");
    }

    #[test]
    fn uuid_is_stable_and_uuid_shaped() {
        let a = stable_uuid("AI answers");
        assert_eq!(a, stable_uuid("AI answers"));
        assert_ne!(a, stable_uuid("AI answers/x"));
        assert_eq!(a.len(), 36);
        let parts: Vec<&str> = a.split('-').collect();
        assert_eq!(parts.len(), 5);
        assert_eq!(parts[2].len(), 4);
        assert!(parts[2].starts_with('4'));
    }

    #[test]
    fn content_declares_pdf_and_pages() {
        let v: Value = serde_json::from_slice(&doc_content(3, 1234)).unwrap();
        assert_eq!(v["fileType"], "pdf");
        assert_eq!(v["pageCount"], 3);
        assert_eq!(v["sizeInBytes"], "1234");
        assert_eq!(v["orientation"], "portrait");
    }

    #[test]
    fn metadata_is_document_under_parent() {
        let v: Value = serde_json::from_slice(&doc_metadata("ai2", "sub", 5)).unwrap();
        assert_eq!(v["type"], "DocumentType");
        assert_eq!(v["parent"], "sub");
        assert_eq!(v["visibleName"], "ai2");
        assert_eq!(v["deleted"], false);
        // 库按字符串读 lastModified
        assert!(v["lastModified"].is_string());
    }

    #[test]
    fn pagedata_has_one_line_per_page() {
        assert_eq!(String::from_utf8(pagedata(2)).unwrap(), "Blank\nBlank\n");
        assert_eq!(String::from_utf8(pagedata(0)).unwrap(), "Blank\n");
    }

    #[test]
    #[test]
    fn first_line_skips_markup_and_truncates() {
        assert_eq!(first_line("# 标题\n\n正文第一行\n第二行"), "标题");
        assert_eq!(first_line("\n\n---\n  最终答案是 42  "), "最终答案是 42");
        assert_eq!(first_line("**加粗**开头"), "**加粗**开头");
        assert_eq!(first_line("* 列表项"), "列表项");
        assert_eq!(first_line(""), "");
        let long = first_line(&"字".repeat(300));
        assert!(long.chars().count() <= 121, "截断后 {} 字", long.chars().count());
        assert!(long.ends_with('…'));
    }

    #[test]
    fn refs_append_and_replace_same_ai() {
        let a = append_ref(None, "sub", "ai-1", &req(3, Some([1.0, 2.0, 3.0, 4.0]), 1), 7, "第一行");
        let v: Value = serde_json::from_slice(&a).unwrap();
        assert_eq!(v["folder"], "sub");
        assert_eq!(v["items"][0]["ai"], "ai-1");
        assert_eq!(v["items"][0]["kind"], "rect");
        assert_eq!(v["items"][0]["page"], 2); // refs 存 0 基
        assert_eq!(v["items"][0]["rect"][2], 3.0);

        // 同一份再发一次：替换不是追加
        let b = append_ref(Some(&a), "sub", "ai-1", &req(4, None, 2), 8, "第一行");
        let v: Value = serde_json::from_slice(&b).unwrap();
        assert_eq!(v["items"].as_array().unwrap().len(), 1);
        assert_eq!(v["items"][0]["kind"], "page");
        assert_eq!(v["items"][0]["page"], 3);

        // 另一份追加
        let c = append_ref(Some(&b), "sub", "ai-2", &req(5, None, 3), 9, "第一行");
        let v: Value = serde_json::from_slice(&c).unwrap();
        assert_eq!(v["items"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn broken_refs_start_over() {
        let v: Value =
            serde_json::from_slice(&append_ref(Some(b"not json"), "s", "a", &req(0, None, 1), 1, ""))
                .unwrap();
        assert_eq!(v["items"].as_array().unwrap().len(), 1);
    }
}
