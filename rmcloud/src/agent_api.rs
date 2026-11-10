//! Agent 网关：`POST /api/v1/agent/ask`（dsh NDJSON → SSE）+ 设备工具代理。C5。
//!
//! - 一次问答 = 一个 `dsh --profile headless --json` 子进程，stdout 逐行 NDJSON；
//!   每读到一条 `type=text` 就推一条 SSE `token`，所以设备是逐字收到的。
//! - 串行：整轮调用持有 `App::dsh_lock`；忙时立即 503（rM2 一次只问一个）。
//! - `session` 默认由 doc 派生，dsh 不认这个 id 时去掉 `--session-id` 重试一次。
//! - 成本：`step_end` 的 usage 累加后交给 `usage::record`（cC 的模块，见 `record_usage`）。
//! - 图：`image_blob` 指向整页 PNG 时，把它物化到 `<state>/cache/ask/<sha>.png`
//!   （长边缩到 1600），`rect` 再从原图裁一张（保原分辨率），prompt 里给**绝对路径**
//!   + `read_image` 指令，让模型直接看图而不是"自己想起来去调工具"（[`prepare`]）。

use std::convert::Infallible;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{Path as UrlPath, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use futures_util::stream::poll_fn;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::png;
use crate::state::App;

/// 整轮 dsh 调用的上限。超时就回 `error` 事件（子进程随 future 一起被杀）。
/// 一次 dsh 调用的硬上限。
///
/// **实测（2026-11-09）**：
/// * 无图的短问答 8~12 s；
/// * 带整页图 + 裁图的 select 问答 11~40 s；
/// * `grade`（要求逐题找错并回 JSON）在**内容密集**的一页上超过 180 s 被砍过一次。
/// 所以放宽到 300 s —— 和 OCR 那条一致。设备端 `net.rs` 对应 300 s。
const DSH_TIMEOUT: Duration = Duration::from_secs(300);
/// SSE 心跳。设备 15 s 没事件会重连，这里 10 s 一次注释行。
const KEEPALIVE: Duration = Duration::from_secs(10);
/// stderr 只留前 64 KiB，防止 dsh 爆日志把内存吃了。
const STDERR_LIMIT: u64 = 64 * 1024;
/// 整页图落盘时的长边上限。
///
/// 实测（2026-11）：3810×4311 的纯手写页 8.7 s 答完；1404×1872 的
/// 「印刷 PDF 底图 + 手写」合成页要 1 min 47 s——大图会诱发模型反复
/// `read_image` 裁剪/放大核对。缩到 1600 一次看完，也避免以后设备发 2x 图。
const MAX_SIDE: u32 = 1600;
/// 设备整页坐标系：`AskReq.rect` 用它，原点左上。
const PAGE_W: f64 = 1404.0;
/// 设备整页坐标系的高度。
const PAGE_H: f64 = 1872.0;
/// 图物化的缓存目录（相对 `cfg.state`）。
const CACHE_DIR: &str = "cache/ask";

/// 写临时文件用的序号，防同进程并发撞名。
static SEQ: AtomicU64 = AtomicU64::new(0);

/// `POST /api/v1/agent/ask` 的请求体（见 `docs/CLOUD.md`）。
#[derive(Debug, Clone, Deserialize)]
pub struct AskReq {
    /// `select` / `live` / `review` / `organize`。
    #[serde(default)]
    pub mode: String,
    /// 笔记 uuid。
    #[serde(default)]
    pub doc: String,
    #[serde(default)]
    pub page: u32,
    /// 选区 `[x0, y0, x1, y1]`，设备像素，原点左上（`select` 才有）。
    #[serde(default)]
    pub rect: Option<[f64; 4]>,
    /// 整页 PNG 的 blob sha。
    #[serde(default)]
    pub image_blob: Option<String>,
    /// 设备已经裁好的选区 PNG 的 blob sha（可选）。
    ///
    /// 给了就直接用它当细节图——设备裁的坐标系一定对，服务器也省一次解码+裁剪。
    /// 整页模式下设备会把它设成和 `image_blob` 一样，两条路去重成同一份缓存。
    #[serde(default)]
    pub crop_blob: Option<String>,
    /// 用户输入（`live` 模式是键盘打的字）。
    #[serde(default)]
    pub text: String,
    /// 可选：续聊的 session id。缺省由 [`session_for`] 从 doc 派生。
    #[serde(default)]
    pub session: Option<String>,
    /// 这一轮回答要落成的 PDF 的 uuid（设备生成）。空 = 不落 PDF（批改/live 之类）。
    #[serde(default)]
    pub ai_doc: String,
    /// 追问的上一份回答 PDF 的 uuid；空 = 从源文档重新起一条链。
    #[serde(default)]
    pub parent: String,
    /// 第几轮（设备分配，只用于记忆/显示）。
    #[serde(default)]
    pub n: u32,
}

/// 一条 NDJSON 行的语义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    /// `type=text`：增量回答文本（原样透传）。
    Text(String),
    /// `type=status && phase=step_end`：本步用量。
    Usage { in_tok: u64, out_tok: u64 },
    /// `turn_end` 里 `reason.kind=error`，或 `type=error`。
    Error(String),
    /// `session` / `thinking` / `turn_start` / `final` / 非 JSON 行……都不需要转发。
    Ignore,
}

