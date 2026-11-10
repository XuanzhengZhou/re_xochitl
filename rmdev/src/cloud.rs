//! 云端事件通道（SSE）+ 设备命令执行。
//!
//! 服务器（`rmcloud`）在有人提交新版本或 agent 想用设备时，往
//! `GET /api/v1/events` 这条长连接里推两种事件（格式见 `docs/CLOUD.md`）：
//!
//! ```text
//! event: rev    data: {"rev":42}                     ← 库变了，去 sync_once
//! event: cmd    data: {"id":"c7","op":"render",...}  ← 跑一条只读命令，结果回传
//! : keepalive                                        ← 20 s 一条注释行
//! ```
//!
//! 这个模块负责：
//!
//! * 一条后台线程守住事件流，断线按 1→2→5→10→30 s 退避重连；
//! * 把事件攒进静态队列，主循环用 [`poll`] 无阻塞取走 —— UI 线程**永远**不做网络 IO；
//! * 命令实现（[`run_cmd`]）：`render` / `strokes` / `search`，只读，不写库；
//! * 结果回传（[`reply`] / [`reply_err`]）：`POST /api/v1/device/result`。
//!
//! 整页渲染要帧缓冲和 UI，所以不在库里实现：由二进制启动时用
//! [`set_renderer`] 把 `main.rs::render_screen` 注进来（无头 `Fb::open_mem()`），
//! 命令线程里调它不会碰到真实面板。

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::doc;
use crate::net;
use crate::pen::Color;

// ---------------------------------------------------------------- 类型

/// 服务器推下来的一条设备命令。
#[derive(Clone, Debug, PartialEq)]
pub struct Cmd {
    pub id: String,
    pub op: String,
    /// `data` 原始对象（`uuid` / `page` / `q` 都在里面），原样交给 `run_cmd`
    pub args: Value,
}

/// SSE 解析状态。一条事件可能分两行（`event:` 然后 `data:`）到。
#[derive(Debug, Default)]
pub struct SseState {
    /// 最近一条 `event:` 的类型
    event: Option<String>,
}

/// 凑齐的一条事件。
#[derive(Clone, Debug, PartialEq)]
pub enum Evt {
    Rev(u64),
    Cmd(Cmd),
    /// 半截 JSON / 缺字段 / 没见过的类型：丢弃并记日志，**不 panic**
    Bad(String),
}

// ---------------------------------------------------------------- 状态

/// 后台线程只起一次（`App::new` 在 `--render` 里也会跑，防重复）。
static STARTED: AtomicBool = AtomicBool::new(false);

/// 事件通道现在是连着的吗。
///
/// 状态栏那颗"云"图标就是靠它判断服务器可达 —— **不再发 HTTP 探活**。
/// 以前每 20 s 一个 `GET /api/v1/health`（一小时 180 个连接），全部走
/// tailscale 用户态协议栈：每个连接都要握手、加密、把无线从省电态叫醒。
/// 长连接本身就证明"服务器可达"，探活是纯白耗电。
pub static EVENTS_UP: AtomicBool = AtomicBool::new(false);

#[inline]
pub fn events_up() -> bool {
    EVENTS_UP.load(Ordering::Relaxed)
}

/// 最新 rev（服务器推的，等着主循环取走）。
static REV: AtomicU64 = AtomicU64::new(0);

/// 攒下的命令。主循环每帧 `take` 一次。
static QUEUE: Mutex<Vec<Cmd>> = Mutex::new(Vec::new());

/// 整页渲染回调：`(spec, out_pgm)`，由 `main.rs` 注册。
type Renderer = fn(&str, &str);
static RENDERER: Mutex<Option<Renderer>> = Mutex::new(None);

/// 读超时。服务器 20 s 一条 keepalive，60 s 还没动静就是真断了。
const READ_TIMEOUT: Duration = Duration::from_secs(60);

/// 断线退避：1s → 2s → 5s → 10s → 30s 封顶。
const BACKOFF: [u64; 5] = [1, 2, 5, 10, 30];

// ---------------------------------------------------------------- 启动 / 取事件

/// 注册整页渲染回调（必须在收到 `render` 命令之前调用；重复调用以后者为准）。
pub fn set_renderer(f: Renderer) {
    *RENDERER.lock().unwrap_or_else(|e| e.into_inner()) = Some(f);
}

/// 启动事件线程。幂等：第二次调用直接返回。
pub fn start() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    if let Err(e) = std::thread::Builder::new()
        .name("cloud-events".into())
        .spawn(events_loop)
    {
        STARTED.store(false, Ordering::SeqCst);
        eprintln!("[cloud] 起不了事件线程：{e}");
    }
}

/// 主循环每帧调：取走攒下的 `(最新 rev, 命令列表)`，不阻塞。
pub fn poll() -> (Option<u64>, Vec<Cmd>) {
    let rev = match REV.swap(0, Ordering::SeqCst) {
        0 => None,
        r => Some(r),
    };
    let cmds = std::mem::take(&mut *QUEUE.lock().unwrap_or_else(|e| e.into_inner()));
    (rev, cmds)
}

fn handle(evt: Evt) {
    match evt {
        Evt::Rev(rev) => {
            eprintln!("[cloud] rev={rev}");
            REV.fetch_max(rev, Ordering::SeqCst);
        }
        Evt::Cmd(cmd) => {
            eprintln!("[cloud] 命令 {} op={}", cmd.id, cmd.op);
            QUEUE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(cmd);
        }
        Evt::Bad(why) => eprintln!("[cloud] 丢弃事件：{why}"),
    }
}

