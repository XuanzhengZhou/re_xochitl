//! 手写转文字：设备端的接线（C14）。
//!
//! 服务器契约照 `cloud/src/ocr.rs` 抄（**别猜**）：
//!
//! * 先 `POST /api/v1/blob`（`X-Sha256`，`net.rs::put_blob`）拿内容寻址的 sha；
//! * 再 `POST /api/v1/ocr`（`Authorization: Bearer <token>`）——
//!   `OcrReq` 只有三个字段：`image_blob` / `mode` / `lang`；
//! * 成功 `200 {"text": "...", "lines": [], "engine": "dsh-read"}`；
//! * 失败非 200 `{"error": "...", "detail": "..."}`；空文字服务器直接
//!   `502 {"error":"ocr failed","detail":"empty result"}`。
//!
//! 这一页的识别结果写进页面的**文字层**（`doc/rmtext.rs` 的 `SceneText`），
//! 不碰墨迹、不碰图层结构：官方 `.rm` 里只有整篇一个 `pos_x/pos_y`，逐行位置
//! 自己排，行距 `size * 1.45`（和 `library.rs::p11_layout` 同一条）。
//!
//! 所有判断都做成纯函数，方便单测；网络在 `run()` 里，由主程序丢后台线程。

use std::time::Duration;

use crate::doc::rmtext::{ParagraphStyle, SceneText};
use crate::net::Config;

/// 服务器 `OcrReq` 的三个字段（`cloud/src/ocr.rs:34`）+ 本机的路由信息。
///
/// `doc` / `page` 不发给服务器（`#[serde(skip)]`）：服务器只认图片 blob，
/// 它们只是让后台结果回来时知道该写哪一页。
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct OcrReq {
    #[serde(skip)]
    pub doc: String,
    #[serde(skip)]
    pub page: usize,
    pub image_blob: String,
    pub mode: String,
    pub lang: String,
}

/// 识别模式：手写（服务器目前不拿这两个值当参数，只做确定性识别）。
pub const MODE: &str = "handwriting";
/// 手写语言。官方 `Settings.handwritingConversionLanguage` 默认英文；服务器只跑英文。
pub const LANG: &str = "en";

/// 正文默认字号：官方文件没有字号，按 `ParagraphStyle::PLAIN.default_size()` 的 25。
pub const SIZE: f32 = 25.0;
/// 行距倍数（官方 QML `line-height: 1.45`，同 `p11_layout`）。
pub const LINE: f32 = 1.45;
/// 文字层默认左上角：官方 `RmText::default()` 的 `pos_x/pos_y`
/// （`rmtext.rs:204`，来自 `scene_stream.py:985-993`），是**页面中心坐标**。
pub const X0: f32 = -468.0;
pub const Y0: f32 = 234.0;

/// 进度/结果提示（主程序把它们塞进现有 toast/notice）。
pub const PROGRESS: &str = "Converting...";
pub const DONE: &str = "Text added";

/// 设备等 120 s（和 `net.rs::ocr` 一样；服务器跑一次 dsh 要几十秒）。
const TIMEOUT: Duration = Duration::from_secs(120);

/// 纯函数：组一个请求（`doc`/`page` 只留本机）。
pub fn req(doc: &str, page: usize, blob: &str) -> OcrReq {
    OcrReq {
        doc: doc.to_string(),
        page,
        image_blob: blob.to_string(),
        mode: MODE.to_string(),
        lang: LANG.to_string(),
    }
}

/// 纯函数：请求 JSON。字段名/数量照服务器 `OcrReq`。
pub fn req_json(r: &OcrReq) -> String {
    serde_json::to_string(r).unwrap_or_default()
}