/// 由 doc 派生的 session：`session-rm-<uuid 前 8 位>`。
///
/// 只留 ASCII 字母数字并转小写（uuid 的短横线去掉），不足 8 位就有几位用几位；
/// 空 doc 给个固定兜底，保证同一篇笔记永远落同一个 session（吃 dsh 的 prompt 缓存）。
pub fn session_for(doc: &str) -> String {
    let id: String = doc
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .take(8)
        .collect();
    let id = if id.is_empty() {
        "00000000".to_string()
    } else {
        id
    };
    format!("session-rm-{id}")
}

/// 一次问答要用到的图。prompt 里只给**绝对路径**，让 dsh 用 `read_image` 直接打开。
///
/// 图不可用时不让请求失败：`full_err` / `rect_err` 换成 prompt 里的
/// 「图不可用（原因）」，其余照常（见 [`prepare`]）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Images {
    /// 整页渲染图（长边已缩到 ≤ [`MAX_SIDE`]）。
    pub full: Option<PathBuf>,
    /// 整页图落盘后的像素尺寸。
    pub full_size: Option<(u32, u32)>,
    /// 框选裁图；按**原图**分辨率裁，细节看这张。
    pub rect: Option<PathBuf>,
    /// 裁图在整页图里对应的像素框 `[x0, y0, x1, y1)`。
    pub rect_box: Option<[u32; 4]>,
    /// 整页图不可用的原因。
    pub full_err: Option<String>,
    /// 裁图不可用的原因（整页图可能仍然是好的）。
    pub rect_err: Option<String>,
}

/// 按 mode 拼提示词。纯函数，单测覆盖关键措辞。
pub fn build_prompt(req: &AskReq, images: &Images) -> String {
    build_prompt_mem(req, images, "")
}

/// 带记忆的版本：`memory` 非空时插在「图」之后、「用户问题」之前。
pub fn build_prompt_mem(req: &AskReq, images: &Images, memory: &str) -> String {
    let mut p = String::new();
    match req.mode.as_str() {
        "select" => {
            p.push_str("你在 rM2 手写笔记的 AI 助手。用户框选了第 ");
            p.push_str(&req.page.to_string());
            p.push_str(" 页的一块区域（rect = [x0,y0,x1,y1]，设备像素，原点左上）：");
            match req.rect {
                Some(r) => p.push_str(&format!("[{}, {}, {}, {}]。", r[0], r[1], r[2], r[3])),
                None => p.push_str("（没有给精确矩形，按整页理解）。"),
            }
            p.push_str("只答选区问题但要能读全页：可以结合整页内容和全库上下文理解题目，回答只针对选区里的问题。\n");
        }
        "live" => {
            p.push_str(
                "实时模式（用户边写边问）。短答、口语、中文：先给结论，最多三句，不复述题目、不铺陈。\n",
            );
            // 设备端现在**会排公式**：回答被渲染成墨迹写到纸上（`panels::inknote`
            // 走服务器 `/api/v1/tex` 的 pdflatex），所以要求模型用 `$...$` 写 TeX。
            // 以前这里让人写 Unicode，是因为那一栏是纯文本流、不渲染公式；现在
            // `∫_{a}^{t}` 这种伪公式会被原样画出来（真机见过 `∫██` 的豆腐块）。
            p.push_str(
                "数学一律用 LaTeX 写在 $...$ 里（例如 $\\int_a^t f(x)\\,dx$、$\\frac{1}{2}$、\n\
                 $x^2+y^2=1$）。**不要**写 `∫_{a}^{t}` 这种 Unicode 伪公式，\n\
                 也不要用 \\[ \\] 或 $$ 块级公式——这一栏只认 $...$。\n",
            );
        }
        "review" => {
            p.push_str("批改模式：逐题批改这一页的作答。每题先说对错，再指出错在哪一步，最后给正确写法，按题号顺序。\n");
        }
        // 批改并回传**可执行批注**（设备端 `panels::annotate` 直接吃这个 JSON）。
        // 注意：这里的坐标是设备像素（1404×1872，原点左上），上面图片段落已经
        // 告诉模型"整页图 N×M 对应设备坐标 1404×1872"，它要自己换算。
        "grade" => {
            p.push_str(
                "批改模式（回传批注）：检查这一页的手写作答，把**错的地方**标出来，并写一句批语。\n",
            );
            p.push_str(
                "只输出一个 JSON 对象，前后不要任何解释、不要 markdown 代码围栏：\n",
            );
            p.push_str("{\"kind\":\"annotate\",\"ops\":[ … ]}\n");
            p.push_str("ops 里可以有三种：\n");
            p.push_str("  {\"op\":\"box\",\"x0\":100,\"y0\":260,\"x1\":420,\"y1\":340}   在出错的地方画框\n");
            p.push_str("  {\"op\":\"stroke\",\"pts\":[[100,260],[420,260]],\"w\":2}  下划线/箭头/划掉\n");
            p.push_str("  {\"op\":\"text\",\"x\":120,\"y\":300,\"text\":\"这里漏了负号\",\"size\":30}  批语，y 是基线左端\n");
            p.push_str(
                "坐标一律用**设备像素**：原点左上，页宽 1404、页高 1872，必须落在页内。\n",
            );
            p.push_str(
                "每题最多一个框加一句批语（批语 ≤20 字）；不要把原题重抄一遍，不要给完整解答。\n",
            );
            // 批语是被**画成笔迹**写到纸上的（`panels::annotate` 把文字转成扫描线），
            // 没有公式渲染：写 `$1/2$` 用户看到的就是美元符号和反斜杠。
            p.push_str(
                "批语的 text 里**不要用 $ 或任何 LaTeX**（它会被画成手写字），\n\
                 数学用 Unicode 或中文（三分之一、±2、x²、∫）。\n",
            );
            p.push_str("如果这一页没有错误，就输出 {\"kind\":\"annotate\",\"ops\":[]}。\n");
            // 成本实测：这个模式下每次输出 6~7k token，而 JSON 本身只有 ~200。
            // 明文压一次"别把推理写出来"（有效无效下次量）。
            p.push_str(
                "推理只在你心里做，**不要输出思考过程**；输出的第一个字符必须是 `{`、最后一个必须是 `}`。\n",
            );
        }
        "organize" => {
            p.push_str("整理模式：把这一页的内容归纳成结构化要点（标题 / 要点 / 待办），并指出相关笔记和建议归档的位置。\n");
        }
        other => {
            p.push_str("模式：");
            p.push_str(other);
            p.push_str("（未知模式，按用户问题正常回答）。\n");
        }
    }
    p.push_str(
        "统一要求：数学一律用 $...$ 表示行内公式；不要用 markdown 代码围栏（三个反引号），不要输出 HTML。\n",
    );
    p.push_str("笔记 doc = ");
    p.push_str(&req.doc);
    p.push_str("，第 ");
    p.push_str(&req.page.to_string());
    p.push_str(" 页。\n");
    push_images(&mut p, req, images);
    if !memory.trim().is_empty() {
        p.push_str("\n—— 之前的对话记忆（同一份笔记 / 这条追问链，按时间顺序，越靠后越近）——\n");
        p.push_str(memory.trim_end());
        p.push('\n');
    }
    if req.text.trim().is_empty() {
        p.push_str("用户没有输入文字，请根据选区/页面内容判断要做什么。\n");
    } else {
        p.push_str("\n用户：\n");
        p.push_str(req.text.trim());
        p.push('\n');
    }
    // **最后再说一遍硬约束**。dsh 复用 session，长历史会把"先叙述我看图/我再检查"
    // 这类习惯固化下来，放中间压不住（真机实测：加了中间那份之后，第 2 轮回答的
    // 第一个字仍然是"我先查看整页渲染图和选区细节图"）。prompt 末尾的指令权重最大。
    let mh = if req.mode == "live" {
        "数学一律用 Unicode 直接写（∫ √ π ≈ ≤ ≥ 和 ₀¹²），不许出现 $ 和任何 LaTeX 命令"
    } else {
        "所有数学必须用 $...$ 包起来（哪怕只是一个数字、一个积分号）——不加 $ 设备端就渲染不出公式，用户会看到一串 ^ 和 _ 的乱码"
    };
    p.push_str(
        &format!(
            "\n—— 输出要求（覆盖上面所有冲突的措辞）——\n\
             1. 第一句就是给用户的答案，不许有任何「我先看一下 / 我先查看 / 让我检查 / 我将」这类叙述；\n\
             2. 不要复述题目、不要说用了哪个工具、不要解释你的步骤计划；\n\
             3. {mh}；\n\
             4. 只有数字或短式子的公式要和前后文字同一行，不要单独占一行。\n"
        ),
    );
    if !req.ai_doc.trim().is_empty() {
        p.push_str(
            "5. 这段回答会被服务器直接 LaTeX 编译成 PDF：不要用 markdown（# 标题、** 加粗、\n\
             | 表格 |、三个反引号），分点就用「1.」「- 」开头的普通行；别写 \\documentclass 或导言区。\n",
        );
    }
    p
}