/// 后台线程主体：连上 → 按行读 → 断了退避重连。没配对就每 5 s 看一眼配置。
fn events_loop() {
    let mut step = 0usize;
    loop {
        let cfg = net::Config::load();
        if !cfg.configured() {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }
        match net::open_stream("GET", "/api/v1/events", b"", READ_TIMEOUT) {
            Ok(reader) => {
                eprintln!("[cloud] 事件通道已连上 {}", cfg.base());
                EVENTS_UP.store(true, Ordering::Relaxed);
                step = 0;
                let mut st = SseState::default();
                for line in reader.lines() {
                    // 读超时 / 连接被关：都当断线，出去重连（半截行直接丢）
                    let Ok(line) = line else { break };
                    if let Some(evt) = parse_sse_line(&mut st, &line) {
                        handle(evt);
                    }
                }
                EVENTS_UP.store(false, Ordering::Relaxed);
                eprintln!("[cloud] 事件通道断开，准备重连");
            }
            Err(e) => {
                EVENTS_UP.store(false, Ordering::Relaxed);
                if e == "not paired" {
                    // 配对页刚写完 server.json：5 s 后再读一次配置
                    std::thread::sleep(Duration::from_secs(5));
                    continue;
                }
                eprintln!("[cloud] 连不上事件通道：{e}");
            }
        }
        let secs = BACKOFF[step];
        step = (step + 1).min(BACKOFF.len() - 1);
        eprintln!("[cloud] {secs} s 后重连");
        std::thread::sleep(Duration::from_secs(secs));
    }
}

// ---------------------------------------------------------------- SSE 解析

/// 喂一行 SSE。凑齐一条完整事件就返回它，其余情况返回 `None`。
///
/// 规则（`text/event-stream` 的子集）：空行结束一条事件；`:` 开头是注释
/// （服务器的 keepalive 就是 `: keepalive`）；`event:` 记类型；`data:` 到齐就
/// 立刻分发 —— 服务器每条事件只发一行 data。
pub fn parse_sse_line(state: &mut SseState, line: &str) -> Option<Evt> {
    if line.is_empty() {
        state.event = None;
        return None;
    }
    if line.starts_with(':') {
        return None;
    }
    let (field, value) = match line.split_once(':') {
        Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
        None => (line, ""),
    };
    match field {
        "event" => {
            state.event = Some(value.trim().to_string());
            None
        }
        "data" => {
            let event = state.event.take().unwrap_or_default();
            Some(parse_data(&event, value))
        }
        // id: / retry: 用不上
        _ => None,
    }
}

fn parse_data(event: &str, data: &str) -> Evt {
    let data = data.trim();
    if data.is_empty() {
        return Evt::Bad(format!("{event}: data 是空的"));
    }
    let v: Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => return Evt::Bad(format!("{event}: JSON 不完整（{e}）")),
    };
    match event {
        "rev" => match v.get("rev").and_then(Value::as_u64) {
            Some(rev) => Evt::Rev(rev),
            None => Evt::Bad("rev 事件缺 rev 字段".into()),
        },
        "cmd" => {
            let id = v.get("id").and_then(Value::as_str).unwrap_or_default();
            let op = v.get("op").and_then(Value::as_str).unwrap_or_default();
            if id.is_empty() || op.is_empty() {
                return Evt::Bad("cmd 事件缺 id/op".into());
            }
            Evt::Cmd(Cmd {
                id: id.to_string(),
                op: op.to_string(),
                args: v,
            })
        }
        other => Evt::Bad(format!("未知事件类型 {other:?}")),
    }
}

// ---------------------------------------------------------------- 回结果

/// 回一条成功结果（阻塞几十毫秒，只许在后台线程里调）。
pub fn reply(id: &str, result: Value) {
    post_result(&json!({"id": id, "ok": true, "result": result}));
}

/// 回一条失败结果。
pub fn reply_err(id: &str, err: &str) {
    post_result(&json!({"id": id, "ok": false, "error": err}));
}

fn post_result(body: &Value) {
    let cfg = net::Config::load();
    if !cfg.configured() {
        eprintln!("[cloud] 没配对，命令结果丢弃：{body}");
        return;
    }
    let url = format!("{}/api/v1/device/result", cfg.base());
    let r = net::request(
        "POST",
        &url,
        &cfg.token,
        Some("application/json"),
        body.to_string().as_bytes(),
        Duration::from_secs(10),
    );
    match r {
        Ok(r) if r.status == 200 => {}
        Ok(r) => eprintln!("[cloud] 回结果失败：{}", r.error()),
        Err(e) => eprintln!("[cloud] 回结果失败：{e}"),
    }
}

// ---------------------------------------------------------------- 命令

/// 执行一条设备命令。只读：不碰写入口，最多渲染到 `/tmp`。
pub fn run_cmd(cmd: &Cmd) -> Result<Value, String> {
    match cmd.op.as_str() {
        "render" => cmd_render(&cmd.args),
        "strokes" => cmd_strokes(&cmd.args, &lib_root()),
        "search" => cmd_search(&cmd.args, &lib_root()),
        _ => Err("unknown op".into()),
    }
}

/// 库根目录：默认看 [`doc::library::DEFAULT_ROOT`]，本机测试用 `RMDEV_LIB_ROOT` 覆盖。
fn lib_root() -> PathBuf {
    match std::env::var("RMDEV_LIB_ROOT") {
        Ok(p) if !p.is_empty() => PathBuf::from(p),
        _ => PathBuf::from(doc::library::DEFAULT_ROOT),
    }
}