/// 走网络：上传 PNG（拿 sha）→ `POST /api/v1/ocr`。**只在后台线程里调**。
pub fn run(cfg: &Config, doc: &str, page: usize, png: &[u8]) -> Result<String, String> {
    if png.is_empty() {
        return Err("Could not render page".to_string());
    }
    let blob = crate::net::put_blob(cfg, png).map_err(|e| net_reason(&e))?;
    let body = req_json(&req(doc, page, &blob));
    let url = format!("{}/api/v1/ocr", cfg.base());
    let r = crate::net::request(
        "POST",
        &url,
        &cfg.token,
        Some("application/json"),
        body.as_bytes(),
        TIMEOUT,
    )
    .map_err(|e| net_reason(&e))?;
    parse_reply(r.status, &r.text())
}

/// 纯函数：解析响应。返回值可直接当 toast 文案（英文原因）。
///
/// 200 且文字非空才 `Ok`；200 但空文字、非 200、非法 JSON 都是 `Err`。
pub fn parse_reply(status: u16, body: &str) -> Result<String, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|_| "Bad response from server".to_string())?;
    if status != 200 {
        let err = v.get("error").and_then(|x| x.as_str()).unwrap_or("");
        let detail = v.get("detail").and_then(|x| x.as_str()).unwrap_or("");
        let all = format!("{err} {detail}").to_ascii_lowercase();
        return Err(if status == 401 || status == 403 {
            "Not paired".to_string()
        } else if all.contains("empty") {
            // 服务器把"没认出字"报成 502 + empty result（ocr.rs:96）
            "No text found".to_string()
        } else if status == 400 || status == 404 || status == 422 {
            "Upload failed".to_string()
        } else if status >= 500 {
            "Server error".to_string()
        } else {
            format!("OCR failed (HTTP {status})")
        });
    }
    let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("");
    if text.trim().is_empty() {
        return Err("No text found".to_string());
    }
    Ok(text.to_string())
}

/// 网络错误的英文原因（`put_blob` / `request` 的 Err 都是给人看的短句）。
fn net_reason(e: &str) -> String {
    let l = e.to_ascii_lowercase();
    if l.contains("unauthorized") || l.contains("401") {
        "Not paired".to_string()
    } else if l.contains("timed out") || l.contains("timeout") {
        "Connection timed out".to_string()
    } else if l.contains("no_blob") || l.contains("bad_blob") {
        "Upload failed".to_string()
    } else {
        "No connection".to_string()
    }
}

/// 纯函数：多行文字 → 每行一个 `SceneText`。
///
/// * 首尾空行丢掉、中间空行跳过（不造空段落），`\r` / 行尾空白清掉；
/// * `x` 全部对齐 `x`（第一行左缘），`y` 逐行加 `size * 1.45`；
/// * 样式统一 PLAIN（正文），颜色 0（黑）。
pub fn layout(text: &str, x: f32, y: f32, size: f32) -> Vec<SceneText> {
    let mut out = Vec::new();
    let mut cy = y;
    for line in text.lines() {
        let line = line.trim_end();
        if line.trim().is_empty() {
            continue;
        }
        let mut t = SceneText::new(x, cy, size, line);
        t.style = ParagraphStyle::PLAIN;
        out.push(t);
        cy += size * LINE;
    }
    out
}

/// 纯函数：往已有文字层**后面追加**（不改已有的）。
///
/// 官方文件只有整篇一个 `pos_x/pos_y`，重新读盘时 `p11_layout` 会按第一条的
/// `x` 重排所有行——所以新行 `x` 取第一条的 `x`、`y` 接在最后一行的下一格，
/// 落盘再读回来位置不会跳。
pub fn scene_texts(existing: &[SceneText], text: &str) -> Vec<SceneText> {
    let (x, y) = match (existing.first(), existing.last()) {
        (Some(first), Some(last)) => (first.x, last.y + last.size * LINE),
        _ => (X0, Y0),
    };
    layout(text, x, y, SIZE)
}

/// 入口判定：没配对 / 正在转 / 没有可写的页 / 可以发。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    NotPaired,
    Busy,
    NoPage,
    Ready,
}

/// 纯函数：这一下点击该不该发请求。
pub fn gate(configured: bool, busy: bool, has_page: bool) -> Gate {
    if !configured {
        Gate::NotPaired
    } else if busy {
        Gate::Busy
    } else if !has_page {
        Gate::NoPage
    } else {
        Gate::Ready
    }
}