/// prompt 里的图片段落：路径 + `read_image` 硬约束 + 选区在两套坐标里的范围。
///
/// 只给路径不够——模型会"想起来再调工具"；这里明确写"先用 read_image 看这张"，
/// 并且限制整页图只看一次（大图反复裁剪是实测里最慢的一环）。
fn push_images(p: &mut String, req: &AskReq, images: &Images) {
    let full = images.full.as_deref();
    if let Some(full) = full {
        let (w, h) = images.full_size.unwrap_or((0, 0));
        p.push_str(&format!(
            "先用 read_image 看这张整页渲染图（含印刷底图和手写）：{}\n",
            full.display()
        ));
        p.push_str(&format!(
            "分辨率：整页图 {w}×{h} 像素，对应设备坐标 1404×1872（已等比缩到长边不超过 {MAX_SIDE}）。\n"
        ));
        p.push_str("整页图只看一次 read_image，不要对大图反复裁剪/放大核对。\n");
    } else if let Some(err) = &images.full_err {
        p.push_str(&format!(
            "图不可用（{err}）：这次没有可看的整页渲染图。\n"
        ));
    }

    // 选区细节图：设备裁好的、服务器按 rect 裁的、或者干脆只有整页图。
    match (images.rect.as_deref(), full) {
        // crop_blob 和 image_blob 是同一张（整页模式）：别把同一个路径说两遍。
        (Some(detail), Some(full)) if detail == full => {
            p.push_str("整页图即选区（没有单独的选区细节图）。\n");
        }
        (Some(detail), _) => {
            if let Some([x0, y0, x1, y1]) = images.rect_box {
                if let Some(rect) = req.rect {
                    p.push_str(&format!(
                        "用户框选的是设备坐标 x∈[{}, {}), y∈[{}, {})，在整页图里对应像素 x∈[{x0}, {x1}), y∈[{y0}, {y1})。\n",
                        rect[0], rect[2], rect[1], rect[3]
                    ));
                }
            }
            p.push_str(&format!(
                "选区细节图（原始分辨率的局部裁片，要看细节就看它，别再放大整页图）：{}\n",
                detail.display()
            ));
        }
        (None, Some(_)) => {
            if let Some(rect) = req.rect {
                let err = images.rect_err.as_deref().unwrap_or("裁图生成失败");
                p.push_str(&format!(
                    "选区（设备坐标 x∈[{}, {}), y∈[{}, {})）没有可用的细节图（{err}），只能看整页图。\n",
                    rect[0], rect[2], rect[1], rect[3]
                ));
            } else {
                p.push_str("整页图即选区（没有单独的选区细节图）。\n");
            }
        }
        (None, None) => {}
    }

    if full.is_some() || images.rect.is_some() {
        if req.mode == "select" {
            p.push_str("除了选区，整页的其他内容和前后页也可能相关，必要时一起看。\n");
        } else {
            p.push_str("这一页的其他内容、前后页和相关笔记都可能相关，必要时一起看。\n");
        }
    }
}