/// `render`：整页灰度 → 2× 降采样的预览 PNG 传 blob，回 `{"png":"<sha256>"}`。
///
/// 屏幕共享只看预览，不需要原分辨率：1404×1872 的整页在设备上要 deflate 2.6 MB，
/// 是这条链路最贵的一段（Mac 上密排书页 78 ms，rM2 上按 8~15× 折算是 0.6~1.2 s）。
/// 降到 702×936 再按 level=4 编，编码快一个数量级、字节少 2~4 倍，
/// 而 32 px 手写字在浏览器里仍然清楚（依据见 [`crate::share::PREVIEW_DIV`]）。
///
/// 内容没变就直接回上次的 sha：同一页连点两次不重新编码、不重新上传。
fn cmd_render(args: &Value) -> Result<Value, String> {
    let uuid = arg_doc(args)?;
    // 渲染走 `render_screen`，那里只认页序号（`doc:<uuid>:<page>`）
    let page = arg_num(args, "page")?;
    let f = *RENDERER.lock().unwrap_or_else(|e| e.into_inner());
    let f = f.ok_or_else(|| "no renderer（render 只能在 rmdev 主程序里跑）".to_string())?;
    static SEQ: AtomicU64 = AtomicU64::new(1);
    let pgm = format!(
        "/tmp/rmdev-cloud-{}-{}.pgm",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    );
    f(&format!("doc:{uuid}:{page}"), &pgm);
    let bytes = std::fs::read(&pgm).map_err(|e| format!("读 {pgm} 失败：{e}"))?;
    let _ = std::fs::remove_file(&pgm);
    let (w, h, gray) = read_pgm(&bytes)?;
    let (small, dw, dh) = crate::share::downscale(&gray, w, h, crate::share::PREVIEW_DIV);
    if small.is_empty() {
        return Err(format!("降采样失败（{w}x{h}）"));
    }
    // 和上次发出去的那张逐字节比：一样就不用再编码/上传（哈希只用在这条路径上，
    // 直接比 656 KB 比算 sha256 便宜一个数量级，而且没有碰撞风险）。
    if let Some(sha) = preview_hit(&uuid, page, &small) {
        eprintln!("[cloud] render {uuid}/{page}：内容没变，复用 {}", short_sha(&sha));
        return Ok(json!({"png": sha}));
    }
    let png = crate::share::png_preview(&small, dw, dh);
    if png.is_empty() {
        return Err(format!("PNG 编码失败（{dw}x{dh}）"));
    }
    let cfg = net::Config::load();
    if !cfg.configured() {
        return Err("not paired".into());
    }
    let sha = net::put_blob(&cfg, &png)?;
    preview_keep(uuid.clone(), page, small, &sha);
    Ok(json!({"png": sha}))
}

/// 一条屏幕共享预览缓存：哪份文档哪一页、降采样后的灰度、服务器上的 blob sha。
struct Preview {
    uuid: String,
    page: u64,
    small: Vec<u8>,
    sha: String,
    at: Instant,
}

/// 预览缓存。只留最近 [`PREVIEW_KEEP`] 页 —— 屏幕共享基本就在一两页之间来回，
/// 每页 656 KB（702×936），不能无限攒。
static PREVIEWS: Mutex<Vec<Preview>> = Mutex::new(Vec::new());

/// 预览缓存留几页。
const PREVIEW_KEEP: usize = 2;

/// 预览缓存的保鲜期。缓存键比的是像素，本来不会错；这道时限只防一种情况：
/// 服务器状态目录被清过/换了一台服务器，设备还记着 sha，那边已经没有这个 blob 了。
/// 过期就老老实实重编重传（10 KB 的量级），不会把某一页永久卡在 503。
const PREVIEW_TTL: Duration = Duration::from_secs(600);

/// 查预览缓存：同一页、降采样后逐字节相同 → 回上次的 sha。
fn preview_hit(uuid: &str, page: u64, small: &[u8]) -> Option<String> {
    let cache = PREVIEWS.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .iter()
        .find(|p| p.uuid == uuid && p.page == page && p.small == small && p.at.elapsed() < PREVIEW_TTL)
        .map(|p| p.sha.clone())
}

/// 记一条预览缓存（同一页只留最新的一条，超出的从最旧的开始丢）。
fn preview_keep(uuid: String, page: u64, small: Vec<u8>, sha: &str) {
    let mut cache = PREVIEWS.lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|p| !(p.uuid == uuid && p.page == page));
    cache.insert(
        0,
        Preview {
            uuid,
            page,
            small,
            sha: sha.to_string(),
            at: Instant::now(),
        },
    );
    cache.truncate(PREVIEW_KEEP);
}

/// 日志里只打 sha 前 12 位。
fn short_sha(sha: &str) -> &str {
    &sha[..sha.len().min(12)]
}