/// 纯函数：被拦下来时给哪句提示（`Ready` 没有提示）。
pub fn gate_notice(g: Gate) -> Option<&'static str> {
    match g {
        Gate::NotPaired => Some("Pair a server first"),
        Gate::Busy => Some("Please wait for the current task"),
        Gate::NoPage => Some("Not available"),
        Gate::Ready => None,
    }
}

/// 纯函数：这一下要不要真的发请求（只有 `Ready` 发）。
pub fn can_send(g: Gate) -> bool {
    g == Gate::Ready
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::rm::RmPage;
    use crate::doc::rmtext::{parse_texts, serialize_texts};
    use crate::pen::{Color, Point, Tool};

    fn sha() -> String {
        "0123456789abcdef".repeat(4)
    }

    // ------------------------------------------------------------ 请求体

    /// `doc` / `page` / `blob` 都要在结构里；JSON 只有服务器认的三个字段。
    #[test]
    fn request_carries_doc_page_blob_and_server_fields() {
        let r = req("1554f45c-b223-40d2-adf7-d436232fa56a", 7, &sha());
        assert_eq!(r.doc, "1554f45c-b223-40d2-adf7-d436232fa56a");
        assert_eq!(r.page, 7);
        assert_eq!(r.image_blob, sha());
        assert_eq!(r.mode, MODE);
        assert_eq!(r.lang, LANG);

        let body = req_json(&r);
        let v: serde_json::Value = serde_json::from_str(&body).expect("请求体必须是合法 JSON");
        assert_eq!(v["image_blob"], sha());
        assert_eq!(v["mode"], "handwriting");
        assert_eq!(v["lang"], "en");
        let obj = v.as_object().unwrap();
        assert_eq!(obj.len(), 3, "服务器 OcrReq 只有三个字段，doc/page 是本机路由");
        assert!(obj.get("doc").is_none());
        assert!(obj.get("page").is_none());
    }

    /// 往返：服务器那三个字段能再读回来，doc/page 保持默认（本机才有）。
    #[test]
    fn request_roundtrip_keeps_server_fields() {
        let r = req("u", 1, &sha());
        let back: OcrReq = serde_json::from_str(&req_json(&r)).unwrap();
        assert_eq!(back.image_blob, r.image_blob);
        assert_eq!(back.mode, r.mode);
        assert_eq!(back.lang, r.lang);
        assert!(back.doc.is_empty());
        assert_eq!(back.page, 0);
    }

    // ------------------------------------------------------------ 响应

    #[test]
    fn parse_reply_ok() {
        let body = r#"{"text":"hello\nworld","lines":[],"engine":"dsh-read"}"#;
        assert_eq!(parse_reply(200, body).unwrap(), "hello\nworld");
    }

    #[test]
    fn parse_reply_empty_text_is_an_error() {
        assert_eq!(parse_reply(200, r#"{"text":""}"#).unwrap_err(), "No text found");
        assert_eq!(
            parse_reply(200, r#"{"text":"  \n\t "}"#).unwrap_err(),
            "No text found"
        );
        assert_eq!(parse_reply(200, r#"{"lines":[]}"#).unwrap_err(), "No text found");
    }

    #[test]
    fn parse_reply_tolerates_extra_fields() {
        let body = r#"{"text":"abc","lines":["abc"],"engine":"dsh-read","cost":0.5,"x":null}"#;
        assert_eq!(parse_reply(200, body).unwrap(), "abc");
    }

    #[test]
    fn parse_reply_rejects_bad_json() {
        assert_eq!(
            parse_reply(200, "<html>502</html>").unwrap_err(),
            "Bad response from server"
        );
        assert_eq!(parse_reply(502, "").unwrap_err(), "Bad response from server");
    }

    /// 服务器各档错误码 → 英文提示（空文字是 502 + empty result）。
    #[test]
    fn parse_reply_status_mapping() {
        assert_eq!(
            parse_reply(401, r#"{"error":"unauthorized"}"#).unwrap_err(),
            "Not paired"
        );
        assert_eq!(
            parse_reply(502, r#"{"error":"ocr failed","detail":"empty result"}"#).unwrap_err(),
            "No text found"
        );
        assert_eq!(
            parse_reply(500, r#"{"error":"io","detail":"disk"}"#).unwrap_err(),
            "Server error"
        );
        assert_eq!(
            parse_reply(400, r#"{"error":"bad_blob"}"#).unwrap_err(),
            "Upload failed"
        );
        assert_eq!(
            parse_reply(404, r#"{"error":"no_blob"}"#).unwrap_err(),
            "Upload failed"
        );
        assert_eq!(
            parse_reply(418, r#"{"error":"teapot"}"#).unwrap_err(),
            "OCR failed (HTTP 418)"
        );
    }

    #[test]
    fn net_errors_become_short_english() {
        assert_eq!(net_reason("unauthorized"), "Not paired");
        assert_eq!(net_reason("read timed out"), "Connection timed out");
        assert_eq!(net_reason("no_blob"), "Upload failed");
        assert_eq!(net_reason("connect 1.2.3.4:8080: refused"), "No connection");
    }

    // ------------------------------------------------------------ 布局

    #[test]
    fn layout_multiline_increases_y_by_line_pitch() {
        let v = layout("one\ntwo\nthree", X0, Y0, SIZE);
        assert_eq!(v.len(), 3);
        assert_eq!(v.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(), ["one", "two", "three"]);
        for w in v.windows(2) {
            assert!(w[1].y > w[0].y, "y 必须递增");
            let d = w[1].y - w[0].y;
            assert!((d - SIZE * 1.45).abs() < 1e-3, "行距应为 size*1.45，实际 {d}");
        }
        // x 对齐首行、样式是正文、字号统一
        assert!(v.iter().all(|t| t.x == X0));
        assert!(v.iter().all(|t| t.size == SIZE));
        assert!(v.iter().all(|t| t.style == ParagraphStyle::PLAIN));
        assert_eq!(v[0].y, Y0);
    }

    #[test]
    fn layout_drops_blank_lines_and_trailing_space() {
        let v = layout("\n  \r\nfirst  \r\n\r\nsecond\n\n", X0, Y0, SIZE);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].text, "first");
        assert_eq!(v[1].text, "second");
        assert!((v[1].y - v[0].y - SIZE * 1.45).abs() < 1e-3);
    }

    #[test]
    fn layout_empty_text_is_empty() {
        assert!(layout("", X0, Y0, SIZE).is_empty());
        assert!(layout(" \n\t\n", X0, Y0, SIZE).is_empty());
    }

    /// 已有文字层：新行接在最后一行下一格，`x` 对齐第一条（官方重排也按第一条）。
    #[test]
    fn append_starts_below_existing_block() {
        let mut a = SceneText::new(X0, Y0, 35.0, "heading");
        a.style = ParagraphStyle::HEADING;
        let existing = vec![a, SceneText::new(X0, Y0 + 35.0 * LINE, 25.0, "body")];
        let new = scene_texts(&existing, "line a\nline b");
        assert_eq!(new.len(), 2);
        assert_eq!(new[0].x, existing[0].x);
        let want = existing[1].y + existing[1].size * LINE;
        assert!((new[0].y - want).abs() < 1e-3);
        assert!((new[1].y - new[0].y - SIZE * LINE).abs() < 1e-3);
        // 原数组一个字节都没动
        assert_eq!(existing.len(), 2);
        assert_eq!(existing[1].text, "body");
    }

    #[test]
    fn fresh_page_uses_official_default_origin() {
        let new = scene_texts(&[], "x");
        assert_eq!(new[0].x, -468.0);
        assert_eq!(new[0].y, 234.0);
    }

    // ------------------------------------------------------------ 不碰墨迹

    fn stroke(y: f32) -> crate::pen::Line {
        let mut l = crate::pen::Line::new(Tool::FinelinerV2, Color::Black, 2.0);
        for i in 0..8 {
            l.push(Point {
                x: 200.0 + i as f32 * 40.0,
                y,
                speed: 1.0,
                dir: 0.5,
                width: 4.0,
                pressure: 0.6,
            });
        }
        l
    }

    /// 假 `.rm`（两图层几笔）→ 写文字层 → 再 parse：layers 逐字段完全一致。
    #[test]
    fn writing_text_does_not_touch_ink() {
        let mut page = RmPage::default();
        page.author = [7u8; 16];
        page.lines_mut().push(stroke(300.0));
        page.lines_mut().push(stroke(500.0));
        page.add_layer().unwrap();
        page.layers[1].name = "Layer 2".into();
        page.layers[1].visible = false;
        page.lines_mut().push(stroke(700.0));
        let base = page.serialize();

        let before = RmPage::parse(&base).unwrap();
        let sig = |p: &RmPage| {
            let mut v: Vec<(String, bool, Vec<String>)> = p
                .layers
                .iter()
                .map(|l| {
                    (
                        l.name.clone(),
                        l.visible,
                        l.lines.iter().map(|x| format!("{x:?}")).collect(),
                    )
                })
                .collect();
            v.push(("author".into(), true, vec![format!("{:?}", p.author)]));
            v
        };
        let ink_before = sig(&before);
        assert_eq!(before.total_lines(), 3);

        let texts = scene_texts(&[], "handwriting line one\nline two");
        let out = serialize_texts(&base, &texts).unwrap();
        let after = RmPage::parse(&out).unwrap();

        assert_eq!(sig(&after), ink_before, "墨迹/图层被文字层写坏了");
        assert_eq!(after.layers.len(), 2);
        assert!(!after.layers[1].visible);
        assert_eq!(after.total_lines(), 3);
        // 文字确实写进去了
        let read = parse_texts(&out).unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[0].text, "handwriting line one");
        // 原文件字节没被就地改
        assert_eq!(RmPage::parse(&base).unwrap().total_lines(), 3);
    }

    /// 空文字层是 `strip_text`：墨迹仍然一致。
    #[test]
    fn removing_text_keeps_ink() {
        let mut page = RmPage::default();
        page.lines_mut().push(stroke(100.0));
        let base = page.serialize();
        let with = serialize_texts(&base, &scene_texts(&[], "abc")).unwrap();
        let cleaned = serialize_texts(&with, &[]).unwrap();
        assert_eq!(RmPage::parse(&cleaned).unwrap().total_lines(), 1);
        assert!(parse_texts(&cleaned).unwrap().is_empty());
    }

    /// 真落盘那条路：临时库里写一页，`save_scene_texts` 走官方 `.rm` +
    /// 同目录临时文件 + rename；写完整页能读回文字、墨迹不变、不留 `.tmp`。
    #[test]
    fn saves_into_official_text_layer_atomically() {
        let dir = std::env::temp_dir().join(format!("rmdev-ocr-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let uuid = "doc1";
        std::fs::create_dir_all(dir.join(uuid)).unwrap();
        std::fs::write(
            dir.join(format!("{uuid}.content")),
            r#"{"cPages":{"pages":[{"id":"p1"}]}}"#,
        )
        .unwrap();
        let mut page = RmPage::default();
        page.author = [3u8; 16];
        page.lines_mut().push(stroke(400.0));
        let base = page.serialize();
        std::fs::write(dir.join(uuid).join("p1.rm"), &base).unwrap();

        let lib = crate::doc::Library::with_root(&dir);
        let texts = scene_texts(&[], "hello\nworld");
        let kind = lib.save_scene_texts(uuid, 0, &texts).unwrap();
        assert_eq!(kind, crate::doc::rmtext::StoreKind::Official);

        let rm = dir.join(uuid).join("p1.rm");
        let after = RmPage::parse(&std::fs::read(&rm).unwrap()).unwrap();
        assert_eq!(after.total_lines(), 1, "墨迹被写坏了");
        let back = lib.load_scene_texts(uuid, 0);
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].text, "hello");
        assert!((back[1].y - back[0].y - SIZE * 1.45).abs() < 0.01);

        let leftovers: Vec<String> = std::fs::read_dir(dir.join(uuid))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "原子写留下了临时文件：{leftovers:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------------------ 状态机

    #[test]
    fn gate_matrix() {
        assert_eq!(gate(false, false, true), Gate::NotPaired);
        assert_eq!(gate(false, true, true), Gate::NotPaired);
        assert_eq!(gate(true, true, true), Gate::Busy);
        assert_eq!(gate(true, false, false), Gate::NoPage);
        assert_eq!(gate(true, false, true), Gate::Ready);
    }

    /// 未配对 / 进行中再点一次：给提示、**不发请求**；可以发的时候没有提示。
    #[test]
    fn gate_notices_and_send_decision() {
        assert_eq!(gate_notice(Gate::NotPaired), Some("Pair a server first"));
        assert!(!can_send(Gate::NotPaired));

        assert_eq!(
            gate_notice(Gate::Busy),
            Some("Please wait for the current task")
        );
        assert!(!can_send(Gate::Busy));

        assert_eq!(gate_notice(Gate::NoPage), Some("Not available"));
        assert!(!can_send(Gate::NoPage));

        assert_eq!(gate_notice(Gate::Ready), None);
        assert!(can_send(Gate::Ready));
    }

    /// 成功/进度文案就这两句（任务要求）。
    #[test]
    fn progress_and_done_texts() {
        assert_eq!(PROGRESS, "Converting...");
        assert_eq!(DONE, "Text added");
    }

    // ------------------------------------------------------------ 端到端（本机 mock 服务器）

    /// 读一个 HTTP 请求：返回 (请求行+头, body)。只够测试用。
    fn read_http(s: &mut std::net::TcpStream) -> (String, Vec<u8>) {
        use std::io::Read;
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = s.read(&mut tmp).unwrap();
            if n == 0 {
                return (String::new(), Vec::new());
            }
            buf.extend_from_slice(&tmp[..n]);
            let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buf[..i]).to_string();
            let len = head
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                })
                .unwrap_or(0);
            while buf.len() < i + 4 + len {
                let n = s.read(&mut tmp).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            return (head, buf[i + 4..i + 4 + len].to_vec());
        }
    }

    /// `run()` 真走两条 HTTP：先 `POST /api/v1/blob`（body = PNG），再
    /// `POST /api/v1/ocr`（body 里 image_blob = 那张 PNG 的 sha，字段恰好三个）。
    #[test]
    fn run_uploads_png_then_posts_the_ocr_request() {
        use std::io::Write;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let mut seen: Vec<(String, Vec<u8>)> = Vec::new();
            for _ in 0..2 {
                let (mut s, _) = listener.accept().unwrap();
                let (head, body) = read_http(&mut s);
                let reply = if head.starts_with("POST /api/v1/blob ") {
                    "HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                } else {
                    let body = r#"{"text":"hello from mock","lines":[],"engine":"dsh-read"}"#;
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                };
                let _ = s.write_all(reply.as_bytes());
                seen.push((head, body));
            }
            seen
        });

        let cfg = Config {
            url: format!("http://127.0.0.1:{port}"),
            token: "test-token".to_string(),
            ..Config::default()
        };
        let gray = [255u8, 0, 0, 255, 255, 255, 255, 255, 255];
        let png = crate::share::png_gray(&gray, 3, 3);
        let text = run(&cfg, "doc-uuid-1", 4, &png).unwrap();
        assert_eq!(text, "hello from mock");

        let seen = server.join().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[0].0.starts_with("POST /api/v1/blob "));
        assert_eq!(seen[0].1, png, "上传的字节必须就是这张 PNG");
        assert!(seen[1].0.starts_with("POST /api/v1/ocr "));
        let sent: serde_json::Value = serde_json::from_slice(&seen[1].1).unwrap();
        assert_eq!(sent["image_blob"], crate::net::sha256_hex(&png));
        assert_eq!(sent["mode"], "handwriting");
        assert_eq!(sent["lang"], "en");
        assert_eq!(sent.as_object().unwrap().len(), 3);
    }
}