/// 物化好的整页图；`data` 留着——要自己裁 rect 时不用再把 16 MB 的 blob 读一遍。
struct Page {
    sha: String,
    path: PathBuf,
    /// 落盘尺寸（长边缩过之后）。
    size: (u32, u32),
    /// 原图尺寸。
    src: (u32, u32),
    data: Vec<u8>,
}

/// 把 `image_blob` / `crop_blob` 物化到 `<state>/cache/ask/`。**全程不失败**：
/// blob 缺失 / 写盘失败 / PNG 解不开都只填 `*_err`，prompt 降级，请求照常。
///
/// 细节图优先级：`crop_blob`（设备裁好，坐标系一定对）→ 服务器按 `rect` 裁 →
/// 只有整页图。`crop_blob == image_blob`（整页模式）时落的是同一份缓存。
fn prepare(app: &App, req: &AskReq) -> Images {
    let mut images = Images::default();
    let dir = app.cfg.state.join(CACHE_DIR);

    // ---- 整页图
    let mut page = None;
    if let Some(sha) = blob_sha(&req.image_blob) {
        match full_page(app, &dir, &sha) {
            Ok(p) => {
                images.full = Some(p.path.clone());
                images.full_size = Some(p.size);
                page = Some(p);
            }
            Err(reason) => images.full_err = Some(reason),
        }
    }

    // ---- 选区细节图
    if let Some(sha) = blob_sha(&req.crop_blob) {
        match cached_blob(app, &dir, &sha) {
            Ok(path) => images.rect = Some(path),
            Err(reason) => images.rect_err = Some(reason),
        }
    }
    if images.rect.is_none() {
        if let (Some(rect), Some(page)) = (req.rect, page.as_ref()) {
            match cut(&dir, page, rect) {
                Ok(path) => {
                    images.rect = Some(path);
                    images.rect_err = None;
                }
                Err(reason) => images.rect_err = Some(reason),
            }
        }
    }

    // ---- 选区在整页图里的像素范围（两种细节图来源都要给）
    if let (Some(rect), Some((w, h))) = (req.rect, images.full_size) {
        let [x0, y0, x1, y1] = rect_pixels(rect, w, h);
        images.rect_box = Some([
            x0.clamp(0, w as i64) as u32,
            y0.clamp(0, h as i64) as u32,
            x1.clamp(0, w as i64) as u32,
            y1.clamp(0, h as i64) as u32,
        ]);
    }
    images
}

/// 整页图物化：读 blob → 缩到长边 ≤ [`MAX_SIDE`] → 落盘。
fn full_page(app: &App, dir: &Path, sha: &str) -> Result<Page, String> {
    let data = read_blob(app, sha)?;
    let Some(src) = png::dims(&data) else {
        return Err("PNG 解不开（读不到 IHDR）".to_string());
    };
    let size = png::scaled_dims(src.0, src.1, MAX_SIDE);
    let path = dir.join(format!("{sha}.png"));
    materialize(dir, &path, &data, src, size)?;
    Ok(Page {
        sha: sha.to_string(),
        path,
        size,
        src,
        data,
    })
}

/// 把任意一个 blob 物化成 `<sha>.png`（细节图用；也是缩到长边 ≤ [`MAX_SIDE`]）。
fn cached_blob(app: &App, dir: &Path, sha: &str) -> Result<PathBuf, String> {
    let data = read_blob(app, sha)?;
    let Some(src) = png::dims(&data) else {
        return Err("PNG 解不开（读不到 IHDR）".to_string());
    };
    let size = png::scaled_dims(src.0, src.1, MAX_SIDE);
    let path = dir.join(format!("{sha}.png"));
    materialize(dir, &path, &data, src, size)?;
    Ok(path)
}

/// 图落盘（幂等）：缓存里已有同尺寸的图就复用，否则该缩的缩、该写的写。
fn materialize(
    dir: &Path,
    path: &Path,
    data: &[u8],
    src: (u32, u32),
    size: (u32, u32),
) -> Result<(), String> {
    if header_dims(path) == Some(size) {
        return Ok(()); // 内容寻址：sha 一样图就一样，尺寸对就不用重写
    }
    let bytes = if size == src {
        // 不用缩：原样落盘，保留原编码（设备/ImageMagick 编的通常比我们小）
        data.to_vec()
    } else {
        let img = png::decode(data).map_err(|e| format!("PNG 解不开：{e}"))?;
        png::encode(&img.fit_within(MAX_SIDE))
    };
    write_atomic(dir, path, &bytes)
}