/// `strokes`：每笔的 bbox/点数/颜色 + 文字层。
fn cmd_strokes(args: &Value, root: &Path) -> Result<Value, String> {
    let uuid = arg_doc(args)?;
    let page = arg_page_str(args)?;
    let path = match resolve_page(root, &uuid, &page) {
        Ok(p) => p,
        Err(e) => {
            // **还没写过字的 PDF/电子书页没有 `.rm` 文件**（`.content` 的
            // `cPages.pages` 是空的，第一笔落下去才生成页 id）。这时候
            // "这一页什么都没有" 才是正确答案，不能报错——AI 问"这页有什么"
            // 得到 error 会以为通道坏了。
            // 只在这一份文档**一个 `.rm` 都没有**时放行（PDF/电子书页被写
            // 第一笔之前就是这种状态）；有别的页写了字说明页号是真错了，
            // 该报错就报错。
            let dir = root.join(&uuid);
            let any_rm = std::fs::read_dir(&dir).map(|rd| {
                rd.flatten()
                    .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("rm"))
            });
            if root.join(format!("{uuid}.metadata")).is_file()
                && matches!(any_rm, Ok(false))
            {
                return Ok(json!({"strokes": [], "text": [], "stored": false}));
            }
            return Err(e);
        }
    };
    let bytes = std::fs::read(&path).map_err(|e| format!("读 {} 失败：{e}", path.display()))?;
    let rm = doc::rm::RmPage::parse(&bytes)
        .map_err(|e| format!("解析 {} 失败：{e}", path.display()))?;
    let mut strokes = Vec::new();
    for line in rm.visible_lines() {
        let Some(bb) = bbox_of(line) else { continue };
        strokes.push(json!({
            "bbox": [bb[0], bb[1], bb[2], bb[3]],
            "n": line.points.len(),
            "color": color_name(line.color),
        }));
    }
    // **坐标系**：官方 `.rm` 里墨迹的 x 以页面**中心**为原点（实测真机笔记本页
    // bbox x 从 -309 到 +302），y 从页顶往下。`renders` 出来的整页图是左上原点，
    // 所以 AI 要把 bbox 换算成图像像素得 `img_x = x + w/2`、`img_y = y`。
    // 直接把 frame 一起回去，省得它猜。
    let (w, h) = page_frame(root, &uuid);
    Ok(json!({
        "strokes": strokes,
        "text": page_texts(&bytes),
        "frame": {"w": w, "h": h, "cx": w / 2, "cy": h / 2,
                  "note": "x 以页面中心为原点（img_x = x + cx），y 从页顶往下"},
    }))
}

/// 库里搜标题 + 文字层（大小写不敏感子串），最多 [`MAX_HITS`] 条。
fn cmd_search(args: &Value, root: &Path) -> Result<Value, String> {
    let q = arg_str(args, "q")?;
    let needle = q.to_lowercase();
    let mut hits: Vec<Value> = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        // 库还没生成：空结果比报错友好（agent 那边只关心 hits）
        return Ok(json!({"hits": []}));
    };
    let mut metas: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("metadata"))
        .collect();
    metas.sort();
    let lib = doc::library::Library::with_root(root);
    for meta in metas {
        if hits.len() >= MAX_HITS {
            break;
        }
        let Some(uuid) = meta
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
        else {
            continue;
        };
        let Ok(raw) = std::fs::read_to_string(&meta) else { continue };
        let Ok(m) = serde_json::from_str::<Value>(&raw) else { continue };
        if m.get("deleted").and_then(Value::as_bool).unwrap_or(false) {
            continue;
        }
        if m.get("parent").and_then(Value::as_str) == Some("trash") {
            continue;
        }
        if m.get("type").and_then(Value::as_str) != Some("DocumentType") {
            continue;
        }
        let title = m
            .get("visibleName")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // 文字层：按 `.content` 里的页顺序扫，命中就带页号和上下文
        let mut found: Option<(usize, String)> = None;
        for (i, pid) in lib.page_ids(&uuid).into_iter().enumerate() {
            let Ok(bytes) = std::fs::read(root.join(&uuid).join(format!("{pid}.rm"))) else {
                continue;
            };
            let text = page_text(&bytes);
            if let Some(at) = ci_find(&text, &needle) {
                found = Some((i, snippet(&text, at, needle.len())));
                break;
            }
        }
        if let Some((page, snip)) = found {
            hits.push(json!({"uuid": uuid, "title": title, "page": page, "snippet": snip}));
        } else if let Some(at) = ci_find(&title, &needle) {
            hits.push(json!({
                "uuid": uuid, "title": title, "page": 0,
                "snippet": snippet(&title, at, needle.len()),
            }));
        }
    }
    Ok(json!({"hits": hits}))
}

const MAX_HITS: usize = 20;

/// `page` 既可以是页 id（`<pageid>.rm` 的文件名），也可以是页序号。
fn resolve_page(root: &Path, uuid: &str, page: &str) -> Result<PathBuf, String> {
    let direct = root.join(uuid).join(format!("{page}.rm"));
    if direct.is_file() {
        return Ok(direct);
    }
    let idx: usize = page
        .parse()
        .map_err(|_| format!("{uuid}: 没有页 {page}"))?;
    let pid = doc::library::Library::with_root(root)
        .page_id_at(uuid, idx)
        .ok_or_else(|| format!("{uuid}: 没有第 {idx} 页"))?;
    let p = root.join(uuid).join(format!("{pid}.rm"));
    if !p.is_file() {
        return Err(format!("{uuid}: 第 {idx} 页的 .rm 不存在"));
    }
    Ok(p)
}

/// 页面画布尺寸：官方 `.content` 的 `customZoomPageWidth/Height`，缺省 1404×1872。
fn page_frame(root: &Path, uuid: &str) -> (i64, i64) {
    let c: Value = std::fs::read_to_string(root.join(format!("{uuid}.content")))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(Value::Null);
    let pick = |k: &str, d: i64| c.get(k).and_then(Value::as_i64).filter(|v| *v > 0).unwrap_or(d);
    (
        pick("customZoomPageWidth", 1404),
        pick("customZoomPageHeight", 1872),
    )
}

fn arg_str(v: &Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| format!("missing {key}"))
}

/// 文档 uuid：服务器侧（`agent_api` 的 prompt、`rmcloud-tool`）叫 `doc`，
/// 早期设备只认 `uuid`。两个都收，省得调用方记两套名字。
fn arg_doc(v: &Value) -> Result<String, String> {
    arg_str(v, "doc").or_else(|_| arg_str(v, "uuid"))
}

fn arg_num(v: &Value, key: &str) -> Result<u64, String> {
    match v.get(key) {
        Some(Value::Number(n)) => n
            .as_u64()
            .ok_or_else(|| format!("{key} 必须是非负整数")),
        Some(Value::String(s)) => s.parse().map_err(|_| format!("{key} 必须是数字：{s}")),
        _ => Err(format!("missing {key}")),
    }
}

fn arg_page_str(v: &Value) -> Result<String, String> {
    match v.get("page") {
        Some(Value::Number(n)) => n
            .as_u64()
            .map(|n| n.to_string())
            .ok_or_else(|| "page 必须是非负整数".to_string()),
        Some(Value::String(s)) if !s.is_empty() => Ok(s.clone()),
        _ => Err("missing page".into()),
    }
}

/// 一笔的 bbox `[x0,y0,x1,y1]`（场景坐标，取点集的包围盒；空笔迹返回 `None`）。
fn bbox_of(line: &crate::pen::Line) -> Option<[f32; 4]> {
    let first = line.points.first()?;
    let (mut x0, mut y0, mut x1, mut y1) = (first.x, first.y, first.x, first.y);
    for p in &line.points {
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    Some([x0, y0, x1, y1])
}

fn color_name(c: Color) -> &'static str {
    match c {
        Color::Gray | Color::HighlighterGray => "gray",
        _ => "black",
    }
}

/// 一页 `.rm` 的文字层（没有文字块就是空串）。
fn page_text(bytes: &[u8]) -> String {
    match doc::rmtext::parse_text(bytes) {
        Ok(Some(t)) => match t.paragraphs() {
            Ok(ps) => ps
                .into_iter()
                .map(|p| p.text)
                .collect::<Vec<_>>()
                .join("\n"),
            Err(_) => String::new(),
        },
        _ => String::new(),
    }
}

fn page_texts(bytes: &[u8]) -> Vec<Value> {
    page_text(bytes)
        .split('\n')
        .filter(|s| !s.trim().is_empty())
        .map(|s| Value::String(s.to_string()))
        .collect()
}

/// 大小写不敏感查找，返回**原文**里的字节下标（`to_lowercase` 可能改变字节长度）。
fn ci_find(hay: &str, needle_lower: &str) -> Option<usize> {
    if needle_lower.is_empty() {
        return None;
    }
    let mut low = String::with_capacity(hay.len());
    // 小写串里每个字节对应的原串字节偏移
    let mut map: Vec<usize> = Vec::with_capacity(hay.len());
    for (i, ch) in hay.char_indices() {
        for lc in ch.to_lowercase() {
            low.push(lc);
            for _ in 0..lc.len_utf8() {
                map.push(i);
            }
        }
    }
    let at = low.find(needle_lower)?;
    map.get(at).copied()
}

/// 命中点周围一小段（按字符边界取，不会切坏 UTF-8）。
fn snippet(text: &str, at: usize, len: usize) -> String {
    const BEFORE: usize = 24;
    const AFTER: usize = 48;
    let mut start = at;
    for _ in 0..BEFORE {
        match text[..start].chars().next_back() {
            Some(c) => start -= c.len_utf8(),
            None => break,
        }
    }
    let mut end = (at + len).min(text.len());
    for _ in 0..AFTER {
        match text[end..].chars().next() {
            Some(c) => end += c.len_utf8(),
            None => break,
        }
    }
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.push_str(text[start..end].trim());
    if end < text.len() {
        out.push('…');
    }
    out
}

// ---------------------------------------------------------------- PGM