/// 按选区从**原图**裁一张（保住原分辨率，细节才看得清）。
fn cut(dir: &Path, page: &Page, rect: [f64; 4]) -> Result<PathBuf, String> {
    let img = png::decode(&page.data).map_err(|e| format!("PNG 解不开：{e}"))?;
    let [x0, y0, x1, y1] = rect_pixels(rect, page.src.0, page.src.1);
    let piece = img
        .crop(x0, y0, x1, y1)
        .ok_or_else(|| "选区太小（不足 8×8 像素）".to_string())?;
    let path = dir.join(format!(
        "{}-rect-{}-{}-{}-{}.png",
        page.sha, rect[0], rect[1], rect[2], rect[3]
    ));
    if header_dims(&path) != Some((piece.width, piece.height)) {
        write_atomic(dir, &path, &png::encode(&piece))?;
    }
    Ok(path)
}

/// 请求里的 blob sha：去空白、转小写、空串当没给。
fn blob_sha(raw: &Option<String>) -> Option<String> {
    raw.as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
}

/// 设备坐标（1404×1872）的选区 → 某张图上的像素框 `[x0, y0, x1, y1)`。
///
/// 图的实际尺寸未必是 1404×1872（设备可能发 2x），所以按 `实际宽/1404`、
/// `实际高/1872` 等比折算；坐标反着给也认（取 min/max）。越界留给 `Image::crop` clamp。
fn rect_pixels(rect: [f64; 4], img_w: u32, img_h: u32) -> [i64; 4] {
    let sx = img_w as f64 / PAGE_W;
    let sy = img_h as f64 / PAGE_H;
    let x = |v: f64| (v * sx).round() as i64;
    let y = |v: f64| (v * sy).round() as i64;
    [
        x(rect[0]).min(x(rect[2])),
        y(rect[1]).min(y(rect[3])),
        x(rect[0]).max(x(rect[2])),
        y(rect[1]).max(y(rect[3])),
    ]
}

/// 从 `<state>/blobs/<sha>` 取整页图字节。
///
/// sqlite 的 `blob` 表只有 `(sha, size)`——字节在文件里（`sync::store_blob` 写的）。
/// 表用来判断"服务器到底有没有这张图"，免得把 `No such file` 当成异常抛给用户。
fn read_blob(app: &App, sha: &str) -> Result<Vec<u8>, String> {
    if !is_sha(sha) {
        return Err(format!("blob sha 不合法：{sha}"));
    }
    let known = app
        .db()
        .query_row("SELECT size FROM blob WHERE sha=?1", [sha], |r| {
            r.get::<_, i64>(0)
        })
        .is_ok();
    if !known {
        return Err(format!("blob 不存在：{sha}"));
    }
    std::fs::read(app.blobs().join(sha)).map_err(|e| format!("读 blob 失败：{e}"))
}

fn is_sha(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 只读 PNG 头拿尺寸（不解码整张图）。
fn header_dims(path: &Path) -> Option<(u32, u32)> {
    let mut head = Vec::with_capacity(33);
    let file = std::fs::File::open(path).ok()?;
    file.take(33).read_to_end(&mut head).ok()?;
    png::dims(&head)
}

/// 先写临时文件再 rename：并发/崩溃都不会留半个 PNG 当缓存。
fn write_atomic(dir: &Path, path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("建缓存目录失败：{e}"))?;
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".tmp-{}-{seq}", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|e| format!("写盘失败：{e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("写盘失败：{e}")
    })
}