/// 解析二进制灰度 PGM（P5）：`P5\n<w> <h>\n<maxval>\n<pixels>`。
///
/// 头里的 `#` 注释行跳过；只支持 8 位（`maxval=255`）。宽高不对 / 魔数不对 / 数据
/// 截断一律 `Err` —— 渲染回调写坏的文件要能看出来，不能画出一张乱码。
pub fn read_pgm(bytes: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    if bytes.len() < 2 || &bytes[..2] != b"P5" {
        return Err("不是 P5 PGM（魔数不对）".into());
    }
    let mut i = 2usize;
    let mut nums = [0usize; 3]; // w h maxval
    for slot in nums.iter_mut() {
        // 跳过空白和注释行
        loop {
            while i < bytes.len() && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            if i < bytes.len() && bytes[i] == b'#' {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            } else {
                break;
            }
        }
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if start == i {
            return Err("PGM 头里缺数字".into());
        }
        let s = std::str::from_utf8(&bytes[start..i]).map_err(|_| "PGM 头不是 ASCII")?;
        *slot = s.parse().map_err(|_| format!("PGM 头数字非法：{s}"))?;
    }
    // 头结束后必须有一个空白字节才进像素
    if i >= bytes.len() || !bytes[i].is_ascii_whitespace() {
        return Err("PGM 头后面没有分隔字节".into());
    }
    i += 1;
    let (w, h, max) = (nums[0], nums[1], nums[2]);
    if w == 0 || h == 0 {
        return Err(format!("PGM 尺寸非法：{w}x{h}"));
    }
    if max != 255 {
        return Err(format!("只支持 8 位 PGM（maxval=255），读到 {max}"));
    }
    let want = w.checked_mul(h).ok_or("PGM 尺寸溢出")?;
    let px = bytes.get(i..).unwrap_or(&[]);
    if px.len() < want {
        return Err(format!("PGM 数据不够：要 {want} 字节，只有 {}", px.len()));
    }
    Ok((w, h, px[..want].to_vec()))
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pen::{Line, Point, Tool};

    fn tmp_lib(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("rmdev-cloud-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// 造一个文档的四件套里够用的两件（`.metadata` + `.content`）。
    fn write_doc(root: &Path, uuid: &str, title: &str, pages: &[&str]) {
        std::fs::create_dir_all(root.join(uuid)).unwrap();
        std::fs::write(
            root.join(format!("{uuid}.metadata")),
            json!({
                "visibleName": title,
                "type": "DocumentType",
                "deleted": false,
                "parent": "",
            })
            .to_string(),
        )
        .unwrap();
        let pages: Vec<Value> = pages.iter().map(|id| json!({"id": id})).collect();
        std::fs::write(
            root.join(format!("{uuid}.content")),
            json!({"cPages": {"pages": pages}}).to_string(),
        )
        .unwrap();
    }

    fn point(x: f32, y: f32) -> Point {
        Point {
            x,
            y,
            speed: 0.0,
            dir: 0.0,
            width: 1.0,
            pressure: 0.5,
        }
    }

    // ------------------------------------------------------------ PGM

    #[test]
    fn pgm_parses_standard_p5() {
        let mut b = b"P5\n2 2\n255\n".to_vec();
        b.extend_from_slice(&[0, 1, 2, 3]);
        let (w, h, px) = read_pgm(&b).unwrap();
        assert_eq!((w, h), (2, 2));
        assert_eq!(px, vec![0, 1, 2, 3]);
    }

    #[test]
    fn pgm_skips_comment_lines() {
        let mut b = "P5\n# 设备帧缓冲\n3 1\n# maxval\n255\n".as_bytes().to_vec();
        b.extend_from_slice(&[9, 8, 7]);
        let (w, h, px) = read_pgm(&b).unwrap();
        assert_eq!((w, h), (3, 1));
        assert_eq!(px, vec![9, 8, 7]);
    }

    #[test]
    fn pgm_rejects_bad_magic_and_bad_size() {
        // 魔数错
        assert!(read_pgm(b"P2\n1 1\n255\n\x00").is_err());
        // 宽高对不上：声称 2x2 只有 3 字节
        assert!(read_pgm(b"P5\n2 2\n255\nabc").is_err());
        // 0 宽
        assert!(read_pgm(b"P5\n0 2\n255\n").is_err());
        // 不是 8 位
        assert!(read_pgm(b"P5\n1 1\n65535\n\x00\x00").is_err());
        // 头就断了
        assert!(read_pgm(b"P5\n2").is_err());
    }

    // ------------------------------------------------------------ SSE

    #[test]
    fn sse_rev_ignores_keepalive_and_blank() {
        let mut st = SseState::default();
        assert_eq!(parse_sse_line(&mut st, ": keepalive"), None);
        assert_eq!(parse_sse_line(&mut st, ""), None);
        assert_eq!(parse_sse_line(&mut st, "event: rev"), None);
        assert_eq!(
            parse_sse_line(&mut st, "data: {\"rev\":42}"),
            Some(Evt::Rev(42))
        );
        // 空行清状态，再来 data 就是「没有 event 类型」
        assert_eq!(parse_sse_line(&mut st, ""), None);
        assert!(matches!(
            parse_sse_line(&mut st, "data: {\"rev\":1}"),
            Some(Evt::Bad(_))
        ));
    }

    #[test]
    fn sse_cmd_carries_id_op_and_args() {
        let mut st = SseState::default();
        assert_eq!(parse_sse_line(&mut st, "event: cmd"), None);
        let evt = parse_sse_line(
            &mut st,
            "data: {\"id\":\"c7\",\"op\":\"render\",\"uuid\":\"u1\",\"page\":3}",
        )
        .unwrap();
        match evt {
            Evt::Cmd(cmd) => {
                assert_eq!(cmd.id, "c7");
                assert_eq!(cmd.op, "render");
                assert_eq!(cmd.args["uuid"], "u1");
                assert_eq!(cmd.args["page"], 3);
            }
            other => panic!("应为 Cmd，得到 {other:?}"),
        }
        // 事件类型被取走了：紧接着的半截 JSON 不能 panic
        assert!(matches!(
            parse_sse_line(&mut st, "data: {\"id\":\"c8\",\"op\":\"rend"),
            Some(Evt::Bad(_))
        ));
        // 缺字段也不 panic
        assert_eq!(parse_sse_line(&mut st, "event: cmd"), None);
        assert!(matches!(
            parse_sse_line(&mut st, "data: {\"id\":\"c9\"}"),
            Some(Evt::Bad(_))
        ));
    }

    #[test]
    fn sse_handles_crlf_and_unknown_events() {
        // `lines()` 已经去掉 \r，但手工构造的输入也不该被 \r 骗
        let mut st = SseState::default();
        assert_eq!(parse_sse_line(&mut st, "event: rev\r"), None);
        assert_eq!(
            parse_sse_line(&mut st, "data: {\"rev\":7}\r"),
            Some(Evt::Rev(7))
        );
        assert_eq!(parse_sse_line(&mut st, "event: nope"), None);
        assert!(matches!(
            parse_sse_line(&mut st, "data: {}"),
            Some(Evt::Bad(_))
        ));
    }

    // ------------------------------------------------------------ 命令

    #[test]
    fn unknown_op_is_rejected() {
        let cmd = Cmd {
            id: "c1".into(),
            op: "write".into(),
            args: json!({}),
        };
        assert_eq!(run_cmd(&cmd), Err("unknown op".to_string()));
    }

    #[test]
    fn bbox_covers_all_points() {
        let mut line = Line::new(Tool::Fineliner, Color::Black, 2.0);
        line.push(point(10.0, 20.0));
        line.push(point(-5.0, 40.0));
        line.push(point(30.0, 5.0));
        assert_eq!(bbox_of(&line), Some([-5.0, 5.0, 30.0, 40.0]));
        // 空笔迹没有 bbox
        assert_eq!(bbox_of(&Line::new(Tool::Fineliner, Color::Black, 2.0)), None);
    }

    /// 还没写过字的 PDF/电子书页：`.content` 的 `cPages.pages` 是空的，
    /// 一个 `.rm` 都没有。`strokes` 必须回"空"而不是报错，否则 AI 问
    /// "这页有什么"会以为通道坏了（真机踩过：`没有第 0 页`）。
    #[test]
    fn strokes_on_page_without_ink_is_empty_not_an_error() {
        let root = tmp_lib("strokes-empty");
        let uuid = "u-empty";
        write_doc(&root, uuid, "没批注过的 PDF", &[]);
        // 真机上这种文档伴生一个 .pdf
        std::fs::write(root.join(format!("{uuid}.pdf")), b"%PDF-1.4").unwrap();

        let v = cmd_strokes(&json!({"doc": uuid, "page": 0}), &root).unwrap();
        assert_eq!(v["strokes"].as_array().unwrap().len(), 0, "{v}");
        assert_eq!(v["stored"], false, "{v}");

        // 文档根本不存在就不该被静默吞掉
        let e = cmd_strokes(&json!({"doc": "nope", "page": 0}), &root).unwrap_err();
        assert!(e.contains("nope"), "{e}");
    }

    /// 坐标是「x 以页面中心为原点」（官方约定，真机笔记本页实测 -309..+302）。
    /// 必须把 frame 一起回去，否则 AI 把 bbox 当左上原点会整体偏 702 px。
    #[test]
    fn strokes_reports_page_frame_for_coordinate_mapping() {
        let root = tmp_lib("strokes-frame");
        let uuid = "u-frame";
        write_doc(&root, uuid, "帧", &["p1"]);
        let mut page = doc::rm::RmPage::default();
        let mut l = Line::new(Tool::Fineliner, Color::Black, 2.0);
        l.push(point(-309.0, 243.0));
        l.push(point(302.0, 244.0));
        page.lines_mut().push(l);
        std::fs::write(root.join(uuid).join("p1.rm"), page.serialize()).unwrap();

        let v = cmd_strokes(&json!({"doc": uuid, "page": 0}), &root).unwrap();
        assert_eq!(v["frame"]["w"], 1404, "{v}");
        assert_eq!(v["frame"]["h"], 1872, "{v}");
        assert_eq!(v["frame"]["cx"], 702, "{v}");
        assert!(v["frame"]["note"].as_str().unwrap().contains("中心"), "{v}");

        // `.content` 里写了别的画布尺寸就照它回
        std::fs::write(
            root.join(format!("{uuid}.content")),
            json!({"cPages": {"pages": [{"id": "p1"}]},
                   "customZoomPageWidth": 1000, "customZoomPageHeight": 1500})
            .to_string(),
        )
        .unwrap();
        let v = cmd_strokes(&json!({"doc": uuid, "page": 0}), &root).unwrap();
        assert_eq!(v["frame"]["w"], 1000, "{v}");
        assert_eq!(v["frame"]["cx"], 500, "{v}");
    }

    #[test]
    fn strokes_reads_bboxes_colors_and_text() {
        let root = tmp_lib("strokes");
        let uuid = "u-strokes";
        write_doc(&root, uuid, "笔迹样本", &["p1"]);

        let mut page = doc::rm::RmPage::default();
        let mut black = Line::new(Tool::Fineliner, Color::Black, 2.0);
        black.push(point(10.0, 20.0));
        black.push(point(40.0, 60.0));
        page.lines_mut().push(black);
        let mut gray = Line::new(Tool::Pencil, Color::Gray, 2.0);
        gray.push(point(-3.0, 7.0));
        page.lines_mut().push(gray);
        let strokes_bytes = page.serialize();
        // 再补一个文字层：`strokes` 的 `text` 要从同一个文件读出来
        let with_text =
            doc::rmtext::serialize_text(&strokes_bytes, &doc::rmtext::RmText::plain("hello"))
                .unwrap();
        std::fs::write(root.join(uuid).join("p1.rm"), with_text).unwrap();

        // 页 id 直接给
        let by_id = cmd_strokes(&json!({"uuid": uuid, "page": "p1"}), &root).unwrap();
        let strokes = by_id["strokes"].as_array().unwrap();
        assert_eq!(strokes.len(), 2, "{by_id}");
        assert_eq!(strokes[0]["bbox"], json!([10.0, 20.0, 40.0, 60.0]));
        assert_eq!(strokes[0]["n"], 2);
        assert_eq!(strokes[0]["color"], "black");
        assert_eq!(strokes[1]["bbox"], json!([-3.0, 7.0, -3.0, 7.0]));
        assert_eq!(strokes[1]["color"], "gray");
        assert_eq!(by_id["text"], json!(["hello"]));

        // 页序号也行（走 `.content` 里的页 id）
        let by_idx = cmd_strokes(&json!({"uuid": uuid, "page": 0}), &root).unwrap();
        assert_eq!(by_idx["strokes"].as_array().unwrap().len(), 2);

        // 没有那页 → Err，不是 panic
        assert!(cmd_strokes(&json!({"uuid": uuid, "page": 9}), &root).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn search_hits_title_and_text_layer_case_insensitively() {
        let root = tmp_lib("search");
        // 标题命中
        write_doc(&root, "u-title", "Quantum Notes", &["p1"]);
        // 文字层命中
        write_doc(&root, "u-text", "Other Book", &["p1"]);
        let base = doc::rm::RmPage::default().serialize();
        let with_text =
            doc::rmtext::serialize_text(&base, &doc::rmtext::RmText::plain("The Quadratic formula"))
                .unwrap();
        std::fs::write(root.join("u-text").join("p1.rm"), with_text).unwrap();
        // 干扰项：已删除的文档不该被搜到
        write_doc(&root, "u-gone", "Quantum Deleted", &[]);
        let mut meta: Value =
            serde_json::from_str(&std::fs::read_to_string(root.join("u-gone.metadata")).unwrap())
                .unwrap();
        meta["deleted"] = json!(true);
        std::fs::write(root.join("u-gone.metadata"), meta.to_string()).unwrap();

        let hits = cmd_search(&json!({"q": "QUANTUM"}), &root).unwrap();
        let hits = hits["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "只该命中标题（小写查询、大写标题）");
        assert_eq!(hits[0]["uuid"], "u-title");
        assert_eq!(hits[0]["title"], "Quantum Notes");
        assert_eq!(hits[0]["page"], 0);

        let hits = cmd_search(&json!({"q": "quadratic"}), &root).unwrap();
        let hits = hits["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["uuid"], "u-text");
        assert_eq!(hits[0]["page"], 0);
        assert!(
            hits[0]["snippet"].as_str().unwrap().contains("Quadratic"),
            "{}",
            hits[0]["snippet"]
        );

        // 不命中
        let none = cmd_search(&json!({"q": "不存在的词"}), &root).unwrap();
        assert!(none["hits"].as_array().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn search_on_missing_library_is_empty_not_error() {
        let root = std::env::temp_dir().join(format!("rmdev-cloud-nolib-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let hits = cmd_search(&json!({"q": "x"}), &root).unwrap();
        assert!(hits["hits"].as_array().unwrap().is_empty());
    }

    #[test]
    fn page_arg_accepts_number_and_string() {
        assert_eq!(arg_page_str(&json!({"page": 3})).unwrap(), "3");
        assert_eq!(arg_page_str(&json!({"page": "p1"})).unwrap(), "p1");
        assert!(arg_page_str(&json!({})).is_err());
        assert_eq!(arg_num(&json!({"page": "4"}), "page").unwrap(), 4);
        assert!(arg_num(&json!({"page": -1}), "page").is_err());
    }

    /// 服务器侧到处写 `doc`，早期设备只认 `uuid`。两个都得收，否则
    /// `POST /api/v1/tool/render {"doc":...}` 会回 `missing uuid`（实测踩过）。
    #[test]
    fn doc_arg_accepts_both_doc_and_uuid() {
        assert_eq!(arg_doc(&json!({"doc": "d1"})).unwrap(), "d1");
        assert_eq!(arg_doc(&json!({"uuid": "u1"})).unwrap(), "u1");
        // 两个都给时 `doc` 优先（服务器口径）
        assert_eq!(arg_doc(&json!({"doc": "d1", "uuid": "u1"})).unwrap(), "d1");
        // 空串当没给
        assert_eq!(arg_doc(&json!({"doc": "", "uuid": "u1"})).unwrap(), "u1");
        assert!(arg_doc(&json!({})).is_err());
        assert!(arg_doc(&json!({"doc": 1})).is_err());
    }

    #[test]
    fn render_without_renderer_is_an_error_not_a_panic() {
        let err = cmd_render(&json!({"uuid": "u", "page": 0})).unwrap_err();
        assert!(err.contains("renderer"), "{err}");
    }

    /// 预览缓存：只有"同一页 + 降采样后逐字节相同"才命中；只留最近两页。
    /// 两条断言放一个测试里（缓存是全局的，拆开并行跑会互相淘汰）。
    #[test]
    fn preview_cache_hits_only_on_same_pixels() {
        let uuid = format!("t-preview-{}", std::process::id());
        let other = format!("t-preview-x-{}", std::process::id());
        let a = vec![9u8; 16];
        let b = vec![8u8; 16];
        // 没记过 → 不命中
        assert!(preview_hit(&uuid, 0, &a).is_none());
        preview_keep(uuid.clone(), 0, a.clone(), "sha-a");
        assert_eq!(preview_hit(&uuid, 0, &a).as_deref(), Some("sha-a"));
        // 同一页但内容变了 / 换页 / 换文档 → 都不命中
        assert!(preview_hit(&uuid, 0, &b).is_none());
        assert!(preview_hit(&uuid, 1, &a).is_none());
        assert!(preview_hit(&other, 0, &a).is_none());

        // 只留最近两页：0..4 依次进来，最后只剩 3 和 2
        for page in 0..4 {
            preview_keep(uuid.clone(), page, vec![page as u8; 4], &format!("sha-{page}"));
        }
        assert_eq!(preview_hit(&uuid, 3, &[3; 4]).as_deref(), Some("sha-3"));
        assert_eq!(preview_hit(&uuid, 2, &[2; 4]).as_deref(), Some("sha-2"));
        assert!(preview_hit(&uuid, 1, &[1; 4]).is_none(), "第三页该被淘汰");
        assert!(preview_hit(&uuid, 0, &[0; 4]).is_none());
    }
}