/// 解析 dsh 的一行 stdout。非 JSON 行、空行一律 `Ignore`（dsh 偶尔混警告）。
pub fn parse_event(line: &str) -> Parsed {
    let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
        return Parsed::Ignore;
    };
    match v.get("type").and_then(Value::as_str).unwrap_or("") {
        "text" => {
            let text = v.get("text").and_then(Value::as_str).unwrap_or("");
            Parsed::Text(text.to_string())
        }
        "status" => match v.get("phase").and_then(Value::as_str).unwrap_or("") {
            "step_end" => {
                let usage = v.get("usage");
                Parsed::Usage {
                    in_tok: usage
                        .and_then(|u| u.get("inputTokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    out_tok: usage
                        .and_then(|u| u.get("outputTokens"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                }
            }
            "turn_end" => {
                let reason = v.get("reason");
                let kind = reason
                    .and_then(|r| r.get("kind"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if kind == "error" {
                    let msg = reason
                        .and_then(|r| r.get("message"))
                        .and_then(Value::as_str)
                        .or_else(|| reason.and_then(|r| r.get("error")).and_then(Value::as_str))
                        .or_else(|| v.get("error").and_then(Value::as_str))
                        .unwrap_or("dsh turn failed");
                    Parsed::Error(msg.to_string())
                } else {
                    Parsed::Ignore
                }
            }
            _ => Parsed::Ignore,
        },
        "error" => {
            let msg = v
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| v.get("error").and_then(Value::as_str))
                .or_else(|| v.get("text").and_then(Value::as_str))
                .unwrap_or("dsh error");
            Parsed::Error(msg.to_string())
        }
        // session / thinking / turn_start / final / 其它未知事件
        _ => Parsed::Ignore,
    }
}

/// 一次 `dsh` 子进程的结果。
#[derive(Debug, Default)]
struct RunOutcome {
    in_tok: u64,
    out_tok: u64,
    /// 拼起来的完整回答（落 PDF 用；流式那份是边读边丢的）。
    text: String,
    /// 已经推过 token（重试会重复，所以推过就不重试）。
    emitted: bool,
    err: Option<String>,
    /// 子进程正常退出（exit 0）。
    exit_ok: bool,
    /// 真的起来过（spawn 失败就不折腾重试了）。
    spawned: bool,
    stderr: String,
}

impl RunOutcome {
    fn unknown_session(&self) -> bool {
        let hit = |s: &str| s.to_ascii_lowercase().contains("unknown session");
        self.err.as_deref().is_some_and(hit) || hit(&self.stderr)
    }

    /// 我们自己拼的 session 不存在：dsh 报 unknown session 或者直接非 0 退出。
    fn should_retry(&self) -> bool {
        self.spawned && (self.unknown_session() || !self.exit_ok)
    }
}

type Tx = mpsc::Sender<Result<SseEvent, Infallible>>;

async fn ask(State(app): State<Arc<App>>, headers: HeaderMap, Json(req): Json<AskReq>) -> Response {
    if !authorized(&app, &headers) {
        return json_error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    // 先探一下锁：忙就 503，别让设备挂在队尾。
    // （探完立刻放掉，真正的持有在 `drive` 里，覆盖整轮调用。极小的竞态窗口里
    //   第二个请求会排队而不是 503，但调用仍然是串行的。）
    if app.dsh_lock.try_lock().is_err() {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "agent busy");
    }

    let (tx, rx) = mpsc::channel::<Result<SseEvent, Infallible>>(32);
    tokio::spawn(async move { drive(app, req, tx).await });

    // tokio 1.54 去掉了 `stream` feature，`Receiver` 不再自带 `Stream`，
    // 用 futures-util 的 `poll_fn` 把一个 `poll_recv` 包成 Stream 交给 axum。
    let mut rx = rx;
    let stream = poll_fn(move |cx| rx.poll_recv(cx));
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(KEEPALIVE).text("keepalive"))
        .into_response()
}

/// handler 的后半段：拿锁、跑 dsh、把事件推进 channel。
async fn drive(app: Arc<App>, req: AskReq, tx: Tx) {
    let started = Instant::now();
    let derived = req
        .session
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none();
    let session = req
        .session
        .clone()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| session_for(&req.doc));
    // 图先落盘（幂等，命中缓存就只读一下 PNG 头），再拼 prompt。
    // 只起一次 dsh：图跟着 prompt 一起交给它，不额外跑一轮。
    let images = prepare(&app, &req);
    if let Some(err) = &images.full_err {
        eprintln!("[agent] 整页图不可用：{err}");
    }
    if let Some(err) = &images.rect_err {
        eprintln!("[agent] 选区裁图不可用：{err}");
    }
    // 记忆：追问用父回答的，否则用源文档累积的（`ai::memory_for` 已截断）。
    let memory = crate::ai::memory_for(&app.cfg.state, &req);
    let prompt = build_prompt_mem(&req, &images, &memory);

    let _guard = app.dsh_lock.lock().await;
    let outcome = tokio::time::timeout(
        DSH_TIMEOUT,
        run(app.clone(), prompt, session.clone(), derived, tx.clone()),
    )
    .await;
    let out = match outcome {
        Ok(out) => out,
        Err(_) => RunOutcome {
            err: Some(format!("dsh 超时（{} 秒）", DSH_TIMEOUT.as_secs())),
            ..Default::default()
        },
    };
    let ms = started.elapsed().as_millis() as u64;

    // TODO: usage.rs 提供 `record` 之后改成
    //   crate::usage::record(&app, &req.mode, Some(&session), out.in_tok, out.out_tok, ms)
    record_usage(&app, &req.mode, &session, out.in_tok, out.out_tok, ms);

    let event = match out.err.clone() {
        Some(message) => SseEvent::default()
            .event("error")
            .json_data(json!({ "message": message })),
        None => {
            let mut payload = json!({
                "session": session,
                "usage": { "in": out.in_tok, "out": out.out_tok },
            });
            // 要落 PDF 的这几种模式（设备给了 ai_doc）：编译 + 存记忆。
            // 失败只带 `pdf_error`，**不影响**已经流式给出的正文。
            if !req.ai_doc.trim().is_empty() && !out.text.trim().is_empty() {
                match crate::ai::store(&app, &req, &out.text, &memory).await {
                    Ok(st) => {
                        payload["ai"] = json!(req.ai_doc);
                        payload["pdf"] = json!(st.sha);
                        eprintln!(
                            "[ai] {} → PDF {} 字（{}）",
                            req.ai_doc,
                            out.text.chars().count(),
                            st.pdf.display()
                        );
                    }
                    Err(e) => {
                        eprintln!("[ai] PDF 失败：{e}");
                        payload["pdf_error"] = json!(e);
                    }
                }
            }
            SseEvent::default().event("done").json_data(payload)
        }
    };
    if let Ok(event) = event {
        let _ = tx.send(Ok(event)).await;
    }
}

/// 跑 dsh；`derived` 的 session 不被认识时去掉 `--session-id` 再试一次。
async fn run(app: Arc<App>, prompt: String, session: String, derived: bool, tx: Tx) -> RunOutcome {
    let mut out = run_once(&app, &prompt, Some(&session), &tx).await;
    if derived && !out.emitted && out.should_retry() {
        out = run_once(&app, &prompt, None, &tx).await;
    }
    out
}

/// 起一次 `dsh --profile headless --json [--session-id X] <prompt>`，边读边发。
async fn run_once(app: &Arc<App>, prompt: &str, session: Option<&str>, tx: &Tx) -> RunOutcome {
    let mut out = RunOutcome::default();
    let mut cmd = Command::new(&app.cfg.dsh);
    cmd.arg("--profile").arg("headless").arg("--json");
    if let Some(session) = session {
        cmd.arg("--session-id").arg(session);
    }
    cmd.arg(prompt);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    apply_env(&mut cmd, &app.cfg);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            out.err = Some(format!("无法启动 dsh（{}）：{e}", app.cfg.dsh));
            return out;
        }
    };
    out.spawned = true;

    let mut stderr = child.stderr.take().expect("stderr 已 piped");
    let stderr_task = tokio::spawn(async move {
        // 必须一直读到 EOF：读满上限就不读了的话，dsh 会因为 stderr 管道满而卡住。
        // 这里只留前 STDERR_LIMIT 字节做诊断。
        let mut kept = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match stderr.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if kept.len() < STDERR_LIMIT as usize {
                        kept.extend_from_slice(&chunk[..n]);
                    }
                }
            }
        }
        String::from_utf8_lossy(&kept).into_owned()
    });

    let stdout = child.stdout.take().expect("stdout 已 piped");
    let mut lines = BufReader::new(stdout).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => match parse_event(&line) {
                Parsed::Text(text) => {
                    if text.is_empty() {
                        continue;
                    }
                    out.text.push_str(&text);
                    out.emitted = true;
                    if tx
                        .send(Ok(SseEvent::default()
                            .event("token")
                            .json_data(json!({ "t": text }))
                            .expect("字符串一定可序列化")))
                        .await
                        .is_err()
                    {
                        // 设备断了，别再烧 token。
                        let _ = child.start_kill();
                        break;
                    }
                }
                Parsed::Usage { in_tok, out_tok } => {
                    out.in_tok += in_tok;
                    out.out_tok += out_tok;
                }
                Parsed::Error(message) => {
                    out.err = Some(message);
                    let _ = child.start_kill();
                    break;
                }
                Parsed::Ignore => {}
            },
            Ok(None) => break,
            Err(e) => {
                out.err = Some(format!("读取 dsh stdout 失败：{e}"));
                let _ = child.start_kill();
                break;
            }
        }
    }

    let status = child.wait().await;
    out.exit_ok = status.as_ref().map(|s| s.success()).unwrap_or(false);
    if out.err.is_none() && !out.exit_ok {
        let code = status
            .ok()
            .and_then(|s| s.code())
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        out.err = Some(format!("dsh 退出码 {code}"));
    }
    out.stderr = match tokio::time::timeout(Duration::from_secs(2), stderr_task).await {
        Ok(Ok(text)) => text,
        _ => String::new(),
    };
    out
}

/// 一次性问答（不流式）：给收信模块的 `!` 指令用，返回完整回答的纯文本。
///
/// 和 `/api/v1/agent/ask` 共用同一套 dsh 调用（`apply_env` / `parse_event`）与
/// `App::dsh_lock`，所以不会和设备的问答撞车。失败返回中文错误串，调用方原样回信。
pub async fn ask_once(app: &App, text: &str) -> Result<String, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("指令是空的".to_string());
    }
    let prompt = format!(
        "你在 rM2 笔记云的邮件指令入口。用户从手机发了主题以 `!` 开头的邮件，\
         下面是主题里的指令，请照做，然后用简体中文纯文本回答（不要 markdown 代码围栏、不要 HTML）：\n\n{text}"
    );

    // 不带 `--session-id`：邮件指令彼此无关，每次新建会话最省心。
    collect_once(app, &prompt, None, "mail").await
}

/// 一次性跑 dsh 并把 NDJSON 收成完整文本（不流式）。
///
/// 收信模块（[`ask_once`]）和错题档案（`profile::scan`）都走这里，只此一份 spawn：
/// 同一把 `App::dsh_lock`（和设备的问答串行，不撞车）、同一个 `apply_env` /
/// `parse_event`、同一套成本记账。`session` 为 `None` 就不带 `--session-id`；
/// `mode` 原样落 `usage` 表。失败返回中文错误串，调用方自己翻成 HTTP 状态码。
pub async fn collect_once(
    app: &App,
    prompt: &str,
    session: Option<&str>,
    mode: &str,
) -> Result<String, String> {
    // **派生 session 不被认识时要重试一次**（和 `ask` 的 `run()` 同一套逻辑）。
    // 真机踩过：`profile/scan` 拿派生 session 去跑，dsh 回
    // `session "session-rm-xxx" does not exist; omit --session-id`，
    // 整个 scan 直接 502。
    let first = collect_inner(app, prompt, session, mode).await;
    match first {
        Ok(v) => Ok(v),
        Err(e)
            if session.is_some() && looks_like_unknown_session(&e) =>
        {
            eprintln!("[agent] {e} → 去掉 --session-id 重试一次");
            collect_inner(app, prompt, None, mode).await
        }
        Err(e) => Err(e),
    }
}

/// dsh 拒绝我们拼出来的 session 的两种说法。
fn looks_like_unknown_session(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("does not exist")
        || e.contains("unknown session")
        || e.contains("no such session")
}

async fn collect_inner(
    app: &App,
    prompt: &str,
    session: Option<&str>,
    mode: &str,
) -> Result<String, String> {
    let _guard = app.dsh_lock.lock().await;
    let started = Instant::now();
    let mut cmd = Command::new(&app.cfg.dsh);
    cmd.arg("--profile").arg("headless").arg("--json");
    if let Some(session) = session.map(str::trim).filter(|s| !s.is_empty()) {
        cmd.arg("--session-id").arg(session);
    }
    cmd.arg(prompt)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    apply_env(&mut cmd, &app.cfg);

    let out = match tokio::time::timeout(DSH_TIMEOUT, cmd.output()).await {
        Err(_) => return Err(format!("dsh 超时（{} 秒）", DSH_TIMEOUT.as_secs())),
        Ok(Err(e)) => return Err(format!("无法启动 dsh（{}）：{e}", app.cfg.dsh)),
        Ok(Ok(out)) => out,
    };
    let ms = started.elapsed().as_millis() as u64;

    let mut answer = String::new();
    let mut in_tok = 0u64;
    let mut out_tok = 0u64;
    let mut failure = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        match parse_event(line) {
            Parsed::Text(t) => answer.push_str(&t),
            Parsed::Usage {
                in_tok: i,
                out_tok: o,
            } => {
                in_tok += i;
                out_tok += o;
            }
            Parsed::Error(message) => {
                failure = Some(message);
                break;
            }
            Parsed::Ignore => {}
        }
    }
    record_usage(app, mode, session.unwrap_or(""), in_tok, out_tok, ms);

    if let Some(message) = failure {
        return Err(message);
    }
    if !out.status.success() {
        let code = out
            .status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        let stderr: String = String::from_utf8_lossy(&out.stderr)
            .trim()
            .chars()
            .take(500)
            .collect();
        return Err(if stderr.is_empty() {
            format!("dsh 退出码 {code}")
        } else {
            format!("dsh 退出码 {code}：{stderr}")
        });
    }
    if answer.trim().is_empty() {
        return Err("dsh 没有返回任何内容".to_string());
    }
    Ok(answer.trim().to_string())
}

/// 注入 `cfg.dsh_env`（每行 `KEY=VALUE`）+ 从 `cfg.dsh` 父目录推导的 node 目录。
fn apply_env(cmd: &mut Command, cfg: &Config) {
    for (k, v) in env_pairs(&cfg.dsh_env) {
        cmd.env(k, v);
    }
    let Some(bin_dir) = Path::new(&cfg.dsh).parent() else {
        return;
    };
    let mut path = std::env::var("PATH").unwrap_or_default();
    let already = path.split(':').any(|p| Path::new(p) == bin_dir);
    if !already {
        path = format!("{}:{path}", bin_dir.display());
    }
    cmd.env("PATH", path);
}

/// 读 `KEY=VALUE` 格式的 env 文件；`#` 注释、空行、`export ` 前缀都容忍。
fn env_pairs(path: &str) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            if key.is_empty() {
                return None;
            }
            Some((
                key.to_string(),
                value
                    .trim()
                    .trim_matches('"')
                    .trim_matches('\'')
                    .to_string(),
            ))
        })
        .collect()
}

/// 成本记账（cC 的 `usage::record` 已落地）。
fn record_usage(app: &App, mode: &str, session: &str, in_tok: u64, out_tok: u64, ms: u64) {
    crate::usage::record(app, mode, Some(session), in_tok, out_tok, ms);
}

/// 设备工具代理：`POST /api/v1/tool/{op}`，内部 `App::call_device`。
///
/// `render` 的设备回包是 `{"png":"<blob sha>"}`，这里直接把 blob 字节吐成 `image/png`，
/// 少一次往返；`strokes` / `search` 原样透传 JSON。设备通道没实现时 `call_device` 会
/// `bail!`，这里翻成 503 + 原始错误串。
async fn tool(
    State(app): State<Arc<App>>,
    UrlPath(op): UrlPath<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !authorized(&app, &headers) {
        return json_error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    if !matches!(op.as_str(), "render" | "strokes" | "search") {
        return json_error(
            StatusCode::NOT_FOUND,
            format!("unknown tool op: {op}"),
        );
    }
    let args = if body.is_empty() {
        json!({})
    } else {
        serde_json::from_slice::<Value>(&body).unwrap_or_else(|_| json!({}))
    };
    match app.call_device(&op, args).await {
        Ok(value) => {
            if op == "render" {
                if let Some(sha) = value.get("png").and_then(Value::as_str) {
                    let path = app.blobs().join(sha);
                    return match tokio::fs::read(&path).await {
                        Ok(bytes) => {
                            ([(header::CONTENT_TYPE, "image/png")], bytes).into_response()
                        }
                        Err(e) => json_error(
                            StatusCode::SERVICE_UNAVAILABLE,
                            format!("render: 设备回了 png={sha}，但 blob 读不到：{e}"),
                        ),
                    };
                }
            }
            Json(value).into_response()
        }
        Err(e) => json_error(StatusCode::SERVICE_UNAVAILABLE, format!("{e:#}")),
    }
}

fn authorized(app: &App, headers: &HeaderMap) -> bool {
    app.authorized(
        headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok()),
    )
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    (status, Json(json!({ "error": message.into() }))).into_response()
}

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/api/v1/agent/ask", post(ask))
        .route("/api/v1/tool/{op}", post(tool))
}

#[cfg(test)]
mod session_retry_tests {
    use super::looks_like_unknown_session;

    /// dsh 拒绝派生 session 的说法（真机原话见 `profile/scan` 那次 502）。
    #[test]
    fn unknown_session_errors_are_recognised() {
        assert!(looks_like_unknown_session(
            "session \"session-rm-grade000\" does not exist; omit --session-id to start a new Session"
        ));
        assert!(looks_like_unknown_session("unknown session: abc"));
        assert!(looks_like_unknown_session("no such session: x"));
        // 别的错误不许当成"session 不认识"去重试（否则会白跑一次 dsh）
        assert!(!looks_like_unknown_session("dsh 超时（300 秒）"));
        assert!(!looks_like_unknown_session("dsh 退出码 1：ENOENT"));
        assert!(!looks_like_unknown_session(""));
    }
}
