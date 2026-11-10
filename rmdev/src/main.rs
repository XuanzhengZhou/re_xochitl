//! rmdev —— 设备上的入口。
//!
//! 两个界面：**库**（列笔记，能新建/打开）→ **笔记本**（写字、工具条可一键收起）。
//! 退出：笔记本里左下角连点 3 下。
//!
//! 运行：`systemctl stop remux && /opt/bin/rm2fb-client /home/root/rmdev`

use std::time::Duration;

use rmdev::canvas::Canvas;
use rmdev::chrome::{Chrome, Menu, Msg, Op, Sec, TOOLS, W as CHROME_W};
use rmdev::doc::library::{Book, TextItem};
use rmdev::doc::pdf::Pdf;
use rmdev::doc::{Library, RmPage};
use rmdev::fb::{
    gray565, Fb, UPDATE_MODE_FULL, WAVEFORM_MODE_A2, WAVEFORM_MODE_DU, WAVEFORM_MODE_GC16,
    WAVEFORM_MODE_GL16,
};
use rmdev::input::power::PowerEvent;
use rmdev::input::{touch::TouchEvent, InputEvent, InputThread, PenEvent};
use rmdev::input::touch::{TwoFinger, TwoFingerAct};
use rmdev::panels::docsearch::{self, DocSearch};
use rmdev::panels::docsettings::{self, DocSettings};
// === P12 begin ===
use rmdev::panels::drawer::{self, Drawer};
// === P12 end ===
// === E1 begin ===
// 错题档案（C13 设备端）：纯逻辑/绘制在 src/panels/mistakes.rs，全部带单测
use rmdev::panels::mistakes::{self, MistakesPanel};
// === E1 end ===
use rmdev::panels::nbsettings::{self, NbSettings};
use rmdev::panels::textcfg::{self, TextCfg};
use rmdev::panels::toc::{self, TocPanel};
use rmdev::panels::pagesel::{self, PageSel};
// === C8 begin ===
// AI 流式回答面板（纯逻辑/绘制在 src/panels/ai.rs，全部带单测）
use rmdev::panels::ai;
// === C8 end ===
// === P8 begin ===
use rmdev::panels::libbar;
// === P8 end ===
use rmdev::panels::poweroff;
use rmdev::panels::sleepscreen;
// === P7 begin ===
// 文档页滑块（纯布局/绘制在 src/panels/slider.rs，全部带单测）
use rmdev::panels::slider;
// === P7 end ===
use rmdev::pen::width::{self, Ctx};
use rmdev::pen::{Line, RectF, Tool};
use rmdev::pen::StrokeBuilder;
use rmdev::select::{self, SelAct};
use rmdev::settings::{battery, disk, help, language, prefs, security, wifi};
// === P5 begin: 设置页新增的三个数据模块 ===
use rmdev::settings::{developer, experimental, reset};
// === P5 end ===
use rmdev::ui::text::Text;
use rmdev::ui::keyboard::{Key, Keyboard, Layer};
use rmdev::ui::widgets::{self, Hits};
use rmdev::viewport::{Gesture, View as PageView};

// === P3 begin ===
// 状态栏通知队列（纯逻辑在 src/notify.rs，带单测）
use rmdev::notify::{Kind as NoticeKind, Queue as NoticeQueue};
// === P3 end ===

// Tags 视图（独立文件；这里只有调用，具体布局在 tagsview.rs）
mod tagsview;

/// 顶部四个工具格（笔/粗细/橡皮/选区）的顶边 = 圆环那格下面。
const TOP_CELLS_Y: i32 = 120;

const PEN_DEV: &str = "/dev/input/event1";
const TOUCH_DEV: &str = "/dev/input/event2";

/// 长按多久算长按。
///
/// 官方用的是 Qt 的 `onPressAndHold`（库卡片、列表行、页总览、图层名、标签
/// 都是它），时长来自 `QStyleHints::mousePressAndHoldInterval()` —— Qt 默认
/// 800 ms，xochitl 里确实调过 `setMousePressAndHoldInterval`，但那个立即数
/// 在去掉符号的调用点里读不出来，所以先用 Qt 默认值。
///
/// 以前是 550 ms：手指刚停下来就弹菜单，滚列表时一动就误触。
const LONG_PRESS_MS: u64 = 800;

/// 按下后位移超过这么多像素，这次长按就作废（**之后也不会再触发**）。
///
/// 官方 `res_0649a07.qml`（scrollOverlay）：
/// ```js
/// var scrollLimit = 15;
/// scrollOverlay.isBeingScrolled = yDelta > scrollLimit || xDelta > scrollLimit;
/// ```
/// 而卡片/列表行的 `onPressAndHold` 第一句就是
/// `if (scrollOverlay.isBeingScrolled) { return; }`，
/// 注释原文：*"if we are dragging, prevent turning a slow drag to a long press"*。
/// 这就是"慢慢滚列表却被当成按住"的解药。
const SCROLL_LIMIT: f32 = 15.0;

/// 页面场景坐标原点：**x 居中于 0，y 自上而下**。
/// 依据：官方 `.content` 里 `customZoomCenterX=0 / customZoomCenterY=936`，
/// 且实测笔迹 x∈[-517,694]、y∈[119,1497]。
/// 视图坐标 = 场景坐标 + (PAGE_DX, PAGE_DY)。
const PAGE_DX: f32 = 702.0;
const PAGE_DY: f32 = 0.0;

/// 抓屏请求（`kill -USR1 <pid>` 置位，主循环空闲时落盘）。
///
/// 面板的 `/tmp/rm2epd.state` 不是"屏上现在的样子"：流式路径里 `prv` 存的
/// 是**上一帧的目标**（编码要用它当起点），只有在两段式/静置时才等于屏上
/// 状态。要截图就抓 UI 的 `plane`——那才是我们想让它显示的东西。
static SHOT_REQ: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_sigusr1(_sig: libc::c_int) {
    SHOT_REQ.store(true, std::sync::atomic::Ordering::Relaxed);
}

// === P6 begin ===
// ---------------------------------------------------------------- 退出信号
/// SIGTERM/SIGINT 的"请退出"请求。
///
/// 信号处理函数里只做 async-signal-safe 的事：置一个原子量。真正的清屏、
/// `wait_idle` 和退出放在主循环里做 —— 面板线程还在跑，在 handler 里碰
/// 帧缓冲会把正在驱动的波形打断（屏幕停在半灰）。
static P6_QUIT_SIG: std::sync::atomic::AtomicI32 =
    std::sync::atomic::AtomicI32::new(0);

/// 这两个信号都按"清屏后退出"处理。
fn p6_is_quit_signal(sig: libc::c_int) -> bool {
    sig == libc::SIGTERM || sig == libc::SIGINT
}

extern "C" fn p6_on_quit_signal(sig: libc::c_int) {
    if p6_is_quit_signal(sig) {
        P6_QUIT_SIG.store(sig, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 主循环每圈取一次；返回收到的信号，取走后清零。
fn p6_take_quit() -> Option<libc::c_int> {
    match P6_QUIT_SIG.swap(0, std::sync::atomic::Ordering::Relaxed) {
        0 => None,
        sig => Some(sig),
    }
}

// ---------------------------------------------------------------- 电子书封面
/// 电子书封面缩略图的尺寸，和 `thumb()` 的整页缩略图一致。
const P6_COVER_W: usize = 426;
const P6_COVER_H: usize = 568;

type P6Cover = std::rc::Rc<Vec<u8>>;

/// 封面缓存 + 后台渲染通道（每个线程一份，主循环只在主线程里跑）。
struct P6CoverState {
    /// uuid → 426×568 灰度封面
    covers: std::collections::HashMap<String, P6Cover>,
    /// 电子书真实页数（副标题 `N pages` 要用，替代原来同步打开补的那份）
    pages: std::collections::HashMap<String, usize>,
    /// 渲染失败过的：不要每帧重试，直接画官方空态
    failed: std::collections::HashSet<String>,
    /// 已排进后台队列/正在渲的
    pending: std::collections::HashSet<String>,
    /// 官方空态（整张白页）的共享缓冲：没渲好时每帧都返回它，不重复分配
    empty: Option<P6Cover>,
    rx: Option<std::sync::mpsc::Receiver<(String, Option<(Vec<u8>, usize)>)>>,
    tx: Option<std::sync::mpsc::Sender<(String, String)>>,
}

std::thread_local! {
    static P6_COVERS: std::cell::RefCell<P6CoverState> =
        std::cell::RefCell::new(P6CoverState {
            covers: std::collections::HashMap::new(),
            pages: std::collections::HashMap::new(),
            failed: std::collections::HashSet::new(),
            pending: std::collections::HashSet::new(),
            empty: None,
            rx: None,
            tx: None,
        });
}

/// 按 `render_doc_bg` 的做法渲首页，再 contain 缩进 426×568。
///
/// 返回 `(灰度缓冲, 总页数)`；打不开 / 没有页 / 渲染失败返回 `None`
/// （调用方画官方空态，不能 panic）。
fn p6_render_cover(path: &str) -> Option<(Vec<u8>, usize)> {
    let book = rmdev::doc::open(path).ok()?;
    let pages = book.pages();
    if pages == 0 {
        return None;
    }
    let asp = book.aspect(0).clamp(0.05, 20.0);
    let mut tw = (P6_COVER_H as f32 * asp).round() as usize;
    let mut th = P6_COVER_H;
    if tw > P6_COVER_W {
        tw = P6_COVER_W;
        th = ((P6_COVER_W as f32) / asp).round() as usize;
    }
    let tw = tw.clamp(1, P6_COVER_W);
    let th = th.max(1);
    let g = book.render(0, tw, th).ok()?;
    if g.len() < tw * th {
        return None;
    }
    let mut out = vec![255u8; P6_COVER_W * P6_COVER_H];
    let ox = (P6_COVER_W - tw) / 2;
    let oy = (P6_COVER_H - th.min(P6_COVER_H)) / 2;
    for y in 0..th.min(P6_COVER_H) {
        let src = y * tw;
        let dst = (oy + y) * P6_COVER_W + ox;
        out[dst..dst + tw].copy_from_slice(&g[src..src + tw]);
    }
    Some((out, pages))
}

/// 收后台渲染结果。非阻塞：`draw_library` 每帧都能顺手收一次。
fn p6_poll_covers() -> bool {
    P6_COVERS.with(|st| {
        let mut st = st.borrow_mut();
        let Some(rx) = st.rx.take() else {
            return false;
        };
        let mut got = Vec::new();
        while let Ok(m) = rx.try_recv() {
            got.push(m);
        }
        st.rx = Some(rx);
        let mut changed = false;
        for (uuid, out) in got {
            changed = true;
            st.pending.remove(&uuid);
            match out {
                Some((buf, pages)) if buf.len() == P6_COVER_W * P6_COVER_H => {
                    st.pages.insert(uuid.clone(), pages);
                    st.covers.insert(uuid, std::rc::Rc::new(buf));
                }
                _ => {
                    st.failed.insert(uuid);
                }
            }
        }
        changed
    })
}

/// 第一次请求时起一个渲染线程；之后所有电子书共用它（字体/排版缓存也能复用）。
fn p6_cover_worker(
    st: &mut P6CoverState,
) -> std::sync::mpsc::Sender<(String, String)> {
    if let Some(tx) = st.tx.as_ref() {
        return tx.clone();
    }
    let (job_tx, job_rx) = std::sync::mpsc::channel::<(String, String)>();
    let (out_tx, out_rx) = std::sync::mpsc::channel::<(String, Option<(Vec<u8>, usize)>)>();
    std::thread::spawn(move || {
        while let Ok((uuid, path)) = job_rx.recv() {
            // 一本坏书不能在 worker 里 panic：线程一死，后面所有封面都不渲了。
            // 失败就回 None，调用方画官方空态。
            let cover = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                p6_render_cover(&path)
            }))
            .unwrap_or(None);
            if out_tx.send((uuid, cover)).is_err() {
                break;
            }
        }
    });
    st.rx = Some(out_rx);
    st.tx = Some(job_tx.clone());
    job_tx
}

/// 排一次后台渲染。已经有缓存/失败过/正在渲就什么都不做。
fn p6_request_cover(uuid: &str, path: String) {
    p6_poll_covers();
    P6_COVERS.with(|st| {
        let mut st = st.borrow_mut();
        if st.covers.contains_key(uuid) || st.failed.contains(uuid) || st.pending.contains(uuid) {
            return;
        }
        // 快速滚过一长串书时别把渲染线程排爆：同时只排 4 本，
        // 剩下的等这一批回来、下一帧再排。
        if st.pending.len() >= 4 {
            return;
        }
        let tx = p6_cover_worker(&mut st);
        st.pending.insert(uuid.to_string());
        let _ = tx.send((uuid.to_string(), path));
    });
}

/// 官方空态：整张白页。共享同一个 `Rc`，滚动时不会每帧分配新缓冲。
fn p6_empty_cover() -> P6Cover {
    P6_COVERS.with(|st| {
        let mut st = st.borrow_mut();
        st.empty
            .get_or_insert_with(|| std::rc::Rc::new(vec![255u8; P6_COVER_W * P6_COVER_H]))
            .clone()
    })
}

/// 还有封面在后台排队/渲染吗（无头出图时用来决定要不要等）。
fn p6_has_pending_covers() -> bool {
    P6_COVERS.with(|st| !st.borrow().pending.is_empty())
}

/// 缓存里已经有封面了吗。
fn p6_has_covers() -> bool {
    P6_COVERS.with(|st| !st.borrow().covers.is_empty())
}

impl App {
    /// 搜索条打开时正文整体下移 `HDR_H`。
    ///
    /// 只是这一帧的显示变换：`nb.view` 不动，所以落笔坐标、缩放、
    /// 关掉搜索后的恢复都不受影响。纯函数在 `docsearch::body_offset`。
    fn p6_display_view(&self, v: PageView) -> PageView {
        match docsearch::body_offset(self.search.is_open()) {
            0 => v,
            off => PageView {
                ty: v.ty + off as f32,
                ..v
            },
        }
    }

    /// 电子书封面：有缓存就返回；没有就排后台渲染并返回 `None`。
    ///
    /// 先查缓存再决定要不要排渲染：热路径（滚动）不该每帧都扫 metadata。
    /// 顺手把后台算出来的真实页数塞进 `epub_pages`，副标题 `N pages`
    /// 才不会因为跳过了原来 `thumb()` 里的同步打开而消失。
    fn p6_ebook_cover(&mut self, b: &Book) -> Option<P6Cover> {
        let uuid = b.uuid.as_str();
        p6_poll_covers();
        let (cached, pages) = P6_COVERS.with(|st| {
            let st = st.borrow();
            (st.covers.get(uuid).cloned(), st.pages.get(uuid).copied())
        });
        if let Some(n) = pages {
            self.epub_pages.insert(b.uuid.clone(), n);
        }
        if cached.is_some() {
            return cached;
        }
        let path = self.document_path(b).display().to_string();
        p6_request_cover(uuid, path);
        None
    }
}
// === P6 end ===

/// 屏幕（视图）坐标 → 场景坐标。`select` 模块吃屏幕坐标，`Line.points` 是场景坐标。
fn to_scene(p: (f32, f32)) -> (f32, f32) {
    (p.0 - PAGE_DX, p.1 - PAGE_DY)
}

/// 矩形：场景 → 屏幕（宽高不变）。画选框/悬浮条和 `inside`/`bar_hit` 都走这个。
fn rect_to_view(r: RectF) -> RectF {
    RectF {
        x: r.x + PAGE_DX,
        y: r.y + PAGE_DY,
        ..r
    }
}

/// 一笔笔迹的包围盒（场景坐标）
fn line_bbox(l: &Line) -> Option<RectF> {
    if l.points.is_empty() {
        return None;
    }
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in &l.points {
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    Some(RectF { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
}

/// 一步撤销/重做会动到的屏幕矩形（已经加上页面原点、外扩了一点）。
/// 用它只重刷那一块，别整屏 GC16 —— 撤销是高频操作，整屏闪一下很难受。
fn op_screen_rect(op: &Op, layers: &[rmdev::doc::rm::Layer]) -> Option<(i32, i32, i32, i32)> {
    let mut acc: Option<(f32, f32, f32, f32)> = None;
    let mut add = |r: RectF| {
        let b = (r.x, r.y, r.x + r.w, r.y + r.h);
        acc = Some(match acc {
            Some(a) => (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)),
            None => b,
        });
    };
    match op {
        Op::Add { lines, .. } => {
            for l in lines {
                if let Some(r) = line_bbox(l) {
                    add(r);
                }
            }
        }
        Op::Remove { items, .. } => {
            for (_, l) in items {
                if let Some(r) = line_bbox(l) {
                    add(r);
                }
            }
        }
        Op::Move {
            layer,
            idx,
            dx,
            dy,
        } => {
            if let Some(layer) = layers.get(*layer) {
                for i in idx {
                    if let Some(l) = layer.lines.get(*i) {
                        if let Some(r) = line_bbox(l) {
                            add(r);
                            add(RectF {
                                x: r.x + dx,
                                y: r.y + dy,
                                ..r
                            });
                        }
                    }
                }
            }
        }
    }
    let (x0, y0, x1, y1) = acc?;
    let pad = 24.0;
    let sx0 = (x0 + PAGE_DX - pad).floor() as i32;
    let sy0 = (y0 + PAGE_DY - pad).floor() as i32;
    let sx1 = (x1 + PAGE_DX + pad).ceil() as i32;
    let sy1 = (y1 + PAGE_DY + pad).ceil() as i32;
    Some((sx0, sy0, sx1 - sx0, sy1 - sy0))
}

/// 笔色 → canvas 落墨浓度。官方三种：黑 / 灰 / 白（白笔是"提亮"，常压在荧光笔上）。
/// 笔工具自己的墨色：官方铅笔/自动铅笔固定灰、荧光笔走 Highlighter 色板
/// （灰度面板上就是浅灰），其余笔用工具栏选中的颜色。
///
/// 存进 `.rm` 的也是这个颜色，所以官方 App 打开同一页看到的浓淡是一致的。
fn tool_color(tool: rmdev::pen::Tool, chosen: rmdev::pen::line::Color) -> rmdev::pen::line::Color {
    use rmdev::pen::line::Color as C;
    use rmdev::pen::Tool as T;
    match tool {
        // 铅笔/自动铅笔官方就固定是灰，浓淡由硬度档位决定
        T::Pencil | T::PencilV2 | T::SharpPencil | T::SharpPencilV2 => C::Gray,
        // 荧光笔的四个颜色是它自己那套（见 chrome::HIGHLIGHTER_COLORS），不能覆盖
        _ => chosen,
    }
}

/// 两个屏幕矩形求并集（增量刷新的脏区合并用）
fn union_rect(a: (i32, i32, i32, i32), b: (i32, i32, i32, i32)) -> (i32, i32, i32, i32) {
    let x = a.0.min(b.0);
    let y = a.1.min(b.1);
    (
        x,
        y,
        (a.0 + a.2).max(b.0 + b.2) - x,
        (a.1 + a.3).max(b.1 + b.3) - y,
    )
}

/// 视图是不是"没动过"：不缩放、不转、不平移。
///
/// 这种状态下页面坐标和屏幕坐标逐像素对齐，增量刷新可以走原路径
/// （`canvas.flush` 内部 `fb.refresh` 的就是对的区域），不必再贴一次变换。
fn view_identity(v: &PageView) -> bool {
    v.scale == 1.0 && v.rot == 0 && v.tx == 0.0 && v.ty == 0.0
}

/// 能不能走"画布逐像素 1:1 贴到屏幕"那条快路：视图没动过**而且**画布和屏幕
/// 一样大。横屏时视口是 1872×1404、画布还是 1404×1872，光看视图会误判。
fn blit_identity(v: &PageView, canvas: (usize, usize), fb: (i32, i32)) -> bool {
    view_identity(v) && canvas.0 == fb.0 as usize && canvas.1 == fb.1 as usize
}

/// 一段线（套索）占的屏幕矩形，四周留 2px 给线宽和抗锯齿
fn seg_box(a: (f32, f32), b: (f32, f32)) -> (i32, i32, i32, i32) {
    let x0 = a.0.min(b.0) as i32 - 2;
    let y0 = a.1.min(b.1) as i32 - 2;
    let x1 = a.0.max(b.0) as i32 + 3;
    let y1 = a.1.max(b.1) as i32 + 3;
    (x0, y0, x1 - x0, y1 - y0)
}

// 下面这些数字全部是**从官方截图逐像素量出来的**（tools/probe.py）：
//   汉堡 33..72，Menu 91..180 基线 72
//   三个 ⊕ 圆圈 x=380/588/855 直径 40，标签跟在圈后 19px，组间空 46
//   放大镜 1324..1363，顶栏下沿 118（2px 黑线）
//   "My files" x=33 基线 192；卡片缩略图 426 宽，30 间距，首行顶 255
//   卡片标题基线 862，页信息基线 897；状态栏 1800..1872
const NAV_MARGIN: i32 = 33;
const GRID_MARGIN: i32 = 30;
const COL_W: i32 = 426;
const COLS: i32 = 3;
const TILE_W: i32 = COL_W;
const TILE_GAP: i32 = 30;
const TILE_H: i32 = 568;
const GRID_TOP: i32 = 255;
const SIDEBAR_W: i32 = 345;
const HEADER_H: i32 = 120;

// 列表模式（官方 `res_06a4413.qml` / `res_06a2825.qml` / `res_06a0e55.qml`）：
//   文档行 = navigatorContentListItemHeight(120) + 40 = 160
//   集合行 = navigatorContentListCollectionHeight = 75
//   文件夹块后面留 designGridHorizontalHeight + spacing = 45 + 15 = 60
const LIST_DOC_H: i32 = 160;
const LIST_COLL_H: i32 = 75;
const LIST_FOLDERS_GAP: i32 = 60;
/// 文档行缩略图：上下各 15，宽 = floor(高 / 1.33)（官方 PageThumbnail）
const LIST_PREVIEW_H: i32 = LIST_DOC_H - 30;
const LIST_PREVIEW_W: i32 = LIST_PREVIEW_H * 100 / 133;
/// 置顶星尺寸 = 标题字号（官方 FontIcon.size: entryName.font.pixelSize）。
/// 星的外径取 14 + 笔画 4 → 墨迹正好 32×32，和标题的 32 号一样大。
const STAR_PX: i32 = 32;
const STAR_RO: f32 = 14.0;

// 库界面的可点动作编号（配合 Hits 使用）
const A_MENU: u32 = 1;
const A_NEW_FOLDER: u32 = 2;
const A_NEW_NOTEBOOK: u32 = 3;
const A_QUICK: u32 = 4;
const A_SEARCH: u32 = 5;
const A_SORT: u32 = 6;
const A_SETTINGS: u32 = 7;
const A_BACK: u32 = 8;
const A_CLEAR: u32 = 9;
const A_CANCEL: u32 = 10;
const A_OK: u32 = 11;
const A_SORT0: u32 = 20;
const A_SORT1: u32 = 21;
/// 排序下拉底部的 View: Grid / List 两行
const A_VIEW0: u32 = 60;
const A_TILE0: u32 = 100;
const A_SIDE0: u32 = 400;
// 侧栏 Filters 展开后的三个子项
const A_FILTER0: u32 = 1300;
const A_EMPTY_TRASH: u32 = 1310;
const A_TR_RESTORE: u32 = 1312;
const A_TR_DELETE: u32 = 1313;
const A_TR_YES: u32 = 1314;
const A_TR_NO: u32 = 1315;
// === P8 begin ===
/// 库多选 → Move 的"选目标文件夹"顶栏：Move here / Cancel
const A_P8_MOVE_HERE: u32 = 9420;
const A_P8_MOVE_CANCEL: u32 = 9421;
// === P8 end ===
/// 页面总览里长按页面的菜单
/// 页总览多选 → Move 二级页：确认 / 取消
const A_MV_OK: u32 = 740;
const A_MV_CANCEL: u32 = 741;

/// 标签面板：宽 416，行高 120，底边贴"标签"按钮下沿（y=1512）
const TAG_PANEL_X: i32 = CHROME_W;
const TAG_PANEL_W: i32 = 416;
const TAG_ROW: i32 = 120;
const TAG_BTN_BOTTOM: i32 = 1512;
const A_TAG_TOGGLE0: u32 = 750;
const A_TAG_ADD: u32 = 790;

/// 分享面板的三行
const A_SHARE_SCREEN: u32 = 720;
const A_SHARE_MAIL: u32 = 721;
const A_SHARE_HWC: u32 = 722;

/// 分享面板几何：官方 `documentViewShareFoldoutWidth = (24+80)*4 = 416`，
/// 行高 = 工具按钮 120；面板底边贴"分享"按钮下沿（y=1752）
const SHARE_PANEL_X: i32 = CHROME_W;
const SHARE_PANEL_W: i32 = 416;
const SHARE_ROW: i32 = 120;
const SHARE_BTN_BOTTOM: i32 = 1752;

/// 面包屑：根 = 回到顶层；A_CRUMB0 + i = 回到 trail[0..=i]
// 新建 / 重命名对话框（1200 段，避开搜索页的 2100 段）
const A_DLG_CANCEL: u32 = 1200;
const A_DLG_CREATE: u32 = 1201;
const A_DLG_COVER0: u32 = 1202;
const A_DLG_TPL0: u32 = 1206;

const A_CRUMB_ROOT: u32 = 700;
const A_CRUMB0: u32 = 710;
const A_CTX_RENAME: u32 = 20;
const A_CTX_TRASH: u32 = 21;
const A_CTX_PIN: u32 = 22;
const A_CTX_CANCEL: u32 = 23;

// 搜索页几何（官方 `shots/official/40-search.png` 1404×1872 实测）：
// 黑条 0..117；Cancel 白字 34 粗、左边距 32（"C" 墨迹落在 x=35）/基线 72；
// 放大镜圈心 (262,55) 外径 15、柄朝左下；白输入框 296,37 269×46，占位字 37 号、基线 72；
// "Last updated" 36 粗、左边距 40、基线 221。
// 结果行：图标 40×40 在 x=40，文字 x=100；首行内容顶 300，行距 174（两行图标顶 300/474）。
// 官方标题基线 = 内容顶 +37（含中文那行）/ +29（纯拉丁行）：首行因为有中文回退字体，
// 整块文字低了 8px（Qt 的行高算法）。我们用同一种字体，没法同时对上两行，统一取 +30；
// 副文案基线同理取 +80（官方 +85 / +76）。
const SEARCH_BAR_H: i32 = 118;
const SEARCH_BOX: (i32, i32, i32, i32) = (296, 37, 269, 46);
/// 输入框里的字（官方占位 "Search My files" 墨宽 247，37 号正好）。
const SEARCH_FIELD_SIZE: f32 = 30.0;
const SEARCH_LIST_TOP: i32 = 300;
const SEARCH_ROW_H: i32 = 174;
const SEARCH_ICON_X: i32 = 40;
const SEARCH_TEXT_X: i32 = 100;
const SEARCH_TITLE_SIZE: f32 = 33.0;
const SEARCH_CAPTION_SIZE: f32 = 21.0;
const SEARCH_EMPTY_SIZE: f32 = 40.0;
/// 搜索结果行。700 段和面包屑 `A_CRUMB_ROOT` 撞过号，挪到 2100 段（最多 100 行）。
const A_RESULT0: u32 = 2100;
const A_PAGE0: u32 = 1000;
const A_ADDPAGE: u32 = 1100;
const A_OVERVIEW_BACK: u32 = 1101;
/// 页总览顶部的 `Table of contents`（打开目录面板）
const A_OVERVIEW_TOC: u32 = 1102;
const A_SETTINGS_BACK: u32 = 30;
const A_SET_TAB0: u32 = 40;
// 设置页（2000..2099 号段）
const A_S_ACCOUNT: u32 = 2000;
const A_S_SUBSCRIPTION: u32 = 2001;
const A_S_SOFTWARE: u32 = 2002;
const A_S_SUBBACK: u32 = 2003;
const A_S_RESTART: u32 = 2004;
const A_S_POWEROFF: u32 = 2005;
const A_S_PAIR: u32 = 2006;
/// 自建服务器：立即同步 / 解除配对（替代官方的"配对云端"）
// 2020/2021 会被 `A_S_TOGGLE0 + 10/11` 撞上（Battery 页 `Auto power off` 就是 2020，
// 点了会被当成"立即同步"→ 弹 `Pair a server first`）。挪到没人用的 2030 段。
const A_S_SYNC_NOW: u32 = 2030;
const A_S_UNPAIR: u32 = 2031;
const A_S_FACTORY: u32 = 2007;
const A_S_CHECK_SYNC: u32 = 2008;
const A_S_CONNECT: u32 = 2009;
const A_S_RELNOTES: u32 = 2080;
// === P5 begin: Experimental 页 7 个开关 + Factory reset 确认框 + 顶部完成提示 ===
/// Experimental 页的开关：`A_S_EXP0 + 行下标`（0..7，2090..2096）
const A_S_EXP0: u32 = 2090;
/// Factory reset 确认框的两个按钮
const A_S_FACTORY_YES: u32 = 2097;
const A_S_FACTORY_NO: u32 = 2098;
/// 设置页顶部的完成提示（点一下收掉）
const A_S_BANNER: u32 = 2099;
// === P5 end ===
/// 开关：+ (group * 4 + slot)
const A_S_TOGGLE0: u32 = 2010;
/// 单选：0=Left 1=Right 2=Normal 3=Large
const A_S_RADIO0: u32 = 2050;
/// 横屏模式那两行（设置页 Accessibility 底部）。**必须避开其它段**：
/// 2050 段只到 +14，2060 是 Help，2200 段是 PIN/语言/Help 的按钮 —— 第一次
/// 实现图省事用了 `A_S_RADIO0 + 7..10`，2060 正好撞上 `A_S_HELP0`，
/// "横屏方向 → Right" 永远打不开。
const A_S_LAND0: u32 = 2300;
/// Help 的三行
const A_S_HELP0: u32 = 2060;
/// Language 的两行
const A_S_LANG0: u32 = 2070;

// 密码 / 语言选择 / Help 子页 / 锁屏（2200 段，避开搜索结果的 2100..2200）
/// 设置页数字键盘：+ digit(0..9)
const A_S_PIN0: u32 = 2200;
const A_S_PIN_DEL: u32 = 2210;
const A_S_PIN_CANCEL: u32 = 2211;
/// 语言列表页：Back / 条目
const A_S_LANG_BACK: u32 = 2220;
const A_S_LANG_ITEM0: u32 = 2221;
/// Help 子页：Back / 教程按钮
const A_S_HELP_BACK: u32 = 2230;
const A_S_TUTORIAL: u32 = 2231;
/// Security 页的 Change passcode
const A_S_CHANGE_PASS: u32 = 2232;
/// 锁屏数字键盘：+ digit(0..9) / 删除
const A_LOCK0: u32 = 2260;
const A_LOCK_DEL: u32 = 2270;
/// 锁屏上 `Show` / `Hide` 那颗（显示数字而不是黑点）。
const A_LOCK_SHOW: u32 = 2271;
/// Wi-Fi 列表行 / Join other network
const A_S_NET0: u32 = 2280;
const A_S_JOIN: u32 = 2299;
// === C0W begin: Wi-Fi 连接面板 / 忘记网络确认框（2300 段，避开 P3 的 2400）===
/// 连接面板的两个按钮：`Cancel` / `Connect`（Join 第一步是 `Next`）
const A_C0W_CANCEL: u32 = 2300;
const A_C0W_CONNECT: u32 = 2301;
/// 忘记网络确认框：`Forget` / `Cancel`
const A_C0W_FORGET: u32 = 2302;
const A_C0W_FORGET_CANCEL: u32 = 2303;
// === C0W end ===

// === P3 begin ===
// 状态栏三指示器 + 通知条（官方 `res_06b6382.qml`）。2400 段，避开上面所有动作号。
const A_P3_BATTERY: u32 = 2400;
const A_P3_WIFI: u32 = 2401;
const A_P3_CLOUD: u32 = 2402;
/// 通知条：点空白处 = 关掉当前条
const A_P3_NOTICE_CLOSE: u32 = 2403;
/// 通知条左侧箭头（上一条）
const A_P3_NOTICE_PREV: u32 = 2404;
/// 通知条右侧箭头（下一条）
const A_P3_NOTICE_NEXT: u32 = 2405;

/// 通知条高度：贴在底栏（`navigatorStatusbarHeight` = 72，y 1800..1872）上方。
const NOTICE_H: i32 = 96;
/// 通知条字号（和旧 toast 黑条一致的那一档）
const NOTICE_SIZE: f32 = 30.0;
// === P3 end ===

// === P4 begin ===
// 磁盘将满 / 只读模式（官方 `res_0686a15.qml`、`res_06b7a13.qml`）。2500 段，
// 避开上面所有动作号。
/// 库顶栏那条磁盘提示（点了进 Settings > Storage）
const A_P4_DISK: u32 = 2500;

/// 提示条几何：黑胶囊占住顶栏三个新建按钮那一排（只读时新建本来就被拦），
/// Menu（x 0..172）和搜索（x 1304..）都留着，还能去回收站删文件。
const P4_BANNER: (i32, i32, i32, i32) = (360, 24, 880, 72);
/// 官方 `res_06b7a13.qml:107`：满了的状态栏原文。
const P4_DISK_FULL: &str = "Disk is full. Not enough disk space.";
/// 官方 `res_06b7a13.qml:109`：将满（此时还能写，不切只读）。
const P4_DISK_ALMOST: &str = "Disk is almost full.";
/// 官方 Storage 页的通知条字号（和底栏通知条同一档）。
const P4_BANNER_SIZE: f32 = 30.0;
// === P4 end ===
// === P9 begin ===
// 页总览增强（官方 `res_06a644c.qml`）：`Go to page` 输入页 / 网格↔列表 /
// 标签筛选胶囊。1110 段，避开总览自己的 1000..1103 和设置页的 2000+。
const A_P9_GO: u32 = 1110;
const A_P9_VIEW: u32 = 1111;
/// `Go to page` 输入页的 `Cancel` / `OK`
const A_P9_GO_CANCEL: u32 = 1112;
const A_P9_GO_OK: u32 = 1113;
/// 标签胶囊：+i（一屏最多画五六个，留 20 个的余量）
const A_P9_TAG0: u32 = 1120;
// === P9 end ===

/// 无障碍「Large」字号相对 Normal 的倍率。
///
/// 官方 accessibilityMode 把 body 28→36（约 1.29）、H1 36→40（1.11），
/// 取 1.15 是个折中：侧栏最长的 "Accessibility" 在 343px 里还放得下。
const FONT_SCALE_LARGE: u16 = 115;

/// 设置页字号倍率（百分比）。`set_size` 里乘上去，所以整页文字都会变。
static SET_FONT_SCALE: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(100);

fn set_font_scale(pct: u16) {
    SET_FONT_SCALE.store(pct, std::sync::atomic::Ordering::Relaxed);
}

/// 页脚「Page N of M」占的屏幕矩形（官方页码居中、基线 1846）。
///
/// 文档设置面板开着时页码要藏起来、收起来时要补回来，两下都只刷这一块，
/// 别为了一条页码整屏重绘。最长的 "Page 100 of 100" 也在框里。
const FOOTER_RECT: (i32, i32, i32, i32) = (400, 1780, 620, 92);

// === SYNC-AUTO begin ===
/// 自动同步的底数间隔（秒）。SSE 的 `event: rev` 已经在推"服务器变了"，
/// 这条定时只是兜底。
const SYNC_BASE: u64 = 600;
/// 低电量（≤20%）且没插电时的底数：口袋/包里别再每 10 分钟唤醒无线。
const SYNC_LOW_BATT: u64 = 1800;
/// 连续几轮无变化之后不再翻倍（10·2³ = 80 → 封顶 60 min）。
const SYNC_QUIET_MAX: u32 = 3;
/// 自动同步间隔的上限。
const SYNC_MAX: u64 = 3600;
/// 多久没有输入就算"没人在看"（秒）：这时候不做网络 I/O。
const SYNC_IDLE_AFTER: u64 = 120;

/// 自动同步间隔的**纯策略**（单测直接钉它）。
///
/// 插电：固定 10 分钟（电不要钱，实时性优先）；
/// 低电（≤20%，没插电）：底数 30 分钟；
/// 其余：10 分钟起步，连续 `quiet` 轮无变化翻倍，封顶 60 分钟。
fn sync_interval_secs(charging: bool, low_batt: bool, quiet: u32) -> u64 {
    if charging {
        return SYNC_BASE;
    }
    let base = if low_batt { SYNC_LOW_BATT } else { SYNC_BASE };
    (base << quiet.min(SYNC_QUIET_MAX)).min(SYNC_MAX)
}
// === SYNC-AUTO end ===

/// 文档空间尺寸 = 竖屏画布尺寸。PDF/电子书的底图永远是按这个尺寸 contain
/// 渲好后铺满画布的；横屏**不改它**（改了笔迹坐标就和页面对不上，见
/// `App::set_landscape`），只把这块空间按视图放大到横屏视口里。
const DOC_W: usize = 1404;
const DOC_H: usize = 1872;

/// 右上角 ×（关闭文档）的命中/重绘矩形。竖屏是官方位置；横屏贴新的右边缘。
/// 左利手时工具条换到右边，× 跟着换到左上角（官方也是这么镜像的）。
fn close_rect() -> (i32, i32, i32, i32) {
    (rmdev::fb::flip_x(rmdev::fb::logical().0 - 104, 90), 10, 90, 90)
}

/// 触控锁图标的矩形（80×64），同样跟着屏幕右边走。
fn lock_rect() -> (i32, i32, i32, i32) {
    (rmdev::fb::flip_x(rmdev::fb::logical().0 - 214, 80), 18, 80, 64)
}

/// 工具条那一列的整条矩形（`y` 往下 `h`）；左利手时它在屏幕右边。
fn band_rect(y: i32, h: i32) -> (i32, i32, i32, i32) {
    (rmdev::chrome::band_x(), y, CHROME_W, h)
}

/// 弹出面板的横向锚点：右利手贴工具条右边，左利手镜像到工具条左边。
fn panel_x(x: i32, w: i32) -> i32 {
    rmdev::fb::flip_x(x, w)
}

/// 纸面（工具条以外）能画的横向范围 `[x0, x1)`。
fn paper_x() -> (i32, i32) {
    let (w, _) = rmdev::fb::logical();
    if rmdev::fb::mirrored() {
        (0, w - CHROME_W)
    } else {
        (CHROME_W, w)
    }
}

/// 横坐标是不是压在工具条那一列下面（左利手时在右边）。
fn under_band(x: i32) -> bool {
    let (a, b) = paper_x();
    x < a || x >= b
}

/// 把横向落点收进纸面那一侧：工具条那一条不吃笔（左利手时是右边那一条）。
fn clamp_paper_i(x: i32) -> i32 {
    let (a, b) = paper_x();
    x.clamp(a, b - 1)
}

fn clamp_paper_f(x: f32) -> f32 {
    let (a, b) = paper_x();
    x.clamp(a as f32 + 1.0, (b - 1) as f32)
}

/// 工具条那一侧把画布裁掉：右利手裁左边（`clip_l`），左利手裁右边（`clip_r`）。
///
/// 纸面贴图是从裁剪线开始整段写的，所以**必须在贴图之前**把它改成这一帧该有的
/// 样子，否则收起工具条那一帧 0..120 那一列一个像素都贴不出去。
fn clip_for_chrome(canvas: &mut Canvas, visible: bool) {
    let (w, _) = rmdev::fb::logical();
    if visible && rmdev::fb::mirrored() {
        canvas.clip_l = 0;
        canvas.clip_r = w - CHROME_W;
    } else {
        canvas.clip_l = if visible { CHROME_W } else { 0 };
        canvas.clip_r = i32::MAX;
    }
}

/// 页脚页码区：竖屏是官方量出来的 400..1020，横屏贴到新的底边。
fn footer_rect() -> (i32, i32, i32, i32) {
    if rmdev::fb::landscape() {
        let (w, h) = rmdev::fb::logical();
        ((w - 620) / 2, h - 92, 620, 92)
    } else {
        FOOTER_RECT
    }
}

/// 库的过滤视图。官方侧栏就这几种。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    MyFiles,
    Notebooks,
    Pdfs,
    Ebooks,
    Favorites,
    Tags,
    Trash,
}

impl View {
    fn title(self) -> &'static str {
        match self {
            View::MyFiles => "My files",
            View::Notebooks => "Notebooks",
            View::Pdfs => "PDFs",
            View::Ebooks => "Ebooks",
            View::Favorites => "Favorites",
            View::Tags => "Tags",
            View::Trash => "Trash",
        }
    }

    /// 空状态文案（官方 "My files" 顶层没有空态）
    /// 空态：(主文案, 副文案行, 图标)。官方 `res_06a0e55.qml:227-310`。
    /// 顶层 My files 没有空态（QML 里显式排除）；Notebooks 落到空分支（不显示）。
    fn empty(self) -> Option<(&'static str, &'static [&'static str], u8)> {
        match self {
            View::Pdfs => Some((
                "You have no documents",
                &[
                    "You can import documents as PDF files through",
                    "the desktop or mobile app.",
                    "",
                    "Read and take notes in a document, and",
                    "share to email directly from your device.",
                ],
                1,
            )),
            View::Ebooks => Some((
                "You have no ebooks",
                &[
                    "You can import books in ePUB format through",
                    "the desktop or mobile app.",
                    "",
                    "You can organise your ebooks by moving them",
                    "into folders. You will also always find them",
                    "collected here.",
                ],
                2,
            )),
            View::Favorites => Some((
                "No favorites yet",
                &[
                    "You can favorite files and folders for quick",
                    "access; everything you favorite can be",
                    "found right here.",
                ],
                3,
            )),
            View::Trash => Some((
                "Trash is empty",
                &[
                    "Files and folders you move to trash will be shown here,",
                    "where you can restore or permanently delete them.",
                ],
                4,
            )),
            _ => None,
        }
    }
}

/// 排序。官方库头 5 项，回收站里第一项文案变成 "Date removed"。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sort {
    Updated,
    PageCount,
    Alphabetical,
    RecentlyOpened,
    FileSize,
}

impl Sort {
    const ALL: [Sort; 5] = [
        Sort::Updated,
        Sort::PageCount,
        Sort::Alphabetical,
        Sort::RecentlyOpened,
        Sort::FileSize,
    ];

    fn label(self, trash: bool) -> &'static str {
        match (self, trash) {
            (Sort::Updated, false) => "Last updated",
            (Sort::Updated, true) => "Date removed",
            (Sort::PageCount, _) => "Page count",
            (Sort::Alphabetical, _) => "Alphabetical",
            (Sort::RecentlyOpened, _) => "Recently opened",
            (Sort::FileSize, _) => "File size",
        }
    }

    fn index(self) -> u32 {
        Sort::ALL.iter().position(|s| *s == self).unwrap_or(0) as u32
    }

    fn from_index(i: u32) -> Sort {
        Sort::ALL[(i as usize).min(Sort::ALL.len() - 1)]
    }
}

/// 图层面板里能点的东西
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LayerAct {
    Select(usize),
    Eye(usize),
    Up(usize),
    Down(usize),
    Merge(usize),
    Delete(usize),
    Bg,
    Add,
    // === P11 begin ===
    /// 文字层那一行（官方 `LayerComponent{isTextLayer: true}`）：点了切到文字工具
    Text,
    // === P11 end ===
}

/// 图层面板几何（官方 `Values.documentViewLayerFoldoutWidth` = (30+84)*7 = 798，
/// 行高 = `documentViewToolButtonSize` = 120；面板底边贴着"图层"按钮下沿）
const LAYER_PANEL_X: i32 = CHROME_W;
const LAYER_PANEL_W: i32 = 798;
const LAYER_ROW: i32 = 120;
const LAYER_BTN_BOTTOM: i32 = 1632;
const LAYER_SMALL_BTN: i32 = 60;
/// 官方 fontSizeBody2 = 25，我们的字体按 1.4 倍换算
const LAYER_FONT: f32 = 25.0 * 1.4;

fn pixel_width_scale() -> f32 {
    std::env::var("RM_WSCALE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.0)
}

/// 应用偏好文件的路径。官方是 `Settings.listMode`（由 xochitl 自己存），
/// 我们只存这一项：库用网格还是列表。真机放 `/home/root/.config/rmdev/`
/// （和 `entry.uuid` 同目录）；本机是 macOS，用 `$HOME` 下的同构路径，
/// 这样 `--render` 也能验证"重启后保持"。
fn prefs_path() -> std::path::PathBuf {
    let base = if cfg!(target_os = "linux") {
        std::path::PathBuf::from("/home/root")
    } else {
        std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
    };
    base.join(".config/rmdev/settings.json")
}

/// `settings.json` 里的一个键。文件不存在/坏了/没有这个键 → `None`。
fn state_get(p: &std::path::Path, key: &str) -> Option<serde_json::Value> {
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get(key).cloned())
}

/// 只改 `settings.json` 里的一个键，**其余键原样保留**。
///
/// 以前这里是"整份重写"（`json!({"listMode": on})`）。加上橡皮记忆之后
/// 就不行了：存一次橡皮模式会把 `listMode` 一起冲掉。
fn state_set(p: &std::path::Path, key: &str, v: serde_json::Value) {
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut body = std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .filter(|v| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    body[key] = v;
    match serde_json::to_string_pretty(&body) {
        Ok(s) => {
            if let Err(e) = std::fs::write(p, s + "\n") {
                eprintln!("[prefs] 写 {} 失败：{e}", p.display());
            }
        }
        Err(e) => eprintln!("[prefs] 序列化失败：{e}"),
    }
}

/// 读 `listMode`，文件不存在/坏了就当网格（官方默认）。
fn load_list_mode(p: &std::path::Path) -> bool {
    state_get(p, "listMode")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 写回 `listMode`。失败只打日志：存不了偏好不该让界面崩。
fn save_list_mode(p: &std::path::Path, on: bool) {
    state_set(p, "listMode", serde_json::json!(on));
}

/// 上次用的橡皮是不是"擦整笔"。
///
/// 存在 `settings.json` 里（和 `listMode` 同一份运行时状态），**不写**
/// `settings` 那份 key=value 偏好：那份文件"存在"本身就代表"用户设置过"，
/// 首次启动的无障碍页靠它判断高亮 Large 还是 Normal，不能因为擦了一次字
/// 就被建出来。
fn load_last_erase_stroke(p: &std::path::Path) -> bool {
    state_get(p, "lastEraserStroke")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 记住"上一次用的是哪一档橡皮"（落盘，跨文档/跨启动）。
///
/// 单独抽成自由函数而不是 `App` 的方法：调用点在 `&mut self.screen` 的借用
/// 里面，拿不到 `&mut self`。只改一个键，不会碰别的状态。
fn persist_last_eraser(stroke: bool) {
    state_set(&prefs_path(), "lastEraserStroke", serde_json::json!(stroke));
}

/// 笔尾橡皮这一笔该不该"删整条笔迹"。
///
/// 做成自由函数是为了能被单测直接打到 —— 真正的调用点在 `&mut self.screen`
/// 的借用里面，测试那里没法构造一个完整的 `App`。
fn tail_stroke_erase(tail: prefs::TailEraser, last: bool) -> bool {
    match tail {
        prefs::TailEraser::Eraser => false,
        prefs::TailEraser::Stroke => true,
        prefs::TailEraser::Last => last,
    }
}

struct Active {
    builder: StrokeBuilder,
    prev: (f32, f32, f32),
    moved: bool,
    tap_x: f32,
    tap_y: f32,
    /// 橡皮这一笔扫过的点（场景坐标 + 半径）：抬笔时按它把真正碰到的笔迹删掉。
    /// 只在 canvas 上擦像素是不够的 —— 屏幕看着擦掉了，翻页回来又全回来。
    erase: Vec<(f32, f32, f32)>,
    /// 这一笔的橡皮是"碰到就删整条"还是"只刷白"。
    ///
    /// 按下时定一次。笔身橡皮 = 菜单里选的那一档；笔尾橡皮
    /// （`ip.rubber`）按设置页 `Pen-tail eraser` 的三档来定
    /// （默认是"真橡皮"：蹭到哪里白到哪里，不会悄悄删掉整条线）。
    stroke_erase: bool,
}

/// 正在编辑的一段文字（键盘盖在页面上，和官方一样能看见文字位置）
struct TextEdit {
    /// 屏幕坐标（光标左上角）
    x: f32,
    y: f32,
    kb: Keyboard,
    /// Some(i) = 在改 texts[i]；None = 新建
    editing: Option<usize>,
    // === P11 begin ===
    /// 这一段（新建时 = 即将新建的这一段）的官方段落样式：落盘进 `.rm` 的
    /// `RootText` 样式码，渲染字号由 `ParagraphStyle::default_size()` 推。
    p11_style: ParagraphStyle,
    // === P11 end ===
}

/// 图层改名键盘：层下标 + 输入框。官方在层名上 `onPressAndHold` 弹
/// `Rename Layer` 黑头 + 键盘（`res_06713f5.qml:112-116`、`res_067887b.qml:2141`）。
struct LayerRename {
    index: usize,
    kb: Keyboard,
}

/// 层名能不能落盘：trim 后非空、没有控制字符。和 `RmPage::rename_layer`
/// 同一套规则（`.rm` 里的层名是裸 UTF-8，空串/控制字符解析回来会退回 `Layer N`）。
fn layer_name_valid(name: &str) -> bool {
    let n = name.trim();
    !n.is_empty() && !n.chars().any(|c| c.is_control())
}

/// 选区：`idx` 是**当前层** `lines` 里的下标，`bounds` 是从采样点重算的包围盒
/// （场景坐标）。
///
/// `Line::bounds` 读盘后全是 0（`doc/rm.rs` 的 `parse_line`），所以这里只信 `bounds_of`。
struct Sel {
    idx: Vec<usize>,
    bounds: RectF,
}

/// 拖动选区：按下点 + 上一次笔位置（都是屏幕坐标）。`moved` 决定要不要记一步撤销。
struct DragSel {
    from: (f32, f32),
    last: (f32, f32),
    moved: bool,
}

struct Notebook {
    uuid: String,
    name: String,
    page: RmPage,
    /// 当前页序号 / 总页数
    page_index: usize,
    page_count: usize,
    /// 页面总览是否打开 / 每页的缩略图缓存
    overview: bool,
    page_thumbs: Option<Vec<(usize, Vec<u8>)>>,
    /// 手指滑动的起点与累计位移（翻页用）
    swipe: Option<(f32, f32, f32)>,
    chrome: Chrome,
    active: Option<Active>,
    exit_taps: u32,
    dirty: bool,
    /// 这次落笔是在点按钮，抬笔时不要再当绘图/手势处理
    tap_consumed: bool,
    /// 这一页上的文字
    texts: Vec<TextItem>,
    // === P11 begin ===
    /// 每段文字的官方段落样式，与 `texts` 同下标。
    /// 不塞进 `library::TextItem`：那组字段就是 sidecar 的 JSON 字节格式（P11 不许动），
    /// 所以样式单独跟着 `texts` 走；长度对不上时按 PLAIN 处理。
    p11_styles: Vec<ParagraphStyle>,
    // === P11 end ===
    /// 文字编辑器（键盘）
    text_edit: Option<TextEdit>,
    /// 文字工具下按下但还没抬笔的位置
    pending_text: Option<(f32, f32)>,
    /// 选区（下标只对**当前层**有效：翻页/切工具/撤销/换层之后一律清掉）
    sel: Option<Sel>,
    /// 套索进行中的折线（屏幕坐标，抬笔时转成场景坐标再交给 `lasso_select`）
    lasso: Option<Vec<(f32, f32)>>,
    /// 正在拖动选区
    drag_sel: Option<DragSel>,
    /// 按下时剪贴板非空：抬笔还没动才算"点一下空白处粘贴"，
    /// 一动就取消、改回套索（否则剪贴板有东西时永远画不出新套索）。
    pending_paste: Option<(f32, f32)>,
    /// 撤销栈：一步可以是一笔，也可以是剪切/粘贴/拖动的多笔
    undo: Vec<Op>,
    /// 重做栈。任何新的改动都会清空它
    redo: Vec<Op>,
    /// "Add tag"的键盘
    tag_edit: Option<Keyboard>,
    /// 图层改名键盘（层名长按/双击）
    layer_rename: Option<LayerRename>,
    /// "擦选区"拖框：(按下点, 当前点)，屏幕坐标。抬笔按这个矩形删笔迹。
    erase_rect: Option<((f32, f32), (f32, f32))>,
    /// 视图变换（缩放/平移）。落笔、渲染、增量刷新都过它；
    /// identity 时行为与接线前完全一致。
    view: PageView,
    /// 最后一支"真笔"的下标（`TOOLS[..len-1]`）。官方点笔按钮时用它切回上一支笔：
    /// `lastWritingTool`。文字工具（下标 8）不算笔，切走时要能切回来。
    last_pen: usize,
    /// 横屏前那一眼的视图。再点一次「Set to portrait view」按原样还回去
    /// （竖屏时为 None）。
    landscape_back: Option<PageView>,
}

impl Notebook {
    /// 橡皮抬笔：把 `path`（场景坐标 + 半径）扫到的笔迹从当前层删掉，记一步撤销。
    ///
    /// 返回 `(删掉几笔, 这些笔迹的场景包围盒)`；后者给调用方算局部刷新范围。
    /// **被删的笔迹可能比橡皮扫过的路径大得多** —— 只按路径包围盒刷新的话，
    /// 笔画探出去的部分会留在屏上（"擦不干净，只消掉一部分"就是它）。
    fn commit_erase(&mut self, path: &[(f32, f32, f32)]) -> (usize, Option<RectF>) {
        let li = self.page.current;
        let (mut n, mut bounds) = self.erase_on_layer(li, path);
        // `AI` 图层不是当前层，但用户必须能把它擦掉：AI 的批注/回答是写进
        // 独立图层的，只擦当前层的话"AI 写的东西永远擦不掉"（真机报过）。
        // 路径橡皮对所有可见图层一视同仁，和官方"橡皮只擦当前层"不同是故意的：
        // AI 层用户没法把它切成当前层，不给这条路等于删不掉。
        if let Some(ai) = rmdev::panels::annotate::ai_index(&self.page) {
            if ai != li {
                let (n2, b2) = self.erase_on_layer(ai, path);
                n += n2;
                bounds = match (bounds, b2) {
                    (None, b) => b,
                    (a, None) => a,
                    (Some(a), Some(b)) => Some(RectF {
                        x: a.x.min(b.x),
                        y: a.y.min(b.y),
                        w: (a.x + a.w).max(b.x + b.w) - a.x.min(b.x),
                        h: (a.y + a.h).max(b.y + b.h) - a.y.min(b.y),
                    }),
                };
            }
        }
        (n, bounds)
    }

    /// 在**指定图层**上按路径删笔迹（`commit_erase` 的内核）。
    fn erase_on_layer(&mut self, li: usize, path: &[(f32, f32, f32)]) -> (usize, Option<RectF>) {
        if path.is_empty() {
            return (0, None);
        }
        let Some(layer) = self.page.layers.get(li) else {
            return (0, None);
        };
        // 命中半径取「`hit_width` 的档位估算」和「采样点的真实笔画宽度」里大的那个。
        //
        // `hit_width = thickness * line_width_factor() + 2`，而 `thickness`
        // 存的是**粗细档位**（1~3），不是像素宽 —— 荧光笔实际画出来二十几
        // 像素，命中半径却只有 2~3 像素。于是擦整笔基本只能擦到中心线，
        // 而荧光笔的中心线和可见笔迹边缘差很远，表现就是「整条擦了也没用，
        // 只擦掉几个网点格子」。
        let half_w = |l: &Line| {
            let by_hit = l.hit_width(1.0) * 0.5;
            let by_pt = l.points.iter().map(|p| p.width).fold(0.0f32, f32::max) * 0.5;
            by_hit.max(by_pt)
        };
        let mut items: Vec<(usize, Line)> = Vec::new();
        let mut bounds: Option<RectF> = None;
        for (i, l) in layer.lines.iter().enumerate() {
            // **橡皮笔迹不能删！** 官方把橡皮也存成"线"，渲染时按顺序做减法。
            // 删掉一条橡皮线 = 把它当时擦掉的笔迹全部"复活"（踩过：Quick sheets
            // 第 3 页原本只显示一条波浪线，删掉几条橡皮线以后满页的手写都冒出来了）。
            if matches!(
                l.tool,
                rmdev::pen::Tool::Eraser
                    | rmdev::pen::Tool::EraseSection
                    | rmdev::pen::Tool::ClearPage
            ) {
                continue;
            }
            let hw = half_w(l) + 1.0;
            // **点到线段**，不是点到点。
            //
            // 原实现只遍历 `l.points` 的采样点。荧光笔又粗又稀疏（快速划一笔
            // 可能只有几个点），橡皮从两个点中间穿过去时离任何采样点都远，
            // 就被判成"没碰到" —— 表现就是"荧光笔碰了不消失"，而圆珠笔
            // 点密所以看不出来。
            let hit = l.points.iter().any(|p| {
                path.iter().any(|&(x, y, r)| {
                    let (dx, dy) = (p.x - x, p.y - y);
                    let rr = r + hw;
                    dx * dx + dy * dy <= rr * rr
                })
            }) || l.points.windows(2).any(|w| {
                let (a, b) = (&w[0], &w[1]);
                let (sx, sy) = (b.x - a.x, b.y - a.y);
                let len2 = sx * sx + sy * sy;
                path.iter().any(|&(x, y, r)| {
                    let rr = r + hw;
                    // 擦点到线段 ab 的最短距离
                    let t = if len2 > 1e-6 {
                        (((x - a.x) * sx + (y - a.y) * sy) / len2).clamp(0.0, 1.0)
                    } else {
                        0.0
                    };
                    let (cx, cy) = (a.x + sx * t, a.y + sy * t);
                    let (dx, dy) = (x - cx, y - cy);
                    dx * dx + dy * dy <= rr * rr
                })
            });
            if hit {
                if let Some(b) = line_bbox(l) {
                    bounds = Some(match bounds {
                        Some(a) => RectF {
                            x: a.x.min(b.x),
                            y: a.y.min(b.y),
                            w: (a.x + a.w).max(b.x + b.w) - a.x.min(b.x),
                            h: (a.y + a.h).max(b.y + b.h) - a.y.min(b.y),
                        },
                        None => b,
                    });
                }
                items.push((i, l.clone()));
            }
        }
        if std::env::var("RM_ERASE_LOG").is_ok() {
            let tools: Vec<u8> = layer
                .lines
                .iter()
                .filter(|l| !matches!(l.tool, Tool::Eraser | Tool::EraseSection | Tool::ClearPage))
                .map(|l| l.tool as u8)
                .collect();
            let (mut ax0, mut ay0, mut ax1, mut ay1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for &(x, y, r) in path {
                ax0 = ax0.min(x - r);
                ay0 = ay0.min(y - r);
                ax1 = ax1.max(x + r);
                ay1 = ay1.max(y + r);
            }
            let cand: Vec<(u8, usize, f32, f32, f32, f32, f32)> = layer
                .lines
                .iter()
                .filter(|l| !matches!(l.tool, Tool::Eraser | Tool::EraseSection | Tool::ClearPage))
                .map(|l| {
                    (
                        l.tool as u8,
                        l.points.len(),
                        l.bounds.x,
                        l.bounds.y,
                        l.bounds.w,
                        l.bounds.h,
                        // 这一条被判定命中半径用的 hw
                        l.hit_width(1.0) * 0.5,
                    )
                })
                .collect();
            eprintln!(
                "[erase] 路径 {} 点 半径框 ({:.0},{:.0})-({:.0},{:.0})，命中 {} 条；候选 (工具,点数,x,y,w,h,hw) {cand:?}",
                path.len(),
                ax0,
                ay0,
                ax1,
                ay1,
                items.len()
            );
        }
        if items.is_empty() {
            return (0, None);
        }
        let n = items.len();
        if let Some(layer) = self.page.layers.get_mut(li) {
            for (i, _) in items.iter().rev() {
                layer.lines.remove(*i);
            }
        }
        self.undo.push(Op::Remove { layer: li, items });
        self.redo.clear();
        self.dirty = true;
        self.clear_sel();
        (n, bounds)
    }

    /// "擦选区"（6d）抬笔：矩形（场景坐标）扫到的整条笔迹删掉，记一步撤销。
    ///
    /// 和 `commit_erase`（路径橡皮）同一套账：`Op::Remove` = 可撤销一步。
    /// 判据用 `select::rect_hit`（包围盒相交），不是 `rect_select` 的"完全框住"——
    /// 橡皮把框扫过的整条擦掉，压住半条也该擦。
    ///
    /// 返回 `(删掉几笔, 这些笔迹的场景包围盒)`；后者给调用方算局部刷新范围
    /// （被删的笔迹可能探出框外）。
    fn commit_erase_rect(&mut self, r: RectF) -> (usize, Option<RectF>) {
        let li = self.page.current;
        let (mut n, mut bounds) = self.erase_rect_on_layer(li, r);
        // 理由同 `commit_erase`：`AI` 图层也得能擦，不然 AI 写的东西删不掉。
        if let Some(ai) = rmdev::panels::annotate::ai_index(&self.page) {
            if ai != li {
                let (n2, b2) = self.erase_rect_on_layer(ai, r);
                n += n2;
                bounds = match (bounds, b2) {
                    (None, b) => b,
                    (a, None) => a,
                    (Some(a), Some(b)) => Some(RectF {
                        x: a.x.min(b.x),
                        y: a.y.min(b.y),
                        w: (a.x + a.w).max(b.x + b.w) - a.x.min(b.x),
                        h: (a.y + a.h).max(b.y + b.h) - a.y.min(b.y),
                    }),
                };
            }
        }
        (n, bounds)
    }

    /// 在**指定图层**上按矩形删笔迹（`commit_erase_rect` 的内核）。
    fn erase_rect_on_layer(&mut self, li: usize, r: RectF) -> (usize, Option<RectF>) {
        let Some(layer) = self.page.layers.get(li) else {
            return (0, None);
        };
        // 橡皮笔迹不能删！官方把橡皮也存成"线"，渲染时按顺序做减法，
        // 删掉一条橡皮线 = 把它当时擦掉的笔迹全部"复活"（理由同 `commit_erase`）。
        let mut idx: Vec<usize> = Vec::new();
        let mut keep: Vec<Line> = Vec::new();
        for (i, l) in layer.lines.iter().enumerate() {
            if matches!(
                l.tool,
                rmdev::pen::Tool::Eraser
                    | rmdev::pen::Tool::EraseSection
                    | rmdev::pen::Tool::ClearPage
            ) {
                continue;
            }
            idx.push(i);
            keep.push(l.clone());
        }
        let hit = select::rect_hit(&keep, r);
        if hit.is_empty() {
            return (0, None);
        }
        let items: Vec<(usize, Line)> = hit.iter().map(|&j| (idx[j], keep[j].clone())).collect();
        let n = items.len();
        if let Some(layer) = self.page.layers.get_mut(li) {
            for (i, _) in items.iter().rev() {
                layer.lines.remove(*i);
            }
        }
        self.undo.push(Op::Remove { layer: li, items });
        self.redo.clear();
        self.dirty = true;
        let bounds = select::bounds_of(&keep, &hit);
        self.clear_sel();
        (n, bounds)
    }

    /// 撤销/重做按钮的可用态（官方不可用时图标变灰）。
    /// 只要往工具条那一列刷一次之前都得先调，否则状态会落后一帧。
    fn sync_undo(&mut self) {
        self.chrome.undo_ok = !self.undo.is_empty();
        self.chrome.redo_ok = !self.redo.is_empty();
    }

    /// 丢掉选区相关的临时状态。
    ///
    /// `sel.idx` 是当前层 `lines` 的下标，任何会让下标失效的操作（翻页、切工具、
    /// 撤销/重做、换图层）之后都必须调它，否则剪切会删错笔迹。
    fn clear_sel(&mut self) {
        if self.sel.is_some() || self.lasso.is_some() {
            eprintln!("[sel] 清掉选区（翻页/切工具/撤销/换层，下标已不可信）");
        }
        self.sel = None;
        self.lasso = None;
        self.drag_sel = None;
        self.pending_paste = None;
        self.erase_rect = None;
    }
}

/// 后台在跑什么（决定结果怎么用）
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Job {
    /// 配对：成功后刷新设置页
    Pair,
    /// 同步一次
    Sync,
    /// 当前页手写转文字
    Ocr,
    /// 发邮件
    Mail,
}

/// 后台任务的产出
#[derive(Clone, Debug)]
enum JobDone {
    /// 配对成功（服务器地址）
    Paired(String),
    /// 同步统计
    Synced { up: usize, down: usize, conflicts: usize, rev: u64 },
    /// 转出来的文字
    Text(String),
    /// 邮件已发
    MailSent,
}

// === P2 begin ===
/// "Send by email"：官方 `res_06b193d.qml` 的整页 sheet。
///
/// 行列表是 `Send to` / `Selected`（发哪几页）/ `Send as`（PDF/PNG/SVG）/
/// `Subject` / `File name`，黑头 `Cancel` | `Send by email` | `Send`，
/// 键盘收起时底部再画一个主按钮 `Send`。解析、布局、编码都在 `rmdev::share`。
#[derive(Clone)]
struct Mail {
    kb: Keyboard,
    /// 发完/取消之后要回哪一页
    back: (String, usize),
    /// 文档名（标题/文件名留空时用它）
    name: String,
    /// 焦点：`share::F_*`（`F_NONE` = 键盘收起）
    focus: u8,
    /// 收件人原文（逗号/分号/空格分隔，可多个）
    to: String,
    subject: String,
    /// `File name` 行的原文（不带上扩展名，落盘时按格式补）
    filename: String,
    /// 页模式：0 = 当前页，1 = 全部页，2 = 区间
    mode: u8,
    /// 区间原文（`1-3, 5`）
    range: String,
    format: rmdev::share::MailFormat,
    /// 总页数（打开时算好，区间校验用）
    total: usize,
    /// 发送失败：标题下红字
    err: bool,
}
// === P2 end ===

/// 配对向导：先输服务器地址，再输 8 位配对码。
struct Pair {
    kb: Keyboard,
    /// 0 = 服务器地址，1 = 配对码
    stage: u8,
    url: String,
}

// === C0W begin ===
/// Wi-Fi 连接面板（官方 `res_065b361.qml` / `res_0687541.qml` 的
/// `Connect to <b>%1</b>`）：SSID 一行 + 密码一行 + 键盘 + `Cancel`/`Connect`。
///
/// 两个入口共用一份状态：
/// - 点扫描列表里**加密**的网络 → `Screen::WifiPass`（`stage` 直接是 1，SSID 只读）；
/// - `Join other network` → `Screen::WifiJoin`（`stage` 0 = 输 SSID，1 = 输密码）。
struct WifiConnect {
    kb: Keyboard,
    /// 目标 SSID。`WifiPass` 从列表带进来；`WifiJoin` 第一步输完填进来。
    ssid: String,
    /// 0 = 输 SSID，1 = 输密码
    stage: u8,
    /// 面板里的红字（输入校验失败）。英文原文，不翻译。
    err: Option<String>,
}
// === C0W end ===

// === P10 begin ===
// 手写转文字整页流程（官方 `res_066c007.qml` + `res_066b1b0.qml`）：
// 页总览选页 → `Convert` → 进度 `Converting to text (%1)` → 结果**新增一页**
// （原页保留）→ 可 `Undo`。下面只有状态机和纯函数，绘制/线程在 `impl App` 里。

/// 结果页上文字块的左边界（页面坐标；渲染时才加 `PAGE_DX/PAGE_DY`）。
const CONV_TEXT_X: f32 = 40.0;
/// 第一行的顶边（和原来"转当前页"那版一样从 120 起）。
const CONV_TEXT_TOP: f32 = 120.0;
/// 行高：`TextItem.size` 26 px，行距按 1.3 倍。
const CONV_LINE_H: f32 = 34.0;
/// 纸面尺寸（rM2 1404×1872）与上下留白，用来算一页能放几行。
const CONV_PAGE_H: f32 = 1872.0;
const CONV_MARGIN: f32 = 120.0;
/// 进度通知**不自动消失**：官方是"转换完才换掉"（`res_066b1b0.qml` 的 states）。
const CONV_NOTICE_TTL: std::time::Duration = std::time::Duration::from_secs(4 * 3600);
/// 通知去重键（官方 `enqueue({ id })`）。
const CONV_NOTICE_KEY: &str = "convert-notes-to-text";
/// 手写语言名。自建服务器只跑英文识别；官方 `%1` 取语言名第一个空格前那一段。
const CONV_LANG: &str = "English";

/// 底部状态条高度：官方 `Values.navigatorStatusbarHeight` = 72。
const CONV_BAR_H: i32 = 72;

/// 一页能放几行文字（按页高切分用）。
fn conv_lines_per_page(page_h: f32) -> usize {
    let usable = page_h - 2.0 * CONV_MARGIN;
    if usable <= 0.0 {
        return 1;
    }
    let n = (usable / CONV_LINE_H) as usize;
    n.max(1)
}

/// 长文字按页高切成一页一页的行；空行留着（保留段落间距），尾部空行丢掉。
fn conv_paginate(text: &str, per_page: usize) -> Vec<Vec<String>> {
    if per_page == 0 {
        return Vec::new();
    }
    let mut lines: Vec<String> = text.lines().map(|s| s.trim_end().to_string()).collect();
    while lines.last().map(|s| s.is_empty()).unwrap_or(false) {
        lines.pop();
    }
    if lines.is_empty() {
        return Vec::new();
    }
    lines.chunks(per_page).map(|c| c.to_vec()).collect()
}

/// 每页的文字块（坐标是页面坐标，`TextItem` 的语义）。
fn conv_items(lines: &[String]) -> Vec<TextItem> {
    lines
        .iter()
        .enumerate()
        .map(|(i, s)| TextItem {
            x: CONV_TEXT_X,
            y: CONV_TEXT_TOP + CONV_LINE_H * i as f32,
            size: 26.0,
            text: s.clone(),
        })
        .collect()
}

/// 进度百分比（0..=100）。没有页时算 0，别除零。
fn conv_percent(done: usize, total: usize) -> u8 {
    if total == 0 {
        return 0;
    }
    ((done.min(total) * 100) / total) as u8
}

/// 官方进度文案（`res_066b1b0.qml:45`）：`Converting to text (%1)`，
/// `%1` 是语言名在**第一个空格之前**那一段（`lang.slice(0, i)`）。
fn conv_progress_text(lang: &str) -> String {
    let short = lang.split(' ').next().unwrap_or(lang);
    format!("Converting to text ({short})")
}

/// 官方 `errorMessage()`（`res_066b1b0.qml:133`）里我们分得出来的几档。
fn conv_error_text(err: &str) -> &'static str {
    let e = err.to_ascii_lowercase();
    if e.is_empty() || e.contains("no text") {
        // 官方 NoTasks / NoText 那一档
        return "Unable to convert notes";
    }
    if e.contains("cannot connect")
        || e.contains("send failed")
        || e.contains("read failed")
        || e.contains("timed out")
        || e.contains("connection")
    {
        // 官方 NetworkError 那一档
        return "Unable to convert text. Please check your Wi-Fi connection.";
    }
    "Unable to convert notes"
}

/// 选中的页 → 处理顺序（**降序**）。一页都没选就转当前页（官方 `acceptEnabled`
/// 一直是 true，空选择不该变成什么都不做）。
fn conv_pages_from_selection(sel: &[usize], current: usize) -> Vec<usize> {
    let mut v: Vec<usize> = if sel.is_empty() { vec![current] } else { sel.to_vec() };
    v.sort_unstable();
    v.dedup();
    v.reverse();
    v
}

/// 又插了一页（序号 `at`）：之前记下的新页序号凡是 `>= at` 的都要 +1。
fn conv_shift_inserted(inserted: &mut [usize], at: usize) {
    for i in inserted.iter_mut() {
        if *i >= at {
            *i += 1;
        }
    }
}

/// `Undo` 的删除顺序：降序（先删后面的页，前面记下的序号才不会失效）。
fn conv_undo_order(inserted: &[usize]) -> Vec<usize> {
    let mut v = inserted.to_vec();
    v.sort_unstable_by(|a, b| b.cmp(a));
    v.dedup();
    v
}

/// 转换态顶栏的三个格子：`Cancel`（左）/ 标题（中）/ `Convert`（右）。
fn conv_header_cancel(w: i32) -> (i32, i32, i32, i32) {
    (0, 0, w / 3, pagesel::HDR_H)
}

fn conv_header_convert(w: i32) -> (i32, i32, i32, i32) {
    (w - w / 3, 0, w / 3, pagesel::HDR_H)
}

/// 底部状态条整条（贴在屏幕最下面）。
fn conv_bar_rect(w: i32, h: i32) -> (i32, i32, i32, i32) {
    (0, h - CONV_BAR_H, w, CONV_BAR_H)
}

/// 状态条靠工具条那一侧的那颗按钮（`Cancel` / `Undo`）。左利手时整条倒过来。
fn conv_bar_button(w: i32, h: i32) -> (i32, i32, i32, i32) {
    (
        rmdev::fb::flip_x(w - 260, 260),
        h - CONV_BAR_H,
        260,
        CONV_BAR_H,
    )
}

/// 状态条里的进度条轨道（官方 `ProgressIndicator` 的位置）。
fn conv_bar_progress(w: i32, h: i32) -> (i32, i32, i32, i32) {
    (
        rmdev::fb::flip_x(w - 260 - 32 - 220, 220),
        h - CONV_BAR_H / 2 - 4,
        220,
        8,
    )
}

/// 进度条已经走完的那一段（按百分比算宽度）。
fn conv_bar_fill(w: i32, h: i32, percent: u8) -> (i32, i32, i32, i32) {
    let r = conv_bar_progress(w, h);
    let p = percent.min(100) as i32;
    (r.0, r.1, r.2 * p / 100, r.3)
}

/// 转换流程走到哪一步。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ConvStage {
    /// 没在转（总览/文档正常）
    Idle,
    /// 页总览里选页，等 `Convert`
    Selecting,
    /// 后台逐页 OCR
    Running,
    /// 结果页已插入，等 `Undo`
    Done,
    /// 失败（官方错误态）
    Failed,
}

/// 一次"手写转文字"的全部状态。纯状态机，绘制和线程在 `impl App`。
struct Conv {
    stage: ConvStage,
    /// 这次转换属于哪个文档
    uuid: String,
    /// 要转的页，**降序**：从后往前处理，插页不会让还没处理的页码失效
    pages: Vec<usize>,
    /// 处理到 `pages` 的第几个
    next: usize,
    /// 已经插进去的新页序号（`Undo` 按降序删）
    inserted: Vec<usize>,
    /// 底部状态条那条通知（不自动消失；完成/失败时替换，退出时清掉）
    notice: Option<rmdev::notify::Notice>,
}

impl Conv {
    fn new() -> Conv {
        Conv {
            stage: ConvStage::Idle,
            uuid: String::new(),
            pages: Vec::new(),
            next: 0,
            inserted: Vec::new(),
            notice: None,
        }
    }

    fn active(&self) -> bool {
        self.stage != ConvStage::Idle
    }

    fn selecting(&self) -> bool {
        self.stage == ConvStage::Selecting
    }

    fn running(&self) -> bool {
        self.stage == ConvStage::Running
    }

    /// 进入选页态（清掉上一次的残留）。
    fn begin_select(&mut self, uuid: &str) {
        self.reset();
        self.uuid = uuid.to_string();
        self.stage = ConvStage::Selecting;
    }

    /// 选页完成，开始逐页转换。
    fn start(&mut self, pages: Vec<usize>) {
        self.pages = pages;
        self.next = 0;
        self.inserted.clear();
        self.stage = ConvStage::Running;
    }

    fn current_page(&self) -> Option<usize> {
        self.pages.get(self.next).copied()
    }

    fn advance(&mut self) {
        self.next += 1;
    }

    fn all_done(&self) -> bool {
        self.next >= self.pages.len()
    }

    fn percent(&self) -> u8 {
        conv_percent(self.next, self.pages.len())
    }

    /// 收尾：插过页才算成功（一页都没认出来 = 官方 `NoText`）。
    fn finish(&mut self) {
        self.stage = if self.inserted.is_empty() {
            ConvStage::Failed
        } else {
            ConvStage::Done
        };
    }

    fn fail(&mut self) {
        self.stage = ConvStage::Failed;
    }

    /// 装一条不自动消失的通知（官方导航栏那条）。
    fn set_notice(&mut self, kind: NoticeKind, text: impl Into<String>) {
        self.notice = Some(
            rmdev::notify::Notice::new(kind, text, std::time::SystemTime::now())
                .with_key(CONV_NOTICE_KEY)
                .with_ttl(CONV_NOTICE_TTL),
        );
    }

    fn reset(&mut self) {
        *self = Conv::new();
    }
}
// === P10 end ===

enum Screen {
    Library,
    Notebook(Box<Notebook>),
    Search(Box<Search>),
    Rename(Box<Rename>),
    Settings(Box<SettingsView>),
    Dialog(Box<Dialog>),
    Pair(Box<Pair>),
    Mail(Box<Mail>),
    // === C0W begin ===
    /// 点加密网络 → 只输密码（SSID 只读）
    WifiPass(Box<WifiConnect>),
    /// Join other network → 先输 SSID，再输密码
    WifiJoin(Box<WifiConnect>),
    // === C0W end ===
    /// 开屏锁屏。密码在 `~/.config/rmdev/passcode`，删掉文件即可恢复。
    Lock(Box<LockView>),
}

/// 官方那套"黑头对话框"：新建笔记本 / 新建文件夹 / 重命名 都是同一个壳。
#[derive(Clone)]
struct Dialog {
    kind: DialogKind,
    kb: Keyboard,
    /// 0 = Last page visited，1 = First page
    cover: usize,
    /// 模板下标（Blank / Lined / Grid / Checklist）
    template: usize,
}

#[derive(Clone)]
enum DialogKind {
    NewNotebook,
    NewFolder,
    Rename { uuid: String },
}

impl DialogKind {
    fn title(&self) -> &'static str {
        match self {
            DialogKind::NewNotebook => "New notebook",
            DialogKind::NewFolder => "New folder",
            DialogKind::Rename { .. } => "Rename",
        }
    }
    fn default_name(&self) -> &'static str {
        match self {
            DialogKind::NewNotebook => "Notebook",
            DialogKind::NewFolder => "Folder",
            DialogKind::Rename { .. } => "",
        }
    }
}

/// 对话框里能选的四张模板卡（官方 New notebook 那排）
const DLG_TEMPLATES: [(&str, &str); 4] = [
    ("Blank", "Blank"),
    ("Lined medium", "Lines medium"),
    ("Grid medium", "Grid medium"),
    ("Checklist", "Checklist medium"),
];

/// General 三个框点进去的子页。
#[derive(Clone, Copy, PartialEq, Eq)]
enum SettingsSub {
    Account,
    Subscription,
    Software,
}

/// 语言选择对话框：手写转换语言 / 键盘布局。
#[derive(Clone, Copy, PartialEq, Eq)]
enum LangKind {
    Handwriting,
    Keyboard,
}

/// Help 里点进去的二级页。
#[derive(Clone, Copy, PartialEq, Eq)]
enum HelpPage {
    About,
    Licenses,
    Tutorial,
}

/// 设置/修改/关闭密码的流程。
#[derive(Clone, Copy, PartialEq, Eq)]
enum PinMode {
    Set,
    Change,
    Remove,
}

/// 设置页里的密码输入（全屏，带数字键盘）。
#[derive(Clone)]
struct PinUi {
    mode: PinMode,
    /// Set: 0=新密码 1=确认；Change: 0=旧密码 1=新密码 2=确认；Remove: 0=旧密码
    stage: u8,
    first: String,
    entered: String,
    msg: String,
    lockout: security::Lockout,
}

impl PinUi {
    fn new(mode: PinMode) -> Self {
        PinUi {
            mode,
            stage: 0,
            first: String::new(),
            entered: String::new(),
            msg: String::new(),
            lockout: security::Lockout::default(),
        }
    }

    fn header(&self) -> &'static str {
        match self.mode {
            PinMode::Set => "Set passcode",
            PinMode::Change => "Change passcode",
            PinMode::Remove => "Turn passcode off",
        }
    }

    fn label(&self) -> &'static str {
        match (self.mode, self.stage) {
            (PinMode::Set, 0) => "Enter your new passcode",
            (PinMode::Set, _) => "Verify your new passcode",
            (PinMode::Change, 0) => "Enter your old passcode",
            (PinMode::Change, 1) => "Enter your new passcode",
            (PinMode::Change, _) => "Verify your new passcode",
            (PinMode::Remove, _) => "Enter your old passcode",
        }
    }
}

/// 开屏锁屏：4 位数字 + 连错 5 次锁 30 秒。
struct LockView {
    entered: String,
    msg: String,
    lockout: security::Lockout,
    /// 上一次画出来的剩余秒数（tick 里只在秒数变化时重绘）。
    shown_secs: u64,
    /// 圆圈里显示数字而不是黑点（官方那颗 `Show` / `Hide`）。
    show_digits: bool,
}

#[derive(Clone)]
struct SettingsView {
    tab: usize,
    /// 每个 tab 一份开关状态：`toggles[group][slot]`（group = tab 号，Software 子页是 8）。
    /// 旧版只有一个 `Vec<bool>` 拿行下标当 key，切 tab 会串状态。
    toggles: Vec<Vec<bool>>,
    right_handed: bool,
    font_large: bool,
    sub: Option<SettingsSub>,
    /// 真机数据。读不到就退回官方截图里的值，绝不 panic。
    version: String,
    disk_used: f32,
    disk_total: f32,
    ssid: String,
    /// 自建服务器配置（没有账号/订阅，配对一次就够）
    server: rmdev::net::Config,
    wifi_signal: Option<i32>,
    networks: Vec<wifi::Network>,
    battery: Option<battery::Battery>,
    /// 语言选择（下标进 language::HANDWRITING / KEYBOARDS）。
    lang_hw: usize,
    lang_kb: usize,
    lang_dialog: Option<LangKind>,
    help_page: Option<HelpPage>,
    help: help::DeviceInfo,
    /// Cloud connection 的检查结果。
    sync_msg: Option<String>,
    /// 密码输入全屏页。
    pin: Option<PinUi>,
    /// 底部黑底提示（英文），带过期时刻。
    notice: Option<(String, std::time::Instant)>,
    prefs: prefs::Prefs,
    // === P5 begin ===
    /// 设置页**顶部**的一次性完成提示（Factory reset 之后），带过期时刻。
    /// 和 `notice`（底部 toast）分开：官方这类提示是顶上那条黑 Header。
    factory_done: Option<(String, std::time::Instant)>,
    /// Factory reset 确认框开着（独占整屏，只有 Yes / No 两个按钮）。
    factory_confirm: bool,
    /// Experimental 页的 7 个开关（落盘 `~/.config/rmdev/experimental`）。
    experimental: experimental::Experimental,
    // === P5 end ===
    // === C0W begin ===
    /// 长按"当前网络"那一行 → 待确认忘记的 SSID（`Some` = 确认框开着）。
    wifi_forget: Option<String>,
    /// 正在后台连的 SSID：Wi-Fi 页状态行显示官方 `Connecting to...`。
    connecting: Option<String>,
    // === C0W end ===
}

impl SettingsView {
    fn on(&self, group: usize, slot: usize) -> bool {
        self.toggles
            .get(group)
            .and_then(|v| v.get(slot))
            .copied()
            .unwrap_or(false)
    }
}

struct Search {
    kb: Keyboard,
}

struct Rename {
    uuid: String,
    kb: Keyboard,
}

/// 整屏 GC16 全刷一次（清残影、也让上一批局部刷新不至于把 FULL 并掉）。
fn full_refresh(fb: &mut Fb) {
    let (w, h) = (fb.width, fb.height);
    fb.refresh(0, 0, w, h, WAVEFORM_MODE_GC16, rmdev::fb::UPDATE_MODE_FULL);
}

/// 实心矩形边框。
fn rect_border(fb: &mut Fb, x: i32, y: i32, w: i32, h: i32, t: i32, tone: u8) {
    let c = tone as u16;
    fb.fill_rect(x, y, w, t, c);
    fb.fill_rect(x, y + h - t, w, t, c);
    fb.fill_rect(x, y, t, h, c);
    fb.fill_rect(x + w - t, y, t, h, c);
}

struct App {
    fb: Fb,
    canvas: Canvas,
    /// 上一次画进状态栏的 `(Wi-Fi 格数, 云连通)`，用来判断要不要补刷那一条。
    net_last: (u8, Option<bool>),
    /// AI 引用菜单开着（第 14 格）；内容永远是"当前这一页"的引用。
    ai_menu: bool,
    /// 切方向之前那一眼的视图（`set_landscape` 存、切回去还原）。
    land_back: Option<PageView>,
    /// 当前页在文档空间（1404×1872 画布）里占的矩形；`None` = 整块画布（笔记本）。
    page_box: Option<(usize, usize, usize, usize)>,
    /// 用户手动选过方向（`Some`）之后，自动规则不再改它 —— 关了就一直是关的
    /// （"我关掉横屏，翻一页它又自己竖起来"是没人想要的）。换文档时清空。
    land_manual: Option<bool>,
    /// 这一下 `Up` 是"刚把引用菜单点开"的那次抬指：吞掉，别拿它当"点别处收菜单"。
    ///
    /// 手指的一次点击是 Down + Up，而工具条的点击在 **Down** 时就分派了
    /// （`ui_tap`）。菜单一开，紧跟着的 Up 又落进"菜单开着 → 点空白收起"
    /// 那条分支，于是一次点按 = 开 + 立刻关 —— 用户看到的就是"点一下它自己
    /// 飞速收起来了"。笔也一样（`ai_menu_pen`）。
    ai_menu_fresh: bool,
    /// 下一次"自动同步"的时刻（启动 15 s 后一次，之后每 10 分钟）。
    sync_auto_at: std::time::Instant,
    /// 这一轮同步是自动触发的：不弹提示、没变化就不重绘。
    sync_is_auto: bool,
    /// 连续几轮自动同步"什么都没变"。用来退避：一直没变化就把间隔翻倍
    /// （10 → 20 → 40 → 60 分钟封顶），有变化立刻回到 10 分钟。
    ///
    /// 为什么：用户的痛点是**耗电**。10 分钟一次心跳在"其实没人在等"的
    /// 时候纯属白耗：每次都要走 tailscale 用户态栈建连、把无线从省电态叫醒。
    /// 而"服务器有新东西"本来就有 SSE 事件推着走（`event: rev` 立刻同步），
    /// 定时轮询只是兜底。
    sync_quiet: u32,
    /// 上一轮自动同步因为"没人在看"被跳过了：用户一碰设备就补上。
    sync_skip_idle: bool,
    /// AI 任务表（Job status 页；关掉卡片之后还在跑的那些都在这儿）。
    jobs: rmdev::aijobs::Jobs,
    /// Job status 整屏列表开着。
    jobs_open: bool,
    /// 当前这一轮对应的任务 id（= 回答文档 uuid），`done`/`failed` 时收尾。
    ai_job: Option<String>,
    /// 上一次点工具条按钮的时间。手指连点同一个按钮（<350ms）只算一次 ——
    /// 否则"想打开笔菜单"连点两下会开了又立刻关，看起来就是"点了没反应"。
    last_tb: Option<(Msg, std::time::Instant)>,
    /// 空闲时要预热的那一页 PDF：(路径, 页号, 渲染宽, 渲染高)。
    ///
    /// 一次整页渲染在设备上要 300~700ms，翻页后立刻做会让主循环僵住，
    /// 用户紧接着滑一下就"没反应"。所以只记下来，等手停下来 400ms 再做。
    prefetch_job: Option<(String, usize, usize, usize)>,
    /// 后台任务（联网的配对 / 同步 / 转文字 / 发邮件）。
    /// 这些都可能阻塞几秒到几十秒，绝不能占主循环——主循环一停，笔和手势全卡住。
    /// 结果在主循环空闲时由 `tick()` 收。
    job: Option<(Job, std::sync::mpsc::Receiver<Result<JobDone, String>>)>,
    /// 后台 Wi-Fi 扫描的收货端。
    ///
    /// `iw dev wlan0 scan` 是一次**主动扫描**：要逐信道发 probe、等回复，实测
    /// 1~3 秒。以前它是在 `tap_settings` 里同步跑的 —— 点一下 Wi-Fi 页，整个
    /// 主循环就停在那儿，这期间的触摸全排在队列里。用户看到的就是
    /// "从 Wi-Fi 切到 Battery 会卡一下"（其实是进 Wi-Fi 那次扫描还没完）。
    ///
    /// 现在扫描扔到后台线程，页面先用旧列表画出来，结果由 `tick()` 收。
    wifi_scan: Option<std::sync::mpsc::Receiver<Vec<wifi::Network>>>,
    // === C0W begin ===
    /// 后台连 Wi-Fi 的收货端：（正在连的 SSID, `connect()` 的结果）。
    ///
    /// `connect()` 要轮询到 `wpa_state=COMPLETED`，最长 20 s；和扫描一样必须
    /// 扔后台线程，否则主循环一停，笔和手势全卡住。
    wifi_conn: Option<(String, std::sync::mpsc::Receiver<Result<(), String>>)>,
    /// 起连接任务的时刻。看门狗：`connect()` 自己 20 s 封顶，超过 30 s 还没回
    /// 就是线程卡在某条 `wpa_cli` 上了 —— 不能让 UI 永远停在 `Connecting to...`。
    wifi_conn_at: std::time::Instant,
    /// Wi-Fi 页"当前网络"那一行的矩形（长按 → Forget 确认框）。`None` = 没画。
    wifi_cur_row: Option<(i32, i32, i32, i32)>,
    /// 打开连接面板前的那份设置页。面板是独立 `Screen`，关掉时原样放回去，
    /// 网络列表和开关状态都不用重读（重读会先闪一下空列表再等扫描）。
    wifi_back: Option<Box<SettingsView>>,
    // === C0W end ===
    /// 最近一次输入事件的时间，用来判断"用户手停了没有"
    last_ev: std::time::Instant,
    /// 电源键手势（短按 = 睡/醒，按住 3 s = 弹关机框）。语义在 `rmdev::power`。
    pg: rmdev::power::Gesture,
    /// 睡着的方式；`None` = 醒着。
    asleep: Option<rmdev::power::Mode>,
    /// 关机框开着（官方 PowerOffDialog，独占输入）。
    poweroff: bool,
    /// 唤醒后短暂不认电源键。
    ///
    /// 唤醒那一下的按下/抬起还排在 channel 里，等我们醒过来才被读到；
    /// 不挡掉就会被判成"又按了一下"，屏幕闪一下就又睡了。
    power_mute: Option<std::time::Instant>,
    /// 睡眠图（`/usr/share/remarkable/suspended.png`，2.6 MB 灰度）。
    /// `sleep_img_loaded` 区分"没加载过"和"加载过但失败"。
    sleep_img: Option<rmdev::doc::reflow::Img>,
    sleep_img_loaded: bool,
    /// 设置里的 Auto sleep：20 分钟无操作就睡（官方默认开）。
    auto_sleep: bool,
    // === SHARE begin ===
    /// 上一次把帧导出给屏幕共享的时刻（共享开着时每 1 s 刷一次）
    share_at: std::time::Instant,
    // === SHARE end ===
    // === P5 begin ===
    /// 设置里的 `Show battery percentage`：关掉后状态栏只画电池图标、不画 `NN%`。
    show_battery_pct: bool,
    /// 左右利手（官方 Handedness）。右手 = 工具条在左。
    right_handed: bool,
    /// 宽页自动进横屏（设置页可关）。
    auto_land: bool,
    /// 横屏往哪边转：`true` = 逆时针，充电口在左。
    land_dir_left: bool,
    // === P5 end ===
    // === BATT begin ===
    /// 过去 10 分钟的电流采样（`tick()` 每 30 s 塞一个），设置页用来算平均耗电。
    battery_usage: battery::Usage,
    /// 上一次采样时刻。`None` = 还没采过（进入主循环第一圈就采）。
    battery_sample_at: Option<std::time::Instant>,
    // === BATT end ===
    /// 上次把当前页写回磁盘的时刻（自动存盘用）
    last_save: std::time::Instant,
    /// 最近一次笔事件（含移动）的时刻：笔动过 250ms 内所有触摸一律丢掉。
    /// 写字时手掌一直压在屏上，官方也是"笔优先"。
    pen_ts: std::time::Instant,
    /// 这一轮手势有没有越过死区。没越过就整轮不动视图（一次点击会被识别成
    /// "捏合 1.011"，以前因此每次点空白都整页重绘一次 1.2 秒）。
    gesture_armed: bool,
    /// 死区里攒的量：dx, dy, 累计缩放比
    gesture_sum: (f32, f32, f32),
    /// 边缘羽化开关。官方没有这个选项，是我们自己加的；和 `last_tool` 一样
    /// 跟着进程走（换文档不丢）。
    feather: bool,
    /// 上次用的书写工具下标。官方每个文档记 `lastUsedTool`，我们的文档状态没有
    /// 落盘，至少做到"关掉再打开还是刚才那支笔"——否则选了书法笔、看完别处回来，
    /// 笔又变回圆珠笔，看着就像"书法笔没生效"。
    last_tool: usize,
    /// 上次用的橡皮是哪一档（`true` = 擦整笔）。
    ///
    /// 和 `last_tool` 是同一类记忆，但这一份**落盘**
    /// （`prefs.last_eraser_stroke`）：在橡皮菜单里选过"Erase stroke"之后，
    /// 切到笔再切回橡皮、或者重开一本笔记，都还是"擦整笔"，而不是每次
    /// 都被重置回刷白橡皮。
    last_erase_stroke: bool,
    /// 笔尾橡皮（笔倒过来那一端）该当哪种橡皮用，来自设置页 Accessibility。
    /// 这是 `prefs.tail_eraser` 的内存副本，改设置时同步。
    tail_eraser: prefs::TailEraser,
    /// 下一次 `draw_settings` 只是"控件状态变了"（翻了个开关），不是整页换内容。
    ///
    /// 置上以后设置页走 **A2 + PARTIAL**：驱动会把整屏矩形缩成"真正变了的那几块"
    /// （一个开关约 130×50），A2 只要 12 相位 ≈ 141 ms；而整页重绘走 46 相位。
    /// 代价是这条路径只画得动纯黑/纯白，所以开关的胶囊/圆点必须用 0 画
    /// （见 `set_toggle_full`），不能用 `tone 20`。
    settings_fast: bool,
    text: Option<Text>,
    library: Library,
    screen: Screen,
    pixel_scale: f32,
    library_rows: usize,
    /// 当前过滤视图 / 排序 / 排序下拉是否展开
    view: View,
    sort: Sort,
    sort_open: bool,
    /// 官方的 `Settings.listMode`：true = 列表，false = 网格。落盘在 `prefs_path()`。
    list_mode: bool,
    /// 本帧绘制出来的卡片列表（点击时按索引取）
    visible: Vec<Book>,
    /// 可点区域
    hits: Hits,
    /// 键盘的可点区域
    kb_hits: Vec<((i32, i32, i32, i32), Key)>,
    /// 当前打开的电子书（PDF/EPUB/TXT/MOBI…）。换文档才重建，翻页/重绘
    /// 复用同一份排版缓存。UI 只认 `doc::Doc` 这一层接口。
    doc: Option<Box<dyn rmdev::doc::Doc>>,
    /// 当前打开的是哪一本
    doc_uuid: Option<String>,
    /// 当前文档的排版参数（字号/行距/边距）。重排格式改了要重排 + 重绘。
    reading_cfg: rmdev::doc::LayoutCfg,
    /// 底图版本号：每换一次 `pdf_bg` 就 +1。
    ///
    /// 页面光栅缓存（`page_fingerprint`）原来只看墨迹，改字号/换页之后底图变了
    /// 但墨迹没变，缓存会命中旧图——所以底图必须参与指纹。
    bg_gen: u64,
    /// 电子书的真实页数（排版后才知道，画缩略图时顺手记下，卡片副标题要用）
    epub_pages: std::collections::HashMap<String, usize>,
    /// PDF 页渲染出来的底图（灰度 1404×1872）
    pdf_bg: Option<Vec<u8>>,
    /// 按下但还没抬起：记位置和时间，用来判断长按
    press: Option<(f32, f32, std::time::Instant)>,
    /// 长按菜单当前针对哪一项
    ctx: Option<Book>,
    /// 菜单刚弹出来的时刻：这之后的极短时间里忽略点按，
    /// 否则触发长按的那一下会被当成"点菜单外面"立刻把菜单关掉。
    ctx_at: Option<std::time::Instant>,
    /// 库里按下但还没抬笔的位置
    pending_tap: Option<(f32, f32)>,
    /// 左下角连点计数（库里也用它退出）
    exit_taps: u32,
    last_flush: std::time::Instant,
    /// 抬笔/切页时把攒着的脏区立刻推掉
    force_flush: bool,
    /// 侧栏抽屉是否展开
    menu_open: bool,
    tiles: Vec<(String, i32, i32, i32, i32)>,
    sidebar_items: Vec<(String, i32, i32, i32, i32)>,
    /// 页面光栅缓存：`(uuid, 页号) → (内容指纹, 墨迹缓冲)`。
    ///
    /// 每次 `redraw_page` 都会把所有笔迹重画一遍（Quick sheets 292 笔），设备上
    /// 几百毫秒。可页面内容其实没变——改个工具、开个菜单都重画一遍是纯浪费。
    /// 指纹覆盖"笔数/工具/点数/末点/硬度/缩放"，任何一条变了就重画。
    /// 只留最近 4 页（每页 2.6 MB）。
    raster: Vec<((String, usize), u64, Vec<u8>)>,
    /// 页面总览的缩略图缓存：`(uuid, 页号) → (指纹, 灰度图)`。
    /// 总览一开就要把每页都光栅化一遍，40 页的笔记要一秒多；开第二次就该是瞬时的。
    page_thumbs_cache: Vec<((String, usize), u64, Vec<u8>)>,
    /// 卡片缩略图缓存。用 `Rc` 是为了重绘时只拷指针：每张 242 KB，
    /// 6 张卡一次重绘就白拷 1.4 MB。
    thumbs: std::collections::HashMap<String, std::rc::Rc<Vec<u8>>>,
    banner: Option<String>,
    /// 最近一次手指落点——直接画在屏幕上，用来判断触摸事件到底有没有进来
    last_touch: Option<(f32, f32, u32)>,
    /// 库内容向上滚了多少像素（0 = 第一行贴住标题下沿）
    scroll: i32,
    /// 库里竖向拖动：(按下 x, 按下 y, 当前 x, 当前 y)
    drag: Option<(f32, f32, f32, f32)>,
    /// 当前所在的文件夹路径（从顶层往下），空 = 库根目录
    trail: Vec<(String, String)>,
    /// 按标签过滤（点标签进入；切视图/回根目录会清掉）
    tag_filter: Option<String>,
    /// 图层面板里画出来的可点区域
    layer_hits: Vec<((i32, i32, i32, i32), LayerAct)>,
    /// 上一次点图层名的 (层下标, 时刻)：400ms 内点同一层 = 双击改名
    layer_tap: Option<(usize, std::time::Instant)>,
    /// 分享面板画出来的可点区域
    share_hits: Vec<((i32, i32, i32, i32), u32)>,
    /// 标签面板画出来的可点区域
    tag_hits: Vec<((i32, i32, i32, i32), u32)>,
    /// 选区剪贴板。放在 App 而不是 Notebook：翻页、切文档都不该丢（和官方一致）。
    clipboard: Vec<Line>,
    /// 套索/选区浮层的待刷新区域（屏幕坐标）。
    /// 这些是直接画进帧缓冲的，不走 canvas，所以增量刷新得自己记账。
    sel_dirty: Option<(i32, i32, i32, i32)>,
    /// 上一次整屏刷新的时刻（限速用）
    last_full: Option<std::time::Instant>,
    /// 长按已经在 tick 里处理过了，紧随其后的抬笔要忽略一次
    skip_up: bool,
    /// 双指手势进行中：这期间的 Move 不翻页/不滚动，抬指也不结算点击
    multi_touch: bool,
    /// 这一轮手势改过视图：等手指全抬起来再整页重绘（EPDC 全屏刷新很贵）
    gesture_moved: bool,
    /// 侧栏 Filters 是否展开（Notebooks / PDFs / Ebooks）
    filters_open: bool,
    /// 回收站里选中的条目（官方：点卡片是多选，不是直接恢复）
    trash_sel: Vec<String>,
    // === P8 begin ===
    /// 库里多选的选中集合（BTreeSet 天然去重且按 uuid 有序）。空 = 不在选择态。
    lib_sel: std::collections::BTreeSet<String>,
    /// Move 动作进行中：库变成"选目标文件夹"模式（官方 `content.moving`）
    lib_moving: bool,
    /// Tag 动作的输入键盘（None = 没开）
    lib_tag: Option<Keyboard>,
    // === P8 end ===
    /// 待确认永久删除的 uuid
    trash_confirm: Option<Vec<String>>,
    /// 文档设置面板（左侧工具条最下面那格）
    ds: DocSettings,
    /// 文档内搜索（文档设置 → Search）：顶部搜索条 + 键盘 + 页面高亮
    search: DocSearch,
    /// Notebook settings 页（文档设置 → Notebook settings）：整屏设置页
    nbset: NbSettings,
    /// Text settings 页（电子书文档设置 → Text size）：整屏 sheet
    tcfg: TextCfg,
    /// Table of contents 面板（页总览顶部那一行 `Table of contents`）：整页目录
    toc: TocPanel,
    /// TOC 面板上的按下点 + 这一轮拖动过没有（拖动过就不算点击）
    toc_press: Option<(f32, f32, bool)>,
    /// 页总览多选（长按任意页进入）
    pagesel: PageSel,
    /// 多选 → Move 的二级页：目标页号的数字输入（键盘贴底，None = 没开）
    move_to: Option<Keyboard>,
    // === P1 begin ===
    /// USB web 文件服务（`src/webui.rs`）。`Some` = 正在 `10.11.99.1:80` 上听。
    usb_web: Option<rmdev::webui::Handle>,
    /// 设置页 `USB web interface` 开关的内存副本（`prefs.usb_web` 的镜像）。
    usb_web_on: bool,
    /// 上次核对「USB 网卡在不在 / 服务该不该跑」的时刻（每 5 s 一次）。
    usb_web_check: std::time::Instant,
    /// 上次打过的状态日志：状态没变就不重复刷屏（绑端口失败会每 5 s 重试）。
    usb_web_note: Option<String>,
    // === P1 end ===
    // === P10 begin ===
    /// 手写转文字整页流程的状态（选页 / 逐页 OCR / 结果 / Undo）
    conv: Conv,
    // === P10 end ===
    /// Tags 视图的状态（tab / 选中的胶囊 / 弹层），绘制在 tagsview.rs
    tags: tagsview::TagsView,
    // === P3 begin ===
    /// 状态栏通知队列（官方 `res_06b6382.qml` 的 current/queue）。
    notices: NoticeQueue,
    /// 从状态栏图标跳设置页时先记下要开的 tab，`open_settings()` 画第一帧前读走。
    pending_settings_tab: Option<usize>,
    // === P3 end ===
    // === P4 begin ===
    /// 磁盘水位（官方 `res_0686a15.qml` 的 `enoughFreeSpaceForCreating`）：
    /// Full = 官方的只读模式，`library` 的所有写入口都会拒绝。
    disk_level: disk::Level,
    /// 上次重算磁盘水位的时刻；`None` = 还没算过（启动后第一次 `tick()` 一定算）。
    disk_checked: Option<std::time::Instant>,
    // === P4 end ===
    // === P9 begin ===
    /// 页总览的排布：网格 / 列表（官方顶栏 `Grid view` / `List view`）。
    /// 跨文档保留，像官方那样是个"记住的偏好"。
    page_view: pagesel::View,
    /// 页总览按页标签筛选（点胶囊选中；`None` = 不过滤）
    overview_tag: Option<String>,
    /// `Go to page` 的页码输入（键盘贴底，None = 没开）
    goto_page: Option<Keyboard>,
    // === P9 end ===
    // === P7 begin ===
    /// 文档页滑块：(最后一次触摸的时刻, 目标页 0-based, 状态)。
    ///
    /// 官方 `pageSlider` 是 `visible` + `pressed` + `pageThumbnailVisible` 三个
    /// 布尔 + 一个 `hidePageSliderTimer`；这里并成一个 `Option`：`None` = 收起，
    /// `Some` = 条浮着（`State::Idle` 等 `HIDE_MS` 到点自动收）。时刻在每次
    /// 按下 / 拖动 / 松手时刷新，等价于官方 `timer.restart()`。
    slider: Option<(std::time::Instant, usize, slider::State)>,
    /// 在底部那条上按下、还没滑够 [`slider::OPEN_UP`] 时的锚点 `(x, y)`。
    ///
    /// 官方 `fastScroll` 是"从底边往上滑"才拉出滑块，横着划是翻页 ——
    /// 所以按下的第一下只记锚点，真往上滑了才进滑块态。
    p7_arm: Option<(f32, f32)>,
    // === P7 end ===
    // === P12 begin ===
    /// 库视图顶部的下拉抽屉（官方 `res_06b82ab.qml` 的 `DocumentDrawer`）
    drawer: Drawer,
    // === P12 end ===
    // === E1 begin ===
    /// 错题档案整屏列表（侧栏 `Mistakes` 打开；网络全在它自己的后台线程里）
    mistakes: MistakesPanel,
    // === E1 end ===
    // === D1 begin ===
    /// 框选问 AI 的工具态/浮层/矩形/提问键盘/上传（`panels::selectai`）
    selai: rmdev::panels::selectai::Panel,
    /// 内联回答卡片（`panels::ai` 的内联模式；整屏 sheet 那条路照旧）
    ask_ai: rmdev::panels::ai::Ai,
    // === D1 end ===
    // === C15 begin ===
    /// 自动加页：当前打开的可绘制文档的 uuid。`Some` = 这本在页序列末尾挂着一张
    /// 虚拟空白页（没有 page id、不进 `.content`、导出/上传都看不到）。
    c15_virt: Option<String>,
    /// 刚由 ⊕ Quick sheets 新建、还没「转正」的空本：离开时一个字没写就整本删掉。
    quick_new: Option<String>,
    /// 当前文档的 AI 引用索引（`<uuid>.airefs.json`，服务器发布时写）。
    ai_refs: rmdev::airefs::Refs,
    /// `View` 弹层：(引用, 弹层矩形)。手指长按框/角标才有。
    ai_view: Option<(rmdev::airefs::Mark, (i32, i32, i32, i32))>,
    /// 手指按在 AI 框上、还没到长按时长：(x, y, 按下时刻, 命中的引用)。
    ai_hold: Option<(f32, f32, std::time::Instant, rmdev::airefs::Mark)>,
    /// 最后一次手指位置（`TouchEvent::Up` 不带坐标，点 `View` 要用）。
    ai_last: Option<(f32, f32)>,
    /// `View` 刚由长按弹出：紧接着的那次抬指要吞掉（否则一松手就把弹层关了）。
    ai_view_fresh: bool,
    /// 触控锁（默认开）：编辑时手指只认左边工具栏 / 右上角退出 / 横向翻页。
    touch_lock: bool,
    /// 触控锁下正在跟踪的一次手指滑动 `(x0, y0, x1, y1)`。
    tlock: Option<(f32, f32, f32, f32)>,
    /// 按在右上角锁图标上（抬起时切换锁）。
    lock_press: bool,
    // === C15 end ===
}

/// 套索的一段：1px 黑线。增量刷新时一段一段往帧缓冲上补。
fn draw_lasso_seg(fb: &mut Fb, a: (f32, f32), b: (f32, f32)) {
    widgets::line(fb, a.0 as i32, a.1 as i32, b.0 as i32, b.1 as i32, 1, 0);
}

/// 2px 虚线框：段长 10、间隔 8。选框四边都从同一个起点算起，四个角的虚线
/// 不会因为边长不同而错位。
/// 引用菜单的箭头：**从框里面**往左上角顶点画，尾巴上挂编号。
///
/// 为什么从里面画：框贴着页面左上角时，往外画的箭头会跑到屏幕外/工具栏底下
/// （第一次上机就是这样，箭头完全看不见）。往内画只要框在屏幕里就一定能看见。
/// 返回编号方块的位置，由调用方把数字画上去。
fn ai_arrow(fb: &mut Fb, x: i32, y: i32, w: i32, h: i32, n: u32) -> (i32, i32) {
    let c = gray565(20);
    // 长度不能超过框的一半，小框里也不会顶到框线外
    let len = 78.min(w / 2).min(h / 2).max(24);
    // 斜线：从 (x+len, y+len) 指向 (x+8, y+8)
    for i in 0..len {
        fb.fill_rect(x + 8 + i, y + 8 + i, 4, 4, c);
    }
    // 箭头两撇（尖头在左上）
    for i in 0..16 {
        fb.fill_rect(x + 8 + i, y + 8 + i / 2, 3, 3, c);
        fb.fill_rect(x + 8 + i / 2, y + 8 + i, 3, 3, c);
    }
    // 尾巴上挂编号：白底黑框小方块，位置贴着斜线末端
    let (bx, by) = (x + 6 + len, y + 6 + len);
    fb.fill_rect(bx, by, 42, 36, gray565(255));
    rect_border(fb, bx, by, 42, 36, 2, 20);
    let _ = n;
    (bx, by)
}

/// 菜单行要显示的"回答第一行"。
///
/// 服务器发布时把回答的第一行写进 `title`；老索引没这个字段就退回 `AI n`
/// （不编造内容）。超宽按字符砍，保证一行放得下。
fn ai_first_line(m: &rmdev::airefs::Mark, t: &mut rmdev::ui::text::Text, max_w: i32) -> String {
    let raw = m.title.trim();
    let raw = if raw.is_empty() {
        format!("AI {}", if m.n == 0 { 1 } else { m.n })
    } else {
        raw.to_string()
    };
    if t.width(28.0, &raw) <= max_w as f32 {
        return raw;
    }
    let mut out = String::new();
    for ch in raw.chars() {
        let mut probe = out.clone();
        probe.push(ch);
        probe.push('…');
        if t.width(28.0, &probe) > max_w as f32 {
            break;
        }
        out.push(ch);
    }
    out.push('…');
    out
}

fn dashed_rect(fb: &mut Fb, r: RectF, t: i32, tone: u8) {
    const ON: i32 = 10;
    const OFF: i32 = 8;
    let c = gray565(tone);
    let (x0, y0) = (r.x as i32, r.y as i32);
    let (w, h) = (r.w as i32, r.h as i32);
    let (x1, y1) = (x0 + w - t, y0 + h - t);
    let mut i = 0;
    while i < w {
        let seg = (w - i).min(ON);
        fb.fill_rect(x0 + i, y0, seg, t, c);
        fb.fill_rect(x0 + i, y1, seg, t, c);
        i += ON + OFF;
    }
    let mut i = 0;
    while i < h {
        let seg = (h - i).min(ON);
        fb.fill_rect(x0, y0 + i, t, seg, c);
        fb.fill_rect(x1, y0 + i, t, seg, c);
        i += ON + OFF;
    }
}

/// Ebooks 图标（官方 `Icons.icon_ebook` 的近似：合起来的书 + 右侧书页线）。
fn ebook_icon(fb: &mut Fb, x: i32, y: i32) {
    let c = gray565(20);
    fb.fill_rect(x, y + 2, 4, 36, c);
    fb.fill_rect(x + 4, y, 32, 3, c);
    fb.fill_rect(x + 4, y + 37, 32, 3, c);
    fb.fill_rect(x + 33, y, 3, 40, c);
    fb.fill_rect(x + 10, y + 10, 18, 3, c);
    fb.fill_rect(x + 10, y + 18, 18, 3, c);
}

// ---------------------------------------------------------------- 搜索结果行图标

/// 搜索结果左边的图标（官方 `Icons.icon_document`，40×40、描边 4）。
///
/// 形状照 `40-search.png` 第一行逐像素量：页面左边 x..x+3、底边 y+36..y+39、
/// 右边 x+36..x+39（从 y+9 起），上边只画到 x+31，右上角是折角：
/// 折角左竖线在 x+26..x+29，折角下横线 y+10..y+13 一直拉到右边。
fn search_icon_doc(fb: &mut Fb, x: i32, y: i32) {
    let c = gray565(20);
    fb.fill_rect(x, y, 4, 40, c);
    fb.fill_rect(x, y + 36, 40, 4, c);
    // 上边：每往下一行右端多 1px（就是右上角的斜切口）
    for j in 0..4 {
        fb.fill_rect(x, y + j, 32 + j, 1, c);
    }
    // 斜切口本身：5px 宽的斜带，从 (31,4) 走到 (35,8)
    for j in 0..5 {
        fb.fill_rect(x + 31 + j, y + 4 + j, 5, 1, c);
    }
    fb.fill_rect(x + 36, y + 8, 4, 32, c);
    fb.fill_rect(x + 26, y, 4, 14, c);
    fb.fill_rect(x + 26, y + 10, 14, 4, c);
}

/// 笔记本图标（官方 `Icons.icon_notebook`）：36×40 的方形描边 + 左侧三个装订齿。
fn search_icon_notebook(fb: &mut Fb, x: i32, y: i32) {
    let c = gray565(20);
    fb.fill_rect(x + 4, y, 36, 4, c);
    fb.fill_rect(x + 4, y + 36, 36, 4, c);
    fb.fill_rect(x + 4, y, 4, 40, c);
    fb.fill_rect(x + 36, y, 4, 40, c);
    for i in 0..3 {
        fb.fill_rect(x, y + 8 + i * 10, 10, 4, c);
    }
}

/// 文件夹图标（官方 `Icons.icon_folder`；搜索页截图里没有文件夹行，按同组
/// 描边图标的画法拼：左上角一个标签 + 主体）。40×40。
fn search_icon_folder(fb: &mut Fb, x: i32, y: i32) {
    let c = gray565(20);
    fb.fill_rect(x + 2, y + 8, 20, 3, c);
    fb.fill_rect(x + 2, y + 8, 3, 8, c);
    fb.fill_rect(x + 19, y + 8, 3, 10, c);
    widgets::outline_rect(fb, x + 2, y + 13, 36, 23, 3, 20);
}

/// 结果行第二行右半：官方 `Values.entrySubtext(entry)`。
/// 文件夹没有副文案；PDF 是上次翻到第几页，笔记本是总页数。
fn search_subtext(b: &Book) -> String {
    if b.doc_type == "CollectionType" {
        return String::new();
    }
    if (b.format == rmdev::doc::Format::Pdf) {
        format!("Page {} of {}", b.last_opened_page + 1, b.page_count)
    } else if b.format.is_ebook() {
        String::new()
    } else if b.page_count == 1 {
        "1 page".to_string()
    } else {
        format!("{} pages", b.page_count)
    }
}

// === P7 begin ===
/// 最近邻 2× 放大（页总览的缩略图是 306×392，预览框是官方的 615×820：
/// 放大到 612×784 正好塞得进去，差那 1~2 px 被预览框的 2 px 黑边压住）。
///
/// 拖动时每帧都要贴一次，所以只做最笨的整数复制 —— 480k 像素在设备上 ~3 ms，
/// 比"现渲一页缩略图"（几百毫秒）便宜两个数量级。
fn p7_scale2(src: &[u8], w: usize, h: usize) -> Vec<u8> {
    let (dw, dh) = (w * 2, h * 2);
    let mut out = vec![0u8; dw * dh];
    for y in 0..h {
        let srow = &src[y * w..(y + 1) * w];
        for dy in 0..2 {
            let base = (y * 2 + dy) * dw;
            let drow = &mut out[base..base + dw];
            for x in 0..w {
                drow[x * 2] = srow[x];
                drow[x * 2 + 1] = srow[x];
            }
        }
    }
    out
}
// === P7 end ===

/// 开机加载条：白底 + 居中一条进度条。
///
/// **不写任何文字**：e-ink 上"Starting…"这类文案官方没有对应的原话，写上去就
/// 成了自造 UI 字符串。一根条子足够表达"在启动、还没好"。
///
/// 为什么要它：`App::blank` 要读文档库、加载字体、生成封面，冷启动头两秒屏上
/// 本来是白的，用户完全分不清"在启动"还是"挂了"。这条 8%→70%→100% 的推进
/// 就是那个反馈。
#[allow(dead_code)] // 见 `main` 里的注释：等 framegen 队列那个 bug 查清再接上
fn boot_progress(fb: &mut Fb, pct: u32) {
    let (w, h) = (fb.width, fb.height);
    let (bw, bh) = (720i32, 16i32);
    let x = (w - bw) / 2;
    let y = h / 2 - bh / 2;
    fb.fill_rect(0, 0, w, h, gray565(255));
    widgets::outline_rect(fb, x - 3, y - 3, bw + 6, bh + 6, 2, 0);
    let filled = (bw as u32 * pct.min(100) / 100) as i32;
    if filled > 0 {
        fb.fill_rect(x, y, filled, bh, gray565(0));
    }
    fb.refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
}

impl App {
    fn new(mut fb: Fb) -> App {
        // 横屏往哪边转是偏好：必须在**画第一帧之前**设好，否则首屏是反的
        fb.set_land_dir_left(prefs::load().land_dir_left);
        let mut app = App::blank(fb);
        // === P4 begin ===
        // 启动算一次磁盘水位：满了先切只读，第一帧库界面就带官方提示条。
        app.p4_check_disk();
        // === P4 end ===
        app.draw_library();
        app
    }

    /// 开屏就有密码：直接画锁屏，别先把库刷到面板上再闪一下。
    fn new_locked(mut fb: Fb) -> App {
        fb.set_land_dir_left(prefs::load().land_dir_left);
        let mut app = App::blank(fb);
        app.screen = Screen::Lock(Box::new(LockView {
            entered: String::new(),
            msg: String::new(),
            lockout: security::Lockout::default(),
            shown_secs: 0,
            show_digits: false,
        }));
        app.draw_lock();
        app
    }

    fn blank(fb: Fb) -> App {
        // 启动时读一次偏好：无障碍字号（官方也是重启后生效）、橡皮记忆、笔尾映射。
        // 从没写过 settings 文件时不放大：官方截图里 Large 高亮、字号仍是正常大小。
        let pref = prefs::load();
        set_font_scale(if pref.font_large { FONT_SCALE_LARGE } else { 100 });
        // 左利手 = 工具条换到右边（整块 UI 左右对调）。和字号不同，官方是**立刻**
        // 生效的，所以设置页里点一下还会再调一次 `set_mirrored`。
        rmdev::fb::set_mirrored(!pref.right_handed);
        eprintln!(
            "[dev] 笔尾橡皮 = {}；上一次橡皮 = {}",
            match pref.tail_eraser {
                prefs::TailEraser::Eraser => "刷白",
                prefs::TailEraser::Stroke => "擦整笔",
                prefs::TailEraser::Last => "跟上次",
            },
            if load_last_erase_stroke(&prefs_path()) { "擦整笔" } else { "刷白" }
        );
        let w = fb.width as usize;
        let h = fb.height as usize;
        App {
            fb,
            canvas: Canvas::new(w, h),
            net_last: net_state(),
            ai_menu: false,
            land_back: None,
            page_box: None,
            land_manual: None,
            ai_menu_fresh: false,
            // 开机先给 15 s：面板 INIT + 首帧都在这段时间里，别和它抢网络
            sync_auto_at: std::time::Instant::now() + Duration::from_secs(15),
            sync_is_auto: false,
            sync_quiet: 0,
            sync_skip_idle: false,
            jobs: rmdev::aijobs::Jobs::load(),
            jobs_open: false,
            ai_job: None,
            last_tb: None,
            prefetch_job: None,
            last_ev: std::time::Instant::now(),
            pg: rmdev::power::Gesture::new(),
            asleep: None,
            poweroff: false,
            power_mute: None,
            sleep_img: None,
            sleep_img_loaded: false,
            auto_sleep: pref.auto_sleep,
            // === SHARE begin ===
            share_at: std::time::Instant::now(),
            // === SHARE end ===
            // === P5 begin ===
            show_battery_pct: pref.battery_percentage,
            right_handed: pref.right_handed,
            auto_land: pref.auto_landscape,
            land_dir_left: pref.land_dir_left,
            // === P5 end ===
            // === BATT begin ===
            battery_usage: battery::Usage::new(),
            battery_sample_at: None,
            // === BATT end ===
            last_save: std::time::Instant::now(),
            pen_ts: std::time::Instant::now() - Duration::from_secs(1),
            gesture_armed: false,
            gesture_sum: (0.0, 0.0, 1.0),
            feather: true,
            last_tool: 0,
            last_erase_stroke: load_last_erase_stroke(&prefs_path()),
            tail_eraser: pref.tail_eraser,
            settings_fast: false,
            text: Text::load(),
            library: Library::new(),
            screen: Screen::Library,
            pixel_scale: pixel_width_scale(),
            library_rows: 0,
            view: View::MyFiles,
            sort: Sort::Updated,
            sort_open: false,
            list_mode: load_list_mode(&prefs_path()),
            visible: Vec::new(),
            hits: Hits::new(),
            kb_hits: Vec::new(),
            doc: None,
            doc_uuid: None,
            reading_cfg: rmdev::doc::LayoutCfg::default(),
            bg_gen: 0,
            epub_pages: std::collections::HashMap::new(),
            pdf_bg: None,
            press: None,
            ctx: None,
            ctx_at: None,
            pending_tap: None,
            exit_taps: 0,
            last_flush: std::time::Instant::now(),
            force_flush: false,
            menu_open: false,
            tiles: Vec::new(),
            sidebar_items: Vec::new(),
            thumbs: std::collections::HashMap::new(),
            banner: None,
            last_touch: None,
            scroll: 0,
            drag: None,
            layer_hits: Vec::new(),
            layer_tap: None,
            share_hits: Vec::new(),
            tag_hits: Vec::new(),
            clipboard: Vec::new(),
            sel_dirty: None,
            last_full: None,
            skip_up: false,
            multi_touch: false,
            gesture_moved: false,
            ds: DocSettings::new(),
            search: DocSearch::new(),
            nbset: NbSettings::new(),
            tcfg: TextCfg::new(),
            toc: TocPanel::new(),
            toc_press: None,
            pagesel: PageSel::new(),
            move_to: None,
            // === P10 begin ===
            conv: Conv::new(),
            // === P10 end ===
            filters_open: false,
            trash_sel: Vec::new(),
            // === P8 begin ===
            lib_sel: std::collections::BTreeSet::new(),
            lib_moving: false,
            lib_tag: None,
            // === P8 end ===
            trash_confirm: None,
            trail: Vec::new(),
            tag_filter: None,
            job: None,
            wifi_scan: None,
            // === C0W begin ===
            wifi_conn: None,
            wifi_conn_at: std::time::Instant::now(),
            wifi_cur_row: None,
            wifi_back: None,
            // === C0W end ===
            raster: Vec::new(),
            page_thumbs_cache: Vec::new(),
            // === P1 begin ===
            usb_web: None,
            usb_web_on: prefs::load_opt().map(|p| p.usb_web).unwrap_or(true),
            // 起点回退 5 s：第一轮 tick 就会去核对一次，USB 已经插着就立刻起来。
            usb_web_check: std::time::Instant::now() - Duration::from_secs(5),
            usb_web_note: None,
            // === P1 end ===
            tags: tagsview::TagsView::default(),
            // === P3 begin ===
            notices: NoticeQueue::new(),
            pending_settings_tab: None,
            // === P3 end ===
            // === P4 begin ===
            // 先按"正常"占位；`App::new` 在画第一帧前算真实水位。
            disk_level: disk::Level::Ok,
            disk_checked: None,
            // === P4 end ===
            // === P9 begin ===
            page_view: pagesel::View::Grid,
            overview_tag: None,
            goto_page: None,
            // === P9 end ===
            // === P7 begin ===
            slider: None,
            p7_arm: None,
            // === P7 end ===
            // === P12 begin ===
            drawer: Drawer::new(),
            // === P12 end ===
            // === E1 begin ===
            mistakes: MistakesPanel::new(),
            // === E1 end ===
            // === D1 begin ===
            selai: rmdev::panels::selectai::Panel::new(),
            ask_ai: rmdev::panels::ai::Ai::new(),
            // === D1 end ===
            // === C15 begin ===
            c15_virt: None,
            quick_new: None,
            ai_refs: rmdev::airefs::Refs::default(),
            ai_view: None,
            ai_hold: None,
            ai_last: None,
            ai_view_fresh: false,
            touch_lock: true, // 用户 2026-11-09：默认锁定（掌心误触太严重）
            tlock: None,
            lock_press: false,
            // === C15 end ===
        }
    }

    // ------------------------------------------------------------ 库

    /// 当前文件夹 uuid（空串 = 根目录）
    fn cwd(&self) -> String {
        self.trail.last().map(|(u, _)| u.clone()).unwrap_or_default()
    }

    /// 进入某个文件夹
    fn enter_folder(&mut self, uuid: &str, name: &str) {
        self.trail.push((uuid.to_string(), name.to_string()));
        self.scroll = 0;
        eprintln!("[ui] 进入文件夹 {name}");
        self.draw_library();
    }

    /// 侧栏目的地。官方的这几个大多只是**同一个库的过滤视图**，不是独立页面。
    fn view_books(&self) -> Vec<Book> {
        let all = match &self.tag_filter {
            Some(t) => self.library.tagged(t),
            None => self.library.entries(&self.cwd(), self.view == View::Trash),
        };
        let mut v: Vec<Book> = all
            .into_iter()
            .filter(|b| match self.view {
                View::Favorites => b.doc_type == "DocumentType" && b.pinned,
                View::Pdfs => b.doc_type == "DocumentType" && (b.format == rmdev::doc::Format::Pdf),
                View::Ebooks => b.doc_type == "DocumentType" && b.format.is_ebook(),
                View::Notebooks => {
                    b.doc_type == "DocumentType" && !(b.format == rmdev::doc::Format::Pdf) && !b.format.is_ebook()
                }
                _ => b.doc_type == "DocumentType" || b.doc_type == "CollectionType",
            })
            .collect();
        match self.sort {
            Sort::Alphabetical => v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase())),
            Sort::PageCount => v.sort_by(|a, b| b.page_count.cmp(&a.page_count)),
            Sort::FileSize => v.sort_by(|a, b| b.size.cmp(&a.size)),
            Sort::RecentlyOpened => v.sort_by(|a, b| b.last_opened.cmp(&a.last_opened)),
            Sort::Updated => {}
        }
        v
    }

    /// 整页缩略图（406×541，按宽度等比缩放，不裁）。
    fn thumb(&mut self, uuid: &str) -> std::rc::Rc<Vec<u8>> {
        if let Some(t) = self.thumbs.get(uuid) {
            return t.clone();
        }
        const TW: usize = 426;
        const TH: usize = 568;
        if let Some(b) = self.library.find(uuid) {
            // === P6 begin ===
            // 电子书没有官方 `<uuid>.thumbnails/*.png`：封面走后台渲染 + 内存缓存。
            // 没渲好之前画白页（官方空态），千万不要在这里同步打开整本书——
            // `draw_library` 每滚一帧都会进来，排版几百毫秒会把列表卡住。
            // 复用手上这次 `find`，不要为判断格式再扫一遍 metadata。
            if b.format.is_ebook() {
                return match self.p6_ebook_cover(&b) {
                    Some(c) => c,
                    None => p6_empty_cover(),
                };
            }
            // === P6 end ===
            // 电子书：拿第一页当封面（官方也是渲染首页；没有封面图时至少不是白卡）
            if b.format.is_ebook() {
                let path = self.document_path(&b);
                let mut out = vec![255u8; TW * TH];
                if let Ok(book) = rmdev::doc::open(path.to_str().unwrap_or("")) {
                    let n = book.pages();
                    self.epub_pages.insert(uuid.to_string(), n);
                    if n > 0 {
                        let idx = (b.last_opened_page as usize).min(n - 1);
                        let asp = book.aspect(idx).clamp(0.05, 20.0);
                        let mut th = TH;
                        let mut tw = ((TH as f32) * asp).round() as usize;
                        if tw > TW {
                            tw = TW;
                            th = ((TW as f32) / asp).round() as usize;
                        }
                        if let Ok(g) = book.render(idx, tw, th) {
                            let ox = (TW - tw) / 2;
                            let oy = (TH - th.min(TH)) / 2;
                            for y in 0..th.min(TH) {
                                let src = y * tw;
                                let dst = (oy + y) * TW + ox;
                                out[dst..dst + tw].copy_from_slice(&g[src..src + tw]);
                            }
                        }
                    }
                }
                let rc = std::rc::Rc::new(out);
                self.thumbs.insert(uuid.to_string(), rc.clone());
                return rc;
            }
            if (b.format == rmdev::doc::Format::Pdf) {
                let path = self.library.root.join(format!("{uuid}.pdf"));
                let mut out = vec![255u8; TW * TH];
                if let Ok(pdf) = Pdf::open(path.to_str().unwrap_or("")) {
                    let idx = b.last_opened_page as usize;
                    let asp = pdf.aspect(idx).max(0.1);
                    let mut th = TH;
                    let mut tw = ((TH as f32) / asp).round() as usize;
                    if tw > TW {
                        tw = TW;
                        th = ((TW as f32) * asp).round() as usize;
                    }
                    if let Ok(g) = pdf.render(idx, tw, th) {
                        let mut img = vec![255u8; TW * TH];
                        let ox = (TW - tw) / 2;
                        let oy = (TH - th.min(TH)) / 2;
                        for y in 0..th.min(TH) {
                            let src = y * tw;
                            let dst = (oy + y) * TW + ox;
                            img[dst..dst + tw].copy_from_slice(&g[src..src + tw]);
                        }
                        out = img;
                    }
                }
                let rc = std::rc::Rc::new(out);
                self.thumbs.insert(uuid.to_string(), rc.clone());
                return rc;
            }
        }
        let scale = TW as f32 / 1404.0;
        let page = self.library.load_page(uuid);
        let grade = rmdev::pen::pencil::DEFAULT;
        let mut c = Canvas::new(TW, TH);
        for l in page.visible_lines() {
            c.ink_line(l, scale, (PAGE_DX * scale, PAGE_DY * scale), grade);
        }
        let mut out = vec![0u8; TW * TH];
        for y in 0..TH {
            let row = &mut out[y * TW..(y + 1) * TW];
            for (x, px) in row.iter_mut().enumerate() {
                *px = 255 - c.dark_at(x, y);
            }
        }
        let rc = std::rc::Rc::new(out);
        self.thumbs.insert(uuid.to_string(), rc.clone());
        rc
    }

    /// 卡片/行副标题。PDF 显示"第几页"，电子书显示真实页数（要排版后才知道），
    /// 其余显示总页数。集合（文件夹）没有副标题。
    fn card_meta(&self, b: &Book) -> String {
        if b.doc_type == "CollectionType" {
            return String::new();
        }
        if (b.format == rmdev::doc::Format::Pdf) {
            return format!("Page {} of {}", b.last_opened_page + 1, b.page_count);
        }
        if b.format.is_ebook() {
            return match self.epub_pages.get(&b.uuid) {
                Some(n) if *n > 1 => format!("{n} pages"),
                Some(1) => "1 page".to_string(),
                _ => String::new(),
            };
        }
        if b.page_count == 1 {
            "1 page".to_string()
        } else {
            format!("{} pages", b.page_count)
        }
    }

    fn draw_tile(&mut self, cx: i32, cy: i32, b: &Book) {
        self.draw_tile_sel(cx, cy, b, false)
    }

    fn draw_tile_sel(&mut self, cx: i32, cy: i32, b: &Book, selected: bool) {
        let (tw, th) = (COL_W, TILE_H);
        let (TW, TH) = (tw, th);
        if b.doc_type == "CollectionType" {
            self.fb.fill_rect(cx, cy, TW, TH, gray565(240));
            let edge = gray565(70);
            self.fb.fill_rect(cx, cy, TW, 1, edge);
            self.fb.fill_rect(cx, cy + TH - 1, TW, 1, edge);
            self.fb.fill_rect(cx, cy, 1, TH, edge);
            self.fb.fill_rect(cx + TW - 1, cy, 1, TH, edge);
            // 文件夹图标
            let (ix, iy) = (cx + TW / 2 - 60, cy + TH / 2 - 50);
            self.fb.fill_rect(ix, iy + 16, 120, 84, gray565(90));
            self.fb.fill_rect(ix + 8, iy, 46, 20, gray565(90));
        } else {
            let th = self.thumb(&b.uuid);
            self.fb.blit_gray(cx, cy, TW, TH, &th);
            let edge = gray565(70);
            self.fb.fill_rect(cx, cy, TW, 1, edge);
            self.fb.fill_rect(cx, cy + TH - 1, TW, 1, edge);
            self.fb.fill_rect(cx, cy, 1, TH, edge);
            self.fb.fill_rect(cx + TW - 1, cy, 1, TH, edge);
        }
        if selected {
            // 官方回收站里选中的卡片：整卡黑框（先做 8px 边框，够醒目且不用反相缩略图）
            let e = gray565(20);
            self.fb.fill_rect(cx, cy, TW, 8, e);
            self.fb.fill_rect(cx, cy + TH - 8, TW, 8, e);
            self.fb.fill_rect(cx, cy, 8, TH, e);
            self.fb.fill_rect(cx + TW - 8, cy, 8, TH, e);
        }
        let mut title_right = 0;
        // 先算好副标题：`card_meta` 借 &self，不能和 `self.text.as_mut()` 的借用重叠
        let meta = self.card_meta(b);
        if let Some(t) = self.text.as_mut() {
            // 置顶星占位（官方 titleInset）：长标题先让出星标宽度，别压在星上
            let inset = if b.pinned { STAR_PX + 16 } else { 0 };
            let name = t.ellipsize_mid_bold(32.0, &b.name, TW - 3 - inset);
            title_right =
                cx + 3 + t.draw_bold(&mut self.fb, cx + 3, cy + TH + 49, 32.0, &name, 20);
            if !meta.is_empty() {
                // 官方副标题也是纯黑，只是字号小（截图里量出来是 0）
                t.draw(&mut self.fb, cx + 3, cy + TH + 86, 26.0, &meta, 20);
            }
        }
        if b.pinned {
            // 官方 res_06a906a.qml：星在标题右边 16px，尺寸 = 标题字号
            self.draw_pin_star(title_right, cy + TH + 49, cx + TW, 20);
        }
    }

    /// 置顶星标（官方 `icon_star_inverted`）。`title_right` 是标题实际画到的右端，
    /// 星贴它右边 16px；`limit` 是卡片/行的右边界，长标题也不会把星顶出画面。
    fn draw_pin_star(&mut self, title_right: i32, baseline: i32, limit: i32, tone: u8) {
        let left = (title_right + 16).min(limit - STAR_PX);
        let cx = left as f32 + STAR_RO;
        let cy = baseline as f32 - 11.0; // 标题大写字高的中线（32 号量出来 cap=23）
        widgets::icon_star_s(&mut self.fb, cx, cy, STAR_RO, tone);
    }

    /// 列表行缩略图：把 426×568 的整页缩略图按面积平均缩进小预览框。
    /// 最近邻缩放会把细笔迹打散，列表里看着像脏点。
    fn blit_thumb_scaled(&mut self, x: i32, y: i32, w: i32, h: i32, uuid: &str) {
        // 电子书也走 `thumb()`：没有 `<uuid>.thumbnails/*.png` 时那里会返回
        // 后台渲染好的封面，没渲好就是白页（官方空态），见 P6 块。
        const SW: i32 = 426;
        const SH: i32 = 568;
        let src = self.thumb(uuid);
        for yy in 0..h {
            let sy0 = yy * SH / h;
            let sy1 = ((yy + 1) * SH / h).max(sy0 + 1);
            for xx in 0..w {
                let sx0 = xx * SW / w;
                let sx1 = ((xx + 1) * SW / w).max(sx0 + 1);
                let mut sum = 0u32;
                let mut n = 0u32;
                for sy in sy0..sy1 {
                    for sx in sx0..sx1 {
                        sum += src[(sy * SW + sx) as usize] as u32;
                        n += 1;
                    }
                }
                self.fb.put(x + xx, y + yy, gray565((sum / n) as u8));
            }
        }
    }

    /// 列表模式的一行（官方 `res_06a4413.qml` 文档行 160 / `res_06a2825.qml` 集合行 75）。
    /// 选中态官方是整行反白（黑底白字、白分隔线）。
    fn draw_list_row(&mut self, y: i32, b: &Book, selected: bool) {
        let w = self.fb.width;
        let (fg, bg) = if selected { (255u8, 0u8) } else { (20, 255) };
        let coll = b.doc_type == "CollectionType";
        let h = if coll { LIST_COLL_H } else { LIST_DOC_H };
        if selected {
            self.fb.fill_rect(0, y, w, h, gray565(bg));
        }
        let mut title_right = 0;
        if coll {
            // 左：40×40 文件夹图标（iconSizeMedium），垂直居中，左边距 33
            let c = gray565(fg);
            let (ix, iy) = (NAV_MARGIN, y + (LIST_COLL_H - 40) / 2);
            self.fb.fill_rect(ix + 2, iy + 8, 20, 3, c);
            self.fb.fill_rect(ix + 2, iy + 8, 3, 8, c);
            self.fb.fill_rect(ix + 19, iy + 8, 3, 10, c);
            widgets::outline_rect(&mut self.fb, ix + 2, iy + 13, 36, 23, 3, fg);
            if let Some(t) = self.text.as_mut() {
                let tx = NAV_MARGIN + 40 + 15;
                let inset = if b.pinned { STAR_PX + 16 } else { 0 };
                let max_w = w - 2 * NAV_MARGIN - 55 - inset;
                let name = t.ellipsize_mid_bold(32.0, &b.name, max_w);
                title_right = tx + t.draw_bold(&mut self.fb, tx, y + 48, 32.0, &name, fg);
            }
        } else {
            // 左：97×130 缩略图（行内上下各 15），1px 边框
            let px = NAV_MARGIN;
            let py = y + 15;
            self.blit_thumb_scaled(
                px + 1,
                py + 1,
                LIST_PREVIEW_W - 2,
                LIST_PREVIEW_H - 2,
                &b.uuid,
            );
            let edge = if selected { 255 } else { 70 };
            widgets::outline_rect(
                &mut self.fb,
                px,
                py,
                LIST_PREVIEW_W,
                LIST_PREVIEW_H,
                1,
                edge,
            );
            // 标题：缩略图右边 30，基线与缩略图顶对齐（官方 top: preview.top）
            let tx = px + LIST_PREVIEW_W + 30 - 3;
            let meta = self.card_meta(b);
            if let Some(t) = self.text.as_mut() {
                let inset = if b.pinned { STAR_PX + 16 } else { 0 };
                let name = t.ellipsize_mid_bold(32.0, &b.name, w - NAV_MARGIN - tx - inset);
                title_right = tx + t.draw_bold(&mut self.fb, tx, y + 40, 32.0, &name, fg);
                // 副标题贴缩略图底（官方 bottom: preview.bottom）
                t.draw(&mut self.fb, tx, y + 138, 26.0, &meta, fg);
            }
        }
        if b.pinned {
            let baseline = if coll { y + 48 } else { y + 40 };
            self.draw_pin_star(title_right, baseline, w - NAV_MARGIN, fg);
        }
        // 行底 1px 分隔线（官方 separatorLine，左右各 margin）
        self.fb.fill_rect(
            NAV_MARGIN,
            y + h - 1,
            w - 2 * NAV_MARGIN,
            1,
            gray565(if selected { 255 } else { 0 }),
        );
    }

    fn draw_library(&mut self) {
        // === E1 begin: 错题档案是整屏 sheet，盖在库上面（唤醒/重绘也走这里）===
        if self.mistakes.is_open() {
            self.draw_mistakes();
            return;
        }
        // === E1 end ===
        // === JOB begin: Job status 也是整屏 sheet ===
        if self.jobs_open {
            self.paint_jobs();
            return;
        }
        // === JOB end ===
        // === P8 begin ===
        // 多选 → Tag 的输入键盘是整屏浮层：所有重绘都先走它，免得被库界面糊掉。
        if self.lib_tag.is_some() {
            self.paint_lib_tag();
            return;
        }
        // === P8 end ===
        let (w, h) = (self.fb.width, self.fb.height);
        self.fb.fill_rect(0, 0, w, h, gray565(255));
        self.hits.clear();
        self.tiles.clear();

        // ---------- 内容
        // 先画内容再画顶栏/标题：这样滚动时卡片是"钻到"标题带下面，而不是盖住它。
        // （hits 后压的优先，所以顶栏按钮也自然压在卡片之上。）
        let top = GRID_TOP;
        let mut visible = self.view_books();
        if self.view == View::Tags {
            // 整个 Tags 视图（顶栏/标题/tabs/胶囊/网格/空态/弹层）都在 tagsview.rs
            self.draw_tags();
            self.visible = Vec::new();
        } else {
            if self.list_mode {
                // 官方列表把文件夹当成 ListView 的 header：集合行全在前、文档行在后。
                // 网格模式保持原来的混排顺序，不动。
                let (mut dirs, docs): (Vec<Book>, Vec<Book>) = visible
                    .into_iter()
                    .partition(|b| b.doc_type == "CollectionType");
                dirs.extend(docs);
                visible = dirs;
            }
            if visible.is_empty() {
                if let Some((head, lines, icon)) = self.view.empty() {
                    self.draw_empty_state(head, lines, icon);
                }
            }
            if self.list_mode {
                let n_dirs = visible
                    .iter()
                    .filter(|b| b.doc_type == "CollectionType")
                    .count();
                let mut y = top - self.scroll;
                for (i, b) in visible.iter().enumerate() {
                    let coll = b.doc_type == "CollectionType";
                    let rh = if coll { LIST_COLL_H } else { LIST_DOC_H };
                    if !coll && i == n_dirs && n_dirs > 0 {
                        // 官方的文件夹块尾部留白：designGridHorizontalHeight + spacing
                        y += LIST_FOLDERS_GAP;
                    }
                    if y > h {
                        break;
                    }
                    if y + rh < HEADER_H {
                        y += rh;
                        continue;
                    }
                    // === P8 begin ===
                    let sel = self.lib_is_sel(&b.uuid);
                    // === P8 end ===
                    self.draw_list_row(y, b, sel);
                    // 命中区至少 80 高：集合行官方只有 75，向下多吃 5px 落在
                    // 行底分隔线上，肉眼看不出来（后压的下一行会拿走重叠部分）。
                    self.hits.push(0, y, w, rh.max(80), A_TILE0 + i as u32);
                    // 长按菜单靠它找落点（网格/列表同一套）
                    self.tiles.push((b.uuid.clone(), 0, y, w, rh));
                    y += rh;
                }
            } else {
                let cols = COLS;
                let (tw, thh) = (COL_W, TILE_H);
                for (i, b) in visible.iter().enumerate() {
                    let cx = NAV_MARGIN + (i as i32 % cols) * (tw + TILE_GAP);
                    let cy = top + (i as i32 / cols) * (thh + 190) - self.scroll;
                    if cy > h - 100 {
                        break;
                    }
                    if cy + thh + 110 < HEADER_H {
                        continue;
                    }
                    // === P8 begin ===
                    let sel = self.lib_is_sel(&b.uuid);
                    // === P8 end ===
                    if sel {
                        self.draw_tile_sel(cx, cy, b, true);
                    } else {
                        self.draw_tile(cx, cy, b);
                    }
                    self.hits.push(cx, cy, tw, thh + 110, A_TILE0 + i as u32);
                    // 长按菜单要靠它找到落点下是哪一项
                    self.tiles
                        .push((b.uuid.clone(), cx, cy, tw, thh + 190));
                }
            }
            self.visible = visible;
        }

        // Tags 视图的顶栏/标题/tabs 由 tagsview 自己画（官方那里没有 +Folder/+Notebook/搜索）
        if self.view != View::Tags {
            self.draw_library_top();
            // === P4 begin ===
            // 将满/满：顶栏中间换成官方提示条（点它进 Settings > Storage）。
            self.draw_disk_banner();
            // === P4 end ===
        }

        // ---------- 侧栏抽屉
        // 官方实测：内容宽 341 + 4px 黑分隔；行高 120；图标 x=33 宽 40；
        // 文字 x=91、基线 = 行顶+72、字号 36；选中行黑底白字，右侧 6px 黑条。
        if self.menu_open {
            self.fb.fill_rect(0, 0, SIDEBAR_W - 4, h, gray565(255));
            self.fb.fill_rect(SIDEBAR_W - 4, 0, 4, h, gray565(0));

            // 能用官方图标字体的就用字体（形状分毫不差），其余的用几何临摹。
            // 码位来自设备 /usr/share/remarkable/webui/fonts/icomoon.woff。
            enum SideIcon {
                Glyph(char),
                Funnel,
                Star,
                Tag,
                // === E1 begin ===
                /// 错题档案：三行"清单"（自绘，icomoon 里没有对应码位）
                Mistakes,
                // === E1 end ===
                // === JOB begin ===
                /// AI 任务：一圈带指针的表盘（自绘）
                Jobs,
                // === JOB end ===
            }
            // === E1 begin: 侧栏加 `Mistakes` 一项（数组 5 → 6）===
            let items: [(&str, Option<View>, SideIcon); 7] = [
            // === E1 end ===
                ("My files", Some(View::MyFiles), SideIcon::Glyph('\u{e9c4}')),
                ("Filters", None, SideIcon::Funnel),
                ("Favorites", Some(View::Favorites), SideIcon::Star),
                ("Tags", Some(View::Tags), SideIcon::Tag),
                // === E1 begin ===
                ("Mistakes", None, SideIcon::Mistakes),
                // === E1 end ===
                // === JOB begin ===
                ("Job status", None, SideIcon::Jobs),
                // === JOB end ===
                ("Trash", Some(View::Trash), SideIcon::Glyph('\u{e957}')),
            ];
            let mut y = 0;
            for (i, (name, v, icon)) in items.iter().enumerate() {
                if *name == "Trash" {
                    y = 1630;
                }
                let sel = matches!(v, Some(v) if *v == self.view);
                let (fg, bg) = if sel { (255u8, 0u8) } else { (20, 255) };
                if sel {
                    self.fb.fill_rect(0, y, SIDEBAR_W - 4, 120, gray565(0));
                } else {
                    self.fb.fill_rect(0, y, SIDEBAR_W - 4, 120, gray565(bg));
                }
                match icon {
                    SideIcon::Glyph(ch) => {
                        if let Some(t) = self.text.as_mut() {
                            t.icon(&mut self.fb, 53, y + 60, 40, *ch, fg);
                        }
                    }
                    SideIcon::Funnel => widgets::icon_funnel(&mut self.fb, 33, y + 40, fg),
                    SideIcon::Star => widgets::icon_star(&mut self.fb, 33, y + 40, fg),
                    SideIcon::Tag => widgets::icon_tag(&mut self.fb, 33, y + 40, fg),
                    // === E1 begin ===
                    SideIcon::Mistakes => {
                        let c = gray565(fg);
                        for k in 0..3 {
                            self.fb.fill_rect(36, y + 42 + k * 14, 8, 8, c);
                            self.fb.fill_rect(56, y + 45 + k * 14, 22, 3, c);
                        }
                    }
                    // === E1 end ===
                    // === JOB begin: 三个点在跑（和列表里的"跑着"同一个记号）
                    SideIcon::Jobs => {
                        for k in 0..3 {
                            widgets::disc(&mut self.fb, 38 + k * 16, y + 60, 5, fg);
                        }
                    }
                    // === JOB end ===
                }
                if let Some(t) = self.text.as_mut() {
                    t.draw_bold(&mut self.fb, 88, y + 73, 36.0, name, fg);
                }
                if matches!(icon, SideIcon::Funnel) {
                    // 官方 ">"：x306..319、y168..190（行内相对 y+48），笔画 5
                    for i in 0..23 {
                        let dx = if i <= 11 { i } else { 22 - i };
                        self.fb.fill_rect(306 + dx, y + 48 + i, 5, 2, gray565(20));
                    }
                }
                // Filters 也要能点（展开 Notebooks / PDFs / Ebooks）
                // === E1 begin: Mistakes 用独立 action，不混进 A_SIDE0 的下标表 ===
                let side_act = if *name == "Mistakes" {
                    Some(A_E1_MISTAKES)
                // === JOB begin ===
                } else if *name == "Job status" {
                    Some(A_E1_JOBS)
                // === JOB end ===
                } else if v.is_some() || *name == "Filters" {
                    Some(A_SIDE0 + i as u32)
                } else {
                    None
                };
                if let Some(a) = side_act {
                    self.hits.push(0, y, SIDEBAR_W, 120, a);
                }
                // === E1 end ===
                y += 120;
            }

            // 底部：Trash 上面一条分隔，然后 Settings 常驻
            self.fb.fill_rect(0, 1750, SIDEBAR_W - 4, 2, gray565(0));
            self.fb.fill_rect(0, 1752, SIDEBAR_W - 4, h - 1752, gray565(255));
            if let Some(t) = self.text.as_mut() {
                t.icon(&mut self.fb, 53, 1812, 40, '\u{e96b}', 20);
                t.draw_bold(&mut self.fb, 88, 1825, 36.0, "Settings", 20);
            }
            self.hits.push(0, 1752, SIDEBAR_W, 120, A_SETTINGS);
            self.sidebar_items.clear();

            // ---------- Filters 展开面板（官方 12-sidebar-filters.png）
            // 左缘接侧栏黑边 x=344，右缘 687，行高 120，三行
            if self.filters_open {
                let (px, pw) = (SIDEBAR_W, 343);
                let rows: [(&str, u32, fn(&mut Fb, i32, i32)); 3] = [
                    ("Notebooks", A_FILTER0, search_icon_notebook),
                    ("PDFs", A_FILTER0 + 1, search_icon_doc),
                    ("Ebooks", A_FILTER0 + 2, ebook_icon),
                ];
                self.fb.fill_rect(px, 120, pw, 3 * 120, gray565(255));
                self.fb.fill_rect(px + pw - 3, 120, 3, 3 * 120, gray565(0));
                self.fb.fill_rect(px, 120, 3, 3 * 120, gray565(0));
                for (i, (label, act, icon)) in rows.iter().enumerate() {
                    let ry = 120 + i as i32 * 120;
                    if i > 0 {
                        self.fb.fill_rect(px, ry, pw, 2, gray565(0));
                    }
                    icon(&mut self.fb, px + 32, ry + 38);
                    if let Some(t) = self.text.as_mut() {
                        t.draw_bold(&mut self.fb, px + 84, ry + 74, 33.0, label, 20);
                    }
                    self.hits.push(px, ry, pw, 120, *act);
                }
            }
        }

        // ---------- 排序下拉（官方 res_06a7343.qml：宽 342，选项行 80）
        // 展开态 = 5 个排序项 + "View" 段头 72 + Grid / List 各 80。
        // 选中项官方用 `inverted`（黑底白字），不是额外画勾。
        // Tags 的那个由 tagsview 自己画（只有一个选项，位置也不同）
        if self.sort_open && self.view != View::Tags {
            let bw = 342;
            let bx = w - NAV_MARGIN - bw;
            let by = HEADER_H + 96; // 下拉按钮下沿：120 + (120-72)/2 + 72
            let sorts_h = Sort::ALL.len() as i32 * 80;
            let view_hdr = 72;
            let view_rows = 2 * 80;
            let bh = sorts_h + view_hdr + view_rows;
            // 官方 dropdownContent 宽 342、2px 黑框是**内边框**（内容宽 338）
            self.fb.fill_rect(bx, by, bw, bh + 2, gray565(0));
            self.fb.fill_rect(bx + 2, by + 2, bw - 4, bh - 2, gray565(255));
            let (ix, iw) = (bx + 2, bw - 4);
            for (i, sort) in Sort::ALL.iter().enumerate() {
                let yy = by + 2 + i as i32 * 80;
                let cur = *sort == self.sort;
                let fg = if cur { 255 } else { 20 };
                if cur {
                    self.fb.fill_rect(ix, yy, iw, 80, gray565(0));
                }
                if let Some(t) = self.text.as_mut() {
                    t.draw(
                        &mut self.fb,
                        ix + 16,
                        yy + 52,
                        28.0,
                        sort.label(self.view == View::Trash),
                        fg,
                    );
                }
                self.hits.push(ix, yy, iw, 80, A_SORT0 + i as u32);
            }
            // View 段头：72 高，上下各 2px 黑线（QML viewHeader 的 border）
            let vy = by + 2 + sorts_h;
            self.fb.fill_rect(ix, vy, iw, 2, gray565(0));
            self.fb.fill_rect(ix, vy + view_hdr - 2, iw, 2, gray565(0));
            if let Some(t) = self.text.as_mut() {
                t.draw_bold(&mut self.fb, ix + 16, vy + 46, 28.0, "View", 20);
            }
            for (i, (label, glyph, on)) in [
                ("Grid", '\u{e919}', !self.list_mode),
                ("List", '\u{e9c3}', self.list_mode),
            ]
            .iter()
            .enumerate()
            {
                let yy = vy + view_hdr + i as i32 * 80;
                let fg = if *on { 255 } else { 20 };
                if *on {
                    self.fb.fill_rect(ix, yy, iw, 80, gray565(0));
                }
                if let Some(t) = self.text.as_mut() {
                    // 图标左边距 16（40px iconSizeMedium），文字接在图标右边 16
                    t.icon(&mut self.fb, ix + 16 + 20, yy + 40, 40, *glyph, fg);
                    t.draw(&mut self.fb, ix + 16 + 40 + 16, yy + 52, 28.0, label, fg);
                }
                self.hits.push(ix, yy, iw, 80, A_VIEW0 + i as u32);
            }
        }

        // === P3 begin ===
        // 通知条压在底栏上方（y 1704..1800）：在状态栏之前画，状态栏的白底
        // 仍然留着，三个可点指示器不会被盖住。
        if !self.menu_open {
            self.draw_notice_bar();
        }
        // === P3 end ===
        if !self.menu_open {
            self.draw_status();
        }

        // === P8 begin ===
        // 选择态的底部动作条：压住状态栏（页总览的动作条也这么盖）。
        // Move 的"选目标文件夹"模式由顶栏接管，底条先收起来。
        if !self.lib_moving && !self.lib_selected().is_empty() {
            let (nd, nf) = self.lib_sel_counts();
            libbar::paint_bar(
                &mut self.fb,
                &mut self.text,
                nd,
                nf,
                self.view == View::Trash,
                w,
                h,
            );
        }
        // === P8 end ===

        // ---------- 永久删除确认（官方 ConfirmChangeDialog）
        if let Some(ids) = self.trash_confirm.clone() {
            self.fb.dim(0, 0, w, h, gray565(140), 1, 2);
            let bw = 900;
            let bh = 420;
            let bx = (w - bw) / 2;
            let by = (h - bh) / 2;
            self.fb.fill_rect(bx, by, bw, bh, gray565(255));
            widgets::outline_rect(&mut self.fb, bx, by, bw, bh, 2, 20);
            let n = ids.len();
            let (head, body) = if n == 1 {
                (
                    "Delete item permanently?".to_string(),
                    "If you delete the item, you won't be able to get it back.".to_string(),
                )
            } else {
                (
                    format!("Delete {n} items permanently?"),
                    "If you delete the items, you won't be able to get them back.".to_string(),
                )
            };
            if let Some(t) = self.text.as_mut() {
                t.draw_bold(&mut self.fb, bx + 48, by + 78, 34.0, &head, 20);
                t.draw(&mut self.fb, bx + 48, by + 132, 26.0, &body, 60);
            }
            // Yes / No：官方左边是 "No, keep item"，右边是 "Yes, delete item"
            let (bw2, bh2) = (360, 90);
            let y2 = by + bh - 130;
            let no_label = if n == 1 { "No, keep item" } else { "No, keep items" };
            let yes_label = if n == 1 { "Yes, delete item" } else { "Yes, delete items" };
            widgets::outline_rect(&mut self.fb, bx + 48, y2, bw2, bh2, 2, 30);
            widgets::outline_rect(&mut self.fb, bx + bw - 48 - bw2, y2, bw2, bh2, 2, 30);
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, bx + 48 + 24, y2 + 58, 28.0, no_label, 20);
                t.draw(&mut self.fb, bx + bw - 48 - bw2 + 24, y2 + 58, 28.0, yes_label, 20);
            }
            self.hits.push(bx + 48, y2, bw2, bh2, A_TR_NO);
            self.hits.push(bx + bw - 48 - bw2, y2, bw2, bh2, A_TR_YES);
        }

        // ---------- 长按菜单
        if let Some(b) = self.ctx.clone() {
            // 背景压暗（真·半透明混合，不能用隔点——屏幕上会变成棋盘格）
            self.fb.dim(0, 0, w, h, gray565(140), 1, 2);
            let bw = 760;
            let pinned = b.pinned;
            let in_trash = self.view == View::Trash;
            let items: Vec<(&str, u32)> = if in_trash {
                vec![("Restore", A_TR_RESTORE), ("Delete", A_TR_DELETE), ("Cancel", A_CTX_CANCEL)]
            } else {
                vec![
                    ("Rename", A_CTX_RENAME),
                    ("Move to trash", A_CTX_TRASH),
                    (
                        // 官方文案：未收藏 "Favorite"，已收藏 "Remove from favorites"
                        if pinned { "Remove from favorites" } else { "Favorite" },
                        A_CTX_PIN,
                    ),
                    ("Cancel", A_CTX_CANCEL),
                ]
            };
            let bh = items.len() as i32 * 96 + 40;
            let bx = (w - bw) / 2;
            let by = (h - bh) / 2;
            self.fb.fill_rect(bx, by, bw, bh, gray565(255));
            widgets::outline_rect(&mut self.fb, bx, by, bw, bh, 2, 20);
            for (i, (label, act)) in items.iter().enumerate() {
                let yy = by + 20 + i as i32 * 96;
                if let Some(t) = self.text.as_mut() {
                    t.draw(&mut self.fb, bx + 40, yy + 62, 34.0, label, 20);
                }
                if i > 0 {
                    self.fb.fill_rect(bx + 40, yy, bw - 80, 1, gray565(225));
                }
                self.hits.push(bx, yy, bw, 96, *act);
            }
        }

        // Tags 的弹层压在最上面（操作条要盖住状态栏，官方的 modal 也是整条宽）
        if self.view == View::Tags {
            self.tags_modals();
        }

        // === P1 begin ===
        // 官方把 `Open the address: http://10.11.99.1 in a web browser.` 放在
        // Settings > Storage 的 `USB web interface` 行下面（`res_0686a15.qml:130`），
        // 不是库顶部横幅。这里原来那条横幅会盖住标题带（`My files` + 排序按钮），
        // 已删；文案由 `set_page_storage` 负责画。
        // === P1 end ===

        // === P12 begin ===
        // 下拉抽屉：压在库内容之上（半屏），最后画。Tags 视图有自己的胶囊交互，
        // 不从顶部下拉（`View all` 切过去时抽屉已经关了）。
        if self.view != View::Tags {
            self.drawer.paint(&mut self.fb, &mut self.text, w, h);
        }
        // === P12 end ===

        if let Some(msg) = self.banner.clone() {
            let bh = 120;
            self.fb.fill_rect(0, h - bh, w, bh, gray565(30));
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, NAV_MARGIN, h - 46, 34.0, &msg, 245);
            }
        }
        if self.view == View::Tags {
            // 点胶囊/切 tab 只刷变化的那块，整屏闪一下太难受
            self.tags_flush();
        } else {
            self.fb.refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
        }
    }

    /// 顶栏 + 标题 + 排序控件。**必须在内容之后画**：滚动时卡片要钻到它下面。
    fn draw_library_top(&mut self) {
        // === P8 begin ===
        // 选择态：整条标题带换成多选顶栏（Move 模式换成"选目标文件夹"）。
        if !self.lib_selected().is_empty() {
            let w = self.fb.width;
            if self.lib_moving {
                self.paint_lib_move_header();
            } else {
                let (nd, nf) = self.lib_sel_counts();
                let all = self.lib_sel_all();
                libbar::paint_header(
                    &mut self.fb,
                    &mut self.text,
                    nd + nf,
                    all,
                    w,
                );
            }
            return;
        }
        // === P8 end ===
        let w = self.fb.width;
        // 标题带整块盖白：官方内容区是裁剪的，滚上来的卡片看不见
        self.fb.fill_rect(0, 0, w, GRID_TOP, gray565(255));

        // ---------- 顶栏：≡ Menu | ⊕ Folder | ⊕ Notebook | ⊕ Quick sheets | 🔍
        let base = 73;
        widgets::hamburger(&mut self.fb, 33, 44, 39, 20);
        self.hits.push(12, 0, 160, HEADER_H, A_MENU);
        if let Some(t) = self.text.as_mut() {
            // Maison 的 M 左边有 3px 侧承，画在 88 才让墨迹从 91 开始（官方量出来的）
            t.draw_bold(&mut self.fb, 88, base, 36.0, "Menu", 20);
        }
        let mut x = 380;
        for (label, act) in [
            ("Folder", A_NEW_FOLDER),
            ("Notebook", A_NEW_NOTEBOOK),
            ("Quick sheets", A_QUICK),
        ] {
            // 官方：回收站里三个新建按钮全部隐藏
            if self.view == View::Trash {
                break;
            }
            widgets::plus_circle(&mut self.fb, x + 20, 60, 20, 20);
            if let Some(t) = self.text.as_mut() {
                t.draw_bold(&mut self.fb, x + 55, base, 36.0, label, 20);
            }
            let tw = self
                .text
                .as_mut()
                .map(|t| t.width_bold(36.0, label) as i32)
                .unwrap_or(0);
            self.hits.push(x - 8, 0, 48 + tw + 25, HEADER_H, act);
            x += 40 + 19 + tw + 46;
        }
        // 回收站里的 "Empty trash"（图标 + 文字，贴搜索左边；官方只在有内容时显示）
        if self.view == View::Trash && !self.visible.is_empty() {
            // 官方：anchors.right = 搜索按钮左边，图标在文字左边
            let mut base_x = 1200;
            if let Some(t) = self.text.as_mut() {
                let tw = t.width_bold(36.0, "Empty trash");
                base_x = 1286 - tw as i32;
                t.draw_bold(&mut self.fb, base_x, base, 36.0, "Empty trash", 20);
            }
            let icon_x = base_x - 50;
            widgets::icon_trash(&mut self.fb, icon_x, 40, 20);
            self.hits.push(icon_x - 20, 0, 1310 - icon_x, HEADER_H, A_EMPTY_TRASH);
        }
        widgets::magnifier(&mut self.fb, 1347, 47, 15, 20);
        self.hits.push(w - 100, 0, 80, HEADER_H, A_SEARCH);
        self.fb.fill_rect(0, HEADER_H - 2, w, 2, gray565(20));

        // ---------- 标题行
        // 在文件夹里：标题 = 当前文件夹名，右边跟面包屑（官方 Breadcrumb.qml：
        // 深的那个用 TextHeaderLarge，祖先用 TextHeaderRegular + › 分隔）
        let (title, in_folder) = match (&self.tag_filter, self.trail.last()) {
            (Some(t), _) => (format!("#{t}"), false),
            (None, Some((_, name))) => (name.clone(), true),
            (None, None) => (self.view.title().to_string(), false),
        };
        let mut title_right = NAV_MARGIN;
        if let Some(t) = self.text.as_mut() {
            t.draw_bold(&mut self.fb, NAV_MARGIN - 3, 192, 40.0, &title, 20);
            title_right = NAV_MARGIN + t.width_bold(40.0, &title) as i32;
        }
        if in_folder {
            let mut x = title_right + 26;
            // 标题已经是最深那一层；面包屑只放**祖先**（根 My files 打头）
            let mut chain: Vec<(Option<usize>, String)> = vec![(None, "My files".to_string())];
            for (i, (_, name)) in self.trail.iter().enumerate() {
                if i + 1 < self.trail.len() {
                    chain.push((Some(i), name.clone()));
                }
            }
            for (idx, name) in chain {
                // › 分隔符（20×20）
                // ›（朝右的尖）
                for k in 0..10 {
                    self.fb.fill_rect(x + k, 171 + k, 3, 3, gray565(20));
                    self.fb.fill_rect(x + k, 189 - k, 3, 3, gray565(20));
                }
                let tx = x + 18;
                let tw = match self.text.as_mut() {
                    Some(t) => {
                        t.draw(&mut self.fb, tx, 192, 28.0, &name, 20);
                        t.width(28.0, &name) as i32
                    }
                    None => 0,
                };
                let act = match idx {
                    None => A_CRUMB_ROOT,
                    Some(i) => A_CRUMB0 + i as u32,
                };
                self.hits.push(x, 140, 18 + tw + 8, 80, act);
                x = tx + tw + 20;
            }
        }
        // 排序控件：Trash / Tags 下不显示
        if self.view != View::Trash && self.view != View::Tags {
            // === P13 begin ===
            // 展开时按钮文字从当前排序名变成 `Sort by`（官方 `res_06a7343.qml:18/71`）
            let lab = rmdev::chrome::sort_button_label(
                self.sort_open,
                self.sort.label(self.view == View::Trash),
            );
            // === P13 end ===
            let tw = self
                .text
                .as_mut()
                .map(|t| t.width_bold(32.0, lab) as i32)
                .unwrap_or(0);
            // 官方：文字右端 1296，箭头 1323..1346，基线 187
            let lx = 1298 - tw;
            if let Some(t) = self.text.as_mut() {
                t.draw_bold(&mut self.fb, lx, 189, 32.0, lab, 20);
            }
            for i in 0..12 {
                // 展开时官方把 chevron_down 换成 chevron_up（上下翻转）
                let yy = if self.sort_open { 177 - i } else { 166 + i };
                self.fb.fill_rect(1323 + i, yy, 3, 3, gray565(20));
                self.fb.fill_rect(1345 - i, yy, 3, 3, gray565(20));
            }
            self.hits.push(lx - 20, 130, w - lx + 20, 80, A_SORT);
        }
    }

    /// 内容还能往上滚多少像素
    fn max_scroll(&mut self) -> i32 {
        let h = self.fb.height;
        if self.view == View::Tags {
            return self.tags_max_scroll();
        }
        if self.list_mode {
            let dirs = self
                .visible
                .iter()
                .filter(|b| b.doc_type == "CollectionType")
                .count() as i32;
            let docs = self.visible.len() as i32 - dirs;
            if dirs + docs == 0 {
                return 0;
            }
            let mut content = GRID_TOP + dirs * LIST_COLL_H + docs * LIST_DOC_H;
            if dirs > 0 {
                content += LIST_FOLDERS_GAP;
            }
            return (content - (h - 80)).max(0);
        }
        let n = self.visible.len() as i32;
        let cols = COLS as i32;
        let rows = (n + cols - 1) / cols;
        if rows == 0 {
            return 0;
        }
        let content = GRID_TOP + (rows - 1) * (TILE_H + 190) + TILE_H + 110;
        (content - (h - 80)).max(0)
    }

    /// 向上滚 dy 像素（负数 = 内容上移）
    fn scroll_by(&mut self, dy: i32) {
        let before = self.scroll;
        self.scroll = (self.scroll - dy).clamp(0, self.max_scroll());
        if self.scroll != before {
            eprintln!("[ui] scroll={}", self.scroll);
            self.draw_library();
        }
    }

    /// 底部状态栏。官方实测：白底，y 1800..1872；闪电 39..56、电池 69..100、
    /// "100%" 116..180（基线约 1843）、Wi-Fi 207..238、云朵 264..293。
    fn draw_status(&mut self) {
        self.draw_status_hits(true);
    }

    /// 状态栏本体。`with_hits=false` 时只画不登记命中表 —— 给"只刷这一条"的
    /// 增量重绘用（详见 [`App::repaint_status`]）。
    fn draw_status_hits(&mut self, with_hits: bool) {
        let (w, h) = (self.fb.width, self.fb.height);
        let y0 = 1800;
        self.fb.fill_rect(0, y0, w, h - y0, gray565(255));

        let (cap, st) = read_battery();
        widgets::bolt(&mut self.fb, 39, 1816, 20);
        widgets::battery(&mut self.fb, 69, 1821, cap, 20);
        // === P5 begin: Show battery percentage 关掉后只画图标、不画 NN% ===
        if let Some(label) = prefs::battery_label(cap, self.show_battery_pct) {
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, 118, 1845, 26.0, &label, 20);
            }
        }
        // === P5 end ===
        // Wi-Fi 跟信号强弱走、云跟服务器连不连得上走（都在后台快照里，见
        // `spawn_net_reader`）。没连上时只剩一个点；云连不上就是带叉那个。
        let (bars, cloud_ok) = net_state();
        widgets::wifi_bars(&mut self.fb, 222, 1846, 20, bars);
        if cloud_ok == Some(true) {
            widgets::cloud(&mut self.fb, 264, 1818, 20);
        } else {
            widgets::cloud_off(&mut self.fb, 264, 1818, 20);
        }
        if env_flag("RM_DEBUG") {
            eprintln!("[status] 电池 {cap}% {st} wifi={bars} cloud={cloud_ok:?}");
        }
        // === P3 begin ===
        // 官方 `res_06b6382.qml`：BatteryIndicator / WifiIndicator / CloudIndicator
        // 各自带一个 MouseArea，点了分别去 Battery 页、Wi-Fi 页、重试同步。
        // 命中区纵向吃满整个状态栏（72 高），手指才够得着。
        // 电量从 x=64 起：左下角 64×64 是"连点三下退出"的开发用区域，别抢。
        if with_hits {
            self.hits.push(64, y0, 126, h - y0, A_P3_BATTERY);
            self.hits.push(195, y0, 58, h - y0, A_P3_WIFI);
            self.hits.push(256, y0, 64, h - y0, A_P3_CLOUD);
        }
        // === P3 end ===
    }

    /// 只刷状态栏那一条（信号格数 / 云状态变了的时候用）。
    ///
    /// 不能走 `draw_library()`：那是整屏重画，每十几秒为两个小图标闪一下不值当。
    /// 命中表不动（三个指示器的坐标没变），所以 `with_hits=false`。
    fn repaint_status(&mut self) {
        if !matches!(self.screen, Screen::Library)
            || self.menu_open
            || self.lib_moving
            || !self.lib_selected().is_empty()
        {
            return;
        }
        self.draw_status_hits(false);
        self.fb.refresh(
            0,
            1800,
            self.fb.width,
            72,
            rmdev::fb::WAVEFORM_MODE_GC16,
            rmdev::fb::UPDATE_MODE_PARTIAL,
        );
    }

    /// 官方空态：图标 90、主文案 36 粗、说明行 26 常规，整块居中并上移 120。
    fn draw_empty_state(&mut self, head: &str, lines: &[&str], icon: u8) {
        let (w, _h) = (self.fb.width, self.fb.height);
        // 官方（12-sidebar-tags.png 量）：图标 90 居中、顶 842；标题基线 1015；
        // 说明首行基线 1099、行距 55。
        let cy = 1015;
        let icon_cy = 887;
        match icon {
            1 => widgets::icon_doc(&mut self.fb, w / 2 - 45, icon_cy - 45, 20),
            2 => ebook_icon(&mut self.fb, w / 2 - 45, icon_cy - 45),
            3 => widgets::icon_star_s(&mut self.fb, w as f32 / 2.0, icon_cy as f32, 44.0, 20),
            _ => widgets::icon_trash(&mut self.fb, w / 2 - 45, icon_cy - 45, 20),
        }
        if let Some(t) = self.text.as_mut() {
            let tw = t.width_bold(40.0, head) as i32;
            t.draw_bold(&mut self.fb, (w - tw) / 2, cy, 40.0, head, 20);
            let mut y = 1099;
            for line in lines {
                if !line.is_empty() {
                    let lw = t.width(30.0, line) as i32;
                    t.draw(&mut self.fb, (w - lw) / 2, y, 30.0, line, 20);
                }
                y += 55;
            }
        }
    }

    fn open(&mut self, uuid: &str) {
        self.open_at(uuid, 0);
    }

    /// `.metadata` 里的 `visibleName`（认不出来就 None）。
    fn meta_visible_name(&self, uuid: &str) -> Option<String> {
        let p = self.library.root.join(format!("{uuid}.metadata"));
        let t = std::fs::read_to_string(p).ok()?;
        let v: serde_json::Value = serde_json::from_str(&t).ok()?;
        v.get("visibleName")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
    }

    /// 按 uuid 直接在库目录里找**电子书本体**（pdf/epub/…），不看元数据。
    ///
    /// 用途：`Library::find()` 读不出元数据时（同步正在覆写 `.metadata`、索引
    /// 陈旧、字段缺失都会这样）兜底用。真踩过：那样一来文档会被当成"空白笔记
    /// 本"，关掉时把 PDF 的 `cPages` 冲成 1 页 —— 13 页的讲义直接少 12 页的
    /// 批注位。文件还在就按文件认，比信任元数据安全。
    fn ebook_path(&self, uuid: &str) -> Option<std::path::PathBuf> {
        let root = &self.library.root;
        let prefix = format!("{uuid}.");
        let rd = std::fs::read_dir(root).ok()?;
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Some(rest) = name.strip_prefix(&prefix) else {
                continue;
            };
            let f = rmdev::doc::Format::of_ext(rest);
            if f.is_known() && f != rmdev::doc::Format::Rm {
                return Some(e.path());
            }
        }
        None
    }

    /// 文档本体在库目录里的路径：`<uuid>.<ext>`。
    ///
    /// `.content` 里的 `fileType` 偶尔缺失或和真实文件对不上（同步回来的书、
    /// 手工拷进去的文件都会这样），所以按扩展名找不到就扫一遍目录，认第一个
    /// `<uuid>.<已知格式后缀>`。
    fn document_path(&self, b: &Book) -> std::path::PathBuf {
        let root = &self.library.root;
        let ext = b.format.ext();
        if !ext.is_empty() {
            let p = root.join(format!("{}.{}", b.uuid, ext));
            if p.exists() {
                return p;
            }
        }
        let prefix = format!("{}.", b.uuid);
        if let Ok(rd) = std::fs::read_dir(root) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let Some(rest) = name.strip_prefix(&prefix) else {
                    continue;
                };
                if rmdev::doc::Format::of_ext(rest).is_known() {
                    return e.path();
                }
            }
        }
        root.join(format!(
            "{}.{}",
            b.uuid,
            if ext.is_empty() { "pdf" } else { ext }
        ))
    }

    fn open_at(&mut self, uuid: &str, idx: usize) {
        let mut page = self.library.load_page_at(uuid, idx);
        let mut page_count = self.library.page_ids(uuid).len().max(1);
        // 换文档：文档设置面板整个归零。除了"收起"还得清 `portrait`——视图换了
        // 新文档就是竖屏，面板里那行文案必须跟着回来，不然会显示"Set to portrait
        // view"却本来就是竖屏。
        self.ds = DocSettings::new();
        // AI 引用索引（服务器发布回答 PDF 时写进来的）。读不到就是空，不影响阅读。
        self.ai_refs = rmdev::airefs::Refs::load(&self.library.root, uuid);
        self.ai_view = None;
        self.ai_menu = false;
        // 文档内搜索 / Notebook settings / 目录都是当前文档的浮层：换文档一律收起
        self.search.close();
        self.nbset.close();
        self.toc.close();
        self.tcfg.close();
        self.pagesel.close();
        self.move_to = None;
        self.banner = None;
        // PDF：把这一页渲染成底图；PDF 的页数以 PDF 自己为准
        self.pdf_bg = None;
        self.page_box = None;
        self.land_manual = None;
        // 下一页预热的参数（渲染尺寸要跟当前页一致才能命中缓存）
        // 本体读不到/渲染不出来时**不能**继续往里走：那会进一张白纸（工具栏和
        // 页码都在、能写字、但一个字都没有），用户完全不知道发生了什么 ——
        // 真机就是靠这个把 41 页的 PDF 变成"白屏"的。宁可退回去，也不给死路。
        let mut unreadable = false;
        // `find()` 读不出元数据时（同步正在写、索引陈旧）别当空白笔记本 ——
        // 见 `ebook_path` 的注释：那会把 PDF 的 `cPages` 冲成 1 页。
        let found = self.library.find(&uuid);
        let fallback = if found
            .as_ref()
            .map(|b| !b.format.is_known() || b.format == rmdev::doc::Format::Rm)
            .unwrap_or(true)
        {
            self.ebook_path(&uuid)
        } else {
            None
        };
        if let Some(b) = found.clone().or_else(|| {
            fallback.as_ref().map(|p| {
                let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
                Book {
                    uuid: uuid.to_string(),
                    name: self.meta_visible_name(uuid).unwrap_or_else(|| "笔记".into()),
                    parent: String::new(),
                    pinned: false,
                    deleted: false,
                    doc_type: "DocumentType".into(),
                    last_modified: 0,
                    last_opened_page: 0,
                    page_count: 0,
                    format: rmdev::doc::Format::of_ext(ext),
                    size: std::fs::metadata(p).map(|m| m.len()).unwrap_or(0),
                    last_opened: 0,
                }
            })
        }) {
            // 所有电子书（PDF/EPUB/TXT/MOBI/CBZ…）走同一条路：页数以文档自己为准，
            // 渲染成底图整屏 contain。格式差异全在 `doc::open` 里面。
            if b.format.is_known() && b.format != rmdev::doc::Format::Rm {
                let path = self.document_path(&b);
                let t_open = std::time::Instant::now();
                match rmdev::doc::open(path.to_str().unwrap_or("")) {
                    Ok(mut d) => {
                        // 按文档持久化的排版（readcfg.json）先套上，再取页数/渲染
                        if let Some(cfg) = textcfg::load_cfg(uuid) {
                            if cfg != d.layout_cfg() {
                                d.set_layout(cfg);
                            }
                        }
                        let n = d.pages();
                        if n > 0 {
                            page_count = n;
                        }
                        // 重排格式的页数会随字号变，卡片副标题要跟着更新
                        self.epub_pages.insert(uuid.to_string(), n);
                        // 每页要能写批注：补齐 `cPages.pages`（只补不删）。
                        // 补完之后这一页的 page id 才是真的，要重新取一次。
                        if b.format.is_ebook() {
                            match self.library.ensure_pages(uuid, n) {
                                Ok(true) => page = self.library.load_page_at(uuid, idx),
                                Ok(false) => {}
                                Err(e) => eprintln!("[doc] 补页失败：{e}"),
                            }
                        }
                        let idx = idx.min(n.saturating_sub(1));
                        let (fw, fh) = (DOC_W, DOC_H);
                        // Doc::aspect 统一是 w/h
                        let asp = d.aspect(idx).clamp(0.05, 20.0);
                        let mut ww = (fh as f32 * asp).round() as usize;
                        let mut hh = fh;
                        if ww > fw {
                            ww = fw;
                            hh = ((fw as f32) / asp).round() as usize;
                        }
                        match d.render(idx, ww, hh) {
                            Ok(g) => {
                                eprintln!(
                                    "[doc] {uuid} {} 第 {idx} 页（{n} 页, {ww}x{hh}, {} ms）",
                                    b.format.label(),
                                    t_open.elapsed().as_millis()
                                );
                                let mut bg = vec![255u8; fw * fh];
                                let ox = (fw - ww) / 2;
                                let oy = (fh - hh.min(fh)) / 2;
                                for y in 0..hh.min(fh) {
                                    let src = y * ww;
                                    let dst = (oy + y) * fw + ox;
                                    bg[dst..dst + ww].copy_from_slice(&g[src..src + ww]);
                                }
                                self.pdf_bg = Some(bg);
                                self.page_box = Some((ox, oy, ww, hh.min(fh)));
                                self.bg_gen = self.bg_gen.wrapping_add(1);
                            }
                            Err(e) => {
                                eprintln!("[doc] 渲染失败：{e}");
                                unreadable = true;
                            }
                        }
                        // 让「文档设置」面板知道这本能不能改字号、现在是第几档
                        self.reading_cfg = d.layout_cfg();
                        let size = rmdev::doc::reflow::FONT_SIZES
                            .iter()
                            .position(|s| (s - self.reading_cfg.font_size).abs() < 0.5);
                        let reflow = d.format().reflowable();
                        self.doc = Some(d);
                        self.doc_uuid = Some(uuid.to_string());
                        self.ds.set_reflow(reflow, size);
                    }
                    Err(e) => {
                        eprintln!("[doc] {e}");
                        unreadable = true;
                    }
                }
            }
        }
        if unreadable {
            // 文案是我们自己写的（官方 QML 里没有"打不开"这句），已在
            // docs/ACCEPT-gaps.md 里登记为自造字符串。
            self.notice(NoticeKind::Error, "Could not open this document");
            self.draw_library();
            return;
        }
        let name = self
            .library
            .books()
            .into_iter()
            .find(|b| b.uuid == uuid)
            .map(|b| b.name)
            .unwrap_or_else(|| "笔记".into());
        eprintln!("[dev] 打开 {} ({} 笔 / {} 层)", name, page.total_lines(), page.layers.len());
        // === P11 begin ===
        // 读文字：官方 `.rm` 优先，没有文字层再看 sidecar
        let (p11_texts, p11_styles) = {
            let v = self.library.load_scene_texts(uuid, idx);
            (
                v.iter().map(|t| t.to_item()).collect::<Vec<TextItem>>(),
                v.iter().map(|t| t.style).collect::<Vec<ParagraphStyle>>(),
            )
        };
        // === P11 end ===
        // 方向：宽页（PPT 那种）自动进横屏。视图按"页面框铺满视口"给初值 ——
        // 竖屏时就是 identity（画布 == 屏幕），横屏时是一个足scale的变换。
        let asp = self
            .doc
            .as_ref()
            .map(|d| d.aspect(idx).clamp(0.05, 20.0))
            .or_else(|| self.page_box.map(|(_, _, w, h)| w as f32 / h.max(1) as f32));
        if let Some(a) = asp {
            self.auto_landscape(a);
        }
        let view0 = if self.landscape() {
            self.box_view()
        } else {
            PageView::identity()
        };
        let mut nb = Notebook {
            uuid: uuid.to_string(),
            name,
            page,
            page_index: idx,
            page_count,
            overview: false,
            page_thumbs: None,
            swipe: None,
            chrome: Chrome::default(),
            active: None,
            exit_taps: 0,
            dirty: false,
            tap_consumed: false,
            // === P11 begin ===
            texts: p11_texts,
            p11_styles,
            // === P11 end ===
            text_edit: None,
            pending_text: None,
            sel: None,
            lasso: None,
            drag_sel: None,
            pending_paste: None,
            undo: Vec::new(),
            redo: Vec::new(),
            tag_edit: None,
            layer_rename: None,
            erase_rect: None,
            view: view0,
            last_pen: 0,
            landscape_back: None,
        };
        // 沿用上一次的工具（官方 `lastUsedTool` 的简化版）和羽化开关
        nb.chrome.selected = self.last_tool.min(TOOLS.len() - 1);
        nb.chrome.feather = self.feather;
        // 橡皮也沿用上一次的档位（"进入绘制界面还记得上一次用的是哪个"）。
        // 这里只是预置进 chrome：`erasing` 仍然是 false，点橡皮那格时才真正用上。
        nb.chrome.erase_stroke = self.last_erase_stroke;
        self.canvas.feather = self.feather;
        if nb.chrome.selected + 1 < TOOLS.len() {
            nb.last_pen = nb.chrome.selected;
        }
        // === C15 begin ===
        // 可绘制的可写文档：页序列末尾挂一张虚拟空白页（只改 `nb.page_count`，
        // 不动 `.content`；用户在这页上落笔时才真的 `add_page`）。
        self.c15_attach(&mut nb);
        // === C15 end ===
        self.show_notebook(nb);
    }

    fn new_book(&mut self) {
        match self.library.create_book_in("未命名笔记", &self.cwd()) {
            Ok(uuid) => {
                eprintln!("[dev] 新建 {uuid}");
                self.open(&uuid);
            }
            Err(e) => eprintln!("[dev] 新建失败：{e}"),
        }
    }

    // ------------------------------------------------------------ 笔记本

    fn show_notebook(&mut self, nb: Notebook) {
        // 先把 screen 定下来，再走统一的整页重绘流程——
        // 这样工具条、关闭按钮、页脚和纸面一次画完，只推一次全屏刷新。
        self.screen = Screen::Notebook(Box::new(nb));
        self.redraw_page();
    }

    fn close_notebook(&mut self) {
        // === C15 begin ===
        // 虚拟空白页上写过东西就先把它变成真页：否则下面按 `page_index` 存盘
        // 会写到一个不存在的页 id 上（`save_page_at` 会另造一页，`.content` 里却没有它）。
        self.c15_commit_virtual();
        // === C15 end ===
        let Screen::Notebook(nb) = &mut self.screen else {
            return;
        };
        // 后面 `p11_save_page_texts` 要可变借 self，`nb` 的借用必须在这之前结束。
        let closing_uuid = nb.uuid.clone();
        self.search.close();
        self.nbset.close();
        self.tcfg.close();
        self.pagesel.close();
        self.move_to = None;
        // === P11 begin ===
        // 文字落盘挪到整页写回**之后**（`RmPage::serialize()` 不写 `RootText`，
        // 先写文字再存页会被整页覆盖掉）
        // === P11 end ===
        if nb.dirty {
            if let Err(e) = self
                .library
                .save_page_at(&nb.uuid, nb.page_index, &nb.page)
            {
                eprintln!("[dev] 保存失败：{e}");
            } else {
                eprintln!("[dev] 已保存 {} ({} 笔)", nb.name, nb.page.total_lines());
            }
        }
        // === P11 begin ===
        let (p11_u, p11_i) = (nb.uuid.clone(), nb.page_index);
        self.p11_save_page_texts(&p11_u, p11_i);
        // === P11 end ===
        // === C15 begin ===
        // 离开文档：虚拟页只活这一次打开
        self.c15_virt = None;
        // === C15 end ===
        // === AIQS begin ===
        // Quick sheets 的空本：一个字没写就整本删掉（连点几下不该在库里留一串空白）。
        // 写过就留着——名字仍是 `Quick sheets`，库按时间排，最近的在最前。
        if let Some(q) = self.quick_new.take() {
            if q == closing_uuid {
                let pages = self.library.page_ids(&q);
                let mut ink = 0usize;
                for i in 0..pages.len() {
                    ink += self.library.load_page_at(&q, i).total_lines();
                }
                if ink == 0 {
                    let _ = self.library.purge(&q);
                    eprintln!("[quick] 空本已删除 {q}");
                } else {
                    eprintln!("[quick] 保留 {q}（{ink} 笔）");
                }
            }
        }
        // === AIQS end ===
        // 库界面是竖屏布局（整屏几何都按 1404×1872 量的）：回库前先把画面转回来，
        // 否则库会横着长在横屏画布里。
        self.force_portrait();
        self.screen = Screen::Library;
        self.draw_library();
    }


    /// 右上角的触控锁图标（笔记本里常驻；锁着 = 闭合，解锁 = 开口）。
    fn paint_lock_chip(&mut self) {
        if !matches!(&self.screen, Screen::Notebook(nb) if !nb.overview) {
            return;
        }
        let (x, y, ..) = lock_rect();
        self.fb.fill_rect(x, y, 80, 64, gray565(255));
        rect_border(&mut self.fb, x, y, 80, 64, 3, 20);
        // 锁体
        self.fb.fill_rect(x + 26, y + 32, 28, 22, 20);
        // 锁梁：锁着左右两根都连到锁体；解锁只留左柱
        let top = y + 14;
        self.fb.fill_rect(x + 30, top, 4, 20, 20);
        if self.touch_lock {
            self.fb.fill_rect(x + 46, top, 4, 20, 20);
            self.fb.fill_rect(x + 30, top, 20, 4, 20);
        } else {
            self.fb.fill_rect(x + 30, top, 20, 4, 20);
            self.fb.fill_rect(x + 46, top, 4, 10, 20);
        }
    }

    /// AI 引用框 / 左上角 `AI n` 角标（`docs/AI-REFS.md`）。
    ///
    /// 坐标是**设备像素**（页面左上为原点），和 `selectai::Rect` 同一套，
    /// 所以过 `view.page_to_screen` 就行（`paint_overlays` 里已经算好 view）。
    /// 只写像素、不推面板：整屏刷新 / 局部重绘都由调用方决定。
    /// 当前是不是横屏（整幅画面转 90°，见 `fb::set_landscape`）。
    fn landscape(&self) -> bool {
        rmdev::fb::landscape()
    }

    /// 当前页在**文档空间**里的矩形 `(x, y, w, h)`。
    ///
    /// 文档空间就是竖屏画布 1404×1872：PDF/电子书的底图是按这个尺寸 contain
    /// 渲好后直接铺满画布的（见 `render_doc_bg`），所以"页面在画布上占哪块"
    /// 必须记下来 —— 横屏放大时要把这块撑满屏幕，而不是把带白边的整块画布
    /// 一起放大（那样页面反而更小）。笔记本没有底图，整块画布就是页。
    fn page_box(&self) -> (f32, f32, f32, f32) {
        match self.page_box {
            Some((x, y, w, h)) => (x as f32, y as f32, w as f32, h as f32),
            None => (0.0, 0.0, self.canvas.w as f32, self.canvas.h as f32),
        }
    }

    /// 把页面框铺进当前屏幕：等比缩放到"刚好装下"，居中。
    ///
    /// 竖屏时框就是整块画布、屏幕也同尺寸 → identity（1:1 贴图那条快路）。
    /// 横屏时只往**工具条右边那块**铺：铺满整个 1872 宽的话，页面最左 120 px
    /// 会被工具条压住 —— 讲义的标题/公式常常就贴着左边，真机上直接看不见。
    fn box_view(&self) -> PageView {
        let (bx, by, bw, bh) = self.page_box();
        let (sw, sh) = (self.fb.width as f32, self.fb.height as f32);
        let land = self.landscape();
        if !land
            && (bw - sw).abs() < 0.5
            && (bh - sh).abs() < 0.5
            && bx.abs() < 0.5
            && by.abs() < 0.5
        {
            return PageView::identity();
        }
        // 可用区域：横屏从工具条右边开始
        let (ax, aw) = if land {
            let (p0, p1) = paper_x();
            (p0 as f32, (p1 - p0) as f32)
        } else {
            (0.0, sw)
        };
        let s = (aw / bw).min(sh / bh);
        let (cx, cy) = (bx + bw * 0.5, by + bh * 0.5);
        let (pw, ph) = (rmdev::viewport::PAGE_W, rmdev::viewport::PAGE_H);
        PageView {
            scale: s,
            rot: 0,
            tx: (pw * 0.5 - cx) * s + (ax + aw * 0.5 - sw * 0.5),
            ty: (ph * 0.5 - cy) * s,
        }
    }

    /// 切横屏 / 竖屏（工具栏第 15 格、或打开横版页面时自动）。
    ///
    /// 三件事：
    /// 1. `fb` 换逻辑尺寸（1872×1404 ↔ 1404×1872），整屏重推；
    /// 2. 视图：横屏按 `page_box` 撑满屏；回竖屏把进之前那一眼原样还回去
    ///    （缩放/平移是用户的，不该被一次方向切换抹掉）；
    /// 3. 临时状态（选框/引用菜单/文档设置/滑动中）一律作废。
    ///
    /// **底图不重渲**：它永远按文档空间 1404×1872 渲。改了底图尺寸就等于
    /// 换了坐标系，已有笔迹（存的就是这块空间的坐标）会和页面错位 ——
    /// 宁可在横屏里把底图放大 1.33 倍（略软），也不动用户的数据。
    fn set_landscape(&mut self, on: bool) {
        if on == self.landscape() || !matches!(self.screen, Screen::Notebook(_)) {
            return;
        }
        if !self.fb.set_landscape(on) {
            return;
        }
        self.ai_menu = false;
        self.ai_menu_fresh = false;
        self.ai_view = None;
        self.ai_hold = None;
        self.ds.close();
        let saved = match &self.screen {
            Screen::Notebook(nb) => Some(nb.view),
            _ => None,
        };
        if on {
            self.land_back = saved;
            let v = self.box_view();
            if let Screen::Notebook(nb) = &mut self.screen {
                nb.clear_sel();
                nb.swipe = None;
                nb.view = v;
            }
        } else {
            let back = self.land_back.take();
            let v = back.unwrap_or_else(|| self.box_view());
            if let Screen::Notebook(nb) = &mut self.screen {
                nb.view = v;
            }
        }
        eprintln!(
            "[fb] {}（逻辑 {}×{}，scale={:.3}）",
            if on { "横屏" } else { "竖屏" },
            self.fb.width,
            self.fb.height,
            match &self.screen {
                Screen::Notebook(nb) => nb.view.scale,
                _ => 1.0,
            }
        );
        self.redraw_page();
    }

    /// 强制回竖屏：库、锁屏、睡眠图、竖屏专用浮层要用。
    ///
    /// 和 `set_landscape` 的区别是它不要求此刻在文档里（那些屏幕压根没有
    /// Notebook），所以视图直接复位成 identity。
    fn force_portrait(&mut self) {
        if !self.landscape() {
            return;
        }
        self.fb.set_landscape(false);
        self.land_back = None;
        self.land_manual = None;
        self.ai_menu = false;
        self.ai_menu_fresh = false;
        self.ai_view = None;
        self.ai_hold = None;
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.view = PageView::identity();
            nb.clear_sel();
            nb.swipe = None;
        }
        eprintln!("[fb] 强制竖屏（逻辑 {}×{}）", self.fb.width, self.fb.height);
    }

    /// 竖屏专用的整屏/贴边浮层是不是开着。
    ///
    /// 这些面板的几何全是按 1404×1872 量的（整屏 sheet、贴底键盘、贴底的
    /// 文档设置面板…），横屏画布只有 1404 高，画出来会缺一截。横屏里一开
    /// 就先转回竖屏：一次整屏重绘，比画出半块面板好。
    fn portrait_only_open(&self) -> bool {
        let nb = match &self.screen {
            Screen::Notebook(nb) if !nb.overview => nb,
            _ => return true,
        };
        self.ds.is_open()
            || self.toc.is_open()
            || self.tcfg.is_open()
            || self.nbset.is_open()
            || self.search.is_open()
            || self.pagesel.is_open()
            || self.move_to.is_some()
            || self.ask_ai.open
            || nb.text_edit.is_some()
            || nb.layer_rename.is_some()
            || nb.tag_edit.is_some()
            || matches!(nb.chrome.menu, Menu::Tags | Menu::Share | Menu::Layers)
    }

    /// 工具栏第 15 格：在横竖屏之间来回切。
    fn toggle_landscape(&mut self) {
        let on = !self.landscape();
        self.land_manual = Some(on);
        self.set_landscape(on);
    }

    /// 打开文档 / 翻页之后按页面方向自动选横竖屏：**宽比高大的页**（PPT 导出的
    /// PDF、16:9 / 4:3 的讲义）在竖屏里被宽度卡死，四周空一大片；转到横屏能大
    /// 一倍多，所以自动进横屏。笔记本页（1404×1872）永远是竖的，不会触发。
    ///
    /// 阈值 1.05 是"明显比正方形宽"：4:3 = 1.33、16:9 = 1.78 都进；接近 1:1 的
    /// 方形页留在竖屏。
    ///
    /// 不动视图、不重绘 —— 调用方（`open_at` / `goto_page`）马上要自己建
    /// Notebook / 重画整页，那时 `box_view()` 会给出对的变换。
    fn auto_landscape(&mut self, aspect: f32) {
        // 设置里关掉"自动横屏"：只有手动第 15 格能进
        if !self.auto_land || self.land_manual.is_some() {
            return;
        }
        let want = aspect > 1.05;
        if want == self.landscape() {
            return;
        }
        if self.fb.set_landscape(want) {
            self.ai_menu = false;
            self.ai_menu_fresh = false;
            self.ai_view = None;
            self.ai_hold = None;
        }
    }

    /// 开关"这一页的 AI 引用"菜单（工具栏第 14 格 / 长按后的 `View`）。
    fn ai_toggle_menu(&mut self) {
        self.ai_menu = !self.ai_menu;
        self.ai_menu_fresh = self.ai_menu;
        if self.ai_menu {
            self.ai_view = None;
            self.ai_hold = None;
        }
        self.ai_sync_chrome();
        if let Screen::Notebook(nb) = &self.screen {
            eprintln!("[ai] 引用菜单 {}（第 {} 页）", if self.ai_menu { "开" } else { "关" }, nb.page_index + 1);
        }
        self.redraw_page();
    }

    /// 选中菜单第 `i` 行：跳到那份回答 PDF 的第 `pdf_page` 页。
    fn ai_pick_row(&mut self, i: usize) {
        let marks = self.ai_marks_here();
        let Some(m) = marks.get(i).cloned() else {
            return;
        };
        self.ai_menu = false;
        self.ai_sync_chrome();
        eprintln!("[ai] 菜单选第 {} 条 → {}", i + 1, m.ai);
        self.open_at(&m.ai, m.pdf_page as usize);
    }

    /// 笔在菜单矩形里的点击也认（用户手里多半拿着笔）；矩形外照常写字。
    /// 返回 `true` = 这一下被菜单吃了。
    fn ai_menu_pen(&mut self, p: &PenEvent) -> bool {
        let n = self.ai_marks_here().len();
        let h = self.fb.height;
        let point = match p {
            PenEvent::Down(q) => (q.x, q.y),
            PenEvent::Up => match self.ai_last {
                Some(v) => v,
                None => return false,
            },
            _ => return false,
        };
        let Some(i) = rmdev::airefs::menu_row_at(n, h, point.0 as i32, point.1 as i32) else {
            return false;
        };
        match p {
            PenEvent::Down(q) => {
                self.ai_last = Some((q.x, q.y));
            }
            PenEvent::Up if self.ai_menu_fresh => self.ai_menu_fresh = false,
            PenEvent::Up => self.ai_pick_row(i),
            _ => {}
        }
        true
    }

    /// 把菜单状态同步给工具条（第 14 格要反白）。
    fn ai_sync_chrome(&mut self) {
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.chrome.ai_refs = self.ai_menu;
        }
    }

    /// 当前页的引用列表（菜单与命中都用它，保证行号一致）。
    fn ai_marks_here(&self) -> Vec<rmdev::airefs::Mark> {
        match &self.screen {
            Screen::Notebook(nb) if !nb.overview => self.ai_refs.marks(nb.page_index as u32),
            _ => Vec::new(),
        }
    }

    fn paint_ai_marks(&mut self, view: &PageView) {
        let page = match &self.screen {
            Screen::Notebook(nb) if !nb.overview => nb.page_index as u32,
            _ => return,
        };
        let marks = self.ai_refs.marks(page);
        // 纸面上只留**框选**提问的虚线框：它是"当时圈的是哪块"的锚点，本身不
        // 占地方。`AI n` 角标、整页提问的角标全部取消 —— 都收到第 14 格的菜单
        // 里（用户 2026-11-09：一个页面堆一堆角标太吵，看列表更顺手）。
        for m in marks.iter().filter(|m| m.boxed) {
            let (mx, my, mw, mh) = m.rect;
            let p0 = view.page_to_screen((mx as f32, my as f32));
            let p1 = view.page_to_screen(((mx + mw) as f32, (my + mh) as f32));
            let (x0, y0) = (p0.0 as i32, p0.1 as i32);
            let (x1, y1) = (p1.0 as i32, p1.1 as i32);
            let (w, h) = (x1 - x0, y1 - y0);
            let (px0, px1) = paper_x();
            if w < 8 || h < 8 || x1 < px0 || x0 >= px1 || y1 < 0 || y0 > self.fb.height {
                continue;
            }
            dashed_rect(
                &mut self.fb,
                RectF {
                    x: x0 as f32,
                    y: y0 as f32,
                    w: w as f32,
                    h: h as f32,
                },
                3,
                95,
            );
        }
        // `View` 弹层（手指长按之后才有）：白底 + 2px 黑框 + 居中文字。
        // 点它**不再直接跳转**，而是打开引用菜单 —— 一页上可能有好几个引用，
        // 先看清是哪一条再选（用户 2026-11-09）。文案是官方原文。
        if let Some((_, r)) = self.ai_view.clone() {
            self.fb.fill_rect(r.0 - 4, r.1 - 4, r.2 + 8, r.3 + 8, gray565(255));
            rect_border(&mut self.fb, r.0 - 4, r.1 - 4, r.2 + 8, r.3 + 8, 2, 20);
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, r.0 + 52, r.1 + 30, 32.0, "View", 20);
            }
        }
        if !self.ai_menu || marks.is_empty() {
            return;
        }
        // 菜单开着：每个**方框**引用的左上角顶点画一个箭头，编号和列表行一致，
        // 这样"列表第 3 条 ↔ 页面上哪个框"一眼就对上了。整页引用没有锚点，
        // 不画指向（用户明确要求）。
        for (i, m) in marks.iter().enumerate().take(6) {
            if !m.boxed {
                continue;
            }
            let p0 = view.page_to_screen((m.rect.0 as f32, m.rect.1 as f32));
            let p1 = view.page_to_screen(
                ((m.rect.0 + m.rect.2) as f32, (m.rect.1 + m.rect.3) as f32),
            );
            let (bx, by) = ai_arrow(
                &mut self.fb,
                p0.0 as i32,
                p0.1 as i32,
                p1.0 as i32 - p0.0 as i32,
                p1.1 as i32 - p0.1 as i32,
                (i + 1) as u32,
            );
            let num = format!("{}", i + 1);
            if let Some(t) = self.text.as_mut() {
                t.draw_bold(&mut self.fb, bx + 13, by + 30, 28.0, &num, 20);
            }
        }
        // 列表本体：白底 + 2px 黑框，一行一条（回答的第一行），点行跳转。
        let (mx, my, mw, _mh) = rmdev::airefs::menu_rect(marks.len(), self.fb.height);
        self.fb
            .fill_rect(mx, my, mw, marks.len().min(6) as i32 * rmdev::airefs::MENU_ROW + 8, gray565(255));
        rect_border(
            &mut self.fb,
            mx,
            my,
            mw,
            marks.len().min(6) as i32 * rmdev::airefs::MENU_ROW + 8,
            2,
            20,
        );
        for (i, m) in marks.iter().enumerate().take(6) {
            let top = my + 4 + i as i32 * rmdev::airefs::MENU_ROW;
            if i + 1 < marks.len().min(6) {
                self.fb
                    .fill_rect(mx + 20, top + rmdev::airefs::MENU_ROW - 2, mw - 40, 2, gray565(200));
            }
            let num = format!("{}", i + 1);
            if let Some(t) = self.text.as_mut() {
                t.draw_bold(&mut self.fb, mx + 22, top + 44, 30.0, &num, 20);
                let label = ai_first_line(m, t, mw - 120);
                t.draw(&mut self.fb, mx + 74, top + 44, 28.0, &label, 20);
            }
        }
    }

    /// 当前文档是不是一份 AI 回答 PDF（服务器建文档时写了 `aiAnswer: true`）。
    /// 是的话，在它上面提问就是「追问」：`parent` 指向它自己。
    fn ai_parent(&self, uuid: &str) -> String {
        let p = self.library.root.join(format!("{uuid}.metadata"));
        let v: serde_json::Value = std::fs::read(p)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if v.get("aiAnswer").and_then(|x| x.as_bool()).unwrap_or(false) {
            uuid.to_string()
        } else {
            String::new()
        }
    }

    /// 手指落点下面有没有 AI 引用（屏幕坐标 → 页面坐标再命中）。
    fn ai_ref_hit(&self, x: f32, y: f32) -> Option<rmdev::airefs::Mark> {
        let (page, view) = match &self.screen {
            Screen::Notebook(nb) if !nb.overview => (nb.page_index as u32, nb.view),
            _ => return None,
        };
        let p = view.screen_to_page((x, y));
        // 整页引用现在只活在菜单里（纸面上没有角标），长按只对"框"生效
        self.ai_refs
            .hit(page, p.0 as i32, p.1 as i32)
            .filter(|m| m.boxed)
    }

    /// 把盖在纸面上的所有浮层画一遍（文字、选区、工具条、各面板、×、页码）。
    ///
    /// 只写帧缓冲、不推面板 —— 调用方决定刷哪块。`redraw_page` 是"整屏 + 一次 full"，
    /// `repaint_rects` 是"几小块 + partial"，后者不会闪屏。
    fn paint_overlays(&mut self) {
        let t0 = std::time::Instant::now();
        // Table of contents 是整页目录：白底盖掉页总览和一切浮层
        if self.toc.is_open() {
            self.toc.draw(&mut self.fb, &mut self.text);
            return;
        }
        // Text settings 是整屏 sheet：白底盖掉纸面、工具条和所有浮层
        if self.tcfg.is_open() {
            self.tcfg.draw(&mut self.fb, &mut self.text);
            return;
        }
        // Notebook settings 是整屏设置页：白底盖掉纸面、工具条和所有浮层
        if self.nbset.is_open() {
            self.paint_nbset();
            return;
        }
        let view = match &self.screen {
            Screen::Notebook(nb) => nb.view,
            _ => PageView::identity(),
        };
        // === P6 begin ===
        // 搜索条打开时正文整体下移 HDR_H（纯函数在 docsearch::body_offset）。
        let view = self.p6_display_view(view);
        // === P6 end ===
        // 页面上的文字（我们自己的存储，画在墨迹之上、工具条之下）
        let texts: Vec<TextItem> = match &self.screen {
            Screen::Notebook(nb) => nb.texts.clone(),
            _ => Vec::new(),
        };
        for it in &texts {
            // 文字坐标是页面坐标（落笔时已加过 PAGE_DX/PAGE_DY），也要过视图变换。
            // 字号没法跟着缩放：`Text::draw` 没有 scale 参数，这里只搬位置。
            let p = view.page_to_screen((it.x as f32 + PAGE_DX, it.y as f32 + PAGE_DY));
            let px = p.0 as i32;
            let py = p.1 as i32;
            if under_band(px) {
                continue;
            }
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, px, py, it.size, &it.text, 20);
            }
        }
        let t_after_text = std::time::Instant::now();
        // AI 引用框/角标：画在纸面和文字之上、工具条之下
        self.paint_ai_marks(&view);
        // 套索折线 / 选框 / 悬浮条：画在纸面和文字之上、工具条之下
        self.paint_selection();
        let t_sel = std::time::Instant::now();
        if let Screen::Notebook(nb) = &self.screen {
            clip_for_chrome(&mut self.canvas, nb.chrome.visible);
            nb.chrome.draw_in(&mut self.fb, &mut self.text);
        }
        self.paint_lock_chip();
        let t_chrome = std::time::Instant::now();
        if matches!(&self.screen, Screen::Notebook(nb) if nb.text_edit.is_some()) {
            self.paint_text_editor();
        }
        // 先把要画哪个面板取出来，避免借用冲突
        let menu = match &self.screen {
            Screen::Notebook(nb) => nb.chrome.menu,
            _ => Menu::None,
        };
        if menu == Menu::Share {
            self.paint_share_panel();
        } else if menu == Menu::Layers {
            self.paint_layers_panel();
        } else if menu == Menu::Tags {
            self.paint_tags_panel();
        }
        if matches!(&self.screen, Screen::Notebook(nb) if nb.tag_edit.is_some()) {
            self.paint_tag_editor();
        }
        // 图层改名键盘盖住所有面板（和 Add tag 同一层）
        if matches!(&self.screen, Screen::Notebook(nb) if nb.layer_rename.is_some()) {
            self.paint_layer_rename();
        }
        self.paint_close();
        let t_panels = std::time::Instant::now();
        // 文档设置面板：盖在工具条和其它面板之上、页脚之下（官方的 foldout）
        self.paint_docsettings();
        self.paint_footer();
        // 文档内搜索条 + 命中高亮 + 键盘：最上层（官方 `SearchResultsHeader` z=1）
        self.paint_search();
        // === P7 begin ===
        // 文档页滑块画在**最上面**（官方注释：`The PageSlider is deliberately
        // stacked over the top of the toolbox`）：贴底白带 + 8 px 黑条 + 游标，
        // 底下一行 `Page 3 of 41`；按着拖的时候头顶浮一个预览框（有页总览缩略图
        // 就贴图，没有就画大号 `Page 3`）。这里只写像素，刷哪块由 `p7_repaint` 定。
        let p7_ctx = if self.slider.is_some() {
            match &self.screen {
                Screen::Notebook(nb) if !nb.overview => {
                    Some((nb.uuid.clone(), nb.page_count.max(1)))
                }
                _ => None,
            }
        } else {
            None
        };
        // 剪贴板提示条（P13）也是贴底的黑条，别互相盖
        let p7_toast = match &self.screen {
            Screen::Notebook(nb) => rmdev::chrome::clip_toast_visible(
                !self.clipboard.is_empty(),
                nb.chrome.selecting,
            ),
            _ => false,
        };
        if let (Some((uuid, total)), Some((_, page, st))) = (p7_ctx, self.slider) {
            if !p7_toast {
                let (w, h) = (self.fb.width, self.fb.height);
                let dragging = st == slider::State::Pressed;
                let thumb = if dragging {
                    self.p7_thumb(&uuid, page)
                } else {
                    None
                };
                let t = thumb
                    .as_ref()
                    .map(|(px, tw, th)| (px.as_slice(), *tw, *th));
                slider::paint(&mut self.fb, &mut self.text, t, w, h, page + 1, total, dragging);
            }
        }
        // === P7 end ===
        // === D6 begin ===
        // Live Note 文字带 + 顶栏按钮：工具条、页脚之后（带子横跨整屏，工具条会
        // 把左边那列刷白），D1 的卡片/键盘之前（那些要压在带子上面）。
        self.d6_paint_overlays();
        // === D6 end ===
        // === D1 begin: 浮层 / 框 / 内联回答卡片 / 提问键盘（最上层） ===
        self.d1_paint();
        // === D1 end ===
        // === E2 begin ===
        self.e2_paint_ocr();
        // === E2 end ===
        if env_flag("RM_TIMING") {
            eprintln!(
                "[ui] overlay 文字={}us 选区={}us 工具条+菜单={}us 面板={}us 页脚={}us",
                (t_after_text - t0).as_micros(),
                (t_sel - t_after_text).as_micros(),
                (t_chrome - t_sel).as_micros(),
                (t_panels - t_chrome).as_micros(),
                t_panels.elapsed().as_micros()
            );
        }
    }

    /// 只重画页面上的某块矩形：把 canvas 那块清零、按顺序重涂所有笔迹，
    /// 再贴回屏幕 + 补浮层 + partial 刷新。撤销/重做/橡皮用它，避免整屏 flash。
    fn repaint_page_rect(&mut self, r: (i32, i32, i32, i32)) {
        self.canvas.clear_rect(r);
        let (lines, grade): (Vec<Line>, usize) = match &self.screen {
            Screen::Notebook(nb) => (
                nb.page.visible_lines().cloned().collect(),
                nb.chrome.pencil_grade,
            ),
            _ => (Vec::new(), rmdev::pen::pencil::DEFAULT),
        };
        // 只重涂**和这块矩形相交**的笔迹：一页写过几百笔之后，整页重涂一次要
        // 几百毫秒（每笔逐点算覆盖），橡皮一抬笔就卡；而实际相交的通常只有几笔。
        let (rx0, ry0) = (r.0 as f32, r.1 as f32);
        let (rx1, ry1) = ((r.0 + r.2) as f32, (r.1 + r.3) as f32);
        for l in &lines {
            let Some(b) = line_bbox(l) else { continue };
            // 页面坐标 → 屏幕坐标（画布就是页面坐标，偏移是 PAGE_DX/DY）
            const PAD: f32 = 8.0; // 笔宽上限的量级，够盖住笔画两端的圆头
            let lx0 = b.x + PAGE_DX - PAD;
            let ly0 = b.y + PAGE_DY - PAD;
            let lx1 = b.x + b.w + PAGE_DX + PAD;
            let ly1 = b.y + b.h + PAGE_DY + PAD;
            if lx1 < rx0 || rx1 < lx0 || ly1 < ry0 || ry1 < ly0 {
                continue;
            }
            self.canvas
                .ink_line(l, self.pixel_scale, (PAGE_DX, PAGE_DY), grade);
        }
        self.canvas.solid_ink();
        // 重涂整页会把 canvas 脏区撑成"所有笔迹的并集"，不清掉的话下一次
        // `flush` 会拿那块去推面板（等于又刷了半屏）。这里只要那块矩形。
        self.canvas.clear_dirty();
        // `r` 是**页面坐标**（canvas 也是页面坐标）：放大/旋转后它在屏幕上的位置
        // 不再是同一个矩形，得先过一遍视图变换，否则撤销只刷新了错的地方，
        // 屏幕上会留着刚撤掉的那一笔。
        let view = match &self.screen {
            Screen::Notebook(nb) => nb.view,
            _ => PageView::identity(),
        };
        // === P6 begin ===
        // 搜索条打开时正文整体下移 HDR_H（纯函数在 docsearch::body_offset）。
        let view = self.p6_display_view(view);
        // === P6 end ===
        let sr = if view_identity(&view) {
            r
        } else {
            self.canvas.screen_rect_of(&view, r)
        };
        // 工具条那 4 格的状态（撤销/重做是否变灰）可能也变了
        self.repaint_rects(
            &[sr, (rmdev::chrome::band_x(), TOP_CELLS_Y, CHROME_W, 4 * 120), (rmdev::chrome::band_x(), 600, CHROME_W, 240)],
            WAVEFORM_MODE_GC16,
        );
    }

    /// 擦选区拖框的实时预览（6d）：把上一帧的虚线框用纸面补回来，再在新位置画框。
    ///
    /// 框是直接画进帧缓冲的（不进 canvas），所以擦旧框只能从 canvas 把那一块
    /// 重贴回来；刷新本身交给 `flush_pending` 的 `sel_dirty`（一批事件只推一次屏）。
    fn repaint_erase_box(
        &mut self,
        old: Option<((f32, f32), (f32, f32))>,
        now: ((f32, f32), (f32, f32)),
    ) {
        let view = match &self.screen {
            Screen::Notebook(nb) => nb.view,
            _ => return,
        };
        let rect_of = |p: ((f32, f32), (f32, f32))| RectF {
            x: p.0.0.min(p.1.0),
            y: p.0.1.min(p.1.1),
            w: (p.0.0 - p.1.0).abs(),
            h: (p.0.1 - p.1.1).abs(),
        };
        let px = |r: RectF| {
            (
                (r.x - 4.0).floor() as i32,
                (r.y - 4.0).floor() as i32,
                r.w.ceil() as i32 + 8,
                r.h.ceil() as i32 + 8,
            )
        };
        let mut dirty = px(rect_of(now));
        if let Some(o) = old {
            dirty = union_rect(dirty, px(rect_of(o)));
        }
        // 纸面补回来（canvas 里没有虚线框），再补被盖住的浮层，最后画新框
        self.canvas.paint_view_rect(&mut self.fb, &view, dirty);
        self.repaint_overlays(dirty.0, dirty.1, dirty.2, dirty.3);
        dashed_rect(&mut self.fb, rect_of(now), 2, 20);
        self.sel_dirty = Some(match self.sel_dirty {
            Some(u) => union_rect(u, dirty),
            None => dirty,
        });
    }

    /// 只重画 + 只刷新给定的几块屏幕矩形（**partial** 刷新）。
    ///
    /// 官方几乎从不整屏刷新：点工具条、开关菜单都只刷那一小块。我们以前每个动作都
    /// `redraw_page`（整屏 GC16 + 250ms 限速），所以"又慢又闪"。
    fn repaint_rects(&mut self, rects: &[(i32, i32, i32, i32)], waveform: u32) {
        if rects.is_empty() {
            return;
        }
        let t_start = std::time::Instant::now();
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.sync_undo();
        }
        let view = match &self.screen {
            Screen::Notebook(nb) => nb.view,
            _ => PageView::identity(),
        };
        // === P6 begin ===
        // 搜索条打开时正文整体下移 HDR_H（纯函数在 docsearch::body_offset）。
        let view = self.p6_display_view(view);
        // === P6 end ===
        // **贴图之前**就得把裁剪线改成"这一帧该有的样子"：`paint_view_rect` 是从
        // `clip_l` 开始贴的。收起工具条时 `clip_l` 还停在 120，那 0..120 那一列
        // 一个像素都贴不出去 → 屏幕保留上一帧的工具条，看起来就是"点了收不回去"。
        if let Screen::Notebook(nb) = &self.screen {
            clip_for_chrome(&mut self.canvas, nb.chrome.visible);
        }
        for &(x, y, w, h) in rects {
            self.canvas.paint_view_rect(&mut self.fb, &view, (x, y, w, h));
        }
        let t_blit = t_start.elapsed();
        self.paint_overlays();
        let t_ov = t_start.elapsed();
        for &(x, y, w, h) in rects {
            let x = x.max(0);
            let y = y.max(0);
            let w = w.min(self.fb.width - x);
            let h = h.min(self.fb.height - y);
            if w > 0 && h > 0 {
                self.fb.refresh(x, y, w, h, waveform, rmdev::fb::UPDATE_MODE_PARTIAL);
            }
        }
        if env_flag("RM_TIMING") {
            let n: i64 = rects.iter().map(|r| r.2 as i64 * r.3 as i64).sum();
            eprintln!(
                "[ui] repaint {} 块 {n}px 贴图={}us 浮层={}us 刷新={}us",
                rects.len(),
                t_blit.as_micros(),
                (t_ov - t_blit).as_micros(),
                (t_start.elapsed() - t_ov).as_micros()
            );
        }
    }

    /// 当前页内容的指纹。便宜（只数点数、不做任何光栅化）。
    fn page_fingerprint(&self) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut mix = |v: u64, h: &mut u64| {
            *h ^= v;
            *h = h.wrapping_mul(0x100_0000_01b3);
        };
        if let Screen::Notebook(nb) = &self.screen {
            mix(nb.chrome.pencil_grade as u64, &mut h);
            mix((self.pixel_scale * 1000.0) as u64, &mut h);
            mix(self.canvas.clip_l as u64, &mut h);
            // 底图（PDF/电子书页）换了必须让光栅缓存失效
            mix(self.bg_gen, &mut h);
            for l in nb.page.visible_lines() {
                mix(l.tool as u64, &mut h);
                mix(l.points.len() as u64, &mut h);
                if let Some(p) = l.points.last() {
                    mix(p.x.to_bits() as u64, &mut h);
                    mix(p.y.to_bits() as u64, &mut h);
                }
            }
        }
        h
    }

    /// 某一页的指纹（总览缩略图用；和 `page_fingerprint` 同构，但按页取）。
    fn page_fp(&self, uuid: &str, idx: usize) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let page = self.library.load_page_at(uuid, idx);
        for l in page.visible_lines() {
            h ^= l.tool as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
            h ^= l.points.len() as u64;
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h
    }

    /// 命中就返回缓存下标并刷新 LRU 顺序。
    fn raster_hit(&mut self, key: &(String, usize), fp: u64) -> Option<usize> {
        let i = self.raster.iter().position(|(k, f, _)| k == key && *f == fp)?;
        let e = self.raster.remove(i);
        self.raster.push(e);
        Some(self.raster.len() - 1)
    }

    fn raster_store(&mut self, key: (String, usize), fp: u64, buf: Vec<u8>) {
        self.raster.retain(|(k, _, _)| k != &key);
        self.raster.push((key, fp, buf));
        if self.raster.len() > 4 {
            self.raster.remove(0);
        }
    }

    fn redraw_page(&mut self) {
        let t_all = std::time::Instant::now();
        let mut t_gap = Duration::ZERO;
        // 撤销/重做按钮的可用态（官方图标不可用时会变灰）
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.sync_undo();
        }
        // 面板没画出来时命中区必须为空，否则会点到"已经不存在的面板"
        self.layer_hits.clear();
        // rm2fb 不支持 WAIT_FOR_UPDATE，整屏大更新之间只能自己限速：
        // 两次 GC16 叠着排队会把 EPDC 队列卡死（进程活、ioctl 不返回）。
        const FULL_GAP: Duration = Duration::from_millis(250);
        if let Some(t) = self.last_full {
            let dt = t.elapsed();
            if dt < FULL_GAP {
                let t0 = std::time::Instant::now();
                std::thread::sleep(FULL_GAP - dt);
                t_gap = t0.elapsed();
            }
        }
        self.last_full = Some(std::time::Instant::now());
        // 整屏重画会把所有浮层一次画齐，攒着的选区脏区就没用了
        self.sel_dirty = None;
        // 工具条那一条归谁：展开时归工具条（纸面不画过去），收起时整条归纸面。
        // **必须在 `paint_all` 之前设**：`paint_all` 依赖 `clip_l` 决定从哪一列开始写。
        // 以前这里放在后面，结果收起那一帧纸面只写了 x>=120，0..120 还是上一帧的
        // 工具条像素，看起来就是"收了但图标还在 + 圆环上多出一个点"。
        if let Screen::Notebook(nb) = &self.screen {
            clip_for_chrome(&mut self.canvas, nb.chrome.visible);
        }
        self.canvas.set_bg(self.pdf_bg.clone());
        let t_bg = t_all.elapsed();
        let key = match &self.screen {
            Screen::Notebook(nb) => Some((nb.uuid.clone(), nb.page_index)),
            _ => None,
        };
        let fp = self.page_fingerprint();
        // 内容没变就直接贴回上次的光栅：开菜单/换工具/切横竖屏都不用重画笔迹。
        let mut restored = false;
        if let Some(k) = &key {
            if let Some(i) = self.raster_hit(k, fp) {
                if self.canvas.set_dark_buf(&self.raster[i].2) {
                    restored = true;
                }
            }
        }
        if !restored {
            self.canvas.clear();
            let (lines, grade) = match &self.screen {
                Screen::Notebook(nb) => (
                    nb.page.visible_lines().cloned().collect::<Vec<_>>(),
                    nb.chrome.pencil_grade,
                ),
                _ => (Vec::new(), rmdev::pen::pencil::DEFAULT),
            };
            // 铅笔硬度不写进 .rm（格式里没有这个字段），重绘按**当前**档位还原浓淡；
            // 笔画内的深浅变化来自每点存的压力，那部分是准的。
            for l in &lines {
                self.canvas
                    .ink_line(l, self.pixel_scale, (PAGE_DX, PAGE_DY), grade);
            }
            if let Some(k) = key {
                self.raster_store(k, fp, self.canvas.dark_buf().to_vec());
            }
        }
        self.canvas.solid_ink();
        // 纸面按视图变换贴到屏幕上。1:1 时 `page_to_screen` 就是"原样"，
        // 逐像素和 `paint_all` 等价（取样点 +0.5，截断后仍是同一像素），
        // 那就走便宜的那条路；只有真缩放/平移过才做逐像素逆变换。
        let view = match &self.screen {
            Screen::Notebook(nb) => nb.view,
            _ => PageView::identity(),
        };
        // === P6 begin ===
        // 搜索条打开时正文整体下移 HDR_H（纯函数在 docsearch::body_offset）。
        let view = self.p6_display_view(view);
        // === P6 end ===
        let t_ink = t_all.elapsed();
        if blit_identity(&view, (self.canvas.w, self.canvas.h), (self.fb.width, self.fb.height)) {
            self.canvas.paint_all(&mut self.fb);
        } else {
            self.canvas.paint_view(&mut self.fb, &view);
        }
        let t_blit = t_all.elapsed();
        self.paint_overlays();
        let t_ov = t_all.elapsed();
        // 整屏已经重画进帧缓冲了，canvas 的脏区作废。不清的话下一次 flush 会把
        // 整屏按 canvas 重写一遍，把刚画的 chrome 浮层（尤其收起态的圆环）抹掉。
        self.canvas.clear_dirty();
        let t_paint = t_all.elapsed();
        self.fb.refresh(
            0,
            0,
            self.fb.width,
            self.fb.height,
            WAVEFORM_MODE_GC16,
            UPDATE_MODE_FULL,
        );
        if env_flag("RM_TIMING") {
            eprintln!(
                "[ui] redraw_page 总计={}ms（bg={}ms 画墨={}ms 贴图={}ms 浮层={}ms 刷新={}ms 限速={}ms）",
                t_all.elapsed().as_millis(),
                t_bg.as_millis(),
                (t_ink - t_bg).as_millis(),
                (t_blit - t_ink).as_millis(),
                (t_ov - t_blit).as_millis(),
                (t_paint - t_ov).as_millis(),
                t_gap.as_millis()
            );
        }
    }

    /// 文档设置面板（左侧工具条最下面那格的下拉）。
    ///
    /// 布局/命中全在 `panels::docsettings` 里，这里只负责把当前文档的
    /// 类型、标题、页码喂进去。
    fn paint_docsettings(&mut self) {
        if !self.ds.is_open() {
            return;
        }
        let (title, page, pages) = match &self.screen {
            Screen::Notebook(nb) => (nb.name.clone(), nb.page_index + 1, nb.page_count),
            _ => return,
        };
        let kind = self.doc_kind();
        // `rect()` 没有 title 参数，表头高度读的是缓存的那份标题
        self.ds.set_title(&title);
        if let Some(t) = self.text.as_mut() {
            self.ds
                .paint(&mut self.fb, t, kind, &title, page, pages);
        }
    }

    /// 打开文档设置面板并缓存标题（`ds.rect()` 要用它算表头高度）。
    fn open_ds_panel(&mut self) {
        let (title, kind) = match &self.screen {
            Screen::Notebook(nb) => (nb.name.clone(), self.doc_kind()),
            _ => return,
        };
        self.ds.set_title(&title);
        self.ds.toggle(kind);
        self.redraw_page();
    }

    /// 当前文档类型：PDF 走 PDF 那套菜单（多一行 Adjust view），其余走笔记本。
    ///
    /// `Notebook` 里没有 is_pdf 字段，这里沿用现有的 PDF 依据——只有 PDF 才会
    /// 渲染底图（`open_at` / `goto_page` 里设的 `pdf_bg`）。
    fn doc_kind(&self) -> docsettings::Kind {
        match &self.doc {
            Some(d) if d.format().is_ebook() => docsettings::Kind::Ebook,
            Some(_) => docsettings::Kind::Pdf,
            None => docsettings::Kind::Notebook,
        }
    }

    /// 文档设置面板的点击。返回 true = 这一下被面板吃掉了，调用方别再往下分派。
    ///
    /// 顺序照面板的语义：命中某行就执行动作；没命中但在面板里只重绘
    /// （开/关子菜单、表头都是这类）；点在面板外就收面板再重绘。
    fn ds_tap(&mut self, x: f32, y: f32) -> bool {
        if !self.ds.is_open() {
            return false;
        }
        let kind = self.doc_kind();
        // `click()` 里普通行会顺手把面板收起来，rect() 之后就没有了 ——
        // 收起后那块像素要补回纸面，所以先记位置。
        let leftover = self.ds.rect(kind);
        match self.ds.click(kind, x, y) {
            Some(act) => {
                eprintln!("[ds] 点到 {act:?}");
                self.ds_action(act);
                // 打开文档内搜索时，面板表头会从键盘上沿露出一条：补掉。
                // （Notebook settings 走整页重绘，不需要。）
                if self.search.is_open() {
                    if let Some(r) = leftover {
                        self.repaint_rects(&[r], WAVEFORM_MODE_GC16);
                    }
                }
            }
            None => {
                let inside = self
                    .ds
                    .rect(kind)
                    .map(|(rx, ry, rw, rh)| {
                        x >= rx as f32
                            && y >= ry as f32
                            && x < (rx + rw) as f32
                            && y < (ry + rh) as f32
                    })
                    .unwrap_or(false);
                if !inside {
                    eprintln!("[ds] 点在面板外 → 收起");
                    self.ds.close();
                }
                self.redraw_page();
            }
        }
        true
    }

    /// 换排版参数（字号/行距/边距）：重排 → 页数可能变 → 重渲当前页。
    ///
    /// 页号保持不变（夹到新页数内）：读到大半本再调字号，不该被弹回第一页。
    fn set_reading_layout(&mut self, cfg: rmdev::doc::LayoutCfg) {
        let Some(d) = self.doc.as_mut() else {
            return;
        };
        if !d.set_layout(cfg) {
            return;
        }
        let n = d.pages();
        self.reading_cfg = cfg;
        // 按文档持久化（readcfg.json）：下次打开这本还是这套排版
        if let Some(uuid) = &self.doc_uuid {
            textcfg::save_cfg(uuid, cfg);
        }
        let (uuid, cur) = match &self.screen {
            Screen::Notebook(nb) => (nb.uuid.clone(), nb.page_index),
            _ => return,
        };
        // 页号尽量不动：读到大半本再调字号，不该被弹回第一页
        let idx = cur.min(n.saturating_sub(1));
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.page_count = n.max(1);
            nb.page_index = idx;
        }
        // **不能走 `goto_page`**：它的第一条判据就是「目标页 == 当前页就直接返回」，
        // 而改字号恰恰是「页号不变、页内容全变」，会被它当成无事发生。
        if let Some((bg, _, _, _)) = self.render_doc_bg(&uuid, idx) {
            self.pdf_bg = Some(bg);
                                self.bg_gen = self.bg_gen.wrapping_add(1);
        }
        let label = self.ds.text_size_label();
        self.banner = Some(format!("Text size: {label}"));
        self.redraw_page();
    }

    /// 渲染文档第 `idx` 页的整屏底图（contain 居中）。返回 `(灰度, 路径, ww, hh)`。
    ///
    /// 抽出来是为了给两条路径共用：翻页（`goto_page`）和**不翻页但底图变了**
    /// （改字号重排）。后者页号不变，`goto_page` 会直接早退。
    fn render_doc_bg(&mut self, uuid: &str, idx: usize) -> Option<(Vec<u8>, String, usize, usize)> {
        let d = self.doc.as_ref()?;
        // 底图永远按**文档空间**渲（不跟屏幕方向走）：笔迹存的就是这块空间的坐标。
        let (fw, fh) = (DOC_W, DOC_H);
        let idx = idx.min(d.pages().saturating_sub(1));
        // Doc::aspect 统一是 w/h
        let asp = d.aspect(idx).clamp(0.05, 20.0);
        let mut ww = (fh as f32 * asp).round() as usize;
        let mut hh = fh;
        if ww > fw {
            ww = fw;
            hh = ((fw as f32) / asp).round() as usize;
        }
        let g = d.render(idx, ww, hh).ok()?;
        let mut bg = vec![255u8; fw * fh];
        let ox = (fw - ww) / 2;
        let oy = (fh - hh.min(fh)) / 2;
        for y in 0..hh.min(fh) {
            let src = y * ww;
            let dst = (oy + y) * fw + ox;
            bg[dst..dst + ww].copy_from_slice(&g[src..src + ww]);
        }
        let path = self
            .library
            .find(uuid)
            .map(|b| self.document_path(&b).display().to_string())
            .unwrap_or_default();
        self.bg_gen = self.bg_gen.wrapping_add(1);
        self.page_box = Some((ox, oy, ww, hh.min(fh)));
        Some((bg, path, ww, hh))
    }

    /// 面板动作的副作用。开/关、二级菜单、横竖文案、当前 fit 全由 `click` 改，
    /// 这里只做它做不了的事。
    fn ds_action(&mut self, act: docsettings::Act) {
        match act {
            docsettings::Act::TextSize => self.open_text_settings(),
            docsettings::Act::NewPage => {
                // 复用总览里那条"加页"路径：library 插入一页 → goto_page 存当前页、
                // 载入新页、整页重绘。
                let (uuid, cur) = match &self.screen {
                    Screen::Notebook(nb) => (nb.uuid.clone(), nb.page_index),
                    _ => return,
                };
                match self.library.add_page(&uuid, cur) {
                    Ok(at) => {
                        eprintln!("[ds] 新增第 {} 页", at + 1);
                        self.goto_page(at);
                    }
                    Err(e) => {
                        eprintln!("[ds] 加页失败：{e}");
                        // === P3 begin ===
                        self.notice(NoticeKind::Error, &format!("Could not add page: {e}"));
                        // === P3 end ===
                    }
                }
            }
            // === D5 begin ===
            // 手写转文字：当前页 → 后台 OCR → 写文字层（C14，见本文件 D5 段）
            docsettings::Act::ConvertText => self.d5_convert_text(),
            // === D5 end ===
            // === C12 begin ===
            // AI 批改：当前页 → 服务器（mode=grade）→ 批注写进 AI 图层（见本文件 C12 段）
            docsettings::Act::GradePage => self.c12_grade_page(),
            // === C12 end ===
            docsettings::Act::SetFit(fit) => {
                if let docsettings::Fit::Width | docsettings::Fit::Height = fit {
                    // 页面尺寸 = 画布尺寸：画布坐标就是页面坐标（1404×1872）
                    let screen = (self.fb.width as f32, self.fb.height as f32);
                    let page = (self.canvas.w as f32, self.canvas.h as f32);
                    // 横屏时页面在屏幕上的宽高是反的，fit 也得按反的算；
                    // 而且换 fit 不该把横竖屏切回竖屏（那是 orientation 那行的事）。
                    let rot = match &self.screen {
                        Screen::Notebook(nb) => nb.view.rot,
                        _ => 0,
                    };
                    let dims = if rot % 180 == 0 {
                        page
                    } else {
                        (page.1, page.0)
                    };
                    let mut v = match fit {
                        docsettings::Fit::Width => PageView::fit_width(screen, dims),
                        _ => PageView::fit_height(screen, dims),
                    };
                    v.set_rot(rot);
                    v.clamp_pan_of(page);
                    if let Screen::Notebook(nb) = &mut self.screen {
                        nb.view = v;
                    }
                    eprintln!("[view] fit {} → scale={:.3}", fit.status(), v.scale);
                } else {
                    // Use current view：保留当前 (scale, tx, ty) 不动，只重绘
                    eprintln!("[view] fit {} → 保留当前视图", fit.status());
                }
                self.redraw_page();
            }
            // 文档内搜索 / Notebook settings 各自的实现都在 panels 里
            docsettings::Act::Search => self.open_doc_search(),
            docsettings::Act::NotebookSettings => self.open_nb_settings(),
            // PDF 和笔记本共用一个 sheet（名字 / 封面 / 模板 / 方向）；
            // 只有封面那行标题按 kind 显示 `PDF cover`（官方 QML 原文）
            docsettings::Act::PdfSettings => self.open_nb_settings(),
            docsettings::Act::ToggleLandscape => {
                // 横竖屏：整页（按 `nb.view`）转 90°，再按"横版长边正好落在屏幕宽里"
                // 缩放并居中夹取；再点一次把转之前那一眼原样还回去。
                //
                // 落笔/擦除/撤销本来就过 `nb.view`（`screen_to_page` / `paint_view`），
                // 所以旋转后坐标自动是对的；撤销的局部刷新范围见 `repaint_page_rect`。
                let screen = (self.fb.width as f32, self.fb.height as f32);
                let page = (self.canvas.w as f32, self.canvas.h as f32);
                let mut to_landscape = false;
                if let Screen::Notebook(nb) = &mut self.screen {
                    if nb.view.rot % 180 == 0 {
                        nb.landscape_back = Some(nb.view);
                        // 转完以后屏幕上的宽高对调：横版长边 1872 要落进 1404
                        let mut v = PageView::fit_width(screen, (page.1, page.0));
                        v.set_rot(90);
                        v.clamp_pan_of(page);
                        nb.view = v;
                        to_landscape = true;
                    } else {
                        nb.view = nb.landscape_back.take().unwrap_or_else(PageView::identity);
                    }
                    // 选框/套索是屏幕上的浮层，转向后一律作废
                    nb.clear_sel();
                }
                let scale = match &self.screen {
                    Screen::Notebook(nb) => nb.view.scale,
                    _ => 1.0,
                };
                eprintln!(
                    "[view] {} scale={:.3}",
                    if to_landscape { "横屏" } else { "竖屏" },
                    scale
                );
                self.redraw_page();
            }
            // Back / AdjustView / Header 只改了面板状态，重绘一次就够
            docsettings::Act::Back | docsettings::Act::AdjustView | docsettings::Act::Header => {
                self.redraw_page();
            }
        }
    }

    // ------------------------------------------------------------ 文档内搜索

    /// 文档设置 → Search：打开顶部搜索条，输入框自动聚焦、键盘贴底。
    ///
    /// 只刷两块：搜索条（顶部 118）和键盘（底部 601）—— 纸面不整屏重绘。
    fn open_doc_search(&mut self) {
        if !matches!(&self.screen, Screen::Notebook(_)) {
            return;
        }
        // 文档设置面板是直接画进帧缓冲的：收起它以后那块像素会留在屏幕上，
        // 必须顺手补回纸面（否则键盘上沿会露出一条面板表头残影）。
        let panel = self.ds.rect(self.doc_kind());
        self.ds.close();
        self.banner = None;
        self.search.show();
        let (w, h) = (self.fb.width, self.fb.height);
        let kb_h = 601;
        let mut rects = vec![(0, 0, w, docsearch::HDR_H), (0, h - kb_h, w, kb_h)];
        if let Some(r) = panel {
            rects.push(r);
        }
        // === P6 begin ===
        // 正文整体让出搜索条（下移 HDR_H）：只刷搜索条和键盘不够，
        // 页面本身也要重贴，否则第一行还压在搜索条下面。
        rects.clear();
        rects.push((0, 0, w, h));
        // === P6 end ===
        self.repaint_rects(&rects, WAVEFORM_MODE_GC16);
        eprintln!("[search] 打开文档内搜索");
    }

    /// 关闭搜索条：把搜索条和键盘盖住的区域补回纸面（局部刷新）。
    fn close_doc_search(&mut self) {
        if !self.search.is_open() {
            return;
        }
        // 高亮是直接画在纸面上的：关之前记下它的位置，一起刷掉
        let mark = self.search_mark().map(|(r, _)| r);
        self.search.close();
        self.kb_hits.clear();
        let (w, h) = (self.fb.width, self.fb.height);
        let kb_h = 601;
        let mut rects = vec![(0, 0, w, docsearch::HDR_H), (0, h - kb_h, w, kb_h)];
        if let Some(r) = mark {
            rects.push(r);
        }
        // === P6 begin ===
        // 关掉搜索：正文要整体回到 y=0，整屏重贴一次（位移已归零）。
        rects.clear();
        rects.push((0, 0, w, h));
        // === P6 end ===
        self.repaint_rects(&rects, WAVEFORM_MODE_GC16);
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.tap_consumed = true;
        }
        eprintln!("[search] 关闭");
    }

    // === P13 begin ===
    // `doc_texts`（一次性把整本文档的文字层读进内存再搜）跟着同步搜索一起删了：
    // 现在每 tick 只 `load_texts` 一页，见 `poll_doc_search`。
    // === P13 end ===

    /// 提交搜索：扫全文档文字层，跳到第一条命中页；收起键盘。
    fn submit_doc_search(&mut self) {
        if !self.search.is_open() {
            return;
        }
        let (cur, pages) = match &self.screen {
            Screen::Notebook(nb) => (nb.page_index, nb.page_count),
            _ => return,
        };
        let old = self.search_mark().map(|(r, _)| r);
        // PDF / 电子书走文档后端（`Doc::search`）：正文里有字，但没有我们的
        // `texts-*.json` 文字层。笔记本仍然搜打字层。
        let needle = self.search.kb.text.trim().to_string();
        let doc_hits = match (&self.doc, needle.is_empty()) {
            (Some(d), false) => Some(d.search(&needle, 200)),
            _ => None,
        };
        let n = match doc_hits {
            Some(h) => {
                eprintln!("[search] 文档后端 {} 条（{needle}）", h.len());
                self.search.submit_doc(h, cur)
            }
            // === P13 begin ===
            // 笔记本的文字层是"每 tick 一页"扫的（`poll_doc_search`）：一次性
            // `find_all` 全文档会把主循环卡住，进度条和 `Cancel` 都没机会响应。
            None => {
                let only = if self.search.page_only() { Some(cur) } else { None };
                self.search.start_scan(&needle, pages, only);
                eprintln!(
                    "[search] 开始分步扫描：{} 页（{:?}，Page only={}）",
                    pages,
                    self.search.committed(),
                    self.search.page_only()
                );
                // 键盘要一起刷掉（提交后 `kb_open` 已经是 false），
                // 底部那条也顺带把上一轮的进度条擦掉
                let (w, h) = (self.fb.width, self.fb.height);
                let mut rects = vec![
                    (0, 0, w, docsearch::HDR_H),
                    (0, h - 601, w, 601),
                ];
                if let Some(r) = old {
                    rects.push(r);
                }
                self.repaint_rects(&rects, WAVEFORM_MODE_GC16);
                return;
            }
            // === P13 end ===
        };
        eprintln!(
            "[search] {:?} → {n} 条（Page only={}）",
            self.search.committed(),
            self.search.page_only()
        );
        let (w, h) = (self.fb.width, self.fb.height);
        let kb_h = 601;
        if let Some(snip) = self.search.snippet() {
            eprintln!("[search] 命中片段：{snip}");
        }
        match self.search.current() {
            // 第一条命中在别的页：翻过去（翻页本身整页重绘，搜索条由浮层补画）
            Some(hit) if hit.page != cur => self.goto_page(hit.page),
            _ => {
                // 同一页：搜索条 + 键盘 + 旧/新高亮各刷一块
                let mut rects = vec![(0, 0, w, docsearch::HDR_H), (0, h - kb_h, w, kb_h)];
                if let Some(r) = old {
                    rects.push(r);
                }
                if let Some((r, _)) = self.search_mark() {
                    rects.push(r);
                }
                self.repaint_rects(&rects, WAVEFORM_MODE_GC16);
            }
        }
    }

    /// `Previous` / `Next`：同一页只刷高亮和搜索条；换页走 `goto_page`。
    fn search_step(&mut self, forward: bool) {
        if !self.search.is_open() {
            return;
        }
        let old = self.search_mark().map(|(r, _)| r);
        let hit = if forward {
            self.search.next()
        } else {
            self.search.prev()
        };
        let Some(hit) = hit else { return };
        let cur = match &self.screen {
            Screen::Notebook(nb) => nb.page_index,
            _ => return,
        };
        if hit.page != cur {
            self.goto_page(hit.page);
            return;
        }
        let mut rects = vec![(0, 0, self.fb.width, docsearch::HDR_H)];
        if let Some(r) = old {
            rects.push(r);
        }
        if let Some((r, _)) = self.search_mark() {
            rects.push(r);
        }
        self.repaint_rects(&rects, WAVEFORM_MODE_GC16);
    }

    /// 当前命中在屏幕上的高亮矩形 + 要把被灰块盖住的那几个字重画一遍的信息。
    ///
    /// 返回 `((x, y, w, h), (命中文字 x, 基线 y, 字号, 命中的那几个字))`。
    fn search_mark(&mut self) -> Option<((i32, i32, i32, i32), (i32, i32, f32, String))> {
        let hit = self.search.current()?;
        let Screen::Notebook(nb) = &self.screen else {
            return None;
        };
        if hit.page != nb.page_index {
            return None;
        }
        let it = nb.texts.get(hit.item)?;
        let p = nb.view.page_to_screen((it.x + PAGE_DX, it.y + PAGE_DY));
        // === P6 begin ===
        // 搜索条打开时正文整体下移 HDR_H，命中高亮/重画文字跟着偏同一份量。
        let p = (
            p.0,
            p.1 + docsearch::body_offset(self.search.is_open()) as f32,
        );
        // === P6 end ===
        let size = it.size;
        let text = it.text.clone();
        // 命中记的是字符下标；字体宽度要按字节切片算
        let bytes: Vec<usize> = text.char_indices().map(|(b, _)| b).collect();
        let s = bytes.get(hit.start).copied().unwrap_or(text.len());
        let e = bytes.get(hit.end).copied().unwrap_or(text.len());
        if s >= e {
            return None;
        }
        // 工具条那一列盖在纸面上：高亮不要画到它下面
        let (clip0, clip1) = if nb.chrome.visible {
            paper_x()
        } else {
            (0, i32::MAX)
        };
        let t = self.text.as_mut()?;
        let pw = t.width(size, &text[..s]) as i32;
        let mw = t.width(size, &text[s..e]).max(4.0) as i32;
        let x = p.0 as i32 + pw;
        let y = p.1 as i32 - (size * 1.15) as i32;
        let hh = (size * 1.45) as i32;
        let x0 = x.max(clip0);
        let x1 = (x + mw).min(clip1);
        if x1 <= x0 {
            return None;
        }
        Some(((x0, y, x1 - x0, hh), (x, p.1 as i32, size, text[s..e].to_string())))
    }

    /// 画搜索条：先页面高亮（灰底 + 重画文字），再顶部搜索条，最后键盘贴底。
    fn paint_search(&mut self) {
        if !self.search.is_open() {
            return;
        }
        let (w, h) = (self.fb.width, self.fb.height);
        if let Some((r, (tx, ty, size, matched))) = self.search_mark() {
            // 灰底反白：只重画"命中的那几个字"，整段重画会把高亮框外也加深一遍
            self.fb.fill_rect(r.0, r.1, r.2, r.3, gray565(190));
            if r.0 == tx {
                if let Some(t) = self.text.as_mut() {
                    t.draw(&mut self.fb, tx, ty, size, &matched, 20);
                }
            }
        }
        if let Some(t) = self.text.as_mut() {
            self.search.paint(&mut self.fb, t, w);
        }
        // === P13 begin ===
        // 底部进度条：`Searching page %1 of %2`（官方 `SearchResultsFooter`）
        if let Some(t) = self.text.as_mut() {
            self.search.paint_progress(&mut self.fb, t, w, h);
        }
        // === P13 end ===
        if self.search.kb_open() {
            let kb = self.search.kb.clone();
            let ky = h - kb.height();
            self.kb_hits = kb.draw(&mut self.fb, &mut self.text, 0, ky, w);
        } else {
            self.kb_hits.clear();
        }
    }

    /// 搜索条点击。开着时吞掉整屏点击（官方搜索态禁画布手势）。
    fn search_tap(&mut self, x: f32, y: f32) -> bool {
        if !self.search.is_open() {
            return false;
        }
        if self.search.kb_open() {
            for (r, k) in self.kb_hits.clone() {
                let (xi, yi) = (x as i32, y as i32);
                if xi >= r.0 && xi < r.0 + r.2 && yi >= r.1 && yi < r.1 + r.3 {
                    self.search_key(k);
                    return true;
                }
            }
        }
        let w = self.fb.width;
        match self.search.hit(x, y, w) {
            Some(docsearch::Act::Cancel) => self.close_doc_search(),
            Some(docsearch::Act::Input) => {
                self.search.toggle_keyboard();
                self.repaint_search_bar();
            }
            Some(docsearch::Act::PageOnly) => {
                self.search.toggle_page_only();
                // 已经搜过的：换范围重搜（官方是清掉结果，这里直接给新结果）
                if self.search.committed().is_empty() {
                    self.repaint_search_bar();
                } else {
                    self.submit_doc_search();
                }
            }
            Some(docsearch::Act::Search) => self.submit_doc_search(),
            Some(docsearch::Act::Previous) => self.search_step(false),
            Some(docsearch::Act::Next) => self.search_step(true),
            None => {}
        }
        true
    }

    /// 搜索输入：回车提交；其它按键先让键盘吃，改过输入就作废旧结果。
    fn search_key(&mut self, k: Key) {
        if !self.search.is_open() {
            return;
        }
        if k == Key::Enter {
            self.submit_doc_search();
            return;
        }
        let old = self.search_mark().map(|(r, _)| r);
        self.search.kb.press(k);
        let had = self.search.edited();
        let mut rects = vec![(0, 0, self.fb.width, docsearch::HDR_H)];
        if had {
            if let Some(r) = old {
                rects.push(r);
            }
        }
        self.repaint_rects(&rects, WAVEFORM_MODE_GC16);
    }

    /// 只重画搜索条和键盘区（键盘显隐时用）。
    fn repaint_search_bar(&mut self) {
        let (w, h) = (self.fb.width, self.fb.height);
        let kb_h = 601;
        self.repaint_rects(
            &[(0, 0, w, docsearch::HDR_H), (0, h - kb_h, w, kb_h)],
            WAVEFORM_MODE_GC16,
        );
    }

    // === P13 begin ===
    /// 文档内搜索的分步扫描：每 tick 搜一页文字层（`submit_doc_search` 起头）。
    ///
    /// 一页一次 `load_texts`（读 `texts-*.json`）+ `find_all`，几十微秒到一两毫秒；
    /// 主循环在这之间照常处理输入，所以 `Cancel` 随时点得动、进度也画得出来。
    fn poll_doc_search(&mut self) {
        let (uuid, cur) = match &self.screen {
            Screen::Notebook(nb) => (nb.uuid.clone(), nb.page_index),
            // 已经不在笔记本里（关文档 / 换屏）：这一轮扫描作废
            _ => {
                self.search.cancel_scan();
                return;
            }
        };
        let Some(page) = self.search.scan_page() else {
            // 扫到最后一页之后的那一圈：收尾
            self.search.cancel_scan();
            self.finish_doc_search(cur);
            return;
        };
        let texts: Vec<String> = self
            .library
            // === P11 begin ===
            // 搜索也按"官方优先"读（文字可能只存在 `.rm` 里，sidecar 只是回退）
            .load_scene_texts(&uuid, page)
            // === P11 end ===
            .into_iter()
            .map(|t| t.text)
            .collect();
        self.search.scan_take(page, &texts);
        if self.search.scan_paint_due() {
            let (w, h) = (self.fb.width, self.fb.height);
            let mut rects = vec![(0, 0, w, docsearch::HDR_H)];
            if self.search.scan_label().is_some() {
                rects.push(docsearch::progress_rect(w, h));
            }
            self.repaint_rects(&rects, WAVEFORM_MODE_GC16);
        }
    }

    /// 扫描结束：有命中就跳到第一条命中的页，否则只把底部进度条刷回纸面。
    fn finish_doc_search(&mut self, cur: usize) {
        let (w, h) = (self.fb.width, self.fb.height);
        let kb_h = 601;
        eprintln!(
            "[search] 扫描完 {:?} → {} 条（Page only={}）",
            self.search.committed(),
            self.search.count(),
            self.search.page_only()
        );
        if let Some(snip) = self.search.snippet() {
            eprintln!("[search] 命中片段：{snip}");
        }
        match self.search.current() {
            // 第一条命中在别的页：翻过去（翻页本身整页重绘，搜索条由浮层补画）
            Some(hit) if hit.page != cur => self.goto_page(hit.page),
            _ => {
                // 同一页 / 没命中：搜索条 + 底部（顺带把进度条刷掉）+ 高亮
                let mut rects = vec![(0, 0, w, docsearch::HDR_H), (0, h - kb_h, w, kb_h)];
                if let Some((r, _)) = self.search_mark() {
                    rects.push(r);
                }
                self.repaint_rects(&rects, WAVEFORM_MODE_GC16);
            }
        }
    }
    // === P13 end ===

    // ------------------------------------------------------------ Notebook settings

    /// 文档设置 → Notebook settings：整屏设置页（名字 / 封面页 / 方向 / 模板）。
    fn open_nb_settings(&mut self) {
        let (uuid, name, page) = match &self.screen {
            Screen::Notebook(nb) => (nb.uuid.clone(), nb.name.clone(), nb.page_index),
            _ => return,
        };
        let template = self.library.page_template(&uuid, page);
        let portrait = self.library.orientation(&uuid) != "landscape";
        let cover = if self.library.cover_page(&uuid) == 0 { 1 } else { 0 };
        self.search.close();
        self.ds.close();
        self.banner = None;
        let kind = match self.doc_kind() {
            docsettings::Kind::Ebook => nbsettings::Kind::Ebook,
            docsettings::Kind::Pdf => nbsettings::Kind::Pdf,
            docsettings::Kind::Notebook => nbsettings::Kind::Notebook,
        };
        self.nbset.set_kind(kind);
        self.nbset.show(&uuid, &name, page, &template, portrait, cover);
        self.nbset.set_kind(kind);
        let pdf = self.doc_kind() == docsettings::Kind::Pdf;
        self.nbset.set_pdf(pdf);
        eprintln!(
            "[nbset] 打开 {} 第 {} 页（模板 {}，{}）",
            name,
            page + 1,
            template,
            if pdf { "PDF" } else { "Notebook" }
        );
        self.redraw_page();
    }

    /// 关闭设置页：`save = true` 时落盘（名字 / 当前页模板 + `.pagedata` / 封面 / 方向）。
    fn close_nb_settings(&mut self, save: bool) {
        if !self.nbset.is_open() {
            return;
        }
        let uuid = self.nbset.uuid().to_string();
        if save {
            let (name, page, cover, portrait) = self.nbset.save_state();
            let tpl = nbsettings::TEMPLATES[self.nbset.template()].1;
            if !name.is_empty() {
                match self.library.rename(&uuid, &name) {
                    Ok(()) => {
                        if let Screen::Notebook(nb) = &mut self.screen {
                            if nb.uuid == uuid {
                                nb.name = name.clone();
                            }
                        }
                        self.thumbs.remove(&uuid);
                    }
                    Err(e) => eprintln!("[nbset] 改名失败：{e}"),
                }
            }
            if let Err(e) = self.library.set_page_template(&uuid, page, tpl) {
                eprintln!("[nbset] 写模板失败：{e}");
            }
            if let Err(e) = self.library.set_cover_page(&uuid, cover == 1) {
                eprintln!("[nbset] 写封面失败：{e}");
            }
            if let Err(e) = self.library.set_orientation(&uuid, portrait) {
                eprintln!("[nbset] 写方向失败：{e}");
            }
            eprintln!(
                "[nbset] 保存：name={name:?} page={} template={tpl} cover={} portrait={portrait}",
                page + 1,
                cover
            );
        }
        self.nbset.close();
        self.kb_hits.clear();
        self.redraw_page();
    }

    /// 设置页整屏绘制（`paint_overlays` 在它开着时直接走这里）。
    fn paint_nbset(&mut self) {
        if !self.nbset.is_open() {
            return;
        }
        let (w, h) = (self.fb.width, self.fb.height);
        if let Some(t) = self.text.as_mut() {
            self.nbset.paint(&mut self.fb, t, w, h);
        }
        if self.nbset.kb_open() {
            let kb = self.nbset.kb.clone();
            let ky = h - kb.height();
            self.kb_hits = kb.draw(&mut self.fb, &mut self.text, 0, ky, w);
        } else {
            self.kb_hits.clear();
        }
    }

    /// 设置页点击。开着时吞掉整屏点击（不能落到纸面上画线）。
    fn nbset_tap(&mut self, x: f32, y: f32) -> bool {
        if !self.nbset.is_open() {
            return false;
        }
        let (w, h) = (self.fb.width, self.fb.height);
        if self.nbset.kb_open() {
            for (r, k) in self.kb_hits.clone() {
                let (xi, yi) = (x as i32, y as i32);
                if xi >= r.0 && xi < r.0 + r.2 && yi >= r.1 && yi < r.1 + r.3 {
                    self.nbset_key(k);
                    return true;
                }
            }
            // 键盘键缝里漏下来的点击不能穿透到下面的模板卡
            if y >= (h - 601) as f32 {
                return true;
            }
        }
        match self.nbset.hit(x, y, w, h) {
            Some(nbsettings::Act::Cancel) => self.close_nb_settings(false),
            Some(nbsettings::Act::Save) => {
                // 名字空着不给存（官方 `acceptEnabled: documentNameIsValid()`）
                if self.nbset.name_valid() {
                    self.close_nb_settings(true);
                }
            }
            Some(nbsettings::Act::Name) => {
                self.nbset.toggle_keyboard();
                self.repaint_nbset_name();
            }
            Some(nbsettings::Act::Cover(i)) => {
                self.nbset.set_cover(i);
                self.repaint_nbset_part(380, 230);
            }
            Some(nbsettings::Act::Orientation(portrait)) => {
                self.nbset.set_orientation(portrait);
                self.repaint_nbset_part(640, 230);
            }
            Some(nbsettings::Act::Template(i)) => {
                self.nbset.set_template(i);
                self.repaint_nbset_part(900, 450);
            }
            None => {}
        }
        true
    }

    /// 设置页里只刷某一带（`y` 起、`h` 高，通宽），不整屏闪。
    fn repaint_nbset_part(&mut self, y: i32, h: i32) {
        let (w, sh) = (self.fb.width, self.fb.height);
        let mut rects = vec![(0, y, w, h)];
        // 键盘压在这一带上时也一起刷新，免得键帽被旧像素盖住
        if self.nbset.kb_open() {
            rects.push((0, sh - 601, w, 601));
        }
        self.repaint_rects(&rects, WAVEFORM_MODE_GC16);
    }

    /// 打字时只刷：表头（Save 可用态）、名字那一行、键盘。
    fn repaint_nbset_name(&mut self) {
        let (w, h) = (self.fb.width, self.fb.height);
        self.repaint_rects(
            &[
                (0, 0, w, nbsettings::HDR_H),
                NbSettings::name_rect(w),
                (0, h - 601, w, 601),
            ],
            WAVEFORM_MODE_GC16,
        );
    }

    /// 设置页键盘：回车 = 收起键盘（名字随 Save 落盘）。
    fn nbset_key(&mut self, k: Key) {
        if !self.nbset.is_open() {
            return;
        }
        if k == Key::Enter {
            self.nbset.toggle_keyboard();
            self.repaint_nbset_name();
            return;
        }
        self.nbset.kb.press(k);
        self.repaint_nbset_name();
    }

    // ------------------------------------------------------------ Text settings

    /// 文档设置 → Text size：整页 "Text settings" sheet（字号 / 边距 / 行距）。
    fn open_text_settings(&mut self) {
        let (uuid, cur) = match &self.screen {
            Screen::Notebook(nb) => (nb.uuid.clone(), nb.page_index),
            _ => return,
        };
        if self.doc.is_none() {
            return;
        }
        let cfg = self.reading_cfg;
        let has_ink = self.doc_has_ink(&uuid);
        self.search.close();
        self.nbset.close();
        self.ds.close();
        self.banner = None;
        self.tcfg.show(cfg, has_ink);
        eprintln!(
            "[textcfg] 打开 {uuid}（第 {} 页，字号 {:.0} / 边距 {} / 行距 {:.2}，墨迹 {has_ink}）",
            cur + 1,
            cfg.font_size,
            cfg.margin,
            cfg.line_spacing
        );
        self.redraw_page();
    }

    /// 这本电子书任何一页有没有墨迹（官方 `hasContentsOnAnyPage()`）。
    ///
    /// 只读本地 `.rm`（当前页已经在内存里，先看它）；没有墨迹就不弹
    /// `Misalignment may occur`，别拿无意义的确认框烦人。
    fn doc_has_ink(&self, uuid: &str) -> bool {
        if let Screen::Notebook(nb) = &self.screen {
            if nb.uuid == uuid && nb.page.total_lines() > 0 {
                return true;
            }
        }
        let n = self.library.page_ids(uuid).len();
        (0..n).any(|i| self.library.load_page_at(uuid, i).total_lines() > 0)
    }

    /// Text settings 页点击。开着时吞掉整屏点击（不能落到纸面上画线）。
    fn tcfg_tap(&mut self, x: f32, y: f32) -> bool {
        if !self.tcfg.is_open() {
            return false;
        }
        let (w, h) = (self.fb.width, self.fb.height);
        if let Some(act) = self.tcfg.hit(x, y, w, h) {
            match self.tcfg.click(act) {
                // Done / Yes：把面板里的草稿配置真正应用（重排 + 重绘 + 落盘）
                Some(cfg) => self.apply_text_settings(cfg),
                // 只改了面板状态：档位高亮、进/出确认框、Cancel 关页
                None => self.redraw_page(),
            }
        }
        true
    }

    /// 把 Text settings 的配置交给已有的排版通道，并同步文档设置行的字号文案。
    fn apply_text_settings(&mut self, cfg: rmdev::doc::LayoutCfg) {
        let size = rmdev::doc::reflow::FONT_SIZES
            .iter()
            .position(|s| (s - cfg.font_size).abs() < 0.5);
        self.ds.set_reflow(true, size);
        eprintln!(
            "[textcfg] 应用：字号 {:.0} / 边距 {} / 行距 {:.2}",
            cfg.font_size, cfg.margin, cfg.line_spacing
        );
        let before = self.reading_cfg;
        self.set_reading_layout(cfg);
        // 配置没变时 `set_reading_layout` 会早退（没重绘），但面板已经关了，
        // 得把纸面画回来——否则整页 sheet 的像素还留在屏上。
        if self.reading_cfg == before {
            self.redraw_page();
        }
    }

    // ------------------------------------------------------------ 手势

    /// 每帧手势：**只改视图状态，不重绘**。
    ///
    /// 全屏 GC16 一次要 300ms 量级，捏合过程中每帧重绘会把 EPDC 队列压垮，
    /// 所以等手指全抬起来再整页重绘一次（见 `on_gesture_end`）。
    fn on_gesture(&mut self, g: Gesture) {
        // 用户 2026-11-09：笔正按在屏上（哪怕静止）时，任何多指手势都不生效。
        // 手掌压出来的"两指捏合/平移"会让整页重绘 → 灰块、残影、卡顿。
        if matches!(&self.screen, Screen::Notebook(nb) if nb.active.is_some()) {
            self.gesture_armed = false;
            self.gesture_sum = (0.0, 0.0, 1.0);
            return;
        }
        // 笔优先：正在写字时手掌压在屏上，不要缩放/平移，也不要整页重绘
        if self.pen_ts.elapsed() < Duration::from_millis(250) {
            self.gesture_armed = false;
            self.gesture_sum = (0.0, 0.0, 1.0);
            return;
        }
        // 死区：一次点击会被触摸板识别成"两指捏合 1.011 + 平移 8px"，
        // 越过阈值之前整轮都不动视图（否则点一下空白就整页重绘一次）。
        match g {
            Gesture::Pinch { factor, dcenter, .. } => {
                self.gesture_sum.0 += dcenter.0;
                self.gesture_sum.1 += dcenter.1;
                self.gesture_sum.2 *= factor;
                let far = self.gesture_sum.0.abs() > 6.0 || self.gesture_sum.1.abs() > 6.0;
                if !self.gesture_armed && (self.gesture_sum.2 - 1.0).abs() < 0.02 && !far {
                    return;
                }
            }
            Gesture::Drag { dx, dy } => {
                self.gesture_sum.0 += dx;
                self.gesture_sum.1 += dy;
                if !self.gesture_armed && self.gesture_sum.0.abs() < 5.0 && self.gesture_sum.1.abs() < 5.0
                {
                    return;
                }
            }
            Gesture::None => return,
        }
        self.gesture_armed = true;
        if matches!(g, Gesture::None) {
            return;
        }
        let Screen::Notebook(nb) = &mut self.screen else {
            return;
        };
        // 页面总览是另一套坐标，手势不参与
        if nb.overview {
            return;
        }
        match g {
            Gesture::None => {}
            Gesture::Pinch {
                factor,
                center,
                dcenter,
            } => {
                // 与 QML updatePinch 同一个组合：先按旧中点缩放，再跟手中点位移
                nb.view
                    .zoom_at(factor, (center.0 - dcenter.0, center.1 - dcenter.1));
                nb.view.pan(dcenter.0, dcenter.1);
                nb.view.clamp_pan();
                self.gesture_moved = true;
            }
            Gesture::Drag { dx, dy } => {
                // 放大后单指 = 平移；1:1 时保持原来的翻页/滚动手感。
                // 只有视图真的动了才算"这一轮改过"，否则一次点击也会白刷一屏。
                if nb.view.scale > 1.01 && (dx != 0.0 || dy != 0.0) {
                    nb.view.pan(dx, dy);
                    nb.view.clamp_pan();
                    self.gesture_moved = true;
                }
            }
        }
    }

    /// 手指全抬起来：这一轮改过视图就整页重绘一次（`redraw_page` 自带 250ms 限速）。
    fn on_gesture_end(&mut self) {
        self.gesture_armed = false;
        self.gesture_sum = (0.0, 0.0, 1.0);
        if !self.gesture_moved {
            return;
        }
        self.gesture_moved = false;
        let (scale, tx, ty) = match &self.screen {
            Screen::Notebook(nb) => (nb.view.scale, nb.view.tx, nb.view.ty),
            _ => return,
        };
        eprintln!("[view] 手势结束 scale={scale:.3} t=({tx:.1},{ty:.1})");
        self.redraw_page();
    }

    /// 双指**整轮**手势结算（6b 点按撤销 / 6c 下滑收工具条）。
    ///
    /// 键盘 / 面板 / 总览开着时不动：这些界面上双指点一下不该改纸面。
    fn on_two_finger(&mut self, act: TwoFingerAct) {
        if act == TwoFingerAct::None {
            return;
        }
        let ready = matches!(&self.screen, Screen::Notebook(nb)
            if !nb.overview
                && nb.tag_edit.is_none()
                && nb.layer_rename.is_none()
                && nb.text_edit.is_none());
        if !ready || self.nbset.is_open() || self.ds.is_open() || self.search.is_open() {
            return;
        }
        match act {
            TwoFingerAct::None => {}
            TwoFingerAct::Undo => {
                // 用户 2026-11-09：**禁用**双指点按撤销。
                // 写字时掌心蹭到两下就是一次撤销，整页重绘（灰块/残影/卡顿），
                // 而且真的会把字撤掉。手势识别器还照常跑，只是不落到纸面上。
                eprintln!("[touch] 双指点按（撤销手势已禁用）");
            }
            TwoFingerAct::HideToolbar => {
                // 已经收起就没有"再收一次"，等同于点圆环的反向操作不做
                if matches!(&self.screen, Screen::Notebook(nb) if nb.chrome.visible) {
                    eprintln!("[touch] 双指下滑 → 收起工具条");
                    self.on_chrome(Msg::Hide);
                }
            }
        }
    }

    // === P13 begin ===
    /// 三指点按 = 重做（官方 `res_067887b.qml:1161` `threeFingerTapFilter`，
    /// `enabled: sceneController.redoAvailable`、`onClick: sceneController.redo()`）。
    ///
    /// 两指的撤销在 `on_two_finger` 里；`touch::classify` 保证同一帧只有一个动作。
    fn on_three_finger(&mut self, act: rmdev::input::touch::ThreeFingerAct) {
        // 用户 2026-11-09：三指点按重做同样禁用（掌心的三个触点就会触发）。
        if act == rmdev::input::touch::ThreeFingerAct::Redo {
            eprintln!("[touch] 三指点按（重做手势已禁用）");
        }
        return;
        #[allow(unreachable_code)]
        if act != rmdev::input::touch::ThreeFingerAct::Redo {
            return;
        }
        // 和双指同一套"什么时候不动纸面"的判据，外加官方的"没得重做就不响应"
        let ready = matches!(&self.screen, Screen::Notebook(nb)
            if !nb.overview
                && nb.tag_edit.is_none()
                && nb.layer_rename.is_none()
                && nb.text_edit.is_none()
                && nb.chrome.redo_ok);
        if !ready || self.nbset.is_open() || self.ds.is_open() || self.search.is_open() {
            return;
        }
        eprintln!("[touch] 三指点按 → 重做");
        self.on_chrome(Msg::Redo);
    }
    // === P13 end ===

    /// 套索折线 + 选框 + 悬浮操作条。三者都画在屏幕坐标上（直接进帧缓冲），
    /// 纸面的墨迹已经由 `canvas.paint_all` 画完了。
    fn paint_selection(&mut self) {
        let (lasso, bounds) = match &self.screen {
            Screen::Notebook(nb) => (nb.lasso.clone(), nb.sel.as_ref().map(|s| s.bounds)),
            _ => return,
        };
        if let Some(poly) = lasso {
            for w in poly.windows(2) {
                draw_lasso_seg(&mut self.fb, w[0], w[1]);
            }
        }
        let Some(b) = bounds else { return };
        let view = rect_to_view(b);
        // 选框：官方 `selectionBorder` 是 2px 黑（`res_066a78f.qml:75-76`），
        // 画成虚线：EPDC 上实线框和笔迹糊在一起分不清。
        dashed_rect(&mut self.fb, view, 2, 20);
        self.paint_sel_bar(view);
    }

    /// 悬浮条：`select::bar_layout` 排好的 2 个 62×62 格子，左边 copy、右边 cut。
    fn paint_sel_bar(&mut self, view: RectF) {
        let bars = select::bar_layout(view);
        let Some(first) = bars.first() else { return };
        let (bx, by) = (first.x, first.y);
        let bw: i32 = bars.iter().map(|b| b.w).sum();
        let bh = first.h;
        // 白底一块，再逐格描 2px 边框（`res_066a78f.qml:239`、`:255`）
        self.fb.fill_rect(bx, by, bw, bh, gray565(255));
        for b in &bars {
            widgets::outline_rect(&mut self.fb, b.x, b.y, b.w, b.h, 2, 20);
        }
        // QML 的 `spacing: -2` 让两个 2px 边框叠成一条分隔线，这里把前一格的
        // 右边框抹掉，留后一格自己的左边框当分隔
        for b in bars.iter().skip(1) {
            self.fb.fill_rect(b.x - 2, b.y + 2, 2, b.h - 4, gray565(255));
        }
        for b in &bars {
            let (ix, iy) = (b.x + (b.w - 40) / 2, b.y + (b.h - 40) / 2);
            match b.act {
                SelAct::Copy => widgets::icon_duplicate(&mut self.fb, ix, iy, 20),
                SelAct::Cut => widgets::icon_cut(&mut self.fb, ix, iy, 20),
                // 条上只有 copy/cut（`select` 模块的规格），别的组合不会出现
                _ => {}
            }
        }
    }

    /// 选区浮层（选框 + 悬浮条）占的屏幕矩形
    fn sel_overlay_rect(&self) -> Option<(i32, i32, i32, i32)> {
        let bounds = match &self.screen {
            Screen::Notebook(nb) => nb.sel.as_ref().map(|s| s.bounds),
            _ => None,
        }?;
        let view = rect_to_view(bounds);
        let mut r = (
            view.x as i32 - 4,
            view.y as i32 - 4,
            view.w as i32 + 8,
            view.h as i32 + 8,
        );
        let bars = select::bar_layout(view);
        if let Some(b) = bars.first() {
            let bw: i32 = bars.iter().map(|b| b.w).sum();
            r = union_rect(r, (b.x, b.y, bw, b.h));
        }
        Some(r)
    }

    /// 套索折线占的屏幕矩形
    fn lasso_rect(&self) -> Option<(i32, i32, i32, i32)> {
        let poly = match &self.screen {
            Screen::Notebook(nb) => nb.lasso.as_ref(),
            _ => None,
        }?;
        if poly.is_empty() {
            return None;
        }
        let (x0, y0) = (
            poly.iter().map(|p| p.0).fold(f32::MAX, f32::min) as i32 - 2,
            poly.iter().map(|p| p.1).fold(f32::MAX, f32::min) as i32 - 2,
        );
        let (x1, y1) = (
            poly.iter().map(|p| p.0).fold(f32::MIN, f32::max) as i32 + 3,
            poly.iter().map(|p| p.1).fold(f32::MIN, f32::max) as i32 + 3,
        );
        Some((x0, y0, x1 - x0, y1 - y0))
    }

    /// 图层面板。官方 `LayersTool`：宽 798、行高 120，面板底边贴"图层"按钮下沿；
    /// 自上而下是**图层从高到低** → Template 行 → 2px 分隔 → Add layer 行（118）。
    fn paint_layers_panel(&mut self) {
        self.layer_hits.clear();
        if env_flag("RM_DEBUG") {
            eprintln!("[layers] 画面板");
        }
        let (names, visible, cur) = match &self.screen {
            Screen::Notebook(nb) => (
                nb.page
                    .layers
                    .iter()
                    .map(|l| l.name.clone())
                    .collect::<Vec<_>>(),
                nb.page.layers.iter().map(|l| l.visible).collect::<Vec<_>>(),
                nb.page.current,
            ),
            _ => return,
        };
        let n = names.len() as i32;
        // === P11 begin ===
        // 官方文字层一行只在有文字时显示（`res_0675d6b.qml:85` 的
        // `sceneController.rootDocumentLength > 0`）
        let has_text = matches!(&self.screen, Screen::Notebook(nb) if !nb.texts.is_empty());
        let h = p11_layers_height(n, has_text);
        // === P11 end ===
        let x0 = panel_x(LAYER_PANEL_X, LAYER_PANEL_W);
        let y0 = LAYER_BTN_BOTTOM - h;
        let (x1, right) = (x0 + LAYER_PANEL_W - 2, x0 + LAYER_PANEL_W - 2);

        for r in 0..n {
            let li = (n - 1 - r) as usize;
            let y = y0 + r * LAYER_ROW;
            let active = li == cur;
            let (bg, fg) = if active { (0u8, 255u8) } else { (255, 20) };
            self.fb.fill_rect(x0, y, LAYER_PANEL_W, LAYER_ROW, gray565(bg));
            self.fb.fill_rect(x0, y, 2, LAYER_ROW, gray565(0));
            self.fb.fill_rect(x1, y, 2, LAYER_ROW, gray565(0));
            // 官方只有最下面那一层（图层 1）有顶边
            if r == n - 1 {
                self.fb.fill_rect(x0, y, LAYER_PANEL_W, 2, gray565(0));
            }

            // 眼睛
            if visible[li] {
                widgets::icon_eye(&mut self.fb, x0 + 40, y + 40, fg);
            } else {
                widgets::icon_eye_closed(&mut self.fb, x0 + 40, y + 40, fg);
            }
            // 名字（先压"选中"的命中区，再压按钮，保证按钮优先）
            let nx = x0 + 120;
            let nw = right - 4 * LAYER_SMALL_BTN - nx - 20;
            let label = match self.text.as_mut() {
                Some(t) => t.ellipsize(LAYER_FONT, &names[li], nw),
                None => names[li].clone(),
            };
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, nx, y + 72, LAYER_FONT, &label, fg);
            }
            self.layer_hits
                .push(((x0, y, LAYER_PANEL_W - 4 * LAYER_SMALL_BTN, LAYER_ROW), LayerAct::Select(li)));
            self.layer_hits
                .push(((x0, y, 120, LAYER_ROW), LayerAct::Eye(li)));

            // 右侧四个小按钮：上、下、合并、删除
            let dim = if active { 130u8 } else { 160u8 };
            for k in 0..4 {
                let bx = right - (4 - k) * LAYER_SMALL_BTN;
                let (ix, iy) = (bx + 10, y + 40);
                match k {
                    0 => {
                        widgets::icon_arrow_up(&mut self.fb, ix, iy, if li + 1 < n as usize { fg } else { dim });
                        if li + 1 < n as usize {
                            self.layer_hits
                                .push(((bx, y, LAYER_SMALL_BTN, LAYER_ROW), LayerAct::Up(li)));
                        }
                    }
                    1 => {
                        widgets::icon_arrow_down(&mut self.fb, ix, iy, if li >= 1 { fg } else { dim });
                        if li >= 1 {
                            self.layer_hits
                                .push(((bx, y, LAYER_SMALL_BTN, LAYER_ROW), LayerAct::Down(li)));
                        }
                    }
                    2 => {
                        if li >= 1 {
                            widgets::icon_merge_layer(&mut self.fb, ix, iy, fg);
                            self.layer_hits
                                .push(((bx, y, LAYER_SMALL_BTN, LAYER_ROW), LayerAct::Merge(li)));
                        }
                    }
                    _ => {
                        if n > 1 {
                            widgets::icon_trash(&mut self.fb, ix, iy, fg);
                            self.layer_hits
                                .push(((bx, y, LAYER_SMALL_BTN, LAYER_ROW), LayerAct::Delete(li)));
                        }
                    }
                }
            }
        }

        // === P11 begin ===
        // 文字层一行（官方 `LayerComponent{isTextLayer: true}`，`res_0675d6b.qml:81-90`）：
        // 图层行之后、Template 之前；行名就是官方的 `qsTr("Text")`
        // （`res_06713f5.qml:64`）。整行点了都切到文字工具。
        if has_text {
            let y = y0 + n * LAYER_ROW;
            self.fb.fill_rect(x0, y, LAYER_PANEL_W, LAYER_ROW, gray565(255));
            self.fb.fill_rect(x0, y, 2, LAYER_ROW, gray565(0));
            self.fb.fill_rect(x1, y, 2, LAYER_ROW, gray565(0));
            widgets::icon_eye(&mut self.fb, x0 + 40, y + 40, 20);
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, x0 + 120, y + 72, LAYER_FONT, "Text", 20);
            }
            self.layer_hits
                .push(((x0, y, LAYER_PANEL_W, LAYER_ROW), LayerAct::Text));
        }
        // === P11 end ===
        // Template / Background 行
        let y = y0 + n * LAYER_ROW;
        // === P11 begin ===
        // 上面那行是文字层之前的排法；有文字层时整段往下挪一行
        let y = y + if has_text { LAYER_ROW } else { 0 };
        // === P11 end ===
        self.fb.fill_rect(x0, y, LAYER_PANEL_W, LAYER_ROW, gray565(255));
        self.fb.fill_rect(x0, y, 2, LAYER_ROW, gray565(0));
        self.fb.fill_rect(x1, y, 2, LAYER_ROW, gray565(0));
        widgets::icon_eye(&mut self.fb, x0 + 40, y + 40, 20);
        if let Some(t) = self.text.as_mut() {
            let label = if self.pdf_bg.is_some() { "Background" } else { "Template" };
            t.draw(&mut self.fb, x0 + 120, y + 72, LAYER_FONT, label, 20);
        }
        widgets::icon_template(&mut self.fb, right - LAYER_SMALL_BTN + 10, y + 40, 20);
        self.layer_hits
            .push(((x0, y, LAYER_PANEL_W, LAYER_ROW), LayerAct::Bg));

        // 2px 分隔
        let y = y + LAYER_ROW;
        self.fb.fill_rect(x0, y, LAYER_PANEL_W, 2, gray565(0));

        // Add layer 行（118 高，左/右/底 2px 黑边）
        let y = y + 2;
        let ah = LAYER_ROW - 2;
        self.fb.fill_rect(x0, y, LAYER_PANEL_W, ah, gray565(255));
        self.fb.fill_rect(x0, y, 2, ah, gray565(0));
        self.fb.fill_rect(x1, y, 2, ah, gray565(0));
        self.fb.fill_rect(x0, y + ah - 2, LAYER_PANEL_W, 2, gray565(0));
        let full = n >= 5;
        if full {
            widgets::icon_info_circle(&mut self.fb, x0 + 40, y + (ah - 40) / 2, 20);
        } else {
            widgets::plus_circle(&mut self.fb, x0 + 60, y + ah / 2, 20, 20);
        }
        if let Some(t) = self.text.as_mut() {
            let label = if full { "Limit of 5 layers reached" } else { "Add layer" };
            t.draw(&mut self.fb, x0 + 120, y + 72, LAYER_FONT, label, 20);
        }
        if !full {
            self.layer_hits.push(((x0, y, LAYER_PANEL_W, ah), LayerAct::Add));
        }
    }

    /// 分享面板：宽 416、行高 120，底边贴"分享"按钮下沿。
    /// 官方三项：屏幕共享 / 发邮件 / 转文字；后两项是 Connect 功能，我们砍掉了订阅，
    /// 所以画成灰的并给一句说明。
    fn paint_share_panel(&mut self) {
        self.share_hits.clear();
        let port = rmdev::share::active_port();
        let x0 = panel_x(SHARE_PANEL_X, SHARE_PANEL_W);
        let n = 3i32;
        // 这两项永远可点：官方不会因为"没连云"把入口灰掉。发邮件要自建服务器时，
        // 灰的是邮件页里的 `Send`（`sh::can_send`），不是入口；转文字没配对会自己提示。
        let mail_tone = 20u8;
        let y0 = SHARE_BTN_BOTTOM - n * SHARE_ROW;
        for r in 0..n {
            let y = y0 + r * SHARE_ROW;
            // === P13 begin ===
            // 文案一律官方原文：屏幕共享 `res_067e8cc.qml:103`、
            // 另两项 `frag_0081_006bd97e.qml:8/10`
            let (inverted, tone) = if r == 0 {
                (port.is_some(), 20u8)
            } else {
                (false, mail_tone)
            };
            let label = rmdev::chrome::share_label(r as usize, port.is_some()).to_string();
            // === P13 end ===
            let (bg, fg) = if inverted { (0u8, 255u8) } else { (250, tone) };
            self.fb.fill_rect(x0, y, SHARE_PANEL_W, SHARE_ROW, gray565(bg));
            self.fb.fill_rect(x0 + SHARE_PANEL_W, y, 2, SHARE_ROW, gray565(20));
            if r + 1 < n {
                self.fb.fill_rect(x0 + 20, y + SHARE_ROW - 1, SHARE_PANEL_W - 40, 1, gray565(225));
            }
            // 图标
            let (ix, iy) = (x0 + 30, y + 40);
            match r {
                0 => widgets::icon_screen(&mut self.fb, ix, iy, fg),
                1 => widgets::icon_send(&mut self.fb, ix, iy, fg),
                _ => {
                    if let Some(t) = self.text.as_mut() {
                        t.draw(&mut self.fb, ix + 8, iy + 30, 30.0, "A", fg);
                    }
                }
            }
            if let Some(t) = self.text.as_mut() {
                // === P13 begin ===
                // 官方 `ToolButton` 的标题是 `Text.WrapAtWordBoundaryOrAnywhere`，
                // 行高 `nameLineHeigt: 1.2`：`Present with Screen Share` 在 416 宽的
                // 面板里折成两行、整体垂直居中（不是省略号）。
                const SHARE_TEXT_DX: i32 = 90;
                let max_w = (SHARE_PANEL_W - SHARE_TEXT_DX - 8) as f32;
                let lines = rmdev::chrome::wrap_words(&label, max_w, &mut |s| t.width(28.0, s));
                let lh = 34; // 28 * 1.2
                let base = y + 76 - (lines.len() as i32 - 1) * (lh / 2);
                for (i, ln) in lines.iter().enumerate() {
                    t.draw(
                        &mut self.fb,
                        x0 + SHARE_TEXT_DX,
                        base + i as i32 * lh,
                        28.0,
                        ln,
                        fg,
                    );
                }
                // === P13 end ===
            }
            let act = match r {
                0 => A_SHARE_SCREEN,
                1 => A_SHARE_MAIL,
                _ => A_SHARE_HWC,
            };
            self.share_hits.push(((x0, y, SHARE_PANEL_W, SHARE_ROW), act));
        }
    }

    /// 分享面板占地
    fn share_panel_rect(&self) -> Option<(i32, i32, i32, i32)> {
        match &self.screen {
            Screen::Notebook(nb) if nb.chrome.menu == Menu::Share => {
                Some((
                    panel_x(SHARE_PANEL_X, SHARE_PANEL_W),
                    SHARE_BTN_BOTTOM - 3 * SHARE_ROW,
                    SHARE_PANEL_W,
                    3 * SHARE_ROW,
                ))
            }
            _ => None,
        }
    }

    fn share_hit(&self, x: f32, y: f32) -> Option<u32> {
        let (x, y) = (x as i32, y as i32);
        self.share_hits
            .iter()
            .rev()
            .find(|((hx, hy, hw, hh), _)| x >= *hx && y >= *hy && x < hx + hw && y < hy + hh)
            .map(|(_, a)| *a)
    }

    /// 分享面板里点了一下
    fn share_click(&mut self, act: u32) {
        match act {
            A_SHARE_SCREEN => match rmdev::share::active_port() {
                Some(p) => {
                    rmdev::share::stop();
                    eprintln!("[share] 停止屏幕共享 :{p}");
                    // === P3 begin ===
                    self.notice(NoticeKind::Info, "Screen sharing stopped");
                    // === P3 end ===
                }
                None => match rmdev::share::start() {
                    Ok(p) => {
                        eprintln!("[share] 屏幕共享 http://10.11.99.1:{p}");
                        // === P3 begin ===
                        self.notice(
                            NoticeKind::Info,
                            &format!(
                                "Screen sharing: http://{}:{p}",
                                rmdev::net::device_ip()
                            ),
                        );
                        // === P3 end ===
                    }
                    // === P3 begin ===
                    Err(e) => self.notice(NoticeKind::Error, &format!("Could not start: {e}")),
                    // === P3 end ===
                },
            },
            A_SHARE_MAIL => {
                // 邮件是整页 sheet：画完就返回，别让下面那行 `redraw_page()`
                // 把文档页再糊回来（会把 sheet 撕成"左边一条"）。
                self.open_mail();
                return;
            }
            // === P10 begin ===
            // 转文字换成官方整页流程：自己切到页总览并画好，别让下面
            // 那行 `redraw_page()` 把文档页糊回来
            _ => {
                self.start_ocr();
                return;
            }
            // === P10 end ===
        }
        self.redraw_page();
    }

    // ------------------------------------------------------------ 文字工具

    /// 键盘盖在页面上；上方一条 96 高的工具条（取消 / 完成）
    fn paint_text_editor(&mut self) {
        let Some((buf, x, y)) = (match &self.screen {
            Screen::Notebook(nb) => nb
                .text_edit
                .as_ref()
                .map(|e| (e.kb.text.clone(), e.x, e.y)),
            _ => None,
        }) else {
            return;
        };
        let (w, h) = (self.fb.width, self.fb.height);
        // 光标位置上显示正在输入的内容（字号按段落样式：`ParagraphStyle::default_size()`）
        // === P11 begin ===
        let p11_sz = self.p11_current_style().default_size();
        if let Some(t) = self.text.as_mut() {
            t.draw(&mut self.fb, x as i32, y as i32, p11_sz, &buf, 20);
            let tw = t.width(p11_sz, &buf) as i32;
            if tw > 0 || buf.is_empty() {
                let ch = p11_sz as i32;
                self.fb
                    .fill_rect(x as i32 + tw + 2, y as i32 - ch + 8, 2, ch, gray565(20));
            }
        }
        // === P11 end ===
        let kb = match &self.screen {
            Screen::Notebook(nb) => nb.text_edit.as_ref().map(|e| e.kb.clone()),
            _ => None,
        };
        let Some(kb) = kb else {
            return;
        };
        let ky = h - kb.height();
        self.kb_hits = kb.draw(&mut self.fb, &mut self.text, 0, ky, w);
        let by = ky - 96;
        self.fb.fill_rect(0, by, w, 96, gray565(245));
        self.fb.fill_rect(0, by, w, 2, gray565(0));
        self.hits.clear();
        let bw = 200;
        self.fb.fill_rect(w - bw - 30, by + 18, bw, 60, gray565(20));
        if let Some(t) = self.text.as_mut() {
            t.draw(&mut self.fb, 30, by + 62, 32.0, "Cancel", 20);
            t.draw(&mut self.fb, w - bw + 40, by + 62, 32.0, "Done", 250);
        }
        self.hits.push(0, by, 260, 96, A_CANCEL);
        self.hits.push(w - bw - 40, by + 14, bw + 40, 68, A_OK);
        // === P11 begin ===
        // 官方写作格式菜单（5 项）：压在工具条中间，Cancel/Done 之外那段
        self.paint_p11_format_menu(w, by);
        // === P11 end ===
    }

    /// 在 (x,y) 处开键盘；点在小字上就是改那一段
    fn open_text_editor(&mut self, x: f32, y: f32) {
        let items: Vec<(usize, f32, f32, f32, String)> = match &self.screen {
            Screen::Notebook(nb) => nb
                .texts
                .iter()
                .enumerate()
                .map(|(i, it)| (i, it.x, it.y, it.size, it.text.clone()))
                .collect(),
            _ => Vec::new(),
        };
        let mut hit = None;
        for (i, ix, iy, size, text) in items {
            let tw = self
                .text
                .as_mut()
                .map(|t| t.width(size, &text) as i32)
                .unwrap_or(0);
            let px = ix as i32 + PAGE_DX as i32;
            let py = iy as i32 + PAGE_DY as i32;
            let eh = (size * 1.4) as i32;
            if (x as i32) >= px - 6
                && (x as i32) <= px + tw + 6
                && (y as i32) >= py - eh
                && (y as i32) <= py + 10
            {
                hit = Some((i, px as f32, py as f32, text));
                break;
            }
        }
        let (ex, ey, buf, editing) = match hit {
            Some((i, px, py, text)) => (px, py, text, Some(i)),
            None => (x, y, String::new(), None),
        };
        // === P11 begin ===
        // 改已有段落：用它自己的样式；新建：从 Body 开始
        let p11_style = match (&self.screen, editing) {
            (Screen::Notebook(nb), Some(i)) => nb
                .p11_styles
                .get(i)
                .copied()
                .unwrap_or(ParagraphStyle::PLAIN),
            _ => ParagraphStyle::PLAIN,
        };
        // === P11 end ===
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.pending_text = None;
            let mut kb = Keyboard::new();
            kb.text = buf;
            nb.text_edit = Some(TextEdit {
                x: ex,
                y: ey,
                kb,
                editing,
                // === P11 begin ===
                p11_style,
                // === P11 end ===
            });
        }
        self.redraw_page();
    }

    /// 打字时的局部重画：只擦写"文字那一行"，再重画键盘。
    fn repaint_text_editor(&mut self) {
        let Some((buf, x, y)) = (match &self.screen {
            Screen::Notebook(nb) => nb
                .text_edit
                .as_ref()
                .map(|e| (e.kb.text.clone(), e.x, e.y)),
            _ => None,
        }) else {
            return;
        };
        let (w, h) = (self.fb.width, self.fb.height);
        // === P11 begin ===
        let p11_sz = self.p11_current_style().default_size();
        let tw = self
            .text
            .as_mut()
            .map(|t| t.width(p11_sz, &buf) as i32)
            .unwrap_or(0);
        // 擦掉这一行（多留一个光标宽，退格时不留残影；高度按最大字号 35 留够）
        let (px0, px1) = paper_x();
        let lx = (x as i32 - 6).max(px0);
        let lw = ((tw + 60).max(80)).min(px1 - lx);
        self.fb.fill_rect(lx, y as i32 - 40, lw, 56, gray565(255));
        if let Some(t) = self.text.as_mut() {
            t.draw(&mut self.fb, x as i32, y as i32, p11_sz, &buf, 20);
            let ch = p11_sz as i32;
            self.fb
                .fill_rect(x as i32 + tw + 2, y as i32 - ch + 8, 2, ch, gray565(20));
        }
        self.fb.refresh(
            lx,
            y as i32 - 42,
            lw + 8,
            60,
            WAVEFORM_MODE_DU,
            rmdev::fb::UPDATE_MODE_PARTIAL,
        );
        // === P11 end ===
        let kb = match &self.screen {
            Screen::Notebook(nb) => nb.text_edit.as_ref().map(|e| e.kb.clone()),
            _ => None,
        };
        if let Some(kb) = kb {
            let ky = h - kb.height();
            self.kb_hits = kb.draw(&mut self.fb, &mut self.text, 0, ky, w);
            self.fb.refresh(
                0,
                ky,
                w,
                h - ky,
                WAVEFORM_MODE_DU,
                rmdev::fb::UPDATE_MODE_PARTIAL,
            );
        }
    }

    fn commit_text_editor(&mut self) {
        let Screen::Notebook(nb) = &mut self.screen else {
            return;
        };
        let Some(e) = nb.text_edit.take() else {
            return;
        };
        let s = e.kb.text.trim_end().to_string();
        // === P11 begin ===
        // 这一段选好的写作格式：删除/新建时样式表要跟着 `texts` 一起动
        let p11_style = e.p11_style;
        // === P11 end ===
        match e.editing {
            Some(i) => {
                if s.trim().is_empty() {
                    if i < nb.texts.len() {
                        nb.texts.remove(i);
                        // === P11 begin ===
                        if i < nb.p11_styles.len() {
                            nb.p11_styles.remove(i);
                        }
                        // === P11 end ===
                    }
                } else if let Some(it) = nb.texts.get_mut(i) {
                    it.text = s;
                    // === P11 begin ===
                    it.size = p11_style.default_size();
                    // === P11 end ===
                }
                // === P11 begin ===
                if let Some(st) = nb.p11_styles.get_mut(i) {
                    *st = p11_style;
                }
                // === P11 end ===
            }
            None => {
                if !s.trim().is_empty() {
                    nb.texts.push(TextItem {
                        x: e.x - PAGE_DX,
                        y: e.y - PAGE_DY,
                        // === P11 begin ===
                        size: p11_style.default_size(),
                        // === P11 end ===
                        text: s,
                    });
                    // === P11 begin ===
                    nb.p11_styles.push(p11_style);
                    // === P11 end ===
                }
            }
        }
        nb.dirty = true;
        // === P11 begin ===
        // 落盘切到官方 `.rm`（失败自动退回 sidecar，见 `Library::save_scene_texts`）
        let (uuid, idx, n) = (nb.uuid.clone(), nb.page_index, nb.texts.len());
        // === C15 begin ===
        // 这段字打在末尾的虚拟空白页上：先把它变成真页，否则这一页没有 page id，
        // `save_scene_texts` 找不到页会直接跳过，字就丢了。
        self.c15_commit_virtual();
        let (uuid, idx) = match &self.screen {
            Screen::Notebook(nb) => (nb.uuid.clone(), nb.page_index),
            _ => (uuid, idx),
        };
        // === C15 end ===
        self.p11_save_page_texts(&uuid, idx);
        eprintln!("[dev] 文字已存 {n} 段");
        // === P11 end ===
        self.redraw_page();
    }

    fn cancel_text_editor(&mut self) {
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.text_edit = None;
        }
        self.redraw_page();
    }

    /// 编辑器打开时的点击：键盘 → 格式菜单 → 取消/完成
    fn tap_text_editor(&mut self, x: f32, y: f32) {
        // === P11 begin ===
        // 格式按钮在键盘上方的工具条里，先判它（不碰 Cancel/Done 的命中区）
        if self.tap_p11_format(x, y) {
            return;
        }
        // === P11 end ===
        for (r, k) in self.kb_hits.clone() {
            let (xi, yi) = (x as i32, y as i32);
            if xi >= r.0 && xi < r.0 + r.2 && yi >= r.1 && yi < r.1 + r.3 {
                if k == Key::Enter {
                    self.commit_text_editor();
                } else if k == Key::Hide {
                    self.commit_text_editor();
                } else {
                    self.handle_key(k);
                }
                return;
            }
        }
        match self.hits.take(x, y) {
            Some(A_OK) => self.commit_text_editor(),
            Some(A_CANCEL) => {
                if let Screen::Notebook(nb) = &mut self.screen {
                    nb.text_edit = None;
                }
                self.redraw_page();
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ 标签面板

    /// 官方 TagsTool 的弹出面板：列出所有标签（当前文档已有的是实心勾），
    /// 最后一行 "Add tag"。
    fn paint_tags_panel(&mut self) {
        self.tag_hits.clear();
        let (uuid, _) = match &self.screen {
            Screen::Notebook(nb) => (nb.uuid.clone(), ()),
            _ => return,
        };
        let mine = self.library.tags_of(&uuid);
        let all = self.library.tags();
        let n = all.len() as i32;
        // === P13 begin ===
        // n 个标签 + 最后一行 `Create new tag`
        let h = rmdev::chrome::tags_panel_height(all.len(), TAG_ROW);
        // === P13 end ===
        let x0 = panel_x(TAG_PANEL_X, TAG_PANEL_W);
        let y0 = TAG_BTN_BOTTOM - h;
        for (i, (name, count)) in all.iter().enumerate() {
            let y = y0 + i as i32 * TAG_ROW;
            self.fb.fill_rect(x0, y, TAG_PANEL_W, TAG_ROW, gray565(250));
            self.fb.fill_rect(x0 + TAG_PANEL_W, y, 2, TAG_ROW, gray565(20));
            self.fb.fill_rect(x0 + 20, y + TAG_ROW - 1, TAG_PANEL_W - 40, 1, gray565(225));
            let on = mine.iter().any(|t| t == name);
            // 勾选框
            widgets::outline_rect(&mut self.fb, x0 + 30, y + 46, 28, 28, 2, 40);
            if on {
                let c = gray565(20);
                for k in 0..10 {
                    self.fb.fill_rect(x0 + 44 + k, y + 58 + k, 3, 3, c);
                    self.fb.fill_rect(x0 + 52 - k, y + 58 + k, 3, 3, c);
                }
            }
            if let Some(t) = self.text.as_mut() {
                let label = t.ellipsize(28.0, name, TAG_PANEL_W - 200);
                t.draw(&mut self.fb, x0 + 76, y + 76, 28.0, &label, 20);
                let cnt = format!("{count}");
                let cw = t.width(24.0, &cnt) as i32;
                t.draw(&mut self.fb, x0 + TAG_PANEL_W - 24 - cw, y + 74, 24.0, &cnt, 120);
            }
            self.tag_hits
                .push(((x0, y, TAG_PANEL_W, TAG_ROW), A_TAG_TOGGLE0 + i as u32));
        }
        // Add tag
        let y = y0 + n * TAG_ROW;
        self.fb.fill_rect(x0, y, TAG_PANEL_W, TAG_ROW, gray565(250));
        self.fb.fill_rect(x0 + TAG_PANEL_W, y, 2, TAG_ROW, gray565(20));
        self.fb.fill_rect(x0, y, 2, TAG_ROW, gray565(20));
        widgets::plus_circle(&mut self.fb, x0 + 44, y + 60, 18, 20);
        if let Some(t) = self.text.as_mut() {
            // === P13 begin ===
            // 官方 `res_067497c.qml:349`：`Create new tag`
            t.draw(
                &mut self.fb,
                x0 + 76,
                y + 76,
                28.0,
                rmdev::chrome::TAG_CREATE_NEW,
                20,
            );
            // === P13 end ===
        }
        self.tag_hits
            .push(((x0, y, TAG_PANEL_W, TAG_ROW), A_TAG_ADD));
    }

    fn tags_panel_rect(&self) -> Option<(i32, i32, i32, i32)> {
        match &self.screen {
            Screen::Notebook(nb) if nb.chrome.menu == Menu::Tags => {
                let n = self.library.tags().len() as i32;
                // === P13 begin ===
                let h = rmdev::chrome::tags_panel_height(n as usize, TAG_ROW);
                // === P13 end ===
                Some((panel_x(TAG_PANEL_X, TAG_PANEL_W), TAG_BTN_BOTTOM - h, TAG_PANEL_W, h))
            }
            _ => None,
        }
    }

    fn tag_hit(&self, x: f32, y: f32) -> Option<u32> {
        let (x, y) = (x as i32, y as i32);
        self.tag_hits
            .iter()
            .rev()
            .find(|((hx, hy, hw, hh), _)| x >= *hx && y >= *hy && x < hx + hw && y < hy + hh)
            .map(|(_, a)| *a)
    }

    fn tag_click(&mut self, act: u32) {
        let uuid = match &self.screen {
            Screen::Notebook(nb) => nb.uuid.clone(),
            _ => return,
        };
        if act == A_TAG_ADD {
            if let Screen::Notebook(nb) = &mut self.screen {
                nb.tag_edit = Some(Keyboard::new());
            }
            self.redraw_page();
            return;
        }
        let idx = (act - A_TAG_TOGGLE0) as usize;
        let all = self.library.tags();
        let Some((name, _)) = all.get(idx) else {
            return;
        };
        let mut mine = self.library.tags_of(&uuid);
        if let Some(pos) = mine.iter().position(|t| t == name) {
            mine.remove(pos);
        } else {
            mine.push(name.clone());
            mine.sort();
        }
        if let Err(e) = self.library.set_tags(&uuid, &mine) {
            eprintln!("[tags] 写入失败：{e}");
        }
        eprintln!("[tags] {uuid} → {mine:?}");
        self.redraw_page();
    }

    /// "Add tag"的键盘（整屏覆盖：标题 + 输入框 + 键盘 + 取消/完成）
    fn paint_tag_editor(&mut self) {
        let Some(kb) = (match &self.screen {
            Screen::Notebook(nb) => nb.tag_edit.clone(),
            _ => None,
        }) else {
            return;
        };
        let (w, h) = (self.fb.width, self.fb.height);
        self.fb.fill_rect(0, 0, w, h, gray565(255));
        self.fb.fill_rect(0, 0, w, 2, gray565(180));
        self.hits.clear();
        if let Some(t) = self.text.as_mut() {
            t.draw(&mut self.fb, NAV_MARGIN, 100, 44.0, "Add tag", 20);
        }
        let (ix, iy, iw, ih) = (NAV_MARGIN, 140, w - 2 * NAV_MARGIN, 84);
        self.fb.fill_rect(ix, iy, iw, ih, gray565(238));
        if let Some(t) = self.text.as_mut() {
            let shown = t.ellipsize(34.0, &kb.text, iw - 40);
            t.draw(&mut self.fb, ix + 20, iy + 58, 34.0, &shown, 20);
        }
        let ky = h - kb.height() - 170;
        self.kb_hits = kb.draw(&mut self.fb, &mut self.text, 0, ky, w);
        let by = ky - 110;
        let bw = (w - 2 * NAV_MARGIN - 20) / 2;
        self.fb.fill_rect(NAV_MARGIN, by, bw, 90, gray565(210));
        self.fb.fill_rect(NAV_MARGIN + bw + 20, by, bw, 90, gray565(40));
        if let Some(t) = self.text.as_mut() {
            t.draw(&mut self.fb, NAV_MARGIN + 100, by + 60, 34.0, "Cancel", 20);
            t.draw(&mut self.fb, NAV_MARGIN + bw + 120, by + 60, 34.0, "Save", 250);
        }
        self.hits.push(NAV_MARGIN, by, bw, 90, A_CANCEL);
        self.hits.push(NAV_MARGIN + bw + 20, by, bw, 90, A_OK);
    }

    fn tap_tag_editor(&mut self, x: f32, y: f32) {
        for (r, k) in self.kb_hits.clone() {
            let (xi, yi) = (x as i32, y as i32);
            if xi >= r.0 && xi < r.0 + r.2 && yi >= r.1 && yi < r.1 + r.3 {
                if k == Key::Enter {
                    self.commit_tag();
                } else {
                    if let Screen::Notebook(nb) = &mut self.screen {
                        if let Some(kb) = nb.tag_edit.as_mut() {
                            kb.press(k);
                        }
                    }
                    self.paint_tag_editor();
                    self.fb.refresh(
                        0,
                        90,
                        self.fb.width,
                        700,
                        WAVEFORM_MODE_DU,
                        rmdev::fb::UPDATE_MODE_PARTIAL,
                    );
                }
                return;
            }
        }
        match self.hits.take(x, y) {
            Some(A_OK) => self.commit_tag(),
            Some(A_CANCEL) => {
                // 从页总览多选进来的：回多选总览，不是回页面
                let page_sel = self.pagesel.tagging();
                self.pagesel.set_tagging(false);
                if let Screen::Notebook(nb) = &mut self.screen {
                    nb.tag_edit = None;
                }
                if page_sel {
                    self.draw_overview();
                } else {
                    self.redraw_page();
                }
            }
            _ => {}
        }
    }

    fn commit_tag(&mut self) {
        // 页总览多选 → Tag：标签打在选中的页上（`.content.pageTags`）。
        // 选中页已经全都有这个标签 → 再点一次是去掉。
        let page_sel = self.pagesel.tagging();
        let Some((uuid, name)) = (match &mut self.screen {
            Screen::Notebook(nb) => nb.tag_edit.take().map(|kb| (nb.uuid.clone(), kb.text.trim().to_string())),
            _ => None,
        }) else {
            return;
        };
        if page_sel {
            self.pagesel.set_tagging(false);
            let sel = self.pagesel.selected();
            if !name.is_empty() && !sel.is_empty() {
                let ids = self.library.page_ids(&uuid);
                let tags = self.library.page_tags(&uuid);
                let all_have = sel.iter().all(|&i| {
                    let Some(id) = ids.get(i) else {
                        return false;
                    };
                    tags.iter().any(|(p, ts)| p == id && ts.iter().any(|t| t == &name))
                });
                match self.library.tag_pages(&uuid, &sel, &name, !all_have) {
                    Ok(n) => eprintln!(
                        "[tags] 页标签{} {name}（{n} 页）",
                        if all_have { "去掉" } else { "打上" }
                    ),
                    Err(e) => eprintln!("[tags] 页标签写入失败：{e}"),
                }
            }
            self.draw_overview();
            return;
        }
        if !name.is_empty() {
            let mut mine = self.library.tags_of(&uuid);
            if !mine.iter().any(|t| t == &name) {
                mine.push(name.clone());
                mine.sort();
            }
            if let Err(e) = self.library.set_tags(&uuid, &mine) {
                eprintln!("[tags] 写入失败：{e}");
            }
            eprintln!("[tags] 新增 {name} → {mine:?}");
        }
        self.redraw_page();
    }

    // ------------------------------------------------------------ 图层改名（6a）

    /// 打开图层改名键盘。名字先填当前值；官方进键盘会全选，我们这里没有
    /// 选中态，直接清空到行尾不方便，保留原名让用户自己删。
    fn open_layer_rename(&mut self, i: usize) {
        let Some(name) = (match &self.screen {
            Screen::Notebook(nb) => nb.page.layers.get(i).map(|l| l.name.clone()),
            _ => None,
        }) else {
            return;
        };
        let mut kb = Keyboard::new();
        kb.text = name;
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.layer_rename = Some(LayerRename { index: i, kb });
        }
        eprintln!("[layers] 改名：第 {} 层", i + 1);
        self.redraw_page();
    }

    /// `Rename Layer`：官方 `Header`（黑头 `Cancel` / 标题 / `Save`，
    /// `res_067887b.qml:2141-2152`）+ 输入框 + 键盘。
    fn paint_layer_rename(&mut self) {
        let Some(kb) = (match &self.screen {
            Screen::Notebook(nb) => nb.layer_rename.as_ref().map(|r| r.kb.clone()),
            _ => None,
        }) else {
            return;
        };
        let (w, h) = (self.fb.width, self.fb.height);
        self.fb.fill_rect(0, 0, w, h, gray565(255));
        self.hits.clear();
        // ---------- 黑头：Cancel | Rename Layer | Save
        self.fb.fill_rect(0, 0, w, nbsettings::HDR_H, gray565(0));
        let valid = layer_name_valid(&kb.text);
        if let Some(t) = self.text.as_mut() {
            t.draw_bold(&mut self.fb, 32, 74, 33.0, "Cancel", 255);
            let title = "Rename Layer";
            let tw = t.width_bold(33.0, title);
            t.draw_bold(
                &mut self.fb,
                (w as f32 / 2.0 - tw / 2.0) as i32,
                74,
                33.0,
                title,
                255,
            );
            let save = "Save";
            let sw = t.width_bold(33.0, save);
            // 官方 `acceptEnabled = Library.isValidName(...)`：名字不合法时 Save 变灰不可点
            t.draw_bold(
                &mut self.fb,
                w - 32 - sw as i32,
                74,
                33.0,
                save,
                if valid { 255 } else { 120 },
            );
        }
        self.hits.push(0, 0, 240, nbsettings::HDR_H, A_CANCEL);
        if valid {
            self.hits.push(w - 240, 0, 240, nbsettings::HDR_H, A_OK);
        }
        // ---------- 输入框（当前层名，官方进键盘会全选）
        let (ix, iy, iw, ih) = (NAV_MARGIN, 150, w - 2 * NAV_MARGIN, 84);
        self.fb.fill_rect(ix, iy, iw, ih, gray565(238));
        if let Some(t) = self.text.as_mut() {
            // 只有一行：太长时从**尾部**截（键盘是追加输入的，得看得见刚敲的字）
            let limit = (iw - 40) as f32;
            let mut shown = kb.text.clone();
            while shown.chars().count() > 1 && t.width(34.0, &shown) > limit {
                shown.remove(0);
            }
            t.draw(&mut self.fb, ix + 20, iy + 58, 34.0, &shown, 20);
        }
        // ---------- 键盘贴底
        let ky = h - kb.height();
        self.kb_hits = kb.draw(&mut self.fb, &mut self.text, 0, ky, w);
    }

    fn tap_layer_rename(&mut self, x: f32, y: f32) {
        let (xi, yi) = (x as i32, y as i32);
        for (r, k) in self.kb_hits.clone() {
            if xi >= r.0 && xi < r.0 + r.2 && yi >= r.1 && yi < r.1 + r.3 {
                if k == Key::Enter {
                    self.commit_layer_rename();
                } else {
                    if let Screen::Notebook(nb) = &mut self.screen {
                        if let Some(rn) = nb.layer_rename.as_mut() {
                            rn.kb.press(k);
                        }
                    }
                    self.paint_layer_rename();
                    // 只有标题栏（Save 灰/白）和输入框那一行会变：分开刷两块，别整屏推
                    for (y, hh) in [(0, nbsettings::HDR_H), (140, 120)] {
                        self.fb.refresh(
                            0,
                            y,
                            self.fb.width,
                            hh,
                            WAVEFORM_MODE_DU,
                            rmdev::fb::UPDATE_MODE_PARTIAL,
                        );
                    }
                }
                return;
            }
        }
        match self.hits.take(x, y) {
            Some(A_OK) => self.commit_layer_rename(),
            Some(A_CANCEL) => {
                if let Screen::Notebook(nb) = &mut self.screen {
                    nb.layer_rename = None;
                }
                self.kb_hits.clear();
                self.redraw_page();
            }
            _ => {}
        }
    }

    /// 改完名立刻写回当前页（`.rm` 走 `RmPage::serialize` 那条老路）。
    fn commit_layer_rename(&mut self) {
        let (uuid, page_index, index, name) = match &self.screen {
            Screen::Notebook(nb) => match &nb.layer_rename {
                Some(rn) => (nb.uuid.clone(), nb.page_index, rn.index, rn.kb.text.clone()),
                None => return,
            },
            _ => return,
        };
        if !layer_name_valid(&name) {
            return;
        }
        let mut done = false;
        if let Screen::Notebook(nb) = &mut self.screen {
            if nb.page.rename_layer(index, &name) {
                nb.layer_rename = None;
                nb.dirty = true;
                match self.library.save_page_at(&uuid, page_index, &nb.page) {
                    Ok(()) => nb.dirty = false,
                    Err(e) => eprintln!("[layers] 改名落盘失败：{e}"),
                }
                // === P11 begin ===
                // 整页写回把 `RootText` 冲掉了：文字层补回去
                if !nb.dirty {
                    self.p11_repair_text_layer();
                }
                // === P11 end ===
                done = true;
            }
        }
        if done {
            eprintln!("[layers] 第 {} 层 → {name:?}", index + 1);
        }
        self.kb_hits.clear();
        self.redraw_page();
    }

    /// 点到了面板里的哪一块（后画的优先，和 Hits 的规则一致）
    fn layer_hit(&self, x: f32, y: f32) -> Option<LayerAct> {
        let (x, y) = (x as i32, y as i32);
        if env_flag("RM_DEBUG") && !self.layer_hits.is_empty() {
            eprintln!("[layers] 查 ({x},{y}) 命中区 {} 个", self.layer_hits.len());
        }
        self.layer_hits
            .iter()
            .rev()
            .find(|((hx, hy, hw, hh), _)| {
                x >= *hx && y >= *hy && x < hx + hw && y < hy + hh
            })
            .map(|(_, a)| *a)
    }

    /// 面板里点了一下
    fn layer_click(&mut self, act: LayerAct) {
        let mut changed = false;
        // 双击层名 = 改名（官方是长按；双击只是同一入口的另一个触发方式）
        let mut rename = None;
        if let Screen::Notebook(nb) = &mut self.screen {
            match act {
                LayerAct::Select(i) => {
                    if i < nb.page.layers.len() {
                        nb.page.current = i;
                        changed = true;
                        let now = std::time::Instant::now();
                        let again = self.layer_tap.is_some_and(|(j, t)| {
                            j == i && now.duration_since(t) < Duration::from_millis(400)
                        });
                        if again {
                            self.layer_tap = None;
                            rename = Some(i);
                        } else {
                            self.layer_tap = Some((i, now));
                        }
                    }
                }
                LayerAct::Eye(i) => {
                    if let Some(l) = nb.page.layers.get_mut(i) {
                        l.visible = !l.visible;
                        changed = true;
                    }
                }
                LayerAct::Up(i) => changed = nb.page.move_layer_up(i),
                LayerAct::Down(i) => changed = nb.page.move_layer_down(i),
                LayerAct::Merge(i) => changed = nb.page.merge_down(i),
                LayerAct::Delete(i) => changed = nb.page.delete_layer(i),
                LayerAct::Bg => {}
                LayerAct::Add => changed = nb.page.add_layer().is_some(),
                // === P11 begin ===
                // 官方文字层行（`res_0675d6b.qml:81-90`）点一下 = 切到文字工具
                LayerAct::Text => {}
                // === P11 end ===
            }
            if changed {
                // 换层/调序/删层之后，选区下标指向的已经不是原来那些笔迹了
                nb.clear_sel();
                // 删层/调序会让撤销栈里的层下标错位，宁可清空也不能撤到别的层上
                if matches!(
                    act,
                    LayerAct::Up(_) | LayerAct::Down(_) | LayerAct::Merge(_) | LayerAct::Delete(_)
                ) {
                    nb.undo.clear();
                    nb.redo.clear();
                }
                nb.dirty = true;
                eprintln!(
                    "[dev] 图层 {:?} → 共 {} 层，当前 {}",
                    act,
                    nb.page.layers.len(),
                    nb.page.current + 1
                );
            }
        }
        // === P11 begin ===
        // 文字层那一行：切到文字工具（和工具条上的 "A" 按钮同一条路径）
        if act == LayerAct::Text {
            if let Screen::Notebook(nb) = &mut self.screen {
                nb.chrome.selected = TOOLS.len() - 1;
                nb.chrome.erasing = false;
                nb.chrome.selecting = false;
                nb.clear_sel();
                nb.chrome.menu = Menu::None;
            }
            self.redraw_page();
            return;
        }
        // === P11 end ===
        if let Some(i) = rename {
            self.on_chrome(Msg::LayerRename(i));
            return;
        }
        if changed {
            self.redraw_page();
        }
    }

    /// 底部的 "Page N of M"。官方：居中，基线约 1846，字号 34。
    /// 键盘弹起来时官方会把它藏掉，否则会压在键盘上。
    fn paint_footer(&mut self) {
        if matches!(&self.screen, Screen::Notebook(nb) if nb.text_edit.is_some()) {
            return;
        }
        // 图层改名是 modal 黑头 + 键盘，页码不压在键盘上
        if matches!(&self.screen, Screen::Notebook(nb) if nb.layer_rename.is_some()) {
            return;
        }
        // 文档设置面板（官方那个 foldout）盖着页脚，页码让位：
        // `35-docsettings-nb.png` 底部没有 Page 3 of 3，而且面板右缘会压住页码。
        if self.ds.is_open() {
            return;
        }
        // 笔记本里的 toast：黑条盖在底部（和库里那条同一套尺寸/字号），有就不再画页码
        if let Some(msg) = self.banner.clone() {
            let (w, h) = (self.fb.width, self.fb.height);
            self.fb.fill_rect(0, h - 120, w, 120, gray565(30));
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, NAV_MARGIN, h - 46, 34.0, &msg, 245);
            }
            return;
        }
        // === P13 begin ===
        // 剪贴板提示条（官方 `res_067887b.qml:1672-1765` 的 `notifierBackground`）：
        // 整条贴底、黑底白字，右边一个 `Clear clipboard`。它有内容且在选区模式时出现，
        // 盖住页码 —— 官方的提示条也压着页脚。
        let (has_clip, selecting) = match &self.screen {
            Screen::Notebook(nb) => (!self.clipboard.is_empty(), nb.chrome.selecting),
            _ => (false, false),
        };
        if rmdev::chrome::clip_toast_visible(has_clip, selecting) {
            let (w, h) = (self.fb.width, self.fb.height);
            let (x, y, bw, bh) = rmdev::chrome::clip_toast_rect(w, h);
            let (cx, ..) = rmdev::chrome::clip_clear_rect(w, h);
            self.fb.fill_rect(x, y, bw, bh, gray565(0));
            // 复制图标：两个叠起来的方框（官方 `Icons.icon_duplicate`）
            widgets::outline_rect(&mut self.fb, x + 30, y + 22, 22, 26, 2, 250);
            widgets::outline_rect(&mut self.fb, x + 36, y + 16, 22, 26, 2, 250);
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, x + 72, y + 46, 28.0, rmdev::chrome::CLIP_COPIED, 250);
                // 竖线 + `Clear clipboard`（官方 `Rectangle { height: 40; width: 1 }` + IconButton）
                self.fb.fill_rect(cx - 20, y + 16, 1, 40, gray565(250));
                // × 图标（`Icons.icon_x_medium`）
                for k in 0..14 {
                    self.fb.fill_rect(cx + k, y + 29 + k, 3, 3, gray565(250));
                    self.fb.fill_rect(cx + 13 - k, y + 29 + k, 3, 3, gray565(250));
                }
                t.draw(&mut self.fb, cx + 34, y + 46, 28.0, rmdev::chrome::CLIP_CLEAR, 250);
            }
            return;
        }
        // === P13 end ===
        let (page, total) = match &self.screen {
            Screen::Notebook(nb) => (nb.page_index + 1, nb.page_count),
            _ => (1, 1),
        };
        // 工具条收起时官方连页码一起收走（整屏只剩左上角那个圆环）
        if matches!(&self.screen, Screen::Notebook(nb) if !nb.chrome.visible) {
            return;
        }
        let label = format!("Page {page} of {total}");
        let (w, h) = (self.fb.width, self.fb.height);
        if let Some(t) = self.text.as_mut() {
            let tw = t.width(34.0, &label) as i32;
            t.draw(&mut self.fb, (w - tw) / 2, h - 26, 34.0, &label, 20);
        }
    }

    /// 右上角的关闭文档按钮：一个圆环里一个 ×。只画不刷。
    fn paint_close(&mut self) {
        // 工具条收起 = 官方 fullscreen：连 header 的 × 一起消失，只剩左上角那个圆环
        if matches!(&self.screen, Screen::Notebook(nb) if !nb.chrome.visible) {
            return;
        }
        // 图层改名的黑头是 modal：只留 Cancel / Save，不画文档的 ×
        if matches!(&self.screen, Screen::Notebook(nb) if nb.layer_rename.is_some()) {
            return;
        }
        if env_flag("RM_DEBUG") {
            eprintln!("[dev] paint_close");
        }
        let (cx, cy, r) = (rmdev::fb::flip_x(rmdev::fb::logical().0 - 61, 0), 58, 20);
        let c = gray565(20);
        let rr = (r * r) as f32;
        let ri = ((r - 3) * (r - 3)) as f32;
        for dy in -r..=r {
            for dx in -r..=r {
                let d = (dx * dx + dy * dy) as f32;
                if d <= rr && d >= ri {
                    self.fb.put(cx + dx, cy + dy, c);
                }
            }
        }
        for i in 0..13 {
            for t in 0..3 {
                self.fb.put(cx - 6 + i, cy - 6 + i + t, c);
                self.fb.put(cx + 6 - i, cy - 6 + i + t, c);
            }
        }
    }

    /// 单独刷一次关闭按钮（笔迹盖住它之后补画用）。
    fn draw_close(&mut self) {
        self.paint_close();
        self.fb.refresh(
            rmdev::fb::flip_x(rmdev::fb::logical().0 - 94, 60),
            24,
            60,
            60,
            WAVEFORM_MODE_DU,
            rmdev::fb::UPDATE_MODE_PARTIAL,
        );
    }

    // ------------------------------------------------------------ 输入

    /// 抬起手指/笔时调用：按住超过 600ms 且没怎么移动就算长按，弹菜单。
    /// 主循环每圈调一次：**按住 550ms 就弹菜单**。
    /// 不再等抬笔事件——注入的抬笔时序/坐标都不可靠，而且真手指长按时
    /// 官方也是按住就弹。
    fn tick(&mut self) {
        // 慢 tick 归因：每段只花几个 `Instant::now()`（vDSO，~20 ns），
        // 但只有整次 tick 超过 200 ms 才把各段打出来。用户报的"整个界面
        // 冻住一会儿"就是主线程在这里被按住，没有这条日志只能靠猜。
        let t_all = std::time::Instant::now();
        let mut t_prev = t_all;
        let mut segs: Vec<(&'static str, u128)> = Vec::new();
        macro_rules! bump {
            ($n:literal) => {{
                let d = t_prev.elapsed().as_millis();
                if d >= 40 {
                    segs.push(($n, d));
                }
                t_prev = std::time::Instant::now();
            }};
        }
        // 横屏里开了竖屏专用浮层（或已经不在文档里）→ 先转回竖屏。
        // 放在 tick 最前面：这些东西都是"下一帧就要画"的，晚一步就会画歪。
        if self.landscape() && self.portrait_only_open() {
            if matches!(self.screen, Screen::Notebook(_)) {
                self.set_landscape(false);
            } else {
                self.force_portrait();
                self.repaint_current();
            }
        }
        // === P6 begin ===
        // 后台封面渲好了：库里要重绘一次才看得到（否则白卡会一直挂到下一次触摸）。
        let got_covers = p6_poll_covers();
        bump!("封面轮询");
        if got_covers && matches!(self.screen, Screen::Library) {
            self.draw_library();
        }
        // === P6 end ===
        bump!("重画库");
        // 抓屏请求（`tools/shot.sh` 用 `kill -USR1` 触发）
        if SHOT_REQ.swap(false, std::sync::atomic::Ordering::Relaxed) {
            match self.fb.dump_pgm("/tmp/rmdev.shot.pgm") {
                Ok(()) => eprintln!("[shot] 已写 /tmp/rmdev.shot.pgm"),
                Err(e) => eprintln!("[shot] 写失败：{e}"),
            }
        }
        // 睡着：什么都不做（面板不用刷新，也不该自动存盘/预热）
        if self.asleep.is_some() {
            return;
        }
        // === D6 begin ===
        self.d6_tick();
        // === D6 end ===
        // === C15 begin ===
        // Live Note 的回答：流式结束 → 后台排墨迹 → 落进 AI 图层 + 局部刷新
        self.c15_live_ink();
        // === C15 end ===

        // 电源键按住 3 s → 关机框。官方是计时器 `onTriggered` 弹框，
        // 不等松手，所以这里放在最前面、每圈都问一次。
        if !self.poweroff && self.pg.poll_long(std::time::Instant::now()) {
            self.open_poweroff();
            return;
        }
        // Auto sleep：官方 Battery settings 原话 "Puts your reMarkable to sleep
        // after 20 minutes of inactivity."，对应 `Settings.idleSuspendDelay`。
        // 只按输入事件算空闲——官方也是看输入，不看你有没有在看书。
        if self.auto_sleep && !self.poweroff {
            let delay = auto_sleep_delay();
            let idle = self.last_ev.elapsed();
            if !delay.is_zero() && idle >= delay {
                eprintln!("[sleep] 空闲 {} s，自动睡眠", idle.as_secs());
                self.sleep_now();
                return;
            }
        }
        self.poll_job();
        // === D5 begin ===
        // 转文字的后台结果：写文字层 + 重绘（不能卡主循环，这里只 try_recv）
        self.d5_poll_ocr();
        // === D5 end ===
        // === E2 begin ===
        self.e2_ocr_tick();
        // === E2 end ===
        // === C12 begin ===
        // AI 批改的后台结果：写进 AI 图层 + 重绘
        self.c12_poll();
        // === C12 end ===
        // === E1 begin: 错题列表的后台回复（try_recv，绝不阻塞主循环）===
        self.e1_poll();
        // === E1 end ===
        // === D1 begin: 框选问 AI（上传结果 / SSE 增量 / 限速刷卡） ===
        self.d1_tick();
        // === D1 end ===
        // === C4 begin ===
        // 云端事件：rev → 后台同步（回来重画当前屏），cmd → 后台执行并把结果回给服务器。
        self.c4_poll_cloud();
        // === C4 end ===
        bump!("云事件");
        // === P3 begin ===
        // 通知 TTL（官方 popupTimeout = 4 s）：过期的从队列里清掉，库界面跟着重画。
        self.tick_notices();
        // === P3 end ===
        // === P4 begin ===
        // 磁盘水位：启动算过一次（`App::new`），之后每 60 s 重算一次。
        // 只有状态变了才重画/弹通知，不能每分钟刷一遍库。
        if self
            .disk_checked
            .map_or(true, |t| t.elapsed() >= Duration::from_secs(60))
        {
            self.p4_check_disk();
        }
        bump!("磁盘水位");
        // === P4 end ===
        // === BATT begin ===
        // 电池电流采样：每 30 s 读一次 sysfs，塞进 10 分钟滑动窗口。
        // 读 sysfs 要 readdir + 几个文件，不能每圈都做；睡着时 tick 早退，不采。
        let now = std::time::Instant::now();
        if self
            .battery_sample_at
            .map_or(true, |t| now.saturating_duration_since(t) >= Duration::from_secs(30))
        {
            self.battery_sample_at = Some(now);
            // 用快照：`battery::read()` 一次 440 ms，不能在 tick 里同步等。
            if let Some(ma) = battery_cached().and_then(|b| b.current_ma()) {
                // 有的内核放电时报负电流，平均耗电只关心大小。
                self.battery_usage.push(now, ma.abs());
            }
        }
        // === BATT end ===
        bump!("电池");
        self.poll_wifi_scan();
        // === C0W begin: 后台连接结果（最长 20 s，每圈收一次）===
        self.poll_wifi_conn();
        // === C0W end ===
        // === SYNC-AUTO begin: 每 10 分钟自动同步一次（启动后 15 s 先来一次）===
        //
        // 同步本身全在后台线程里（`start_sync` 起线程，结果由 `poll_job` 收），
        // 主循环这边只有一次 `Instant` 比较；完成时没拉到东西连重绘都不做。
        if now >= self.sync_auto_at {
            // 没人在看（120 s 没输入）又没插电：这一轮直接不做网络 I/O。
            // 隔 2 分钟再看一眼 —— 用户一碰设备，`handle()` 会把这一轮提前叫醒。
            let idle = self.last_ev.elapsed() >= Duration::from_secs(SYNC_IDLE_AFTER);
            if idle && !self.charging() {
                // 不碰网络，5 分钟后再说；用户一碰设备就立刻补（见 `handle`）
                self.sync_skip_idle = true;
                self.sync_auto_at = now + Duration::from_secs(300);
            } else {
                self.sync_skip_idle = false;
                let secs = self.sync_next_secs();
                self.sync_auto_at = now + Duration::from_secs(secs);
                eprintln!(
                    "[sync] 自动同步（下一轮 {} 分钟后；连续 {} 轮无变化）",
                    secs / 60,
                    self.sync_quiet
                );
                self.start_sync_auto();
            }
        }
        // === SYNC-AUTO end ===
        // === SB begin: 状态栏的 Wi-Fi/云状态变化时只补刷那一条 ===
        // 后台线程（`spawn_net_reader`）每 5 s 更新一次快照；这里只做比较，
        // 变了才刷 —— 否则每圈都刷会一直占着面板。
        let ns = net_state();
        if ns != self.net_last {
            self.net_last = ns;
            self.repaint_status();
        }
        // === SB end ===
        bump!("扫描");
        // === P7 begin ===
        // 页滑块静置自动收（官方 `hidePageSliderTimer`，`Values.popupTimeout`
        // = 4000 ms，见 `slider::HIDE_MS`）。按着的时候（`Pressed`/`Canceled`）
        // 不收 —— 官方的 `onTriggered` 也是 `if (!pageSlider.pressed)`。
        if let Some((t0, _, slider::State::Idle)) = self.slider {
            if t0.elapsed() >= Duration::from_millis(slider::HIDE_MS) {
                self.p7_hide();
            }
        }
        // 离开文档正文视图（回库 / 进页总览 / 关掉文档）就把状态丢掉，
        // 免得切回来时冒出来一个没人拖的游标。
        if self.slider.is_some()
            && !matches!(&self.screen, Screen::Notebook(nb) if !nb.overview)
        {
            self.slider = None;
        }
        // === P7 end ===
        // === P13 begin ===
        // 文档内搜索分步扫描：每 tick 一页（`submit_doc_search` 起头）
        if self.search.scanning() {
            self.poll_doc_search();
        }
        // === P13 end ===
        bump!("页面状态");
        // === P1 begin ===
        // USB web 服务：每 5 s 对一次账（开关 / 网卡 / 服务实际状态）。
        self.poll_usb_web();
        bump!("USB web");
        if t_all.elapsed().as_millis() >= 150 {
            eprintln!("[slow] tick 前段 {}ms {segs:?}", t_all.elapsed().as_millis());
        }
        // === SHARE begin ===
        // 屏幕共享读的是我们自己导出的 PGM（rm2fb 的 shm 已经没人写了）。
        if rmdev::share::active_port().is_some()
            && self.share_at.elapsed() >= Duration::from_millis(1000)
        {
            self.share_at = std::time::Instant::now();
            if let Err(e) = self.fb.dump_pgm(rmdev::share::FRAME_PGM) {
                eprintln!("[share] 导帧失败：{e}");
            }
        }
        // === SHARE end ===
        // === P1 end ===
        // 锁屏倒计时：每秒重画一次剩余秒数（只有数字变了才刷）。
        let mut redraw_lock = false;
        if let Screen::Lock(l) = &mut self.screen {
            let rem = l.lockout.remaining_secs(std::time::Instant::now());
            if rem > 0 && rem != l.shown_secs {
                l.shown_secs = rem;
                l.msg = format!("Try again in {rem} seconds.");
                redraw_lock = true;
            } else if rem == 0 && l.shown_secs > 0 {
                l.shown_secs = 0;
                l.msg.clear();
                redraw_lock = true;
            }
        }
        if redraw_lock {
            self.draw_lock();
        }
        // 设置页提示条 4 秒后消失。
        let mut clear_notice = false;
        if let Screen::Settings(s) = &self.screen {
            if let Some((_, until)) = &s.notice {
                clear_notice = *until <= std::time::Instant::now();
            }
        }
        if clear_notice {
            if let Screen::Settings(s) = &mut self.screen {
                s.notice = None;
            }
            self.draw_settings();
        }
        // === P5 begin: Factory reset 顶部提示 6 秒后也清掉（和 toast 同一套）===
        let mut clear_done = false;
        if let Screen::Settings(s) = &self.screen {
            if let Some((_, until)) = &s.factory_done {
                clear_done = *until <= std::time::Instant::now();
            }
        }
        if clear_done {
            if let Screen::Settings(s) = &mut self.screen {
                s.factory_done = None;
            }
            self.draw_settings();
        }
        // === P5 end ===
        // === P4 begin ===
        // 只读（磁盘满）时存盘一定会失败：把自动存盘的计时一直往后推，
        // 否则主循环每 500 µs 一圈都在重试、刷日志。内存里的笔迹不丢，
        // 水位回到 Ok 后最多 5 s 就会照常落盘。
        if self.library.is_read_only() {
            self.last_save = std::time::Instant::now();
        }
        // === P4 end ===
        // 自动存盘：手停 5 秒就把当前页写回磁盘。
        // 以前只在翻页/退出时存，进程一被 kill（部署、崩溃、断电）当前页就没了。
        if self.last_save.elapsed() >= Duration::from_secs(5) {
            let mut saved = false;
            if let Screen::Notebook(nb) = &mut self.screen {
                if nb.dirty
                    && nb.text_edit.is_none()
                    && self.last_ev.elapsed() >= Duration::from_secs(5)
                {
                    match self.library.save_page_at(&nb.uuid, nb.page_index, &nb.page) {
                        Ok(()) => {
                            nb.dirty = false;
                            saved = true;
                            // === P11 begin ===
                            // 整页写回把 `RootText` 冲掉了：文字层补回去
                            self.p11_repair_text_layer();
                            // === P11 end ===
                        }
                        Err(e) => eprintln!("[dev] 自动存盘失败：{e}"),
                    }
                }
            }
            if saved {
                self.last_save = std::time::Instant::now();
                if env_flag("RM_TIMING") {
                    eprintln!("[dev] 自动存盘完成");
                }
            }
        }
        // 空闲预热：手停了 400ms 才动，免得抢用户正在等的这一次刷新。
        if let Some(job) = self.prefetch_job.take() {
            if self.last_ev.elapsed() < Duration::from_millis(400) {
                self.prefetch_job = Some(job);
            } else {
                let (path, page, ww, hh) = job;
                let t = std::time::Instant::now();
                if let Ok(pdf) = Pdf::open(&path) {
                    let _ = pdf.prefetch(page as u32, ww as u32, hh as u32);
                }
                if env_flag("RM_TIMING") {
                    eprintln!("[pdf] 空闲预热第 {} 页（{}ms）", page + 1, t.elapsed().as_millis());
                }
            }
        }
        // === P12 begin ===
        // 抽屉开着 = 盖住了半屏内容：按住不能穿透去弹下面的卡片菜单
        if self.drawer.is_open() {
            self.press = None;
            return;
        }
        // === P12 end ===
        // === AIREF begin ===
        // 手指按在 AI 框上够 800 ms：弹 `View`（笔尖那条路完全不碰）。
        if let Some((_, _, t0, m)) = self.ai_hold.clone() {
            if t0.elapsed() >= std::time::Duration::from_millis(LONG_PRESS_MS) {
                self.ai_hold = None;
                let r = rmdev::airefs::view_rect(&m, self.fb.width, self.fb.height);
                self.ai_view = Some((m, r));
                self.ai_view_fresh = true;
                eprintln!("[ai] 长按引用，弹 View");
                self.redraw_page();
                return;
            }
        }
        // === AIREF end ===
        let Some((x, y, t0)) = self.press else {
            return;
        };
        if t0.elapsed() < std::time::Duration::from_millis(LONG_PRESS_MS) {
            return;
        }
        self.press = None;
        // === C0W begin: 设置页 Wi-Fi"当前网络"行：长按 → Forget 确认框 ===
        // 官方 `res_0689b2f.qml` 的 ssidItem 是 `onClicked`（弹 Connect/Forget），
        // 我们的交互按需求改成"长按当前网络行 → Forget"，所以这里先拦下来。
        if matches!(&self.screen, Screen::Settings(_)) {
            if self.wifi_hold(x, y) {
                self.skip_up = true; // 别让这次抬指顺手再点一下
            }
            return;
        }
        // === C0W end ===
        let in_overview = matches!(&self.screen, Screen::Notebook(nb) if nb.overview);
        let in_library = matches!(&self.screen, Screen::Library);
        let in_layers = matches!(&self.screen, Screen::Notebook(nb) if nb.chrome.menu == Menu::Layers);
        if in_layers {
            // 官方：层名 `onPressAndHold` → 弹 `Rename Layer` 键盘。
            // 抬指那一下本来就不会结算成点击（按下时已被 ui_tap 吃掉），不用 skip_up。
            if let Some(LayerAct::Select(i)) = self.layer_hit(x, y) {
                self.on_chrome(Msg::LayerRename(i));
            }
        } else if in_overview {
            if self.open_page_select(x, y) {
                self.skip_up = true;
            }
        } else if in_library && self.view == View::Tags && !self.menu_open {
            if self.tags_hold(x, y) {
                self.skip_up = true; // 别让这次抬笔顺手把操作条点掉
            }
        } else if in_library && self.ctx.is_none() && !self.menu_open {
            if self.library_hold(x, y) {
                self.skip_up = true;
            }
        }
        bump!("长按/存盘");
        let total = t_all.elapsed().as_millis();
        if total >= 200 {
            eprintln!("[slow] tick {total}ms {segs:?}");
        }
    }

    /// 库里的长按：按下点落在哪张卡片上
    /// 落点下是哪一项。用画的时候记下的 `tiles`：网格和列表共用一套，
    /// 列表模式没有「行列公式」可算。
    fn item_at(&self, x: f32, y: f32) -> Option<Book> {
        let (xi, yi) = (x as i32, y as i32);
        let (uuid, ..) = self
            .tiles
            .iter()
            .find(|(_, tx, ty, tw, th)| xi >= *tx && xi < tx + tw && yi >= *ty && yi < ty + th)?;
        self.visible.iter().find(|b| &b.uuid == uuid).cloned()
    }

    fn library_hold(&mut self, x: f32, y: f32) -> bool {
        // === P8 begin ===
        // 长按 = 进多选并选中落点下这一项（官方 `onPressAndHold:
        // Library.documentSelection.toggle(entry.id)`）。已经在多选里 = 切换选中。
        if self.lib_tag.is_some() {
            return true; // 标签键盘开着，别在背后改选中
        }
        if self.trash_confirm.is_some() {
            return true; // 永久删除确认框是模态的
        }
        let Some(b) = self.item_at(x, y) else {
            return false;
        };
        if self.lib_moving {
            return true; // 选目标文件夹时长按不改变选中
        }
        self.ctx = None;
        self.ctx_at = None;
        self.sort_open = false;
        self.menu_open = false;
        self.filters_open = false;
        self.lib_sel_toggle(&b.uuid);
        eprintln!(
            "[libsel] 长按 {} → 选中 {} 项",
            b.name,
            self.lib_selected().len()
        );
        self.draw_library();
        true
        // === P8 end ===
    }

    /// 按下后一动超过 `SCROLL_LIMIT` 就把长按候选作废。
    ///
    /// 只清 `press`（长按候选），不动 `pending_tap`（"这次抬指算不算点击"）：
    /// 两件事是分开的 —— 手指拖出 20 px 之后，这次既不该弹菜单，也不该算点击，
    /// 后者由原来那套 40 px / 24 px 的判定继续管。
    fn cancel_long_press_if_moved(&mut self, x: f32, y: f32) {
        if let Some((x0, y0, _)) = self.press {
            if (x - x0).abs() > SCROLL_LIMIT || (y - y0).abs() > SCROLL_LIMIT {
                self.press = None;
            }
        }
    }

    fn maybe_long_press(&mut self, x: f32, y: f32) -> bool {
        // === P12 begin ===
        // 抽屉开着时同理：半屏内容被盖住，长按不弹卡片菜单
        if self.drawer.is_open() {
            return false;
        }
        // === P12 end ===
        // 只有库界面有这套长按菜单：设置/搜索页的笔事件也走这条路径，
        // 别拿上一次库界面留下的 tiles 误弹库菜单。
        if !matches!(self.screen, Screen::Library) {
            return false;
        }
        let Some((px, py, t0)) = self.press.take() else {
            if env_flag("RM_DEBUG") {
                eprintln!("[ctx] 没有 press 记录");
            }
            return false;
        };
        if env_flag("RM_DEBUG") {
            eprintln!(
                "[ctx] 按住 {}ms 位移 {:.0},{:.0}",
                t0.elapsed().as_millis(),
                x - px,
                y - py
            );
        }
        if t0.elapsed() < std::time::Duration::from_millis(LONG_PRESS_MS) {
            return false;
        }
        if (x - px).abs() > SCROLL_LIMIT || (y - py).abs() > SCROLL_LIMIT {
            return false;
        }
        // 用**按下点**找目标：抬手事件常常带的是面板的"无触摸"坐标 (0,1871)，
        // 拿它去找必然落空。
        let hit = self.item_at(px, py);
        if env_flag("RM_DEBUG") {
            eprintln!(
                "[ctx] 落点 {:.0},{:.0} → {:?}",
                px,
                py,
                hit.as_ref().map(|b| b.name.clone())
            );
        }
        // === P8 begin ===
        if let Some(b) = hit {
            if self.lib_tag.is_some() {
                return true;
            }
            self.ctx = None;
            self.ctx_at = None;
            self.sort_open = false;
            self.menu_open = false;
            self.filters_open = false;
            self.lib_sel_toggle(&b.uuid);
            eprintln!(
                "[libsel] 长按 {} → 选中 {} 项",
                b.name,
                self.lib_selected().len()
            );
            self.draw_library();
            return true;
        }
        false
        // === P8 end ===
    }

    // === P8 begin ===
    // ------------------------------------------------------------ 库文档多选
    //
    // 官方 `res_06a18e1.qml`：长按 `documentSelection.toggle(id)` 进选择态，
    // 顶栏 `Cancel` + 选中数 + `Select all` / `Deselect all`，底部动作条按
    // 选中内容显隐（显隐规则在 `panels::libbar::items`）。
    // 回收站沿用原来的 `trash_sel`（点卡片本来就是多选），其余视图用
    // `lib_sel`（BTreeSet，天然去重且按 uuid 有序）。

    /// 这个条目在不在当前选择态里。
    fn lib_is_sel(&self, uuid: &str) -> bool {
        if self.view == View::Trash {
            self.trash_sel.iter().any(|u| u == uuid)
        } else {
            self.lib_sel.contains(uuid)
        }
    }

    // === P12 begin ===
    /// 抽屉数据（Recent / Favorites / Tags）只在打开时抓一次，画的时候不再扫库。
    fn reload_drawer(&mut self) {
        self.drawer.reload(&self.library);
    }

    /// 收抽屉 + 重画库（抽屉压在内容上，收起来必须整屏重画）。
    fn p12_close(&mut self) {
        if self.drawer.is_open() {
            self.drawer.close();
            self.draw_library();
        }
    }

    /// 切库视图（和侧栏 A_SIDE0 那套一致：滚动归零、清过滤、Tags 走 on_open）。
    fn p12_set_view(&mut self, v: View) {
        self.view = v;
        self.scroll = 0;
        self.tag_filter = None;
        if v == View::MyFiles {
            self.trail.clear();
        }
        if v == View::Tags {
            self.tags.on_open();
        }
        self.menu_open = false;
        self.sort_open = false;
    }

    /// 抽屉的点击分发。返回 true = 这一下归抽屉，库里的其它命中一律吞掉
    /// （官方那个抽屉就是个铺满屏幕的 MouseArea）。
    fn p12_tap(&mut self, x: f32, y: f32) -> bool {
        if !self.drawer.is_open() {
            return false;
        }
        let (w, h) = (self.fb.width, self.fb.height);
        match self.drawer.hit(x, y, w, h) {
            Some(drawer::Act::ViewAll(sec)) => {
                match sec {
                    // 官方抽屉里 Recent 没有 `View all`（只有 Tags 有）；这里落到
                    // 最接近的已有视图：My files + 按最近打开排序。
                    drawer::Sec::Recent => {
                        self.p12_set_view(View::MyFiles);
                        self.sort = Sort::RecentlyOpened;
                    }
                    drawer::Sec::Favorites => self.p12_set_view(View::Favorites),
                    drawer::Sec::Tags => self.p12_set_view(View::Tags),
                }
                self.p12_close();
            }
            Some(drawer::Act::Pick(sec, i)) => {
                if sec == drawer::Sec::Tags {
                    // 标签没有"打开某篇"这个动作：切到 Tags 视图。官方从抽屉过去
                    // 也是走 `navigator.sidebar.activateFilterAndHide(LibraryView.Tags, "")`。
                    self.p12_set_view(View::Tags);
                    self.p12_close();
                } else if let Some((uuid, page)) = self
                    .drawer
                    .book(sec, i)
                    .map(|b| (b.uuid.clone(), b.last_opened_page as usize))
                {
                    self.drawer.close();
                    self.open_at(&uuid, page);
                    // 文档没打开（文件坏了之类）就还得把库画回来
                    if matches!(self.screen, Screen::Library) {
                        self.draw_library();
                    }
                } else {
                    self.p12_close();
                }
            }
            // 点在抽屉外面：关（官方 MouseArea 的 onClicked 也是关抽屉）
            Some(drawer::Act::Close) => self.p12_close(),
            // 抽屉里没点到东西：吞掉，不关
            None => {}
        }
        true
    }
    // === P12 end ===

    /// 当前选中的 uuid（回收站用原来的 `trash_sel`）。
    fn lib_selected(&self) -> Vec<String> {
        if self.view == View::Trash {
            self.trash_sel.clone()
        } else {
            self.lib_sel.iter().cloned().collect()
        }
    }

    /// 退出选择态，顺手把 Move / Tag 的二级状态也收掉。
    fn lib_sel_clear(&mut self) {
        self.lib_sel.clear();
        self.trash_sel.clear();
        self.lib_moving = false;
        self.lib_tag = None;
        self.ctx = None;
        self.ctx_at = None;
    }

    /// 切换一项的选中；最后一个取消掉就自动退出选择态。
    fn lib_sel_toggle(&mut self, uuid: &str) {
        if self.view == View::Trash {
            if let Some(p) = self.trash_sel.iter().position(|u| u == uuid) {
                self.trash_sel.remove(p);
            } else {
                self.trash_sel.push(uuid.to_string());
            }
        } else if !self.lib_sel.remove(uuid) {
            self.lib_sel.insert(uuid.to_string());
        }
        if self.lib_selected().is_empty() {
            self.lib_moving = false;
        } else {
            // 选择态要占满底部，旧提示条别压在动作条上
            self.banner = None;
        }
    }

    /// 选中当前视图里的所有条目（回收站也走这套）。
    fn lib_sel_insert(&mut self, uuid: &str) {
        if self.view == View::Trash {
            if !self.trash_sel.iter().any(|u| u == uuid) {
                self.trash_sel.push(uuid.to_string());
            }
        } else {
            self.lib_sel.insert(uuid.to_string());
        }
    }

    /// 当前视图是不是已经全选（决定顶栏画 `Select all` 还是 `Deselect all`）。
    fn lib_sel_all(&self) -> bool {
        !self.visible.is_empty() && self.visible.iter().all(|b| self.lib_is_sel(&b.uuid))
    }

    /// 选中集合里文档 / 文件夹各几个（`libbar::items` 的显隐就靠它）。
    ///
    /// 只扫当前 `visible`，不再逐个 `library.find`：选择态下选中的条目一定在
    /// 当前列表里（Move 模式换目录时顶栏不用动作条），而 `entries()` 每次都要
    /// 把全库 metadata 读一遍，帧内调两次会明显卡。
    fn lib_sel_counts(&self) -> (usize, usize) {
        let mut docs = 0;
        let mut folders = 0;
        for b in &self.visible {
            if !self.lib_is_sel(&b.uuid) {
                continue;
            }
            if b.doc_type == "CollectionType" {
                folders += 1;
            } else {
                docs += 1;
            }
        }
        (docs, folders)
    }

    /// 选择态下点屏幕：先看动作条/顶栏，再看卡片（点卡片 = 切换选中）。
    fn lib_selection_tap(&mut self, x: f32, y: f32) {
        let (w, h) = (self.fb.width, self.fb.height);
        if self.lib_moving {
            // Move 的"选目标文件夹"模式：文件夹可进，顶栏管 Move here / Cancel
            match self.hits.take(x, y) {
                Some(A_P8_MOVE_HERE) => self.lib_move_commit(),
                Some(A_P8_MOVE_CANCEL) => {
                    self.lib_sel_clear();
                    self.draw_library();
                }
                Some(a) if (A_TILE0..A_TILE0 + 200).contains(&a) => {
                    let i = (a - A_TILE0) as usize;
                    if let Some(b) = self.visible.get(i).cloned() {
                        if b.doc_type == "CollectionType" {
                            self.enter_folder(&b.uuid, &b.name);
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        let (nd, nf) = self.lib_sel_counts();
        if nd + nf == 0 {
            // 选中的条目在别处被删/移走了：别把人卡在没有 Cancel 的界面里
            self.lib_sel_clear();
            self.draw_library();
            return;
        }
        let all = self.lib_sel_all();
        if let Some(act) = libbar::hit(nd, nf, self.view == View::Trash, all, x, y, w, h) {
            self.libbar_action(act);
            return;
        }
        if let Some(b) = self.item_at(x, y) {
            self.lib_sel_toggle(&b.uuid);
            self.draw_library();
        }
    }

    /// 动作条 / 顶栏点了一下。
    fn libbar_action(&mut self, act: libbar::Act) {
        match act {
            libbar::Act::Cancel => {
                eprintln!("[libsel] 退出多选");
                self.lib_sel_clear();
                self.draw_library();
            }
            libbar::Act::SelectAll => {
                let ids: Vec<String> = self.visible.iter().map(|b| b.uuid.clone()).collect();
                for id in ids {
                    self.lib_sel_insert(&id);
                }
                eprintln!("[libsel] 全选 {} 项", self.lib_selected().len());
                self.draw_library();
            }
            libbar::Act::DeselectAll => {
                self.lib_sel_clear();
                self.draw_library();
            }
            libbar::Act::Duplicate => self.lib_duplicate(),
            libbar::Act::Move => self.lib_move_begin(),
            libbar::Act::Tag => self.lib_tag_begin(),
            libbar::Act::Send => self.lib_send(),
            libbar::Act::Favorite => self.lib_favorite(),
            libbar::Act::Rename => self.lib_rename(),
            libbar::Act::Trash => self.lib_trash(),
            libbar::Act::Delete => self.lib_delete_confirm(),
            libbar::Act::Upload => {
                // 上传要云端 integration，本分支没有；官方文案 `Not available`
                //（同 `res_066c007.qml` 的不可用项）。
                self.lib_sel_clear();
                self.notice(NoticeKind::Info, "Not available");
            }
            libbar::Act::Restore => self.lib_restore(),
        }
    }

    /// 复制选中的文档（动作条只在"恰好一个文档"时给 Duplicate）。
    fn lib_duplicate(&mut self) {
        let ids = self.lib_selected();
        let Some(uuid) = ids.first() else {
            return;
        };
        match self.library.duplicate(uuid) {
            Ok(new) => eprintln!("[libsel] 复制 {uuid} → {new}"),
            Err(e) => eprintln!("[libsel] 复制失败：{e}"),
        }
        self.lib_sel_clear();
        self.draw_library();
    }

    /// 从回收站恢复选中的条目。反馈文案照官方 `res_06a2de1.qml`。
    fn lib_restore(&mut self) {
        let ids = self.lib_selected();
        if ids.is_empty() {
            return;
        }
        let name = if ids.len() == 1 {
            self.library.find(&ids[0]).map(|b| b.name)
        } else {
            None
        };
        let mut n = 0usize;
        for u in &ids {
            match self.library.restore(u) {
                Ok(()) => n += 1,
                Err(e) => eprintln!("[libsel] 恢复 {u} 失败：{e}"),
            }
        }
        self.lib_sel_clear();
        let msg = match (ids.len(), name) {
            (1, Some(name)) => format!("{name} was restored to My files"),
            (k, _) => format!("{k} items were restored to My files"),
        };
        eprintln!("[libsel] 恢复 {n}/{} 项", ids.len());
        self.notice(NoticeKind::Info, &msg);
    }

    /// `Favorite`：官方 `selectionToggleFavorite()` —— 选中的条目全都已收藏
    /// 就一起取消，否则一起收藏。官方那项文案恒为 `Favorite`，只换图标，
    /// 所以这里只能靠日志说明这一下是加还是减。
    fn lib_favorite(&mut self) {
        let ids = self.lib_selected();
        if ids.is_empty() {
            return;
        }
        let all = ids
            .iter()
            .all(|u| self.library.find(u).map(|b| b.pinned).unwrap_or(false));
        let want = !all;
        let mut n = 0usize;
        for u in &ids {
            match self.library.set_pinned(u, want) {
                Ok(()) => n += 1,
                Err(e) => eprintln!("[libsel] 收藏 {u} 失败：{e}"),
            }
        }
        eprintln!(
            "[libsel] {} {n}/{} 项",
            if want { "收藏" } else { "取消收藏" },
            ids.len()
        );
        self.lib_sel_clear();
        self.draw_library();
    }

    /// `Rename`：只在单选时出现（官方 `size === 1`），复用新建那套键盘。
    fn lib_rename(&mut self) {
        let ids = self.lib_selected();
        let [uuid] = ids.as_slice() else {
            return;
        };
        let Some(b) = self.library.find(uuid) else {
            return;
        };
        let mut d = Dialog {
            kind: DialogKind::Rename { uuid: uuid.clone() },
            kb: Keyboard::new(),
            cover: 0,
            template: 0,
        };
        d.kb.text = b.name.clone();
        self.lib_sel_clear();
        self.screen = Screen::Dialog(Box::new(d));
        self.draw_dialog();
    }

    /// `Move to trash`：官方 `selectionMoveToTrash()`，移完直接回库、不带 toast。
    fn lib_trash(&mut self) {
        let ids = self.lib_selected();
        if ids.is_empty() {
            return;
        }
        let mut n = 0usize;
        for u in &ids {
            match self.library.delete_book(u) {
                Ok(()) => {
                    n += 1;
                    self.thumbs.remove(u);
                }
                Err(e) => eprintln!("[libsel] 移到回收站 {u} 失败：{e}"),
            }
        }
        eprintln!("[libsel] 移到回收站 {n}/{} 项", ids.len());
        self.lib_sel_clear();
        self.draw_library();
    }

    /// 回收站里的 `Delete`：先弹官方的 `Yes, delete item(s)` 确认框
    /// （`res_06953193.qml:41-46` 那两项里的永久删除）。
    fn lib_delete_confirm(&mut self) {
        let ids = self.lib_selected();
        if ids.is_empty() {
            return;
        }
        self.trash_confirm = Some(ids);
        self.draw_library();
    }

    /// Move：进入"选目标文件夹"模式（官方 `content.moving`）。
    fn lib_move_begin(&mut self) {        if self.lib_selected().is_empty() {
            return;
        }
        self.lib_moving = true;
        eprintln!("[libsel] 选目标文件夹（当前 {}）", self.cwd());
        self.draw_library();
    }

    /// `Move here` 能不能点：目标目录和源父目录不同，且不能把文件夹搬进
    /// 它自己 / 它的子孙里。
    fn lib_move_here_visible(&self) -> bool {
        let ids = self.lib_selected();
        let Some(first) = ids.first() else {
            return false;
        };
        let cwd = self.cwd();
        // 目标就是某个被移动的文件夹 / 它的子孙 → 会成环
        let mut p = cwd.clone();
        let mut guard = 0;
        while !p.is_empty() && guard < 64 {
            if ids.iter().any(|u| u == &p) {
                return false;
            }
            p = match self.library.find(&p) {
                Some(b) => b.parent,
                None => break,
            };
            guard += 1;
        }
        // 官方 `hasMoveToTarget`：当前目录 = 源父目录时不显示
        self.library
            .find(first)
            .map(|b| b.parent != cwd)
            .unwrap_or(false)
    }

    /// 把选中的条目搬到当前目录。
    fn lib_move_commit(&mut self) {
        let to = self.cwd();
        let ids = self.lib_selected();
        let mut n = 0usize;
        for u in &ids {
            match self.library.move_to(u, &to) {
                Ok(()) => n += 1,
                Err(e) => eprintln!("[libsel] 移动 {u} 失败：{e}"),
            }
        }
        eprintln!(
            "[libsel] 移动 {n}/{} 项 → {}",
            ids.len(),
            if to.is_empty() { "My files" } else { &to }
        );
        self.lib_sel_clear();
        self.draw_library();
    }

    /// Move 模式的顶栏：黑条 `Cancel` + 官方提示语 + 右边 `Move here`。
    fn paint_lib_move_header(&mut self) {
        let w = self.fb.width;
        let n = self.lib_selected().len();
        let here = self.lib_move_here_visible();
        self.fb.fill_rect(0, 0, w, libbar::BAND_H, gray565(255));
        self.fb.fill_rect(0, 0, w, libbar::HDR_H, gray565(0));
        if let Some(t) = self.text.as_mut() {
            t.draw_bold(&mut self.fb, 33, 73, 32.0, "Cancel", 250);
            // 官方 `frag_0065_006a4d4c.qml`：`Select a destination for your item%1`
            let label = if n > 1 {
                "Select a destination for your items"
            } else {
                "Select a destination for your item"
            };
            let tw = t.width_bold(28.0, label) as i32;
            t.draw_bold(&mut self.fb, (w - tw) / 2, 73, 28.0, label, 250);
            if here {
                let hw = t.width_bold(30.0, "Move here") as i32;
                t.draw_bold(&mut self.fb, w - 33 - hw, 73, 30.0, "Move here", 250);
            }
        }
        self.hits.push(0, 0, 220, libbar::HDR_H, A_P8_MOVE_CANCEL);
        if here {
            self.hits
                .push(w - 33 - 260, 0, 260, libbar::HDR_H, A_P8_MOVE_HERE);
        }
    }

    /// Tag：复用 "Add tag" 的整屏键盘，保存时把标签并到所有选中条目上。
    fn lib_tag_begin(&mut self) {
        if self.lib_selected().is_empty() {
            return;
        }
        self.lib_tag = Some(Keyboard::new());
        self.paint_lib_tag();
    }

    /// `Add tag` 键盘（标题 + 输入框 + 键盘 + Cancel/Save），结构照文档里的那套。
    fn paint_lib_tag(&mut self) {
        let Some(kb) = self.lib_tag.clone() else {
            return;
        };
        let (w, h) = (self.fb.width, self.fb.height);
        self.fb.fill_rect(0, 0, w, h, gray565(255));
        self.fb.fill_rect(0, 0, w, 2, gray565(180));
        self.hits.clear();
        if let Some(t) = self.text.as_mut() {
            t.draw(&mut self.fb, NAV_MARGIN, 100, 44.0, "Add tag", 20);
        }
        let (ix, iy, iw, ih) = (NAV_MARGIN, 140, w - 2 * NAV_MARGIN, 84);
        self.fb.fill_rect(ix, iy, iw, ih, gray565(238));
        if let Some(t) = self.text.as_mut() {
            let shown = t.ellipsize(34.0, &kb.text, iw - 40);
            t.draw(&mut self.fb, ix + 20, iy + 58, 34.0, &shown, 20);
        }
        let ky = h - kb.height() - 170;
        self.kb_hits = kb.draw(&mut self.fb, &mut self.text, 0, ky, w);
        let by = ky - 110;
        let bw = (w - 2 * NAV_MARGIN - 20) / 2;
        self.fb.fill_rect(NAV_MARGIN, by, bw, 90, gray565(210));
        self.fb.fill_rect(NAV_MARGIN + bw + 20, by, bw, 90, gray565(40));
        if let Some(t) = self.text.as_mut() {
            t.draw(&mut self.fb, NAV_MARGIN + 100, by + 60, 34.0, "Cancel", 20);
            t.draw(&mut self.fb, NAV_MARGIN + bw + 120, by + 60, 34.0, "Save", 250);
        }
        self.hits.push(NAV_MARGIN, by, bw, 90, A_CANCEL);
        self.hits.push(NAV_MARGIN + bw + 20, by, bw, 90, A_OK);
        self.fb
            .refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
    }

    fn tap_lib_tag(&mut self, x: f32, y: f32) {
        for (r, k) in self.kb_hits.clone() {
            let (xi, yi) = (x as i32, y as i32);
            if xi >= r.0 && xi < r.0 + r.2 && yi >= r.1 && yi < r.1 + r.3 {
                if k == Key::Enter {
                    self.commit_lib_tag();
                } else {
                    if let Some(kb) = self.lib_tag.as_mut() {
                        kb.press(k);
                    }
                    self.paint_lib_tag();
                }
                return;
            }
        }
        match self.hits.take(x, y) {
            Some(A_OK) => self.commit_lib_tag(),
            Some(A_CANCEL) => {
                self.lib_tag = None;
                self.draw_library();
            }
            _ => {}
        }
    }

    /// 把键盘里输入的标签并到所有选中条目（已有的不重复加）。
    fn commit_lib_tag(&mut self) {
        let Some(kb) = self.lib_tag.take() else {
            return;
        };
        let name = kb.text.trim().to_string();
        let ids = self.lib_selected();
        if !name.is_empty() {
            for u in &ids {
                let mut mine = self.library.tags_of(u);
                if !mine.iter().any(|t| t == &name) {
                    mine.push(name.clone());
                    mine.sort();
                }
                if let Err(e) = self.library.set_tags(u, &mine) {
                    eprintln!("[libsel] 标签写入失败：{e}");
                } else {
                    eprintln!("[libsel] {u} 标签 → {mine:?}");
                }
            }
        }
        self.lib_sel_clear();
        self.draw_library();
    }

    /// Send：官方 `shareEntriesAction`。邮件页这版只有收件人，所以先把选中的
    /// 那本书打开、再把条目名交给邮件页（附件就是这本书的当前页）。
    fn lib_send(&mut self) {
        let ids = self.lib_selected();
        let Some(uuid) = ids.first().cloned() else {
            return;
        };
        eprintln!("[libsel] 发送 {uuid}");
        self.lib_sel_clear();
        self.open_at(&uuid, 0);
        self.open_mail();
    }
    // === P8 end ===

    fn tap_library(&mut self, x: f32, y: f32) {
        // === JOB begin: Job status 整屏开着时，点击归它 ===
        if self.jobs_open {
            self.jobs_activate_at(x, y);
            return;
        }
        // === JOB end ===
        // === P12 begin ===
        // 抽屉开着就它说了算：点到抽屉里 = 处理，点到外面 = 收起来，
        // 点到抽屉空白处 = 吞掉（不许穿透到后面的卡片）。
        if self.drawer.is_open() && self.p12_tap(x, y) {
            return;
        }
        // === P12 end ===
        // === P8 begin ===
        // 多选 → Tag 的键盘是整屏浮层，先吃事件
        if self.lib_tag.is_some() {
            self.tap_lib_tag(x, y);
            return;
        }
        // === P8 end ===
        // Tags 视图的弹层（改名键盘 / 删除确认 / 长按操作条）最优先
        if self.view == View::Tags && self.tags_modal_tap(x, y) {
            return;
        }
        // 库里也要能退出（左下角连点 3 下）
        if x < 64.0 && y > (self.fb.height - 64) as f32 {
            self.exit_taps += 1;
            if self.exit_taps >= 3 {
                eprintln!("[dev] 退出");
                std::process::exit(0);
            }
            return;
        }
        self.exit_taps = 0;
        let had_banner = self.banner.is_some();
        self.banner = None;
        // 永久删除确认框最优先
        if let Some(ids) = self.trash_confirm.clone() {
            match self.hits.take(x, y) {
                Some(A_TR_YES) => {
                    for u in &ids {
                        let _ = self.library.purge(u);
                    }
                    eprintln!("[trash] 永久删除 {} 项", ids.len());
                    self.trash_confirm = None;
                    self.trash_sel.clear();
                    self.draw_library();
                }
                _ => {
                    self.trash_confirm = None;
                    self.draw_library();
                }
            }
            return;
        }
        // === P8 begin ===
        // 选择态：动作条 / 顶栏按钮 / 点卡片切换选中；点别处保持选中不动。
        if !self.lib_selected().is_empty() {
            self.lib_selection_tap(x, y);
            return;
        }
        // === P8 end ===
        if self.ctx.is_some() {
            // 菜单外面点一下不再关闭——只有点 Cancel / 选中某一项才关。
            // （之前"点外面关闭"会被触发长按那一下的多余 Up 事件误伤，
            //   表现为菜单一闪就没。）
            let _ = &self.ctx_at;
            let hit = self.hits.take(x, y);
            eprintln!("[ctx] 菜单里点 ({x:.0},{y:.0}) -> {hit:?}");
            match hit {
                Some(A_CTX_RENAME) => {
                    let b = self.ctx.take().unwrap();
                    let mut d = Dialog {
                        kind: DialogKind::Rename {
                            uuid: b.uuid.clone(),
                        },
                        kb: Keyboard::new(),
                        cover: 0,
                        template: 0,
                    };
                    d.kb.text = b.name.clone();
                    self.screen = Screen::Dialog(Box::new(d));
                    self.draw_dialog();
                    return;
                }
                Some(A_CTX_TRASH) => {
                    let b = self.ctx.take().unwrap();
                    let _ = self.library.delete_book(&b.uuid);
                    eprintln!("[ui] 已移到回收站 {}", b.name);
                    self.thumbs.remove(&b.uuid);
                    self.draw_library();
                    return;
                }
                Some(A_CTX_PIN) => {
                    let b = self.ctx.take().unwrap();
                    let _ = self.library.set_pinned(&b.uuid, !b.pinned);
                    self.draw_library();
                    return;
                }
                Some(A_TR_RESTORE) => {
                    let b = self.ctx.take().unwrap();
                    let _ = self.library.restore(&b.uuid);
                    eprintln!("[trash] 已恢复 {}", b.name);
                    self.trash_sel.clear();
                    self.draw_library();
                    return;
                }
                Some(A_TR_DELETE) => {
                    let b = self.ctx.take().unwrap();
                    self.trash_confirm = Some(vec![b.uuid.clone()]);
                    self.ctx = None;
                    self.draw_library();
                    return;
                }
                Some(A_CTX_CANCEL) => {
                    // 之前漏了这个分支：点 Cancel 什么也不发生，菜单永远关不掉
                    self.ctx = None;
                    self.draw_library();
                    return;
                }
                _ => {
                    // 点在菜单外面：什么都不做，菜单留着
                    return;
                }
            }
        }
        let act = self.hits.take(x, y);
        match act {
            Some(a) => self.do_action(a),
            None => {
                // 点空白：关掉浮层；刚才有提示条的话也要重画（把它擦掉）
                if self.menu_open || self.sort_open || had_banner {
                    self.menu_open = false;
                    self.sort_open = false;
                    self.draw_library();
                }
            }
        }
    }

    fn do_action(&mut self, a: u32) {
        // 只有"选了一个排序/布局"才收起下拉；点按钮本身是开/关切换，
        // 不能在函数末尾无条件收起（否则点一下开了又立刻关，看起来就是点了没反应）。
        let mut close_sort = false;
        match a {
            // === E1 begin: 侧栏 `Mistakes` → 整屏错题列表 ===
            A_E1_MISTAKES => {
                self.e1_open_mistakes();
                return;
            }
            // === JOB begin ===
            A_E1_JOBS => {
                self.e1_open_jobs();
                return;
            }
            // === JOB end ===
            // === E1 end ===
            // === P3 begin ===
            A_P3_BATTERY => {
                // 官方 batteryClicked() → Settings > Battery（tab 下标 2）
                self.open_settings_tab(2);
                return;
            }
            A_P3_WIFI => {
                // 官方 wifiClicked() → Settings > Wi-Fi（tab 下标 1）
                self.open_settings_tab(1);
                return;
            }
            A_P3_CLOUD => {
                // 官方 syncClicked()：重试一次同步。走现有后台任务入口，不新写网络代码。
                self.start_sync();
                return;
            }
            _ if (A_P3_NOTICE_CLOSE..=A_P3_NOTICE_NEXT).contains(&a) => {
                self.notice_action(a);
                return;
            }
            // === P3 end ===
            A_MENU => {
                self.menu_open = !self.menu_open;
                self.sort_open = false;
            }
            A_NEW_FOLDER => {
                self.open_dialog(DialogKind::NewFolder);
                return;
            }
            A_NEW_NOTEBOOK => {
                self.open_dialog(DialogKind::NewNotebook);
                return;
            }
            A_QUICK => {
                self.quick_sheet();
                return;
            }
            A_EMPTY_TRASH => {
                let ids: Vec<String> = self.visible.iter().map(|b| b.uuid.clone()).collect();
                if !ids.is_empty() {
                    self.trash_confirm = Some(ids);
                }
                self.draw_library();
                return;
            }
            A_SEARCH => {
                self.open_search();
                return;
            }
            A_SORT => {
                self.sort_open = !self.sort_open;
                // 按钮文字要跟着从排序名变 `Sort by`（P13），只重画下拉不够
                self.draw_library();
                return;
            }
            _ if (A_SORT0..A_SORT0 + 9).contains(&a) => {
                self.sort = Sort::from_index(a - A_SORT0);
                self.scroll = 0;
                close_sort = true;
            }
            _ if (A_VIEW0..A_VIEW0 + 2).contains(&a) => {
                // 官方 Settings.listMode：写完立刻切布局，并落盘（重启后保持）
                let on = a == A_VIEW0 + 1;
                if self.list_mode != on {
                    self.list_mode = on;
                    save_list_mode(&prefs_path(), on);
                    self.scroll = 0;
                    eprintln!("[ui] 布局 = {}", if on { "List" } else { "Grid" });
                }
                close_sort = true;
            }
            A_SETTINGS => {
                self.open_settings();
                return;
            }
            // === P4 begin ===
            // 库顶栏的磁盘提示条：官方点它 `showSettings(Values.SettingsPage.Storage)`
            // （`res_06a2de1.qml:312` `onDiskFullClicked`），Storage 是第 4 个 tab。
            A_P4_DISK => {
                self.open_settings_tab(3);
                return;
            }
            // === P4 end ===
            _ if (A_FILTER0..A_FILTER0 + 3).contains(&a) => {
                let v = match a - A_FILTER0 {
                    0 => View::Notebooks,
                    1 => View::Pdfs,
                    _ => View::Ebooks,
                };
                self.view = v;
                self.trail.clear();
                self.tag_filter = None;
                self.menu_open = false;
                self.filters_open = false;
                self.scroll = 0;
            }
            _ if (A_SIDE0..A_SIDE0 + 16).contains(&a) => {
                let i = (a - A_SIDE0) as usize;
                // === E1 begin: `Mistakes` 插在 Trash 前，给它留空位，Trash 仍是下标 4 ===
                let views: [Option<View>; 6] = [
                    Some(View::MyFiles),
                    None,
                    Some(View::Favorites),
                    Some(View::Tags),
                    None,
                    Some(View::Trash),
                ];
                // === E1 end ===
                if i == 1 {
                    // Filters：展开 / 收起子项（侧栏不关）
                    self.filters_open = !self.filters_open;
                    self.draw_library();
                    return;
                }
                self.filters_open = false;
                if let Some(Some(v)) = views.get(i) {
                    self.view = *v;
                    self.menu_open = false;
                    self.scroll = 0;
                    // 官方：侧栏 My files 永远回库根目录
                    if *v == View::MyFiles {
                        self.trail.clear();
                    }
                    if *v == View::Tags {
                        self.tags.on_open();
                    }
                    self.tag_filter = None;
                }
                self.draw_library();
                return;
            }
            _ if (A_TILE0..A_TILE0 + 200).contains(&a) => {
                let i = (a - A_TILE0) as usize;
                let Some(b) = self.visible.get(i).cloned() else {
                    return;
                };
                if self.view == View::Trash {
                    // 官方：回收站里点卡片 = 选中/取消（多选），不是直接恢复
                    if let Some(p) = self.trash_sel.iter().position(|u| u == &b.uuid) {
                        self.trash_sel.remove(p);
                    } else {
                        self.trash_sel.push(b.uuid.clone());
                    }
                    eprintln!("[trash] 选中 {} 个", self.trash_sel.len());
                } else if b.doc_type == "CollectionType" {
                    let (uuid, name) = (b.uuid.clone(), b.name.clone());
                    self.enter_folder(&uuid, &name);
                    return;
                } else {
                    self.open_at(&b.uuid, b.last_opened_page as usize);
                    return;
                }
            }
            A_CRUMB_ROOT => {
                self.trail.clear();
                self.tag_filter = None;
                self.scroll = 0;
                self.draw_library();
                return;
            }
            _ if (A_CRUMB0..A_CRUMB0 + 64).contains(&a) => {
                let i = (a - A_CRUMB0) as usize;
                if i + 1 < self.trail.len() {
                    self.trail.truncate(i + 1);
                }
                self.scroll = 0;
                self.draw_library();
                return;
            }
            _ if tagsview::is_tags_action(a) => {
                self.tags_action(a);
                return;
            }
            _ => {}
        }
        if close_sort {
            self.sort_open = false;
        }
        self.draw_library();
    }

    /// ⊕ Quick sheets：**每次都新建一本**（用户 2026-11-09 的要求，官方是复用固定那本）。
    ///
    /// 新建的空本记在 [`App::quick_new`]，离开文档时如果一个字都没写就整本删掉
    /// （`close_notebook`）——不然连点几下，库里会堆一串空白 Quick sheets。
    fn quick_sheet(&mut self) {
        match self.library.create_book("Quick sheets") {
            Ok(uuid) => {
                self.quick_new = Some(uuid.clone());
                self.open(&uuid);
            }
            // === P3 begin ===
            Err(e) => self.notice(NoticeKind::Error, &format!("Could not create: {e}")),
            // === P3 end ===
        }
    }

    /// 底部一条轻提示（官方也有类似的 toast）。
    ///
    /// 笔记本里不能走 `draw_library()`：那会把整个库界面糊到屏幕上。
    /// 笔记本里改成整页重绘，黑条由 `paint_footer` 画，下一次按下时由 `handle` 擦掉。
    fn toast(&mut self, msg: &str) {
        eprintln!("[ui] {msg}");
        self.banner = Some(msg.to_string());
        if matches!(self.screen, Screen::Notebook(_)) {
            self.redraw_page();
        } else if matches!(self.screen, Screen::Search(_)) {
            // 搜索页也有键盘，toast 不能把库界面糊上来
            self.draw_search();
        } else {
            self.draw_library();
        }
    }

    // ------------------------------------------------------------ 搜索 / 重命名

    fn draw_search(&mut self) {
        let (w, h) = (self.fb.width, self.fb.height);
        let Some(kb) = (match &self.screen {
            Screen::Search(s) => Some(s.kb.clone()),
            _ => None,
        }) else {
            return;
        };
        self.fb.fill_rect(0, 0, w, h, gray565(255));
        self.hits.clear();

        // ---------- 顶部黑条：Cancel + 放大镜 + 白色输入框
        self.fb.fill_rect(0, 0, w, SEARCH_BAR_H, gray565(0));
        if let Some(t) = self.text.as_mut() {
            t.set_bold(true);
            t.draw(&mut self.fb, 33, 72, 37.0, "Cancel", 250);
            t.set_bold(false);
        }
        self.hits.push(0, 0, 200, SEARCH_BAR_H, A_BACK);
        // 放大镜：圈心 (262,55)、外径 15、柄 4px 朝左下（官方 x 239..278 / y 40..78）
        widgets::ring(&mut self.fb, 262, 55, 15, 4, 250);
        widgets::line(&mut self.fb, 251, 64, 240, 75, 4, 250);

        let (bx, by, bw, bh) = SEARCH_BOX;
        self.fb.fill_rect(bx, by, bw, bh, gray565(255));
        let q = kb.text.clone();
        if let Some(t) = self.text.as_mut() {
            if q.is_empty() {
                t.draw(&mut self.fb, bx + 9, by + 35, SEARCH_FIELD_SIZE, "Search My files", 20);
            } else {
                let shown = t.ellipsize(SEARCH_FIELD_SIZE, &q, bw - 20);
                t.draw(&mut self.fb, bx + 9, by + 35, SEARCH_FIELD_SIZE, &shown, 20);
            }
        }
        // 点输入框（含放大镜）＝清空/聚焦；命中区放到 100 高，手指点得到。
        self.hits.push(bx - 66, 10, bw + 76, 100, A_CLEAR);

        // ---------- "Last updated"：36 号粗体、左边距 40、基线 221
        if let Some(t) = self.text.as_mut() {
            t.set_bold(true);
            t.draw(&mut self.fb, 40, 224, 41.0, "Last updated", 20);
            t.set_bold(false);
        }

        // ---------- 结果行
        let results = self.library.search(&q);
        let ky = h - kb.height();
        let mut y = SEARCH_LIST_TOP;
        for (i, b) in results.iter().enumerate() {
            // 键盘盖住的地方不画（官方是 ListView，最后一行可以露出上半截）
            if y + 40 > ky {
                break;
            }
            if b.doc_type == "CollectionType" {
                search_icon_folder(&mut self.fb, SEARCH_ICON_X, y);
            } else if (b.format == rmdev::doc::Format::Pdf) || b.format.is_ebook() {
                search_icon_doc(&mut self.fb, SEARCH_ICON_X, y);
            } else {
                search_icon_notebook(&mut self.fb, SEARCH_ICON_X, y);
            }
            if let Some(t) = self.text.as_mut() {
                t.set_bold(true);
                let name = t.ellipsize_mid(SEARCH_TITLE_SIZE, &b.name, w - SEARCH_TEXT_X - 20);
                t.draw(&mut self.fb, SEARCH_TEXT_X, y + 31, SEARCH_TITLE_SIZE, &name, 20);
                t.set_bold(false);
                // 第二行 = fullPathName()（顶层就是 My files）+ entrySubtext()
                t.draw(&mut self.fb, SEARCH_TEXT_X, y + 77, SEARCH_CAPTION_SIZE, "My files", 20);
                let sub = search_subtext(b);
                if !sub.is_empty() {
                    let x = SEARCH_TEXT_X + t.width(SEARCH_CAPTION_SIZE, "My files") as i32 + 24;
                    let sub = t.ellipsize(SEARCH_CAPTION_SIZE, &sub, w - x - 20);
                    t.draw(&mut self.fb, x, y + 77, SEARCH_CAPTION_SIZE, &sub, 20);
                }
            }
            self.hits.push(0, y, w, SEARCH_ROW_H, A_RESULT0 + i as u32);
            y += SEARCH_ROW_H;
        }
        if results.is_empty() {
            // 官方这页 QML 缺失：空态借用回收站那种"居中粗体标题"的样式。
            if let Some(t) = self.text.as_mut() {
                t.set_bold(true);
                let msg = "No results found";
                let tw = t.width(SEARCH_EMPTY_SIZE, msg) as i32;
                t.draw(&mut self.fb, (w - tw) / 2, 706, SEARCH_EMPTY_SIZE, msg, 20);
                t.set_bold(false);
            }
        }
        self.visible = results;

        // ---------- 键盘贴底
        self.kb_hits = kb.draw(&mut self.fb, &mut self.text, 0, ky, w);

        // toast 黑条：和库界面同尺寸，压在键盘上；画完就取走，下一次重绘即消失。
        if let Some(msg) = self.banner.take() {
            self.fb.fill_rect(0, h - 120, w, 120, gray565(30));
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, NAV_MARGIN, h - 46, 34.0, &msg, 245);
            }
        }
        self.fb.refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
    }

    // ------------------------------------------------------------ 黑头对话框

    fn open_dialog(&mut self, kind: DialogKind) {
        let mut kb = Keyboard::new();
        kb.text = kind.default_name().to_string();
        self.screen = Screen::Dialog(Box::new(Dialog {
            kind,
            kb,
            cover: 0,
            template: 0,
        }));
        self.draw_dialog();
    }

    fn draw_dialog(&mut self) {
        let (w, h) = (self.fb.width, self.fb.height);
        let Some(d) = (match &self.screen {
            Screen::Dialog(d) => Some((*d).clone()),
            _ => None,
        }) else {
            return;
        };
        let is_nb = matches!(d.kind, DialogKind::NewNotebook);
        let is_rename = matches!(d.kind, DialogKind::Rename { .. });

        self.fb.fill_rect(0, 0, w, h, gray565(255));
        self.hits.clear();

        // ---------- 黑头 118：Cancel | 标题 | Create
        self.fb.fill_rect(0, 0, w, 118, gray565(0));
        if let Some(t) = self.text.as_mut() {
            t.draw_bold(&mut self.fb, 32, 74, 33.0, "Cancel", 255);
            let tw = t.width_bold(33.0, d.kind.title());
            t.draw_bold(
                &mut self.fb,
                (w as f32 / 2.0 - tw / 2.0) as i32,
                74,
                33.0,
                d.kind.title(),
                255,
            );
            let ok = if is_rename { "Save" } else { "Create" };
            let cw = t.width_bold(33.0, ok);
            t.draw_bold(&mut self.fb, w - 32 - cw as i32, 74, 33.0, ok, 255);
        }
        self.hits.push(0, 0, 240, 118, A_DLG_CANCEL);
        self.hits.push(w - 260, 0, 260, 118, A_DLG_CREATE);

        // ---------- Name：标签 36 粗、基线 206；输入区反白、字号 40
        if let Some(t) = self.text.as_mut() {
            t.draw_bold(&mut self.fb, 33, 206, 36.0, "Name", 20);
        }
        let name = d.kb.text.clone();
        let tw = match self.text.as_mut() {
            Some(t) => t.width(40.0, &name) as i32,
            None => 0,
        };
        self.fb.fill_rect(33, 259, tw + 10, 42, gray565(0));
        if let Some(t) = self.text.as_mut() {
            t.draw(&mut self.fb, 37, 292, 40.0, &name, 255);
        }
        // 输入框下面 4px 黑线，通到 x=1370
        self.fb.fill_rect(33, 306, w - 66, 4, gray565(0));
        self.hits.push(0, 240, w, 90, 0);

        if is_nb {
            // ---------- Notebook cover：标签基线 400；单选圈 40px
            if let Some(t) = self.text.as_mut() {
                t.draw_bold(&mut self.fb, 33, 400, 36.0, "Notebook cover", 20);
            }
            for (i, label) in ["Last page visited", "First page"].iter().enumerate() {
                let cy = 463 + i as i32 * 60;
                widgets::ring(&mut self.fb, 52, cy, 20, 4, 20);
                if d.cover == i {
                    widgets::disc(&mut self.fb, 52, cy, 9, 20);
                }
                if let Some(t) = self.text.as_mut() {
                    t.draw(&mut self.fb, 82, cy + 12, 34.0, label, 20);
                }
                self.hits.push(20, cy - 30, 480, 60, A_DLG_COVER0 + i as u32);
            }

            // ---------- Templates：标签基线 646；四张卡 334 宽、335 步进、高 270
            if let Some(t) = self.text.as_mut() {
                t.draw_bold(&mut self.fb, 33, 646, 36.0, "Templates", 20);
                let vw = t.width(34.0, "View all");
                t.draw(&mut self.fb, 1327 - vw as i32, 646, 34.0, "View all", 20);
            }
            // "View all" 右边那个 "›"（官方 1337..1353 × 626..646）
            for i in 0..11 {
                self.fb.fill_rect(1337 + i, 626 + i, 3, 3, gray565(20));
                self.fb.fill_rect(1348 - i, 637 + i, 3, 3, gray565(20));
            }
            let top = 691;
            for (i, (label, _tpl)) in DLG_TEMPLATES.iter().enumerate() {
                let tx = 33 + i as i32 * 335;
                let sel = d.template == i;
                if sel {
                    self.fb.fill_rect(tx, top, 334, 270, gray565(0));
                }
                self.draw_template_preview(tx, top, 334, 270, i, if sel { 255 } else { 20 });
                if let Some(t) = self.text.as_mut() {
                    let lw = t.width_bold(26.0, label);
                    t.draw_bold(
                        &mut self.fb,
                        tx + (334 - lw as i32) / 2,
                        top + 320,
                        26.0,
                        label,
                        20,
                    );
                }
                self.hits.push(tx, top, 334, 340, A_DLG_TPL0 + i as u32);
            }
        }

        // ---------- 键盘贴底（官方 601 高）
        let kb = d.kb.clone();
        let ky = h - kb.height();
        self.kb_hits = kb.draw(&mut self.fb, &mut self.text, 0, ky, w);
        self.fb.refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
    }

    /// 对话框里四张模板卡的小预览（黑白线稿，不用图片）。
    fn draw_template_preview(&mut self, x: i32, y: i32, w: i32, h: i32, kind: usize, fg: u8) {
        let c = gray565(fg);
        let (ix, iy) = (x + 52, y + 34);
        let (iw, ih) = (w - 104, h - 68);
        widgets::outline_rect(&mut self.fb, ix, iy, iw, ih, 2, fg);
        match kind {
            1 => {
                for k in 1..8 {
                    let ly = iy + ih * k / 8;
                    self.fb.fill_rect(ix + 14, ly, iw - 28, 2, c);
                }
            }
            2 => {
                for k in 1..8 {
                    let ly = iy + ih * k / 8;
                    self.fb.fill_rect(ix + 14, ly, iw - 28, 2, c);
                    let lx = ix + iw * k / 8;
                    self.fb.fill_rect(lx, iy + 14, 2, ih - 28, c);
                }
            }
            3 => {
                for k in 0..4 {
                    let ly = iy + 16 + k * 26;
                    widgets::outline_rect(&mut self.fb, ix + 14, ly, 16, 16, 2, fg);
                    self.fb.fill_rect(ix + 44, ly + 7, iw - 64, 2, c);
                }
            }
            _ => {}
        }
    }

    fn tap_dialog(&mut self, x: f32, y: f32) {
        // 键盘优先
        for (r, k) in self.kb_hits.clone() {
            let (xi, yi) = (x as i32, y as i32);
            if xi >= r.0 && xi < r.0 + r.2 && yi >= r.1 && yi < r.1 + r.3 {
                if k == Key::Enter {
                    self.commit_dialog();
                    return;
                }
                if let Screen::Dialog(d) = &mut self.screen {
                    d.kb.press(k);
                }
                self.draw_dialog();
                return;
            }
        }
        match self.hits.take(x, y) {
            Some(A_DLG_CANCEL) => {
                self.screen = Screen::Library;
                self.draw_library();
            }
            Some(A_DLG_CREATE) => self.commit_dialog(),
            Some(a) if (A_DLG_COVER0..A_DLG_COVER0 + 2).contains(&a) => {
                if let Screen::Dialog(d) = &mut self.screen {
                    d.cover = (a - A_DLG_COVER0) as usize;
                }
                self.draw_dialog();
            }
            Some(a) if (A_DLG_TPL0..A_DLG_TPL0 + 8).contains(&a) => {
                if let Screen::Dialog(d) = &mut self.screen {
                    d.template = (a - A_DLG_TPL0) as usize;
                }
                self.draw_dialog();
            }
            _ => {}
        }
    }

    fn commit_dialog(&mut self) {
        let Screen::Dialog(d) = &self.screen else { return };
        let mut name = d.kb.text.trim().to_string();
        if name.is_empty() {
            name = d.kind.default_name().to_string();
        }
        let kind = d.kind.clone();
        let (cover, template) = (d.cover, d.template);
        match kind {
            DialogKind::NewFolder => {
                match self.library.create_folder(&name, &self.cwd()) {
                    Ok(_) => eprintln!("[ui] New folder: {name}"),
                    Err(e) => eprintln!("[ui] New folder failed: {e}"),
                }
                self.screen = Screen::Library;
                self.draw_library();
            }
            DialogKind::NewNotebook => {
                match self.library.create_book_tpl(
                    &name,
                    &self.cwd(),
                    cover == 1,
                    DLG_TEMPLATES[template].1,
                ) {
                    Ok(uuid) => {
                        self.screen = Screen::Library;
                        self.open_at(&uuid, 0);
                    }
                    Err(e) => eprintln!("[ui] New notebook failed: {e}"),
                }
            }
            DialogKind::Rename { uuid } => {
                let _ = self.library.rename(&uuid, &name);
                self.screen = Screen::Library;
                self.thumbs.remove(&uuid);
                self.draw_library();
            }
        }
    }

    fn draw_rename(&mut self) {
        let (w, h) = (self.fb.width, self.fb.height);
        let Some(kb) = (match &self.screen {
            Screen::Rename(r) => Some(r.kb.clone()),
            _ => None,
        }) else {
            return;
        };
        self.fb.fill_rect(0, 0, w, h, gray565(255));
        self.fb.fill_rect(0, 0, w, 2, gray565(180));
        self.hits.clear();

        if let Some(t) = self.text.as_mut() {
            t.draw(&mut self.fb, NAV_MARGIN, 100, 44.0, "Rename", 20);
        }
        let (ix, iy, iw, ih) = (NAV_MARGIN, 140, w - 2 * NAV_MARGIN, 84);
        self.fb.fill_rect(ix, iy, iw, ih, gray565(238));
        if let Some(t) = self.text.as_mut() {
            let shown = t.ellipsize(34.0, &kb.text, iw - 40);
            t.draw(&mut self.fb, ix + 20, iy + 58, 34.0, &shown, 20);
        }

        let ky = h - kb.height() - 170;
        self.kb_hits = kb.draw(&mut self.fb, &mut self.text, 0, ky, w);

        // 取消 / 确定
        let by = ky - 110;
        let bw = (w - 2 * NAV_MARGIN - 20) / 2;
        self.fb.fill_rect(NAV_MARGIN, by, bw, 90, gray565(210));
        self.fb.fill_rect(NAV_MARGIN + bw + 20, by, bw, 90, gray565(40));
        if let Some(t) = self.text.as_mut() {
            t.draw(&mut self.fb, NAV_MARGIN + 60, by + 60, 34.0, "Cancel", 20);
            t.draw(&mut self.fb, NAV_MARGIN + bw + 80, by + 60, 34.0, "Save", 250);
        }
        self.hits.push(NAV_MARGIN, by, bw, 90, A_CANCEL);
        self.hits.push(NAV_MARGIN + bw + 20, by, bw, 90, A_OK);
        self.fb.refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
    }

    // ------------------------------------------------------------ 自建服务器

    /// 当前页渲染成 24 位 BMP（只画墨迹，不画模板线——OCR/邮件附件要的是笔画）。
    fn current_page_bmp(&mut self) -> Option<Vec<u8>> {
        let (uuid, idx) = match &self.screen {
            Screen::Notebook(nb) => (nb.uuid.clone(), nb.page_index),
            _ => return None,
        };
        let (w, h) = (1404usize, 1872usize);
        let gray = self.page_thumb(&uuid, idx, w, h);
        Some(gray_to_bmp(&gray, w, h))
    }

    // === P10 begin ===
    // 手写转文字整页流程。入口是分享面板的 `Convert notes to text`
    // （`res_067e8cc.qml` 的 `shareConvert`），流程照 `res_066c007.qml`：
    // 页总览选页 → `Convert` → 每页整页渲染 → 后台 OCR → 结果**新增一页**。

    /// 某一页整页渲染成 24 位 BMP（只画墨迹，不画模板线——OCR 要的是笔画）。
    fn conv_page_bmp(&mut self, uuid: &str, idx: usize) -> Vec<u8> {
        let (w, h) = (1404usize, 1872usize);
        let gray = self.page_thumb(uuid, idx, w, h);
        gray_to_bmp(&gray, w, h)
    }

    /// 转换态的那本笔记现在还在前台吗（用户可能中途切走）。
    fn conv_is_current_doc(&self) -> bool {
        self.conv.active() && matches!(&self.screen, Screen::Notebook(nb) if nb.uuid == self.conv.uuid)
    }

    /// 分享面板 → `Convert notes to text`：进页总览的选页态。
    ///
    /// 服务器没配对就**立刻**提示并退出（不进选页、不卡 UI）。
    fn start_ocr(&mut self) {
        let uuid = match &self.screen {
            Screen::Notebook(nb) => nb.uuid.clone(),
            _ => return,
        };
        // 只有真笔记能插页：电子书的页由文件本身决定（官方对非 Notebook 文档
        // 也是先筛"有手写的页"）。这里直接给一句提示，别把书的结构写坏。
        let is_rm = self
            .library
            .find(&uuid)
            .map(|b| b.format == rmdev::doc::Format::Rm)
            .unwrap_or(false);
        // 计划 §13：转不了（不是笔记 / 服务器没配对）就一句 `Not available` 退出，
        // 不进选页，也不卡住 UI。
        if !is_rm || !rmdev::net::Config::load().configured() {
            self.notice(NoticeKind::Info, "Not available");
            return;
        }
        if self.job.is_some() {
            self.notice(NoticeKind::Info, "Please wait for the current task");
            return;
        }
        let cur = match &self.screen {
            Screen::Notebook(nb) => nb.page_index,
            _ => 0,
        };
        let total = self.library.page_ids(&uuid).len().max(1);
        self.conv.begin_select(&uuid);
        // 先把多选态摆好（默认选中当前页），再让 `open_overview` 画第一帧：
        // 整屏刷新很贵，进选页只刷一次。
        self.pagesel.show(total, cur.min(total - 1));
        eprintln!("[ocr] 进入选页（第 {} 页 / 共 {} 页）", cur + 1, total);
        self.open_overview();
    }

    /// 官方 `Convert`：拿总览里选中的页，从最后一页开始逐页丢后台。
    fn conv_start(&mut self) {
        if !self.conv.selecting() {
            return;
        }
        let current = match &self.screen {
            Screen::Notebook(nb) => nb.page_index,
            _ => 0,
        };
        let pages = conv_pages_from_selection(&self.pagesel.selected(), current);
        eprintln!("[ocr] 开始转换 {} 页：{pages:?}", pages.len());
        // 转换会往文档里插页、页码会变：先把当前页的墨迹/文字存盘，
        // 免得后面按新页码重画时把还没落盘的笔画丢掉。
        if let Screen::Notebook(nb) = &self.screen {
            let _ = self.library.save_texts(&nb.uuid, nb.page_index, &nb.texts);
            if nb.dirty {
                let _ = self
                    .library
                    .save_page_at(&nb.uuid, nb.page_index, &nb.page);
            }
        }
        self.conv.start(pages);
        // 官方进度文案（不自动消失；完成/失败时替换，退出时清掉）
        self.conv.set_notice(NoticeKind::Info, conv_progress_text(CONV_LANG));
        self.conv_next();
        self.draw_overview();
    }

    /// 给下一张选中的页起一个后台 OCR（结果走 `poll_job` → `conv_page_text`）。
    /// 主循环不被阻塞：一次只等一页，回来再起下一页。
    fn conv_next(&mut self) {
        if !self.conv.running() || self.job.is_some() {
            return;
        }
        let Some(idx) = self.conv.current_page() else {
            return;
        };
        let cfg = rmdev::net::Config::load();
        let uuid = self.conv.uuid.clone();
        let bmp = self.conv_page_bmp(&uuid, idx);
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = rmdev::net::ocr(&cfg, &bmp, "handwriting", "en").map(JobDone::Text);
            let _ = tx.send(r);
        });
        self.job = Some((Job::Ocr, rx));
    }

    /// 某一页的 OCR 回来了：按页高切成几段，每段在原页之后插一页。
    /// **原页一个像素都不动**（`insert_text_page` 只写新页）。
    fn conv_page_text(&mut self, text: &str) {
        if !self.conv.running() {
            return;
        }
        let Some(orig) = self.conv.current_page() else {
            return;
        };
        let uuid = self.conv.uuid.clone();
        let pages = conv_paginate(text, conv_lines_per_page(CONV_PAGE_H));
        let mut err: Option<String> = None;
        for (seg, lines) in pages.iter().enumerate() {
            // 第 seg 段接在原页后面 → 插入点 `orig + seg`，新页序号 = 插入点 + 1
            let after = orig + seg;
            match self.library.insert_text_page(&uuid, after, &conv_items(lines)) {
                Ok(at) => {
                    // 新页插在前面已经插过的页之前时，那些序号整体后移一格
                    conv_shift_inserted(&mut self.conv.inserted, at);
                    self.conv.inserted.push(at);
                }
                Err(e) => {
                    err = Some(e.to_string());
                    break;
                }
            }
        }
        if let Some(e) = err {
            self.conv_failed(&e);
            return;
        }
        self.conv.advance();
        if self.conv.all_done() {
            self.conv_finish();
            return;
        }
        self.conv_next();
        if self.conv_is_current_doc() {
            self.draw_overview();
        }
    }

    /// 全部页都转完：重开总览（新页立刻可见），状态条换成官方结果文案。
    fn conv_finish(&mut self) {
        self.conv.finish();
        if self.conv.stage != ConvStage::Done {
            eprintln!("[ocr] 没有认出任何文字");
            self.conv_failed("no text");
            return;
        }
        let n = self.conv.inserted.len();
        eprintln!("[ocr] 转换完成，新增 {n} 页");
        // 官方 `res_066b1b0.qml:79` 的 done 态文案
        self.conv.set_notice(NoticeKind::Info, "Converted text added as new page");
        self.job = None;
        self.pagesel.close();
        if self.conv_is_current_doc() {
            // 页结构变了：页码/页内容/文字层一起重新对一遍，再画总览
            self.pages_changed();
        } else {
            // 用户早就切走了：页已经落盘，状态别挂着
            self.conv.reset();
        }
    }

    /// 失败：官方错误态（状态条一句错误；官方这时不画动作按钮）。
    fn conv_failed(&mut self, err: &str) {
        let text = conv_error_text(err);
        eprintln!("[ocr] 转换失败：{err} → {text}");
        self.conv.fail();
        self.conv.set_notice(NoticeKind::Error, text);
        self.job = None;
        self.pagesel.close();
        if !self.conv_is_current_doc() {
            return;
        }
        if self.conv.inserted.is_empty() {
            // 页结构没变：只把错误条画出来，别重载当前页（内存里可能还有没存的笔画）
            self.draw_overview();
        } else {
            // 已经插了一部分：页码变了，重新对一遍
            self.pages_changed();
        }
    }

    /// `Cancel` / 点状态条：退出转换态。后台线程杀不掉，丢接收端即可
    /// （它发结果时 `send` 失败，不会落页）。官方 `Cancel` 连页总览一起关掉。
    fn conv_exit(&mut self) {
        if self.conv.running() {
            eprintln!("[ocr] 取消转换");
            self.job = None;
        }
        let here = self.conv_is_current_doc();
        self.conv.reset();
        self.pagesel.close();
        if here {
            // `close_overview` 里也会 reset（幂等）
            self.close_overview();
        }
    }

    /// 官方 `Undo`：把这次插进去的页删掉（原页本来就没动过）。
    fn conv_undo(&mut self) {
        let uuid = self.conv.uuid.clone();
        let plan = conv_undo_order(&self.conv.inserted);
        for idx in plan {
            let _ = self.library.delete_page(&uuid, idx);
        }
        eprintln!("[ocr] Undo：删掉 {} 页", self.conv.inserted.len());
        let here = self.conv_is_current_doc();
        self.conv.reset();
        if here {
            self.pages_changed();
        }
    }

    /// 转换态的浮层：官方 convert 头部（`Select pages for conversion` /
    /// `Convert` / `Cancel`，`res_066c007.qml:278`）+ 底部状态条
    /// （`res_066b1b0.qml` 整条黑底）。
    fn paint_convert_overlay(&mut self) {
        if !self.conv.active() {
            return;
        }
        let (w, h) = (self.fb.width, self.fb.height);
        if self.conv.selecting() || self.conv.running() {
            // 官方转换时页总览自己的顶栏和动作条都不出现，整条盖掉
            self.fb.fill_rect(0, 0, w, pagesel::HDR_H, gray565(255));
            self.fb.fill_rect(0, pagesel::HDR_H, w, 2, gray565(20));
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, NAV_MARGIN, 76, 32.0, "Cancel", 20);
            }
            let title = "Select pages for conversion";
            let tw = self
                .text
                .as_mut()
                .map(|t| t.width(34.0, title) as i32)
                .unwrap_or(0);
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, (w - tw) / 2, 78, 34.0, title, 20);
            }
            // `Convert` 只在选页态可点；转换中画灰
            let cr = conv_header_convert(w);
            let tone = if self.conv.selecting() { 20u8 } else { 150u8 };
            let cw = self
                .text
                .as_mut()
                .map(|t| t.width_bold(32.0, "Convert") as i32)
                .unwrap_or(0);
            let cx = cr.0 + cr.2 - NAV_MARGIN - cw;
            if let Some(t) = self.text.as_mut() {
                t.draw_bold(&mut self.fb, cx, 76, 32.0, "Convert", tone);
            }
            // 底部动作条整块盖白（官方转换时没有页动作条）
            self.fb.fill_rect(0, h - pagesel::BAR_H, w, pagesel::BAR_H, gray565(255));
        }
        self.paint_convert_bar(w, h);
    }

    /// 底部状态条：黑底白字 + 进度条 + 右侧动作按钮（`Cancel` / `Undo`）。
    fn paint_convert_bar(&mut self, w: i32, h: i32) {
        let Some(notice) = self.conv.notice.clone() else {
            return;
        };
        let (bx, by, bw, bh) = conv_bar_rect(w, h);
        self.fb.fill_rect(bx, by, bw, bh, gray565(0));
        let cy = by + bh / 2;
        let mut tx = NAV_MARGIN;
        if notice.kind == NoticeKind::Error {
            // 官方 `StatusIndicatorError` 前面那枚 warning 圈
            widgets::ring(&mut self.fb, tx + 14, cy, 13, 3, 250);
            self.fb.fill_rect(tx + 12, cy - 8, 4, 12, gray565(250));
            self.fb.fill_rect(tx + 12, cy + 7, 4, 4, gray565(250));
            tx += 40;
        }
        let label = match self.conv.stage {
            ConvStage::Running => Some("Cancel"),
            ConvStage::Done => Some("Undo"),
            // 官方错误态不画动作按钮；但已经插进去的页得能撤掉
            ConvStage::Failed if !self.conv.inserted.is_empty() => Some("Undo"),
            _ => None,
        };
        let mut right = if label.is_some() { w - 292 } else { w - NAV_MARGIN };
        // 转换中右边是进度条，文字别压上去
        if self.conv.running() {
            right = right.min(conv_bar_progress(w, h).0 - 24);
        }
        if let Some(t) = self.text.as_mut() {
            let shown = t.ellipsize(NOTICE_SIZE, &notice.text, right - tx);
            t.draw(&mut self.fb, tx, by + 46, NOTICE_SIZE, &shown, 250);
        }
        // 进度条：官方 `ProgressIndicator`（只在转的时候出现）
        if self.conv.running() {
            let r = conv_bar_progress(w, h);
            self.fb.fill_rect(r.0, r.1, r.2, r.3, gray565(60));
            let f = conv_bar_fill(w, h, self.conv.percent());
            if f.2 > 0 {
                self.fb.fill_rect(f.0, f.1, f.2, f.3, gray565(250));
            }
        }
        if let Some(l) = label {
            let (ux, uy, uw, uh) = conv_bar_button(w, h);
            // 官方按钮前面一条白色分隔线
            self.fb.fill_rect(ux - 2, uy + 16, 2, uh - 32, gray565(250));
            let lw = self
                .text
                .as_mut()
                .map(|t| t.width(30.0, l) as i32)
                .unwrap_or(0);
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, ux + (uw - lw) / 2, uy + 46, 30.0, l, 250);
            }
        }
    }

    /// 转换态的点击。返回 true = 这次点击归转换态（不再往总览传）。
    fn tap_convert(&mut self, x: f32, y: f32) -> bool {
        if !self.conv.active() {
            return false;
        }
        let (w, h) = (self.fb.width, self.fb.height);
        let (xi, yi) = (x as i32, y as i32);
        let in_rect = |r: (i32, i32, i32, i32)| {
            xi >= r.0 && xi < r.0 + r.2 && yi >= r.1 && yi < r.1 + r.3
        };
        // 选页/转换中：顶栏和底部动作条都归转换态
        if self.conv.selecting() || self.conv.running() {
            if yi < pagesel::HDR_H {
                if in_rect(conv_header_cancel(w)) {
                    self.conv_exit();
                } else if self.conv.selecting() && in_rect(conv_header_convert(w)) {
                    self.conv_start();
                }
                return true;
            }
            if yi >= h - pagesel::BAR_H {
                // 转换中状态条上那颗 `Cancel`（官方 processing 态的动作按钮）
                if self.conv.running() && in_rect(conv_bar_button(w, h)) {
                    self.conv_exit();
                }
                return true;
            }
            if let Some(a) = self.hits.take(x, y) {
                if (A_PAGE0..A_PAGE0 + 100).contains(&a) {
                    self.pagesel.toggle((a - A_PAGE0) as usize);
                    self.draw_overview();
                }
            }
            return true;
        }
        // 结果态：只有底部那条可点，其它点击留给正常总览（可以点开结果页看）
        if !in_rect(conv_bar_rect(w, h)) {
            return false;
        }
        if in_rect(conv_bar_button(w, h)) && !self.conv.inserted.is_empty() {
            self.conv_undo();
        } else {
            self.conv_exit();
        }
        true
    }
    // === P10 end ===

    /// 立刻同步一次（后台线程，结果走 `poll_job`）。
    /// 自动同步：**安静**（不弹 Syncing/Sync completed，失败也不弹）。
    ///
    /// 为什么单独一条：自动同步每 10 分钟一次，要是每次都弹提示、每次都整屏
    /// 重画，用户正在写字时会被打断。所以：
    /// - 有别的任务在跑就这一轮直接跳过（不排队、不提示）；
    /// - 完成时只有真的拉下来东西才重绘。
    /// 现在插着电吗。电量策略只用缓存（`BATT` 每 10 s 刷一次），
    /// `read_battery()` 走 uevent，一次 400 ms 级，不能在 tick 里随口调。
    fn charging(&self) -> bool {
        match battery_cached() {
            Some(b) => {
                b.usb_online
                    || matches!(
                        b.state,
                        rmdev::settings::battery::ChargeState::Charging
                            | rmdev::settings::battery::ChargeState::Full
                    )
            }
            None => false,
        }
    }

    /// 下一轮自动同步隔多久。
    ///
    /// 三个因子：
    /// * 底数 10 分钟；连续 [`SYNC_QUIET_MAX`] 轮没变化就翻倍，封顶 60 分钟；
    /// * 电量 ≤ 20% 且没插电 → 底数就拉到 30 分钟（口袋里别再折腾无线）；
    /// * 插着电 → 退回 10 分钟（电不要钱，实时性优先）。
    fn sync_next_secs(&self) -> u64 {
        let (cap, _) = read_battery();
        let charging = self.charging();
        sync_interval_secs(charging, cap <= 20, self.sync_quiet)
    }

    fn start_sync_auto(&mut self) {
        if self.job.is_some() {
            return;
        }
        let cfg = rmdev::net::Config::load();
        if !cfg.configured() {
            return;
        }
        self.sync_is_auto = true;
        self.start_sync();
    }

    fn start_sync(&mut self) {
        let cfg = rmdev::net::Config::load();
        if !cfg.configured() {
            // === P3 begin ===
            self.notice(NoticeKind::Info, "Pair a server first");
            // === P3 end ===
            return;
        }
        if self.job.is_some() {
            // === P3 begin ===
            self.notice(NoticeKind::Info, "Please wait for the current task");
            // === P3 end ===
            return;
        }
        let root = self.library.root.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = rmdev::sync::sync_once(&cfg, &root).map(|st| JobDone::Synced {
                up: st.up,
                down: st.down,
                conflicts: st.conflicts,
                rev: st.rev,
            });
            let _ = tx.send(r);
        });
        self.job = Some((Job::Sync, rx));
        // === P3 begin ===
        // 自动那一轮不弹（`start_sync_auto` 已经置位）
        if !self.sync_is_auto {
            self.notice(NoticeKind::Info, "Syncing...");
        }
        // === P3 end ===
    }

    /// 配对向导：第一步服务器地址，第二步配对码。
    fn open_pair(&mut self) {
        self.screen = Screen::Pair(Box::new(Pair {
            kb: Keyboard::new(),
            stage: 0,
            url: String::new(),
        }));
        self.draw_pair();
    }

    fn draw_pair(&mut self) {
        let (w, h) = (self.fb.width, self.fb.height);
        let Some((kb, stage, url)) = (match &self.screen {
            Screen::Pair(p) => Some((p.kb.clone(), p.stage, p.url.clone())),
            _ => None,
        }) else {
            return;
        };
        self.fb.fill_rect(0, 0, w, h, gray565(255));
        self.fb.fill_rect(0, 0, w, 2, gray565(180));
        self.hits.clear();

        let title = if stage == 0 { "Sync server" } else { "Pairing code" };
        if let Some(t) = self.text.as_mut() {
            t.set_bold(true);
            t.draw(&mut self.fb, NAV_MARGIN, 100, 40.0, title, 20);
            t.set_bold(false);
        }
        let hint = if stage == 0 {
            "Address of your own server, e.g. http://192.168.1.10:8080"
        } else {
            "The 8-digit code printed by your server"
        };
        if let Some(t) = self.text.as_mut() {
            t.draw(&mut self.fb, NAV_MARGIN, 166, 27.0, hint, 110);
        }

        let (ix, iy, iw, ih) = (NAV_MARGIN, 200, w - 2 * NAV_MARGIN, 84);
        self.fb.fill_rect(ix, iy, iw, ih, gray565(238));
        let shown_src = if stage == 0 && kb.text.is_empty() { url.clone() } else { kb.text.clone() };
        let empty_hint = stage == 0 && kb.text.is_empty();
        if let Some(t) = self.text.as_mut() {
            let shown = t.ellipsize(34.0, &shown_src, iw - 40);
            t.draw(&mut self.fb, ix + 20, iy + 58, 34.0, &shown, if empty_hint { 110 } else { 20 });
        }

        // 键盘贴屏底，按钮紧跟输入框——中间不再留一大片空白
        let ky = h - kb.height() - 16;
        self.kb_hits = kb.draw(&mut self.fb, &mut self.text, 0, ky, w);

        let by = iy + ih + 48;
        let bw = (w - 2 * NAV_MARGIN - 20) / 2;
        self.fb.fill_rect(NAV_MARGIN, by, bw, 90, gray565(210));
        self.fb.fill_rect(NAV_MARGIN + bw + 20, by, bw, 90, gray565(40));
        if let Some(t) = self.text.as_mut() {
            t.draw(&mut self.fb, NAV_MARGIN + 60, by + 60, 34.0, "Cancel", 20);
            let ok = if stage == 0 { "Next" } else { "Pair" };
            t.draw(&mut self.fb, NAV_MARGIN + bw + 80, by + 60, 34.0, ok, 250);
        }
        self.hits.push(NAV_MARGIN, by, bw, 90, A_CANCEL);
        self.hits.push(NAV_MARGIN + bw + 20, by, bw, 90, A_OK);
        self.fb.refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
    }

    // === P2 begin ===
    //
    // 发邮件对话框（官方 `res_06b193d.qml`）：黑头 + 行列表 + 底部主按钮。
    // 纯逻辑（收件人/页码区间解析、布局文案、PNG·PDF 编码）在 `rmdev::share`，
    // 这里只负责绘制和命中。

    fn open_mail(&mut self) {
        use rmdev::share as sh;
        let nb = match &self.screen {
            Screen::Notebook(nb) => Some((
                nb.uuid.clone(),
                nb.page_index,
                nb.name.clone(),
                nb.page_count,
            )),
            _ => None,
        };
        let Some((uuid, page, name, pages_field)) = nb else {
            return;
        };
        // 页数以 `.content` 为准（PDF/电子书由 `nb.page_count` 给）
        let total = self.library.page_ids(&uuid).len().max(pages_field).max(1);
        let to = rmdev::net::Config::load().mail_to;
        let mut kb = Keyboard::new();
        kb.text = to.clone();
        self.screen = Screen::Mail(Box::new(Mail {
            kb,
            back: (uuid, page),
            name: name.clone(),
            focus: sh::F_NONE,
            to,
            subject: name.clone(),
            filename: name.clone(),
            mode: 0,
            range: format!("1-{total}"),
            format: sh::MailFormat::Pdf,
            total,
            err: false,
        }));
        self.draw_mail();
    }

    /// 字段现在生效的文本：正在编辑的那个字段取键盘缓冲，其余取已提交的值。
    fn mail_field(m: &Mail, f: u8) -> &str {
        use rmdev::share as sh;
        if m.focus == f {
            &m.kb.text
        } else {
            match f {
                sh::F_TO => &m.to,
                sh::F_SUBJECT => &m.subject,
                sh::F_FILE => &m.filename,
                sh::F_RANGE => &m.range,
                _ => "",
            }
        }
    }

    /// 行列表的一行：标题 + 值 + 下划线；聚焦时值后面画一根光标。
    fn mail_row(
        fb: &mut Fb,
        t: &mut Text,
        r: (i32, i32, i32, i32),
        title: &str,
        value: &str,
        tone: u8,
        focused: bool,
    ) {
        use rmdev::share as sh;
        let (x, y, w, _h) = r;
        t.draw_bold(fb, x + 33, y + 52, 36.0, title, 20);
        let shown = t.ellipsize(34.0, value, w - 66);
        t.draw(fb, x + 33, y + 104, 34.0, &shown, tone);
        if focused {
            let tw = t.width(34.0, &shown) as i32;
            fb.fill_rect(x + 35 + tw, y + 72, 2, 34, gray565(20));
        }
        fb.fill_rect(x + 33, y + sh::ROW_H - 2, w - 66, 1, gray565(220));
    }

    fn draw_mail(&mut self) {
        use rmdev::share as sh;
        let (w, h) = (self.fb.width, self.fb.height);
        let Some(m) = (match &self.screen {
            Screen::Mail(m) => Some((**m).clone()),
            _ => None,
        }) else {
            return;
        };
        let kb_open = m.focus != sh::F_NONE;
        let to = Self::mail_field(&m, sh::F_TO).to_string();
        let subject = Self::mail_field(&m, sh::F_SUBJECT).to_string();
        let file_raw = Self::mail_field(&m, sh::F_FILE).to_string();
        let range = Self::mail_field(&m, sh::F_RANGE).to_string();
        let pages = sh::pick(m.mode as usize, m.back.1 + 1, &range, m.total);
        let configured = rmdev::net::Config::load().configured();
        let can = sh::can_send(&to, &pages, m.format, configured, self.job.is_some());
        let file_shown = if file_raw.trim().is_empty() {
            sh::with_ext(&m.name, m.format)
        } else {
            sh::with_ext(&file_raw, m.format)
        };
        // 区间写错：把用户输入原样（灰）显示，`Send` 也灰着，绝不悄悄发全部
        let pages_label = sh::pages_label(&pages);
        let pages_tone = if pages_label.is_empty() { 110 } else { 20 };
        let pages_shown = if pages_label.is_empty() {
            range.clone()
        } else {
            pages_label
        };

        self.fb.fill_rect(0, 0, w, h, gray565(255));
        let rows = sh::layout(w, h, kb_open);
        let rect = |a: sh::Act| {
            rows.iter()
                .find(|(b, _)| *b == a)
                .map(|(_, r)| *r)
                .unwrap_or((0, 0, 0, 0))
        };

        // ---------- 黑头：`Cancel` | `Send by email` | `Send`
        self.fb.fill_rect(0, 0, w, sh::HDR_H, gray565(0));
        if let Some(t) = self.text.as_mut() {
            t.draw_bold(&mut self.fb, 32, 74, 33.0, sh::CANCEL, 255);
            let tw = t.width_bold(33.0, sh::MAIL_TITLE);
            t.draw_bold(
                &mut self.fb,
                (w as f32 / 2.0 - tw / 2.0) as i32,
                74,
                33.0,
                sh::MAIL_TITLE,
                255,
            );
            let sw = t.width_bold(33.0, sh::SEND);
            t.draw_bold(
                &mut self.fb,
                w - 32 - sw as i32,
                74,
                33.0,
                sh::SEND,
                if can { 255 } else { 130 },
            );
            if m.err {
                // 发送失败：标题下的红字（16 级灰屏只能用最深的灰）
                t.draw(&mut self.fb, 33, sh::HDR_H + 40, 26.0, sh::SEND_FAILED, sh::ERR_TONE);
            }
        }

        // ---------- 行列表
        if let Some(t) = self.text.as_mut() {
            let r = rect(sh::Act::To);
            let hint = to.trim().is_empty();
            Self::mail_row(
                &mut self.fb,
                t,
                r,
                sh::SEND_TO,
                if hint { sh::ADD_EMAIL } else { to.as_str() },
                if hint { 110 } else { 20 },
                m.focus == sh::F_TO,
            );
            let r = rect(sh::Act::Pages);
            Self::mail_row(
                &mut self.fb,
                t,
                r,
                sh::SELECTED,
                &pages_shown,
                pages_tone,
                m.focus == sh::F_RANGE,
            );
            // `Send as`：三个单选，SVG 没有渲染器 → 灰显 `Not available` 且点不到
            t.draw_bold(&mut self.fb, 33, sh::SEND_AS_Y, 36.0, sh::SEND_AS, 20);
            for (i, f) in sh::FORMATS.iter().enumerate() {
                let cy = sh::radio_cy(i);
                let tone = if f.available() { 20 } else { 150 };
                widgets::ring(&mut self.fb, 52, cy, 20, 4, tone);
                if m.format == *f && f.available() {
                    widgets::disc(&mut self.fb, 52, cy, 9, 20);
                }
                t.draw(&mut self.fb, 92, cy + 12, 34.0, f.label(), tone);
                if !f.available() {
                    t.draw(&mut self.fb, 210, cy + 12, 30.0, sh::NOT_AVAILABLE, 150);
                }
            }
            let r = rect(sh::Act::Subject);
            Self::mail_row(
                &mut self.fb,
                t,
                r,
                sh::SUBJECT,
                &subject,
                20,
                m.focus == sh::F_SUBJECT,
            );
            let r = rect(sh::Act::Filename);
            Self::mail_row(
                &mut self.fb,
                t,
                r,
                sh::FILE_NAME,
                &file_shown,
                20,
                m.focus == sh::F_FILE,
            );
        }

        // ---------- 底部主按钮（键盘收起时才有）
        if !kb_open {
            let r = rect(sh::Act::Send);
            self.fb
                .fill_rect(r.0, r.1, r.2, r.3, gray565(if can { 0 } else { 205 }));
            if let Some(t) = self.text.as_mut() {
                let tw = t.width_bold(40.0, sh::SEND);
                t.draw_bold(
                    &mut self.fb,
                    r.0 + (r.2 - tw as i32) / 2,
                    r.1 + 60,
                    40.0,
                    sh::SEND,
                    if can { 255 } else { 130 },
                );
            }
        }

        // ---------- 键盘
        if kb_open {
            let ky = h - m.kb.height() - 16;
            self.kb_hits = m.kb.draw(&mut self.fb, &mut self.text, 0, ky, w);
        } else {
            self.kb_hits.clear();
        }
        // 这一页不走 `self.hits`，命中全在 `share::hit`
        self.hits.clear();
        self.fb.refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
    }

    fn tap_mail(&mut self, x: f32, y: f32) {
        use rmdev::share as sh;
        let (w, h) = (self.fb.width, self.fb.height);
        let kb_open = matches!(&self.screen, Screen::Mail(m) if m.focus != sh::F_NONE);
        if kb_open {
            for (r, k) in self.kb_hits.clone() {
                let (xi, yi) = (x as i32, y as i32);
                if xi >= r.0 && xi < r.0 + r.2 && yi >= r.1 && yi < r.1 + r.3 {
                    if k == Key::Enter {
                        // 键盘上的回车 = 官方 `Confirm`：收下输入、收起键盘
                        self.mail_commit();
                    } else if let Screen::Mail(m) = &mut self.screen {
                        m.kb.press(k);
                    }
                    self.draw_mail();
                    return;
                }
            }
        }
        let Some(a) = sh::hit(x, y, w, h, kb_open) else {
            return;
        };
        match a {
            sh::Act::Cancel => return self.back_to_doc(),
            sh::Act::Send => return self.send_mail_now(),
            sh::Act::To => self.mail_focus(sh::F_TO),
            sh::Act::Subject => self.mail_focus(sh::F_SUBJECT),
            sh::Act::Filename => self.mail_focus(sh::F_FILE),
            sh::Act::Pages => self.mail_cycle_pages(),
            sh::Act::Format(f) => {
                if f.available() {
                    if let Screen::Mail(m) = &mut self.screen {
                        m.format = f;
                    }
                }
            }
        }
        self.draw_mail();
    }

    /// 把键盘缓冲存回当前字段，并收起键盘。
    fn mail_commit(&mut self) {
        use rmdev::share as sh;
        if let Screen::Mail(m) = &mut self.screen {
            let text = m.kb.text.clone();
            match m.focus {
                sh::F_TO => m.to = text,
                sh::F_SUBJECT => m.subject = text,
                sh::F_FILE => m.filename = text,
                sh::F_RANGE => m.range = text,
                _ => {}
            }
            m.focus = sh::F_NONE;
        }
        self.kb_hits.clear();
    }

    /// 点行切焦点；同一行再点一下收起键盘。
    fn mail_focus(&mut self, f: u8) {
        use rmdev::share as sh;
        let same = matches!(&self.screen, Screen::Mail(m) if m.focus == f);
        self.mail_commit();
        if same {
            return;
        }
        if let Screen::Mail(m) = &mut self.screen {
            m.focus = f;
            // 页码是数字，直接切到符号层
            m.kb.layer = if f == sh::F_RANGE {
                Layer::Symbols
            } else {
                Layer::Lower
            };
            let text = match f {
                sh::F_TO => m.to.clone(),
                sh::F_SUBJECT => m.subject.clone(),
                sh::F_FILE => m.filename.clone(),
                _ => m.range.clone(),
            };
            m.kb.text = text;
        }
    }

    /// `Selected` 行：当前页 → 全部页 → 区间 → 当前页；进区间时顺手升键盘。
    fn mail_cycle_pages(&mut self) {
        use rmdev::share as sh;
        let mode = match &self.screen {
            Screen::Mail(m) => m.mode,
            _ => return,
        };
        self.mail_commit();
        if let Screen::Mail(m) = &mut self.screen {
            m.mode = (mode + 1) % 3;
            if m.mode == 2 {
                m.focus = sh::F_RANGE;
                m.kb.layer = Layer::Symbols;
                m.kb.text = m.range.clone();
            }
        }
    }
    // === P2 end ===

    /// 关掉浮层回到刚才那一页。
    fn back_to_doc(&mut self) {
        let back = match &self.screen {
            Screen::Mail(m) => Some(m.back.clone()),
            _ => None,
        };
        match back {
            Some((uuid, page)) => self.open_at(&uuid, page),
            None => {
                self.screen = Screen::Library;
                self.draw_library();
            }
        }
    }

    // === P2 begin ===
    /// 点 `Send`：先渲染（PNG 多页 = 多张，PDF 合成一个文件），再后台投递。
    fn send_mail_now(&mut self) {
        use rmdev::share as sh;
        // 键盘可能还开在某个字段上：先收下最新输入
        self.mail_commit();
        let Some((uuid, page, name, total, to, subject, filename, mode, range, format)) =
            (match &self.screen {
                Screen::Mail(m) => Some((
                    m.back.0.clone(),
                    m.back.1,
                    m.name.clone(),
                    m.total,
                    m.to.clone(),
                    m.subject.clone(),
                    m.filename.clone(),
                    m.mode,
                    m.range.clone(),
                    m.format,
                )),
                _ => None,
            })
        else {
            return;
        };
        let pages = sh::pick(mode as usize, page + 1, &range, total);
        let mut cfg = rmdev::net::Config::load();
        // 收件人没填好 / 服务器没配 / 正在发：`Send` 灰着，红字不出现
        if !sh::can_send(&to, &pages, format, cfg.configured(), self.job.is_some()) {
            if let Screen::Mail(m) = &mut self.screen {
                m.err = false;
            }
            self.draw_mail();
            return;
        }
        let Ok(to_list) = sh::validate_to(&to) else {
            return;
        };
        // 整页渲染（只画墨迹，和 OCR 走同一条路）
        let all = pages.resolve(total);
        let idxs: Vec<usize> = all.iter().copied().take(sh::MAX_PAGES).collect();
        if idxs.len() < all.len() {
            eprintln!("[mail] 页数超过 {}，只发前 {} 页", sh::MAX_PAGES, idxs.len());
        }
        let mut imgs = Vec::with_capacity(idxs.len());
        // `self.doc` 是"最近打开过的电子书"的缓存，换回笔记本它还在，
        // 所以必须按 `doc_uuid` 比对，不能只看 `doc` 有没有值。
        let is_this_doc = self.doc_uuid.as_deref() == Some(uuid.as_str());
        for &i in idxs.iter() {
            // 墨迹层（只画笔画，和 OCR 导出走同一条路径）
            let ink = self.page_thumb(&uuid, i, 1404, 1872);
            // PDF/电子书再叠上自己的整页底图（底图和墨迹都是白底黑线，取更黑的像素）
            let gray = if is_this_doc {
                match self.render_doc_bg(&uuid, i) {
                    Some((bg, _, _, _)) if bg.len() == ink.len() => sh::compose(&bg, &ink),
                    _ => ink,
                }
            } else {
                ink
            };
            imgs.push(sh::PageImage {
                number: i + 1,
                gray,
                w: 1404,
                h: 1872,
            });
        }
        let spec = sh::MailSpec {
            to: to_list.clone(),
            pages: pages.clone(),
            format,
            subject: subject.clone(),
            filename: filename.clone(),
        };
        let atts = sh::attachments(&spec, &name, &imgs);
        if atts.is_empty() {
            // 没有可发的附件（SVG 没有渲染器 / 空页）：按发送失败提示
            if let Screen::Mail(m) = &mut self.screen {
                m.err = true;
            }
            self.draw_mail();
            return;
        }
        let body_text = sh::body(&spec, &name);
        let subject_line = sh::subject(&spec, &name);
        // 记住收件人，下次打开填上
        if cfg.mail_to != to {
            cfg.mail_to = to.clone();
            let _ = cfg.save();
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let owned: Vec<(Vec<u8>, String, &'static str)> = atts
                .into_iter()
                .map(|a| (a.data, a.filename, a.mime))
                .collect();
            let refs: Vec<(&[u8], String, &str)> = owned
                .iter()
                .map(|(d, n, m)| (d.as_slice(), n.clone(), *m))
                .collect();
            let r = rmdev::net::send_mail_multi(&cfg, &to_list, &subject_line, &body_text, &refs)
                .map(|_| JobDone::MailSent);
            let _ = tx.send(r);
        });
        self.job = Some((Job::Mail, rx));
        if let Screen::Mail(m) = &mut self.screen {
            m.err = false;
        }
        // 这里不能 toast：`toast()` 会把库界面画到邮件页上来。
        // "发送中" 由 `Send` 变灰表达（`can_send` 的 busy）。
        self.draw_mail();
    }
    // === P2 end ===

    fn tap_pair(&mut self, x: f32, y: f32) {
        for (r, k) in self.kb_hits.clone() {
            let (xi, yi) = (x as i32, y as i32);
            if xi >= r.0 && xi < r.0 + r.2 && yi >= r.1 && yi < r.1 + r.3 {
                if k == Key::Enter {
                    self.pair_advance();
                    return;
                }
                if let Screen::Pair(p) = &mut self.screen {
                    p.kb.press(k);
                }
                self.draw_pair();
                return;
            }
        }
        match self.hits.take(x, y) {
            Some(A_CANCEL) => {
                if let Screen::Pair(p) = &self.screen {
                    if p.stage == 1 {
                        let url = p.url.clone();
                        if let Screen::Pair(p) = &mut self.screen {
                            p.stage = 0;
                            p.url = url;
                            p.kb = Keyboard::new();
                        }
                        self.draw_pair();
                        return;
                    }
                }
                self.back_to_settings();
            }
            Some(A_OK) => self.pair_advance(),
            _ => {}
        }
    }

    /// 配对向导的下一步：地址 → 配对码 → 真去连。
    fn pair_advance(&mut self) {
        let Some((stage, text)) = (match &self.screen {
            Screen::Pair(p) => Some((p.stage, p.kb.text.trim().to_string())),
            _ => None,
        }) else {
            return;
        };
        if stage == 0 {
            if text.is_empty() {
                // === P3 begin ===
                self.notice(NoticeKind::Info, "Enter the server address");
                // === P3 end ===
                return;
            }
            if !text.starts_with("http://") {
                // === P3 begin ===
                self.notice(NoticeKind::Info, "The address must start with http://");
                // === P3 end ===
                return;
            }
            if let Screen::Pair(p) = &mut self.screen {
                p.url = text;
                p.stage = 1;
                p.kb = Keyboard::new();
            }
            self.draw_pair();
            return;
        }
        if text.is_empty() {
            // === P3 begin ===
            self.notice(NoticeKind::Info, "Enter the pairing code");
            // === P3 end ===
            return;
        }
        let url = match &self.screen {
            Screen::Pair(p) => p.url.clone(),
            _ => return,
        };
        self.start_pair(url, text);
    }

    /// 后台去配对（联网可能要几秒，不能占主循环）。
    fn start_pair(&mut self, url: String, code: String) {
        if self.job.is_some() {
            // === P3 begin ===
            self.notice(NoticeKind::Info, "Please wait for the current task");
            // === P3 end ===
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = rmdev::net::pair(&url, &code, "reMarkable 2")
                .map(|c| JobDone::Paired(c.url))
                .map_err(|e| e);
            let _ = tx.send(r);
        });
        self.job = Some((Job::Pair, rx));
        // === P3 begin ===
        self.notice(NoticeKind::Info, "Pairing...");
        // === P3 end ===
    }

    fn back_to_settings(&mut self) {
        if let Screen::Settings(s) = &mut self.screen {
            s.sub = Some(SettingsSub::Account);
        } else {
            self.open_settings();
            if let Screen::Settings(s) = &mut self.screen {
                s.sub = Some(SettingsSub::Account);
            }
        }
        self.draw_settings();
    }

    /// 主循环空闲时收后台结果。
    fn poll_job(&mut self) {
        let Some((job, rx)) = &self.job else { return };
        let job = *job;
        match rx.try_recv() {
            Ok(Ok(done)) => {
                self.job = None;
                match done {
                    JobDone::Paired(url) => {
                        let host = url.trim_start_matches("http://").trim_end_matches('/').to_string();
                        // === P3 begin ===
                        self.notice(NoticeKind::Info, &format!("Paired with {host}"));
                        // === P3 end ===
                        self.refresh_server_config();
                        self.draw_settings();
                    }
                    // === P3 begin ===
                    // 同步完成：官方 res_06bbee5.qml 的 syncCompleteStatusText。
                    // 官方的通知条只有这一句，up/rev 这类统计只进日志。
                    JobDone::Synced { down, conflicts, .. } => {
                        let auto = std::mem::take(&mut self.sync_is_auto);
                        if !auto {
                            self.notice(NoticeKind::Info, "Sync completed");
                        }
                        // === P3 end ===
                        // 拉下来的文件可能改了任何一页：缩略图/光栅/页缓存全部作废
                        if down > 0 || conflicts > 0 {
                            self.thumbs.clear();
                            self.raster.clear();
                        }
                        self.refresh_server_config();
                        // 同步回来了：Job status 里那些"跑着"的可能已经发布完了
                        let jobs_changed = self.jobs_reconcile_and_report();
                        // === SYNC-AUTO begin ===
                        // 自动同步：什么都没拉下来、也没冲突 → 一个像素都不动。
                        // 后台同步绝不能因为"每 10 分钟跑一次"就让前台闪一下/卡一下。
                        if auto {
                            // 有东西下来 → 立刻回到 10 分钟；什么都没变 → 退避一轮
                            if down == 0 && conflicts == 0 && !jobs_changed {
                                self.sync_quiet = (self.sync_quiet + 1).min(SYNC_QUIET_MAX);
                                eprintln!("[sync] 自动同步：无变化，不重绘");
                                return;
                            }
                            self.sync_quiet = 0;
                        }
                        // === SYNC-AUTO end ===
                        if matches!(self.screen, Screen::Settings(_)) {
                            self.draw_settings();
                        } else if let Screen::Notebook(nb) = &self.screen {
                            // 在笔记里同步（比如刚发布了一份回答）：只重读 AI 索引并重绘，
                            // **不能** draw_library —— 那会把库界面糊到正在看的页面上。
                            let uuid = nb.uuid.clone();
                            self.ai_refs = rmdev::airefs::Refs::load(&self.library.root, &uuid);
                            self.redraw_page();
                        } else {
                            self.draw_library();
                        }
                    }
                    // === P10 begin ===
                    // 一页的 OCR 结果：插页 + 起下一页（不能阻塞主循环）
                    JobDone::Text(t) => self.conv_page_text(&t),
                    // === P10 end ===
                    // === P2 begin ===
                    // 发出去了：收起对话框回到刚才那一页（官方 `accepted()` 也是关掉）
                    JobDone::MailSent => {
                        // 还停在邮件页才收起来（用户可能已经 Cancel 走了）
                        if matches!(self.screen, Screen::Mail(_)) {
                            self.back_to_doc();
                        }
                        self.toast("Email sent");
                    }
                    // === P2 end ===
                }
            }
            Ok(Err(e)) => {
                self.job = None;
                eprintln!("[net] {job:?} 失败：{e}");
                // === P2 begin ===
                // 发信失败：红字留在标题下，不要把库界面 toast 上来
                if job == Job::Mail && matches!(self.screen, Screen::Mail(_)) {
                    if let Screen::Mail(m) = &mut self.screen {
                        m.err = true;
                    }
                    self.draw_mail();
                    return;
                }
                // === P2 end ===
                // === P10 begin ===
                // 转文字失败走官方错误文案（`res_066b1b0.qml` 的 errorMessage）
                if job == Job::Ocr {
                    self.conv_failed(&e);
                    return;
                }
                // === P10 end ===
                // === P3 begin ===
                if job == Job::Sync {
                    // 官方错误指示器文案（res_06b7a13.qml）
                    self.notice(NoticeKind::Error, "Sync failed.");
                } else {
                    self.notice(NoticeKind::Error, &e);
                }
                // === P3 end ===
                if job == Job::Pair {
                    self.draw_pair();
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.job = None;
                // === P10 begin ===
                if job == Job::Ocr {
                    self.conv_failed("connection");
                    return;
                }
                // === P10 end ===
                // === P3 begin ===
                if job == Job::Sync {
                    // 自动同步失败只记日志：每 10 分钟弹一次错会把人烦死，
                    // 而且状态栏那颗云本来就会变成带叉的（`spawn_net_reader`）。
                    if !std::mem::take(&mut self.sync_is_auto) {
                        self.notice(NoticeKind::Error, "Sync failed.");
                    }
                } else {
                    self.notice(NoticeKind::Error, "Connection lost");
                }
                // === P3 end ===
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    /// 设置页读的是缓存里的配置，配对/同步之后要重新读一次。
    fn refresh_server_config(&mut self) {
        let cfg = rmdev::net::Config::load();
        if let Screen::Settings(s) = &mut self.screen {
            s.server = cfg;
        }
    }

    // ------------------------------------------------------------ 页面总览

    /// 把某一页渲染成缩略图（灰度，255 白 / 0 黑）。
    fn page_thumb(&mut self, uuid: &str, idx: usize, tw: usize, th: usize) -> Vec<u8> {
        let scale = tw as f32 / 1404.0;
        let page = self.library.load_page_at(uuid, idx);
        let grade = rmdev::pen::pencil::DEFAULT;
        let mut c = Canvas::new(tw, th);
        for l in page.visible_lines() {
            c.ink_line(l, scale, (PAGE_DX * scale, PAGE_DY * scale), grade);
        }
        let mut out = vec![0u8; tw * th];
        for y in 0..th {
            for x in 0..tw {
                out[y * tw + x] = 255 - c.dark_at(x, y);
            }
        }
        out
    }

    fn open_overview(&mut self) {
        let (uuid, cur) = match &self.screen {
            Screen::Notebook(nb) => (nb.uuid.clone(), nb.page_index),
            _ => return,
        };
        // === P10 begin ===
        // 换了一本文档：上一次的转文字状态作废（结果页早就落盘了）
        if self.conv.active() && self.conv.uuid != uuid {
            self.conv.reset();
        }
        // === P10 end ===
        // 页数以 `.content` 为准：空文档落笔后 `save_page_at` 会把页登记回来，
        // 内存里的 `nb.page_count` 可能还是 0
        let total = self.library.page_ids(&uuid).len();
        // 每次重进总览都从干净的多选态开始（Move/Delete/Duplicate 之后也走这里）；
        // 多选 → Tag 的键盘如果还挂着也收掉，不然它会盖住整个总览
        if self.pagesel.tagging() {
            if let Screen::Notebook(nb) = &mut self.screen {
                nb.tag_edit = None;
            }
        }
        self.pagesel.close();
        // === P10 begin ===
        // 转文字的选页态是 `start_ocr` 刚摆好的（它紧接着调这里画第一帧），
        // 别在进总览时清掉——否则要多刷一次整屏。
        if self.conv.selecting() {
            self.pagesel.show(total, cur.min(total.saturating_sub(1)));
        }
        // === P10 end ===
        self.move_to = None;
        let tw = 306;
        let th = 392;
        // 总览是另一套坐标：文档设置面板先收起，免得它的命中区吃掉总览的点击
        self.ds.close();
        // 笔记本里那条 toast 黑条也别跟到总览上（总览有自己的提示位）
        self.banner = None;
        let mut thumbs = Vec::new();
        for i in 0..total {
            let fp = self.page_fp(&uuid, i);
            if let Some(pos) = self
                .page_thumbs_cache
                .iter()
                .position(|(k, f, _)| k == &(uuid.clone(), i) && *f == fp)
            {
                let e = self.page_thumbs_cache.remove(pos);
                thumbs.push((i, e.2.clone()));
                self.page_thumbs_cache.push(e);
                continue;
            }
            let t = std::time::Instant::now();
            let img = self.page_thumb(&uuid, i, tw, th);
            if env_flag("RM_TIMING") {
                eprintln!("[ui] 总览缩略图 第{}页 {}ms", i + 1, t.elapsed().as_millis());
            }
            self.page_thumbs_cache.push(((uuid.clone(), i), fp, img.clone()));
            thumbs.push((i, img));
        }
        while self.page_thumbs_cache.len() > 48 {
            self.page_thumbs_cache.remove(0);
        }
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.overview = true;
            nb.page_thumbs = Some(thumbs);
            nb.page_count = total;
            nb.page_index = cur.min(total.saturating_sub(1));
        }
        self.draw_overview();
    }

    fn close_overview(&mut self) {
        // === P10 begin ===
        // 关总览 = 退出转文字流程（结果页已经落盘，不用再留 Undo 状态）
        self.conv.reset();
        // === P10 end ===
        self.pagesel.close();
        self.move_to = None;
        // === P9 begin ===
        // 标签筛选不跨出总览：重进时从 `All pages` 开始；跳页输入也不会留到下一屏
        self.overview_tag = None;
        self.goto_page = None;
        // === P9 end ===
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.overview = false;
        }
        self.redraw_page();
    }

    // === P9 begin ===
    // ------------------------------------------------------------ 总览增强

    /// 当前文档的页标签：按 `page_ids()` 的页序对齐成 `(pageId, 标签)`，再从
    /// 里面收集这一份文档用到的标签名并量好文字宽（胶囊宽度要跟文字对齐）。
    fn overview_tags(&mut self) -> (Vec<(String, Vec<String>)>, Vec<(String, i32)>) {
        let (ids, raw) = match &self.screen {
            Screen::Notebook(nb) => (
                self.library.page_ids(&nb.uuid),
                self.library.page_tags(&nb.uuid),
            ),
            _ => (Vec::new(), Vec::new()),
        };
        let mut aligned: Vec<(String, Vec<String>)> =
            ids.iter().map(|id| (id.clone(), Vec::new())).collect();
        for (pid, tags) in &raw {
            if let Some(i) = ids.iter().position(|x| x == pid) {
                aligned[i].1 = tags.clone();
            }
        }
        let mut names: Vec<String> = Vec::new();
        for (_, tags) in &aligned {
            for t in tags {
                if !names.iter().any(|n| n == t) {
                    names.push(t.clone());
                }
            }
        }
        let mut filters = Vec::with_capacity(names.len());
        for name in names {
            let tw = self
                .text
                .as_mut()
                .map(|t| t.width_bold(28.0, &name) as i32)
                .unwrap_or(120);
            filters.push((name, tw));
        }
        (aligned, filters)
    }

    /// 顶栏的两颗文字按钮：`Go to page` 和 `Grid view` / `List view`。
    /// 几何来自 `panels::pagesel::PageSel::chrome_hits`。
    fn paint_overview_top(&mut self, w: i32, h: i32, filters: &[(String, i32)]) {
        for (r, act) in self.pagesel.chrome_hits(w, h, filters) {
            let (label, id) = match act {
                pagesel::Act::GoToPage => ("Go to page", A_P9_GO),
                pagesel::Act::ToggleView => (self.page_view.label(), A_P9_VIEW),
                // 胶囊在底下那一条，另外画
                _ => continue,
            };
            if let Some(t) = self.text.as_mut() {
                let tw = t.width_bold(pagesel::TOP_BTN_SIZE, label) as i32;
                t.draw_bold(
                    &mut self.fb,
                    r.0 + (r.2 - tw) / 2,
                    74,
                    pagesel::TOP_BTN_SIZE,
                    label,
                    20,
                );
            }
            self.hits.push(r.0, r.1, r.2, r.3, id);
        }
    }

    /// 列表视图：一行一页，`Page N` + 这一页的标签胶囊（官方 `PagesListItem`）。
    fn paint_overview_list(
        &mut self,
        w: i32,
        h: i32,
        shown: &[usize],
        aligned: &[(String, Vec<String>)],
        sel: bool,
        cur: usize,
    ) {
        let rows = pagesel::list_rows(w, h, shown.len());
        for (n, idx) in shown.iter().enumerate() {
            let Some(r) = rows.get(n).copied() else {
                break;
            };
            // 行底那条 2px 分隔线
            self.fb.fill_rect(r.0, r.1 + r.3 - 2, r.2, 2, gray565(210));
            // 选中 / 当前页：黑框（官方 PagesListItem 的选中态）
            if sel && self.pagesel.is_selected(*idx) {
                widgets::outline_rect(&mut self.fb, r.0 + 2, r.1 + 2, r.2 - 4, r.3 - 6, 4, 0);
            } else if !sel && *idx == cur {
                widgets::outline_rect(&mut self.fb, r.0 + 2, r.1 + 2, r.2 - 4, r.3 - 6, 2, 0);
            }
            let label = format!("Page {}", idx + 1);
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, r.0 + 20, r.1 + 78, 34.0, &label, 20);
            }
            // 右边挂这一页的标签，从右往左排，让开左边的页码
            let tags = aligned
                .get(*idx)
                .map(|(_, ts)| ts.as_slice())
                .unwrap_or(&[]);
            let mut x = r.0 + r.2 - 20;
            for name in tags.iter().rev() {
                let tw = self
                    .text
                    .as_mut()
                    .map(|t| t.width_bold(22.0, name) as i32)
                    .unwrap_or(0);
                let pw = tw + 28;
                if x - pw < r.0 + 260 {
                    break;
                }
                self.p9_pill(x - pw, r.1 + (r.3 - 36) / 2, pw, 36, name, 22.0, false);
                x -= pw + 10;
            }
            self.hits.push(r.0, r.1, r.2, r.3, A_PAGE0 + *idx as u32);
        }
    }

    /// 一颗标签胶囊（官方 `TagPill`）：选中黑底白字，没选中只描边。
    fn p9_pill(&mut self, x: i32, y: i32, w: i32, h: i32, label: &str, size: f32, on: bool) {
        if on {
            Self::p9_fill_capsule(&mut self.fb, x, y, w, h, 0);
        } else {
            // 描边胶囊先铺白底：最底下那条会压在越界的缩略图上，
            // 不铺白底的话图会从胶囊里透出来
            Self::p9_fill_capsule(&mut self.fb, x, y, w, h, 20);
            Self::p9_fill_capsule(&mut self.fb, x + 2, y + 2, w - 4, h - 4, 255);
        }
        if let Some(t) = self.text.as_mut() {
            let tw = t.width_bold(size, label) as i32;
            let base = y + h / 2 + (size * 0.35) as i32;
            t.draw_bold(
                &mut self.fb,
                x + (w - tw) / 2,
                base,
                size,
                label,
                if on { 250 } else { 20 },
            );
        }
    }

    /// 实心胶囊（选中态）：两头半圆 + 中间矩形（官方 `antialiasing: false` 硬边）。
    fn p9_fill_capsule(fb: &mut Fb, x: i32, y: i32, w: i32, h: i32, tone: u8) {
        let r = h / 2;
        if w <= h {
            widgets::disc(fb, x + w / 2, y + r, w / 2, tone);
            return;
        }
        widgets::disc(fb, x + r, y + r, r, tone);
        widgets::disc(fb, x + w - r, y + r, r, tone);
        fb.fill_rect(x + r, y, w - 2 * r, h, gray565(tone));
    }

    /// 底部的标签胶囊行（官方 `TagFilter`）：点一颗过滤、再点一下取消。
    fn paint_overview_tags(&mut self, w: i32, h: i32, filters: &[(String, i32)]) {
        let sel = self.overview_tag.clone();
        for (r, act) in self.pagesel.chrome_hits(w, h, filters) {
            let pagesel::Act::TagFilter(i) = act else {
                continue;
            };
            let Some((name, _)) = filters.get(i) else {
                continue;
            };
            let on = sel.as_deref() == Some(name.as_str());
            let name = name.clone();
            self.p9_pill(r.0, r.1, r.2, r.3, &name, 28.0, on);
            self.hits.push(r.0, r.1, r.2, r.3, A_P9_TAG0 + i as u32);
        }
    }

    /// 点了一颗胶囊：没选过就选它，已经是它就取消。
    fn toggle_overview_tag(&mut self, i: usize) {
        let (_, filters) = self.overview_tags();
        let Some((name, _)) = filters.get(i) else {
            return;
        };
        let name = name.clone();
        self.overview_tag = if self.overview_tag.as_deref() == Some(name.as_str()) {
            None
        } else {
            Some(name)
        };
        self.draw_overview();
    }

    /// `Select all`（标签筛选开着时）：只选当前显示出来的页。
    fn select_shown_pages(&mut self) {
        let (aligned, _) = self.overview_tags();
        let shown = pagesel::filter_pages(&aligned, self.overview_tag.as_deref());
        self.pagesel.select_only(&shown);
    }

    /// 顶栏 `Go to page`：整屏数字输入（复用 Move to page 那套键盘）。
    fn open_goto_page(&mut self) {
        let mut kb = Keyboard::new();
        // 数字在符号层第一行，直接翻过去
        kb.layer = Layer::Symbols;
        self.goto_page = Some(kb);
        self.banner = None;
        self.paint_goto_page();
    }

    fn paint_goto_page(&mut self) {
        let Some(kb) = self.goto_page.clone() else {
            return;
        };
        let (w, h) = (self.fb.width, self.fb.height);
        let n = match &self.screen {
            Screen::Notebook(nb) => nb.page_count,
            _ => 0,
        };
        self.fb.fill_rect(0, 0, w, h, gray565(255));
        self.fb.fill_rect(0, 0, w, 2, gray565(180));
        self.hits.clear();
        let (ix, iy, iw, ih) = (NAV_MARGIN, 140, w - 2 * NAV_MARGIN, 84);
        if let Some(t) = self.text.as_mut() {
            t.draw(&mut self.fb, NAV_MARGIN, 100, 44.0, "Go to page", 20);
            // 官方 `Page (%1 - %2)`
            let hint = format!("Page (1 - {})", n.max(1));
            let hw = t.width(30.0, &hint) as i32;
            t.draw(&mut self.fb, w - NAV_MARGIN - hw, 100, 30.0, &hint, 120);
        }
        self.fb.fill_rect(ix, iy, iw, ih, gray565(238));
        if let Some(t) = self.text.as_mut() {
            let shown = t.ellipsize(34.0, &kb.text, iw - 240);
            t.draw(&mut self.fb, ix + 20, iy + 58, 34.0, &shown, 20);
        }
        if let Some(msg) = self.banner.clone() {
            self.fb.fill_rect(0, iy + ih + 12, w, 84, gray565(30));
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, NAV_MARGIN, iy + ih + 68, 32.0, &msg, 245);
            }
        }
        let ky = h - kb.height() - 170;
        self.kb_hits = kb.draw(&mut self.fb, &mut self.text, 0, ky, w);
        let by = ky - 110;
        let bw = (w - 2 * NAV_MARGIN - 20) / 2;
        self.fb.fill_rect(NAV_MARGIN, by, bw, 90, gray565(210));
        self.fb.fill_rect(NAV_MARGIN + bw + 20, by, bw, 90, gray565(40));
        if let Some(t) = self.text.as_mut() {
            t.draw(&mut self.fb, NAV_MARGIN + 100, by + 60, 34.0, "Cancel", 20);
            t.draw(&mut self.fb, NAV_MARGIN + bw + 120, by + 60, 34.0, "OK", 250);
        }
        self.hits.push(NAV_MARGIN, by, bw, 90, A_P9_GO_CANCEL);
        self.hits.push(NAV_MARGIN + bw + 20, by, bw, 90, A_P9_GO_OK);
        self.fb.refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
    }

    fn tap_goto_page(&mut self, x: f32, y: f32) {
        for (r, k) in self.kb_hits.clone() {
            let (xi, yi) = (x as i32, y as i32);
            if xi >= r.0 && xi < r.0 + r.2 && yi >= r.1 && yi < r.1 + r.3 {
                match k {
                    Key::Enter => {
                        self.commit_goto_page();
                        return;
                    }
                    // 页号只收数字
                    Key::Ch(c) if c.is_ascii_digit() => {}
                    Key::Backspace => {}
                    _ => return,
                }
                if let Some(kb) = self.goto_page.as_mut() {
                    if kb.text.len() < 4 {
                        kb.press(k);
                    }
                }
                self.paint_goto_page();
                return;
            }
        }
        match self.hits.take(x, y) {
            Some(A_P9_GO_CANCEL) => {
                self.goto_page = None;
                self.banner = None;
                self.draw_overview();
            }
            Some(A_P9_GO_OK) => self.commit_goto_page(),
            _ => {}
        }
    }

    /// `OK` / 键盘 Enter：页号合法就跳过去并关掉总览，不合法只提示范围
    /// （官方输入框挂着 `IntValidator(1..pageCount)`，面板不关）。
    fn commit_goto_page(&mut self) {
        let Some(kb) = self.goto_page.clone() else {
            return;
        };
        let n = match &self.screen {
            Screen::Notebook(nb) => nb.page_count,
            _ => return,
        };
        let Some(page) = pagesel::parse_page(&kb.text, n) else {
            self.banner = Some(format!("Page (1 - {})", n.max(1)));
            self.paint_goto_page();
            return;
        };
        eprintln!("[pages] Go to page {page} / {n}");
        self.goto_page = None;
        self.banner = None;
        self.overview_goto(page - 1);
    }

    /// 跳到第 `idx` 页并关掉总览（点缩略图走的那条路，`Go to page` 复用）。
    fn overview_goto(&mut self, idx: usize) {
        let uuid = match &self.screen {
            Screen::Notebook(nb) => nb.uuid.clone(),
            _ => return,
        };
        let page = self.library.load_page_at(&uuid, idx);
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.page = page;
            nb.page_index = idx;
            nb.page_count = self.library.page_ids(&uuid).len().max(1);
            nb.overview = false;
            nb.active = None;
        }
        self.redraw_page();
    }
    // === P9 end ===

    fn draw_overview(&mut self) {
        let (w, h) = (self.fb.width, self.fb.height);
        // 多选的二级浮层（Add tag 键盘 / Move 数字输入）是整屏的，盖掉总览
        if matches!(&self.screen, Screen::Notebook(nb) if nb.tag_edit.is_some()) {
            self.paint_tag_editor();
            self.fb.refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
            return;
        }
        if self.move_to.is_some() {
            self.paint_move_to();
            return;
        }
        // === P9 begin ===
        if self.goto_page.is_some() {
            self.paint_goto_page();
            return;
        }
        // === P9 end ===
        let Some((cur, total)) = (match &self.screen {
            Screen::Notebook(nb) => Some((nb.page_index, nb.page_count)),
            _ => None,
        }) else {
            return;
        };
        let thumbs = match &self.screen {
            Screen::Notebook(nb) => nb.page_thumbs.clone().unwrap_or_default(),
            _ => Vec::new(),
        };
        let sel_open = self.pagesel.is_open();
        self.fb.fill_rect(0, 0, w, h, gray565(255));
        self.hits.clear();

        // 顶部：返回 + 标题（多选时右边还有 Select all / Select none，在 paint_page_select 里）
        widgets::back_arrow(&mut self.fb, 46, 72, 12, 20);
        if let Some(t) = self.text.as_mut() {
            t.draw(&mut self.fb, 91, 72, 36.0, "Pages", 20);
        }
        self.hits.push(0, 0, 220, 120, A_OVERVIEW_BACK);
        // 顶部右侧：`Table of contents`（官方 `res_067dfe1.qml:160` 那一行）。
        // 字号、右边距照左边的 `Pages` 标题；点它开整页目录。
        let toc_tw = self
            .text
            .as_mut()
            .map(|t| t.width(36.0, "Table of contents") as i32)
            .unwrap_or(360);
        let toc_x = w - 33 - toc_tw;
        // 多选态右边是 `Select all` / `Select none`，两个都右对齐会叠在一起：
        // 官方多选时这一行动作收进底部动作条，所以这里直接不画也不给命中区。
        if !sel_open {
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, toc_x, 72, 36.0, "Table of contents", 20);
            }
            self.hits
                .push(toc_x - 20, 0, w - toc_x + 20, 118, A_OVERVIEW_TOC);
        }
        // === P9 begin ===
        // 这一份文档的页标签（按页序对齐）+ 标签胶囊（名字 + 文字宽）
        let (aligned, filters) = self.overview_tags();
        // 要显示的页号（升序）；没筛标签就是全部页
        let shown = pagesel::filter_pages(&aligned, self.overview_tag.as_deref());
        if !sel_open {
            self.paint_overview_top(w, h, &filters);
        }
        // 列表视图：底下的网格那段照着 `thumbs` 画，这里清空让它空跑，
        // 行由 `paint_overview_list` 画（原来的网格循环一行都不动）
        let list_mode = self.page_view == pagesel::View::List;
        if list_mode {
            self.paint_overview_list(w, h, &shown, &aligned, sel_open, cur);
        }
        let thumbs: Vec<(usize, Vec<u8>)> = if list_mode {
            Vec::new()
        } else {
            thumbs
                .into_iter()
                .filter(|(i, _)| shown.contains(i))
                .collect()
        };
        // === P9 end ===
        self.fb.fill_rect(0, 118, w, 2, gray565(20));

        // 网格：cell 342×510，4 列，外边距 15，item 再缩 15
        let (cell_w, cell_h) = (342, 510);
        let cols = 4;
        let mut y = 140;
        // 多选时底部 120 是动作条，那一行缩略图不画（也不给命中区）
        let limit = if sel_open {
            h - pagesel::BAR_H - 392 - 70
        } else {
            h - 120
        };
        for (n, (idx, img)) in thumbs.iter().enumerate() {
            let col = (n % cols) as i32;
            let row = (n / cols) as i32;
            let cx = 15 + col * cell_w + 15;
            let cy = y + row * cell_h + 15;
            if cy > limit {
                break;
            }
            let (tw, th) = (306, 392);
            for yy in 0..th {
                for xx in 0..tw {
                    self.fb
                        .put(cx + xx, cy + yy, gray565(img[(yy * tw + xx) as usize]));
                }
            }
            widgets::outline_rect(&mut self.fb, cx, cy, tw, th, 1, 60);
            if sel_open && self.pagesel.is_selected(*idx) {
                // 选中：加粗黑框 + 右上角黑圆白勾（官方 PagesGridItem 的选中态）
                widgets::outline_rect(&mut self.fb, cx - 4, cy - 4, tw + 8, th + 8, 4, 0);
                let (dx, dy) = (cx + tw - 36, cy + 36);
                widgets::disc(&mut self.fb, dx, dy, 32, 0);
                widgets::line(&mut self.fb, dx - 15, dy, dx - 4, dy + 11, 5, 250);
                widgets::line(&mut self.fb, dx - 4, dy + 11, dx + 16, dy - 12, 5, 250);
            } else if *idx == cur && !sel_open {
                widgets::outline_rect(&mut self.fb, cx - 4, cy - 4, tw + 8, th + 8, 4, 0);
            }
            if let Some(t) = self.text.as_mut() {
                let label = format!("{}", idx + 1);
                let lw = t.width(26.0, &label) as i32;
                t.draw(&mut self.fb, cx + (tw - lw) / 2, cy + th + 40, 26.0, &label, 20);
            }
            self.hits.push(cx, cy, tw, th + 60, A_PAGE0 + *idx as u32);
        }

        // 多选时底条盖住右下角，「加一页」不画也不吃点击
        if !sel_open {
            let bx = w - 40 - 200;
            let by = h - 40 - 100;
            self.fb.fill_rect(bx, by, 200, 100, gray565(30));
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, bx + 30, by + 62, 30.0, "+ Page", 250);
            }
            self.hits.push(bx, by, 200, 100, A_ADDPAGE);
        }

        // === P9 begin ===
        // 底部标签胶囊（官方 `TagFilter`）：盖在缩略图上面，点一颗过滤
        if !sel_open {
            self.paint_overview_tags(w, h, &filters);
        }
        // 官方 `noTagsHeader`：筛出来一页都没有
        if self.overview_tag.is_some() && shown.is_empty() {
            let msg = "You have no content with this combination of tags.\nSelect fewer tags to find what you’re looking for.";
            for (i, line) in msg.split('\n').enumerate() {
                if let Some(t) = self.text.as_mut() {
                    let lw = t.width(32.0, line) as i32;
                    t.draw(&mut self.fb, (w - lw) / 2, 620 + i as i32 * 56, 32.0, line, 120);
                }
            }
        }
        // === P9 end ===

        // 多选：顶栏按钮 + 底部动作条（或删除确认框）
        self.paint_page_select();

        if let Some(msg) = self.banner.clone() {
            let bh = 100;
            self.fb.fill_rect(0, h - bh - 140, w, bh, gray565(30));
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, NAV_MARGIN, h - 76 - 140, 32.0, &msg, 245);
            }
        }
        // === P10 begin ===
        // 转文字的选页头部 / 底部状态条画在最上面
        self.paint_convert_overlay();
        // === P10 end ===
        let _ = total;
        self.fb.refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
    }

    /// 多选态的两块 UI：顶栏 `Select all` / `Select none`，底部动作条 /
    /// 删除确认框。几何全在 `panels::pagesel` 里（`hits()`），这里只照着画。
    fn paint_page_select(&mut self) {
        if !self.pagesel.is_open() {
            return;
        }
        let (w, h) = (self.fb.width, self.fb.height);
        if self.pagesel.confirming() {
            self.fb.dim(0, 0, w, h, gray565(140), 1, 2);
            let (bx, by, bw, bh) = PageSel::delete_dialog(w, h);
            self.fb.fill_rect(bx, by, bw, bh, gray565(255));
            widgets::outline_rect(&mut self.fb, bx, by, bw, bh, 2, 20);
            let (bw2, bh2) = (380, 88);
            let y2 = by + bh - 150;
            self.fb.fill_rect(bx + 48, y2, bw2, bh2, gray565(235));
            self.fb.fill_rect(bx + bw - 48 - bw2, y2, bw2, bh2, gray565(20));
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, bx + 48, by + 90, 34.0, "Delete page permanently?", 20);
                t.draw(&mut self.fb, bx + 48 + 120, y2 + 58, 32.0, "Cancel", 20);
                t.draw(&mut self.fb, bx + bw - 48 - bw2 + 130, y2 + 58, 32.0, "Delete", 250);
            }
            return;
        }
        // 顶栏右边那个下划线按钮：全选前是 `Select all`，全选后是 `Select none`
        if let Some((r, act)) = self
            .pagesel
            .hits(w, h)
            .into_iter()
            .find(|(_, a)| matches!(a, pagesel::Act::All | pagesel::Act::None))
        {
            let label = if act == pagesel::Act::None {
                "Select none"
            } else {
                "Select all"
            };
            if let Some(t) = self.text.as_mut() {
                let tw = t.width_bold(30.0, label) as i32;
                let tx = r.0 + r.2 - tw;
                t.draw_bold(&mut self.fb, tx, 74, 30.0, label, 20);
                // 官方那行是下划线文字按钮（`TextHeaderSmall` + `font.underline`）
                self.fb.fill_rect(tx, 82, tw, 2, gray565(20));
            }
        }
        // 动作条：5 项等分。选 0 页时 Tag/Move/Delete 画灰（命中区那边也不会给）
        let by = h - pagesel::BAR_H;
        self.fb.fill_rect(0, by, w, pagesel::BAR_H, gray565(0));
        let count = self.pagesel.count();
        let items: [(&str, bool); 5] = [
            ("Tag", count > 0),
            ("Move", count > 0),
            ("Delete", count > 0),
            ("Duplicate", true),
            ("Cancel", true),
        ];
        for (i, (label, on)) in items.iter().enumerate() {
            let x0 = w * i as i32 / 5;
            let x1 = w * (i as i32 + 1) / 5;
            if let Some(t) = self.text.as_mut() {
                let tw = t.width_bold(32.0, label) as i32;
                t.draw_bold(
                    &mut self.fb,
                    x0 + (x1 - x0 - tw) / 2,
                    by + 76,
                    32.0,
                    label,
                    if *on { 250 } else { 120 },
                );
            }
        }
    }

    /// 总览里长按落在哪一页上 → 进入多选并选中该页（替代原来的单页
    /// Duplicate/Delete 菜单）。返回 true = 这次长按被吃掉了。
    fn open_page_select(&mut self, x: f32, y: f32) -> bool {
        // === P9 begin ===
        // `Go to page` 的输入页开着时，长按不算选页（那是键盘上的长按）
        if self.goto_page.is_some() {
            return false;
        }
        // === P9 end ===
        // 二级浮层开着时长按不算（那是键盘上的长按）
        if self.move_to.is_some()
            || matches!(&self.screen, Screen::Notebook(nb) if nb.tag_edit.is_some())
        {
            return false;
        }
        let Some(a) = self.hits.take(x, y) else {
            return false;
        };
        if !(A_PAGE0..A_PAGE0 + 100).contains(&a) {
            return false;
        }
        let idx = (a - A_PAGE0) as usize;
        // 已经在多选里：长按 = 再点一下（切换选中），别把已有选择清掉
        if self.pagesel.is_open() && !self.pagesel.confirming() {
            self.pagesel.toggle(idx);
            self.draw_overview();
            return true;
        }
        let total = match &self.screen {
            Screen::Notebook(nb) => nb.page_count,
            _ => return false,
        };
        self.pagesel.show(total, idx);
        eprintln!("[pages] 进入多选，选中第 {} 页", idx + 1);
        self.draw_overview();
        true
    }

    /// 多选动作条 / 删除确认框里点了一下。
    fn page_select_action(&mut self, act: pagesel::Act) {
        match act {
            pagesel::Act::All => {
                // === P9 begin ===
                // 标签筛选开着时只选**看得见**的页（官方 `setPageNumbers(model)`），
                // 否则右键式的 Delete 会连带删掉被筛掉的页
                if self.overview_tag.is_some() {
                    self.select_shown_pages();
                } else {
                    self.pagesel.select_all();
                }
                // === P9 end ===
                self.draw_overview();
            }
            pagesel::Act::None => {
                self.pagesel.select_none();
                self.draw_overview();
            }
            pagesel::Act::Toggle(i) => {
                self.pagesel.toggle(i);
                self.draw_overview();
            }
            pagesel::Act::Close => {
                self.pagesel.close();
                self.draw_overview();
            }
            pagesel::Act::Tag => self.open_page_tag(),
            pagesel::Act::Move => self.open_move_to(),
            pagesel::Act::Delete => {
                self.pagesel.ask_delete();
                self.draw_overview();
            }
            pagesel::Act::DeleteCancel => {
                self.pagesel.cancel_delete();
                self.draw_overview();
            }
            pagesel::Act::DeleteConfirm => self.delete_selected_pages(),
            pagesel::Act::Duplicate => self.duplicate_selected_pages(),
            // === P9 begin ===
            // 顶栏 / 胶囊的动作号在多选态收起时不会走到这里（几何在 `chrome_hits`）
            pagesel::Act::GoToPage | pagesel::Act::ToggleView | pagesel::Act::TagFilter(_) => {}
            // === P9 end ===
        }
    }

    /// 多选 → Tag：复用 "Add tag" 的键盘，确认时把标签打到选中的页上
    /// （全部页都已经有这个标签 = 去掉，见 `commit_tag`）。
    fn open_page_tag(&mut self) {
        if self.pagesel.count() == 0 {
            return;
        }
        self.pagesel.set_tagging(true);
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.tag_edit = Some(Keyboard::new());
        }
        self.draw_overview();
    }

    /// 多选 → Move 的二级页：`Move to page` + 数字输入（键盘贴底）。
    fn open_move_to(&mut self) {
        if self.pagesel.count() == 0 {
            return;
        }
        let mut kb = Keyboard::new();
        // 数字在符号层第一行，直接翻过去，省得用户再点一次 123
        kb.layer = Layer::Symbols;
        self.move_to = Some(kb);
        self.banner = None;
        self.paint_move_to();
    }

    fn paint_move_to(&mut self) {
        let Some(kb) = self.move_to.clone() else {
            return;
        };
        let (w, h) = (self.fb.width, self.fb.height);
        let n = match &self.screen {
            Screen::Notebook(nb) => nb.page_count,
            _ => 0,
        };
        self.fb.fill_rect(0, 0, w, h, gray565(255));
        self.fb.fill_rect(0, 0, w, 2, gray565(180));
        self.hits.clear();
        let (ix, iy, iw, ih) = (NAV_MARGIN, 140, w - 2 * NAV_MARGIN, 84);
        if let Some(t) = self.text.as_mut() {
            t.draw(&mut self.fb, NAV_MARGIN, 100, 44.0, "Move to page", 20);
            // 官方 `Header` 里输入框旁边就是 `Page (%1 - %2)`
            let hint = format!("Page (1 - {})", n.max(1));
            let hw = t.width(30.0, &hint) as i32;
            t.draw(&mut self.fb, w - NAV_MARGIN - hw, 100, 30.0, &hint, 120);
        }
        self.fb.fill_rect(ix, iy, iw, ih, gray565(238));
        if let Some(t) = self.text.as_mut() {
            let shown = t.ellipsize(34.0, &kb.text, iw - 240);
            t.draw(&mut self.fb, ix + 20, iy + 58, 34.0, &shown, 20);
        }
        if let Some(msg) = self.banner.clone() {
            self.fb.fill_rect(0, iy + ih + 12, w, 84, gray565(30));
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, NAV_MARGIN, iy + ih + 68, 32.0, &msg, 245);
            }
        }
        let ky = h - kb.height() - 170;
        self.kb_hits = kb.draw(&mut self.fb, &mut self.text, 0, ky, w);
        let by = ky - 110;
        let bw = (w - 2 * NAV_MARGIN - 20) / 2;
        self.fb.fill_rect(NAV_MARGIN, by, bw, 90, gray565(210));
        self.fb.fill_rect(NAV_MARGIN + bw + 20, by, bw, 90, gray565(40));
        if let Some(t) = self.text.as_mut() {
            t.draw(&mut self.fb, NAV_MARGIN + 100, by + 60, 34.0, "Cancel", 20);
            t.draw(&mut self.fb, NAV_MARGIN + bw + 120, by + 60, 34.0, "OK", 250);
        }
        self.hits.push(NAV_MARGIN, by, bw, 90, A_MV_CANCEL);
        self.hits.push(NAV_MARGIN + bw + 20, by, bw, 90, A_MV_OK);
        self.fb.refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
    }

    fn tap_move_to(&mut self, x: f32, y: f32) {
        for (r, k) in self.kb_hits.clone() {
            let (xi, yi) = (x as i32, y as i32);
            if xi >= r.0 && xi < r.0 + r.2 && yi >= r.1 && yi < r.1 + r.3 {
                match k {
                    Key::Enter => {
                        self.commit_move_to();
                        return;
                    }
                    // 目标页号只收数字（符号层数字在第一行）
                    Key::Ch(c) if c.is_ascii_digit() => {}
                    Key::Backspace => {}
                    _ => return,
                }
                if let Some(kb) = self.move_to.as_mut() {
                    if kb.text.len() < 4 {
                        kb.press(k);
                    }
                }
                self.paint_move_to();
                return;
            }
        }
        match self.hits.take(x, y) {
            Some(A_MV_CANCEL) => {
                self.move_to = None;
                self.draw_overview();
            }
            Some(A_MV_OK) => self.commit_move_to(),
            _ => {}
        }
    }

    fn commit_move_to(&mut self) {
        let Some(kb) = self.move_to.clone() else {
            return;
        };
        let (uuid, n) = match &self.screen {
            Screen::Notebook(nb) => (nb.uuid.clone(), nb.page_count),
            _ => return,
        };
        let page = kb
            .text
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|p| *p >= 1 && *p <= n);
        let Some(page) = page else {
            // 官方输入框带 `IntValidator(1..pageCount)`：越界就给范围提示，不关面板
            self.banner = Some(format!("Page (1 - {})", n.max(1)));
            self.paint_move_to();
            return;
        };
        let plan = self.pagesel.move_plan(page - 1);
        let mut steps = 0;
        for (from, to) in plan {
            if let Err(e) = self.library.move_page(&uuid, from, to) {
                eprintln!("[pages] 移动第 {} 页 → {} 失败：{e}", from + 1, to + 1);
                break;
            }
            steps += 1;
        }
        eprintln!(
            "[pages] 移动 {} 页到第 {page} 页（{steps} 步）",
            self.pagesel.count()
        );
        self.move_to = None;
        self.banner = None;
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.page_index = (page - 1).min(nb.page_count.saturating_sub(1));
        }
        self.pagesel.close();
        self.pages_changed();
    }

    /// 多选 → Delete：确认后删掉选中的页（`.content` 里倒序删，`.rm` 跟着删）。
    fn delete_selected_pages(&mut self) {
        let uuid = match &self.screen {
            Screen::Notebook(nb) => nb.uuid.clone(),
            _ => return,
        };
        let sel = self.pagesel.selected();
        if sel.is_empty() {
            self.pagesel.close();
            self.draw_overview();
            return;
        }
        match self.library.delete_pages(&uuid, &sel) {
            Ok(n) => eprintln!(
                "[pages] 删除 {n} 页（剩 {} 页）",
                self.library.page_ids(&uuid).len()
            ),
            Err(e) => {
                eprintln!("[pages] 删页失败：{e}");
                self.banner = Some(format!("Delete failed: {e}"));
                self.pagesel.close();
                self.draw_overview();
                return;
            }
        }
        self.pagesel.close();
        self.pages_changed();
    }

    /// 多选 → Duplicate：倒序复制（每次插在本页后面，前面的下标不会错位）。
    fn duplicate_selected_pages(&mut self) {
        let uuid = match &self.screen {
            Screen::Notebook(nb) => nb.uuid.clone(),
            _ => return,
        };
        let sel = self.pagesel.selected();
        if sel.is_empty() {
            return;
        }
        let mut n = 0;
        for &i in sel.iter().rev() {
            match self.library.duplicate_page(&uuid, i) {
                Ok(_) => n += 1,
                Err(e) => {
                    eprintln!("[pages] 复制第 {} 页失败：{e}", i + 1);
                    self.banner = Some(format!("Duplicate failed: {e}"));
                    break;
                }
            }
        }
        eprintln!("[pages] 复制 {n} 页");
        self.pagesel.close();
        self.pages_changed();
    }

    /// 页序 / 页数变了之后，把当前页和总览都重新对一遍。
    ///
    /// 笔记本（`.rm`）走轻量路径：重读当前页 + 重进总览；PDF/电子书页序动了
    /// 底图也得按新页号重渲，干脆重新打开一次。
    fn pages_changed(&mut self) {
        let (uuid, idx) = match &self.screen {
            Screen::Notebook(nb) => (nb.uuid.clone(), nb.page_index),
            _ => return,
        };
        let n = self.library.page_ids(&uuid).len();
        let is_rm = self
            .library
            .find(&uuid)
            .map(|b| b.format == rmdev::doc::Format::Rm)
            .unwrap_or(true);
        if !is_rm {
            self.open_at(&uuid, idx.min(n.saturating_sub(1)));
            self.open_overview();
            return;
        }
        let idx = if n == 0 { 0 } else { idx.min(n - 1) };
        let page = if n == 0 {
            RmPage::default()
        } else {
            self.library.load_page_at(&uuid, idx)
        };
        let texts = if n == 0 {
            Vec::new()
        } else {
            // === P11 begin ===
            self.library.load_scene_texts(&uuid, idx)
            // === P11 end ===
        };
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.page_count = n;
            nb.page_index = idx;
            nb.page = page;
            // === P11 begin ===
            nb.texts = texts.iter().map(|t| t.to_item()).collect();
            nb.p11_styles = texts.iter().map(|t| t.style).collect();
            // === P11 end ===
            nb.dirty = false;
            nb.active = None;
            nb.text_edit = None;
            nb.clear_sel();
            nb.undo.clear();
            nb.redo.clear();
        }
        self.open_overview();
    }

    fn tap_overview(&mut self, x: f32, y: f32) {
        let (w, h) = (self.fb.width, self.fb.height);
        // === P10 begin ===
        // 转文字：选页头部 / 状态条 / 缩略图切换选中都在它自己那套里
        if self.tap_convert(x, y) {
            return;
        }
        // === P10 end ===
        // 二级浮层优先：Add tag 键盘 / Move 数字输入（都是整屏）
        if matches!(&self.screen, Screen::Notebook(nb) if nb.tag_edit.is_some()) {
            self.tap_tag_editor(x, y);
            return;
        }
        if self.move_to.is_some() {
            self.tap_move_to(x, y);
            return;
        }
        // === P9 begin ===
        // `Go to page` 的整屏输入在最上层
        if self.goto_page.is_some() {
            self.tap_goto_page(x, y);
            return;
        }
        // === P9 end ===
        // 多选态：面板命中 → 动作；点缩略图 = 切换选中；点空白不穿透
        if self.pagesel.is_open() {
            if let Some(act) = self.pagesel.hit(x, y, w, h) {
                self.page_select_action(act);
                return;
            }
            if !self.pagesel.confirming() {
                // 顶栏返回箭头 = 退出多选（官方 rejectText 选中时是 `Cancel`）
                if x < 220.0 && y < 120.0 {
                    self.pagesel.close();
                    self.draw_overview();
                    return;
                }
                if let Some(a) = self.hits.take(x, y) {
                    if (A_PAGE0..A_PAGE0 + 100).contains(&a) {
                        self.pagesel.toggle((a - A_PAGE0) as usize);
                        self.draw_overview();
                    }
                }
            }
            return;
        }
        match self.hits.take(x, y) {
            Some(A_OVERVIEW_BACK) => self.close_overview(),
            Some(A_OVERVIEW_TOC) => self.open_toc(),
            Some(A_ADDPAGE) => {
                let (uuid, cur) = match &self.screen {
                    Screen::Notebook(nb) => (nb.uuid.clone(), nb.page_index),
                    _ => return,
                };
                match self.library.add_page(&uuid, cur) {
                    Ok(at) => {
                        if let Screen::Notebook(nb) = &mut self.screen {
                            nb.page_count = self.library.page_ids(&uuid).len().max(1);
                            nb.page_index = at;
                            nb.dirty = false;
                        }
                        eprintln!("[dev] 新增第 {at} 页");
                        self.open_overview();
                    }
                    Err(e) => {
                        self.banner = Some(format!("加页失败：{e}"));
                        self.draw_overview();
                    }
                }
            }
            Some(a) if (A_PAGE0..A_PAGE0 + 100).contains(&a) => {
                let idx = (a - A_PAGE0) as usize;
                let uuid = match &self.screen {
                    Screen::Notebook(nb) => nb.uuid.clone(),
                    _ => return,
                };
                let page = self.library.load_page_at(&uuid, idx);
                // === P10 begin ===
                // 从总览跳页也要把这一页的文字层读回来：转文字的结果页整页都是
                // 文字，不读的话点进去看到的是一张白纸（原来只换墨迹不换文字）。
                let texts = self.library.load_texts(&uuid, idx);
                // === P10 end ===
                if let Screen::Notebook(nb) = &mut self.screen {
                    nb.page = page;
                    nb.page_index = idx;
                    nb.page_count = self.library.page_ids(&uuid).len().max(1);
                    nb.overview = false;
                    nb.active = None;
                    // === P10 begin ===
                    nb.texts = texts;
                    // === P10 end ===
                }
                self.redraw_page();
            }
            // === P9 begin ===
            Some(A_P9_GO) => self.open_goto_page(),
            Some(A_P9_VIEW) => {
                self.page_view = self.page_view.toggle();
                self.draw_overview();
            }
            Some(a) if (A_P9_TAG0..A_P9_TAG0 + 20).contains(&a) => {
                self.toggle_overview_tag((a - A_P9_TAG0) as usize)
            }
            // === P9 end ===
            _ => {}
        }
    }

    // ------------------------------------------------------------ 目录（TOC）

    /// 打开 `Table of contents`。目录来自 `Doc::toc()`（只有电子书后端有；
    /// 笔记和没有标题的 TXT 是空表，面板里显示 `No table of contents`）。
    fn open_toc(&mut self) {
        let (uuid, cur) = match &self.screen {
            Screen::Notebook(nb) => (nb.uuid.clone(), nb.page_index),
            _ => return,
        };
        // `self.doc` 只在打开电子书时填；切到笔记后可能还留着上一本书，先对 uuid
        let items = if self.doc_uuid.as_deref() == Some(uuid.as_str()) {
            self.doc.as_ref().map(|d| d.toc()).unwrap_or_default()
        } else {
            Vec::new()
        };
        eprintln!("[toc] 打开目录（{} 项，当前第 {} 页）", items.len(), cur + 1);
        self.toc.show(items, cur);
        self.repaint_toc(true);
    }

    /// 目录面板的点击：点条目 → 跳页并关面板；`Cancel` → 回页总览。
    fn toc_click(&mut self, x: f32, y: f32) {
        let (w, h) = (self.fb.width, self.fb.height);
        let hit = self.toc.hits(w, h).into_iter().find(|(r, _)| {
            x >= r.0 as f32
                && y >= r.1 as f32
                && x < (r.0 + r.2) as f32
                && y < (r.1 + r.3) as f32
        });
        let Some((_, act)) = hit else {
            return;
        };
        match act {
            toc::Act::Close => {
                self.toc.close();
                self.draw_overview();
            }
            toc::Act::Pick(i) => {
                let Some(page) = self.toc.pick(i) else {
                    return;
                };
                let cur = match &self.screen {
                    Screen::Notebook(nb) => nb.page_index,
                    _ => return,
                };
                if page == cur {
                    // 点的就是当前章：`goto_page` 对同页直接返回，回总览把面板收掉
                    self.draw_overview();
                    return;
                }
                if let Screen::Notebook(nb) = &mut self.screen {
                    nb.overview = false;
                    nb.active = None;
                }
                self.goto_page(page);
            }
        }
    }

    /// 目录面板的输入：点条目 / 手指或笔上下拖滚动（每 80px 一行）。
    ///
    /// 面板是整页盖在页总览上的：开着头所有输入都归它，一个都不能漏到后面的
    /// 缩略图（漏下去就会"点了目录跳到别的页"）。
    fn toc_input(&mut self, ev: InputEvent) {
        let h = self.fb.height;
        // 0 = 按下，1 = 移动，2 = 抬起。抬笔/抬指事件不带坐标，用按下点当点击位置。
        let (phase, x, y) = match ev {
            InputEvent::Touch(TouchEvent::Down { x, y }) => (0, x, y),
            InputEvent::Touch(TouchEvent::Move { x, y }) => (1, x, y),
            InputEvent::Touch(TouchEvent::Up) => (2, 0.0, 0.0),
            InputEvent::Pen(PenEvent::Down(p)) => (0, p.x, p.y),
            InputEvent::Pen(PenEvent::Move(p)) => (1, p.x, p.y),
            InputEvent::Pen(PenEvent::Up) => (2, 0.0, 0.0),
            _ => return,
        };
        match phase {
            0 => self.toc_press = Some((x, y, false)),
            1 => {
                let Some((x0, y0, _)) = self.toc_press else {
                    return;
                };
                let dy = y - y0;
                if dy.abs() < toc::DRAG_STEP {
                    return;
                }
                // 手指往下拖 = 看前面的条目（内容跟着手走）
                let step = if dy > 0.0 { -1 } else { 1 };
                if self.toc.scroll(step, h) {
                    self.repaint_toc(false);
                }
                // 锚点跟着手走：一次拖动可以连续滚多行；`moved` 保证抬指不算点击
                self.toc_press = Some((x0, y, true));
            }
            _ => {
                let Some((x, y, moved)) = self.toc_press.take() else {
                    return;
                };
                if !moved {
                    self.toc_click(x, y);
                }
            }
        }
    }

    /// 目录整页重画。`full` = 整屏刷新（第一次打开）；滚动时只刷列表那一块，
    /// 免得拖动时一串整屏 GC16 排队把 EPDC 堵死（见 `redraw_page` 的 `FULL_GAP`）。
    fn repaint_toc(&mut self, full: bool) {
        let (w, h) = (self.fb.width, self.fb.height);
        self.paint_overlays();
        if full {
            self.fb
                .refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
        } else {
            self.fb.refresh(
                0,
                toc::LIST_TOP,
                w,
                h - toc::LIST_TOP,
                WAVEFORM_MODE_GC16,
                rmdev::fb::UPDATE_MODE_PARTIAL,
            );
        }
    }

    // ------------------------------------------------------------ Settings
    //
    // 几何全部量自官方 20-settings*.png（1404×1872 真机截图）：
    //   左栏 343 + 2px 分线；Back 行 118 高（< 尖端 x=33，22×39；文字 x=78 基线 72）
    //   tab 行 120；图标 40×40 @x=24；文字 x=79 基线 row+71
    //   右栏标题居中 x=874 基线 161
    //   内容列 x=420 宽 900：框高 90 / 2px 边框；值 x=453 基线 框顶+57；› 尖 x=1282
    //   开关行：标签基线 B，胶囊 90×45 @x=1176 顶 = B-34，滑块 d=35
    //   子页：内容 x=144..1259（左右 145），Back 比左栏整体 +9px，整屏居中 x=702

    const SET_SBW: i32 = 343;
    const SET_LEFT: i32 = 420;
    const SET_W: i32 = 900;
    const SET_H: i32 = 90;
    const SET_VALUE_X: i32 = 449;
    const SET_CHEV_X: i32 = 1282;
    const SET_PILL_X: i32 = 1176;
    const SET_PILL_W: i32 = 90;
    const SET_PILL_H: i32 = 45;
    const SET_SUB_X: i32 = 144;
    const SET_SUB_W: i32 = 1116;
    const SET_SUB_CX: i32 = 702;

    // 字号（按官方截图里的字宽/字高反解）：值 36 / 段落 32 / 标签·按钮 35 / 标题 40 / 左栏 34
    const SZ_TITLE: f32 = 38.0;
    const SZ_LABEL: f32 = 35.0;
    const SZ_VALUE: f32 = 36.0;
    const SZ_PARA: f32 = 32.0;
    const SZ_SIDE: f32 = 34.0;

    const SET_TABS: [(&'static str, fn(&mut Fb, i32, i32, u8)); 10] = [
        ("General", si_general),
        ("Wi-Fi", si_wifi),
        ("Battery", si_battery),
        ("Storage", si_storage),
        ("Security", si_lock),
        ("Language", si_globe),
        ("Accessibility", si_font),
        // === P5 begin: 官方第 8/9 页（Developer / Experimental，都在 Help 之前）===
        ("Developer", si_cogs),
        ("Experimental", si_flask),
        // === P5 end ===
        ("Help", si_help),
    ];

    /// 我方能拿到的粗体字面（NotoSans-Bold）比官方 UI 里的粗体宽约 4%，
    /// 用一点点负字距补回来，字高才对得上（官方标题 40px / 大写 30px）。
    fn set_track(_size: f32, _bold: bool) -> f32 {
        // 换成 Maison Neue 之后字距本身就和官方一致，不再做补偿
        0.0
    }

    /// 设置页的字号是照官方截图定的一套「官方字号」，换字体后要按字面宽度换算：
    /// Maison Book 比原来的 NotoSansSC 宽约 18%（正文要收小），
    /// Maison Bold 反而窄约 8%（粗体要放大）。
    ///
    /// 再乘上无障碍页的全局倍率（Normal 100 / Large 115）。
    fn set_size(size: f32, bold: bool) -> f32 {
        let base = if bold { size * 1.08 } else { size * 0.85 };
        base * SET_FONT_SCALE.load(std::sync::atomic::Ordering::Relaxed) as f32 / 100.0
    }

    /// 一行字（逐字排，粗体带负字距）。`bold` 决定字重，返回结束 x。
    fn set_text(&mut self, x: i32, base: i32, size: f32, bold: bool, msg: &str, tone: u8) -> i32 {
        let track = Self::set_track(size, bold);
        let size = Self::set_size(size, bold);
        match self.text.as_mut() {
            Some(t) => {
                t.set_bold(bold);
                let mut pen = x as f32;
                for ch in msg.chars() {
                    let mut buf = [0u8; 4];
                    let cs: &str = ch.encode_utf8(&mut buf);
                    t.draw(&mut self.fb, pen.round() as i32, base, size, cs, tone);
                    pen += t.width(size, cs) + track;
                }
                t.set_bold(false);
                (pen - track) as i32
            }
            None => x,
        }
    }

    /// 以 `cx` 居中画一行字。
    fn set_center(&mut self, cx: i32, base: i32, size: f32, bold: bool, msg: &str, tone: u8) {
        let track = Self::set_track(size, bold);
        let size = Self::set_size(size, bold);
        let n = msg.chars().count() as f32;
        let mut total = 0.0;
        if let Some(t) = self.text.as_mut() {
            t.set_bold(bold);
            for ch in msg.chars() {
                let mut buf = [0u8; 4];
                let cs: &str = ch.encode_utf8(&mut buf);
                total += t.width(size, cs);
            }
            t.set_bold(false);
        }
        total += track * (n - 1.0).max(0.0);
        // `set_text` 会再走一次 `set_size`（历史行为，别动，否则所有居中标题都会变）。
        // 这里把无障碍倍率除掉，保证全局倍率只作用一次。
        let g = SET_FONT_SCALE.load(std::sync::atomic::Ordering::Relaxed) as f32 / 100.0;
        let size = if g > 0.01 { size / g } else { size };
        self.set_text(cx - (total / 2.0) as i32, base, size, bold, msg, tone);
    }

    fn set_para(&mut self, x: i32, base: i32, msg: &str) -> i32 {
        self.set_text(x, base, Self::SZ_PARA, false, msg, 20)
    }

    /// 左栏：Back 行 + 8 个 tab。
    fn set_sidebar(&mut self, tab: usize) {
        let h = self.fb.height;
        self.fb.fill_rect(Self::SET_SBW, 0, 2, h, gray565(0));
        widgets::line(&mut self.fb, 33, 60, 54, 41, 5, 20);
        widgets::line(&mut self.fb, 33, 60, 54, 79, 5, 20);
        self.set_text(79, 73, Self::SZ_SIDE, true, "Back", 20);
        self.fb.fill_rect(0, 118, Self::SET_SBW, 2, gray565(0));
        self.hits.push(0, 0, Self::SET_SBW, 118, A_SETTINGS_BACK);
        for (i, (name, icon)) in Self::SET_TABS.iter().enumerate() {
            let y = 120 + 120 * i as i32;
            let sel = i == tab;
            if sel {
                self.fb.fill_rect(0, y, Self::SET_SBW, 120, gray565(0));
            }
            let tone = if sel { 255 } else { 20 };
            icon(&mut self.fb, 24, y + 40, tone);
            self.set_text(80, y + 72, Self::SZ_SIDE, true, name, tone);
            self.hits.push(0, y, Self::SET_SBW, 120, A_SET_TAB0 + i as u32);
        }
    }

    /// 子页的 Back 行（比主界面那行右移 9px）。
    fn set_back_sub(&mut self) {
        widgets::line(&mut self.fb, 42, 60, 63, 41, 5, 20);
        widgets::line(&mut self.fb, 42, 60, 63, 79, 5, 20);
        self.set_text(88, 73, Self::SZ_SIDE, true, "Back", 20);
        self.hits.push(0, 0, Self::SET_SBW, 118, A_S_SUBBACK);
    }

    /// 一个面板框（90 高、2px 边框）+ 框里的值 + ›。整框可点。
    fn set_box(&mut self, y: i32, value: &str, act: u32) {
        widgets::outline_rect(&mut self.fb, Self::SET_LEFT, y, Self::SET_W, Self::SET_H, 2, 30);
        self.set_text(Self::SET_VALUE_X, y + 58, Self::SZ_VALUE, false, value, 20);
        widgets::chevron_right(&mut self.fb, Self::SET_CHEV_X, y + 45, 15, 20);
        self.hits.push(Self::SET_LEFT, y, Self::SET_W, Self::SET_H, act);
    }

    /// `Off ‹胶囊› On`。`pill_y` 是胶囊顶边。
    fn set_switch(&mut self, x: i32, pill_y: i32, on: bool, tone: u8) {
        let (pw, ph) = (Self::SET_PILL_W, Self::SET_PILL_H);
        let cy = pill_y + ph / 2;
        if on {
            si_fill_capsule(&mut self.fb, x, pill_y, pw, ph, tone);
            widgets::disc(&mut self.fb, x + 67, cy, 17, 255);
        } else {
            widgets::disc(&mut self.fb, x + 22, cy, 17, tone);
        }
        si_capsule(&mut self.fb, x, pill_y, pw, ph, 2, tone);
        if let Some(t) = self.text.as_mut() {
            let wo = t.width(Self::SZ_LABEL, "Off") as i32;
            t.draw(&mut self.fb, x - 16 - wo, cy + 15, Self::SZ_LABEL, "Off", tone);
            t.draw(&mut self.fb, x + pw + 18, cy + 15, Self::SZ_LABEL, "On", tone);
        }
    }

    /// 开关行：可选左侧 40×40 图标 + 粗体标签 + 右侧开关。`base` 是标签基线。
    #[allow(clippy::too_many_arguments)]
    fn set_toggle_full(
        &mut self,
        pill_x: i32,
        label_x: i32,
        base: i32,
        label: &str,
        icon: Option<fn(&mut Fb, i32, i32, u8)>,
        on: bool,
        act: u32,
        hit: (i32, i32, i32, i32),
    ) {
        let mut tx = label_x;
        if let Some(f) = icon {
            f(&mut self.fb, label_x, base - 31, 20);
            tx = label_x + 70;
        }
        self.set_text(tx, base, Self::SZ_LABEL, true, label, 20);
        // 胶囊/圆点用**纯黑**：翻开关走 A2 二值快刷（12 相位 ≈ 141 ms），
        // 而 A2 只画得动 0/15。`tone 20` 是灰阶 1，A2 表里整串 0 码，开关会消失。
        self.set_switch(pill_x, base - 34, on, 0);
        self.hits.push(hit.0, hit.1, hit.2, hit.3, act);
    }

    /// 主界面里的开关行（整行 x=345..1404 可点）。
    fn set_toggle(
        &mut self,
        base: i32,
        label: &str,
        icon: Option<fn(&mut Fb, i32, i32, u8)>,
        on: bool,
        act: u32,
    ) {
        self.set_toggle_full(
            Self::SET_PILL_X,
            Self::SET_LEFT,
            base,
            label,
            icon,
            on,
            act,
            (345, base - 40, 1059, 90),
        );
    }

    /// 小号文字按钮（官方 `font.pixelSize: fontSizeBody2` = 25 的那两个：
    /// Change passcode / Join other network）。2px 边框、常规字重。
    fn set_text_button(&mut self, x: i32, y: i32, w: i32, h: i32, label: &str, size: f32, act: u32) {
        widgets::outline_rect(&mut self.fb, x, y, w, h, 2, 30);
        let base = y + h / 2 + (size * 0.36) as i32;
        self.set_center(x + w / 2, base, size, false, label, 20);
        self.hits.push(x, y, w, h, act);
    }

    /// 按钮：2px 边框，粗体文字居中（可带重启/电源图标）。
    fn set_button(&mut self, x: i32, y: i32, w: i32, h: i32, label: &str, icon: u8, act: u32) {
        widgets::outline_rect(&mut self.fb, x, y, w, h, 2, 30);
        let iw = if icon > 0 { 39 } else { 0 };
        let gap = if icon > 0 { 21 } else { 0 };
        let tw = match self.text.as_mut() {
            Some(t) => {
                t.set_bold(true);
                let v = t.width(Self::SZ_LABEL, label) as i32;
                t.set_bold(false);
                v
            }
            None => 0,
        };
        let start = x + (w - iw - gap - tw) / 2;
        match icon {
            1 => si_reload(&mut self.fb, start, y + 26, 20),
            2 => widgets::icon_power(&mut self.fb, start + 20, y + 46, 20),
            _ => {}
        }
        self.set_text(start + iw + gap, y + 57, Self::SZ_LABEL, true, label, 20);
        self.hits.push(x, y, w, h, act);
    }

    /// 用当前字体的缩放后字号量一段字的宽度（和 `set_text` 同一套换算）。
    fn set_measure(&mut self, size: f32, msg: &str) -> i32 {
        match self.text.as_mut() {
            Some(t) => t.width(Self::set_size(size, false), msg) as i32,
            None => 0,
        }
    }

    /// 按词折行画一段英文，返回下一行的基线 y。
    fn set_wrap(
        &mut self,
        x: i32,
        mut y: i32,
        size: f32,
        text: &str,
        max_w: i32,
        line_h: i32,
    ) -> i32 {
        let mut line = String::new();
        for word in text.split_whitespace() {
            let test = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if !line.is_empty() && self.set_measure(size, &test) > max_w {
                self.set_text(x, y, size, false, &line, 20);
                y += line_h;
                line = word.to_string();
            } else {
                line = test;
            }
        }
        if !line.is_empty() {
            self.set_text(x, y, size, false, &line, 20);
            y += line_h;
        }
        y
    }

    /// 黑底白字提示条（英文）。过期时间在 `tick` 里清。
    fn set_toast(&mut self, msg: &str) {
        let tw = self.set_measure(30.0, msg);
        let w = (tw + 80).min(1324);
        let x = (self.fb.width - w) / 2;
        let (y, h) = (1720, 84);
        self.fb.fill_rect(x, y, w, h, gray565(0));
        self.set_center(702, y + 55, 30.0, false, msg, 255);
    }

    /// 二级页顶部的 Back 行（可指定动作号）。
    fn set_back_row(&mut self, act: u32) {
        widgets::line(&mut self.fb, 42, 60, 63, 41, 5, 20);
        widgets::line(&mut self.fb, 42, 60, 63, 79, 5, 20);
        self.set_text(88, 73, Self::SZ_SIDE, true, "Back", 20);
        self.hits.push(0, 0, Self::SET_SBW, 118, act);
    }

    /// 4 个密码圆点：实心 = 已输入。`cy` 是圆心。
    /// 锁屏那 4 个圆环 + 右边的 Show / Hide（官方 Row：72 空白 + 4×⌀60 + 72）。
    fn lock_dots(&mut self, cy: i32, entered: &str, show: bool) {
        let (d, gap) = (60, 25); // 圆环 ⌀60、项间距 25
        let total = 72 + 4 * d + 3 * gap + 72 + 4 * gap;
        let x0 = (self.fb.width - total) / 2;
        let dots_x = x0 + 72 + gap;
        for i in 0..4 {
            let cx = dots_x + i * (d + gap) + d / 2;
            widgets::ring(&mut self.fb, cx, cy, d / 2, 2, 20);
            let ch = entered.chars().nth(i as usize);
            match (show, ch) {
                // Show：圆圈里显示数字
                (true, Some(c)) => {
                    self.set_center(cx, cy + 14, 38.0, true, &c.to_string(), 20);
                }
                // 否则填一个实心点（官方 ⌀56 的黑圆）
                (false, Some(_)) => widgets::disc(&mut self.fb, cx, cy, d / 2 - 2, 20),
                _ => {}
            }
        }
        // Show / Hide（官方 TextHeaderSmall 22 + 下划线）
        let label = if show { "Hide" } else { "Show" };
        let lx = dots_x + 4 * d + 4 * gap;
        let tw = self
            .text
            .as_mut()
            .map(|t| t.width(22.0, label))
            .unwrap_or(0.0);
        self.set_text(
            lx + ((72.0 - tw) / 2.0) as i32,
            cy + 8,
            22.0,
            false,
            label,
            20,
        );
        let ux = lx + ((72.0 - tw) / 2.0) as i32;
        self.fb
            .fill_rect(ux, cy + 13, tw as i32, 2, gray565(20));
        self.hits.push(lx, cy - 40, 72, 80, A_LOCK_SHOW);
    }

    /// 锁屏的提示条（官方 `NotificationItem`：490×84 的框 + 挂锁 + 文案）。
    fn lock_notice(&mut self, msg: &str) {
        let (bw, bh) = (490, 84);
        let (bx, by) = ((self.fb.width - bw) / 2, 640);
        widgets::outline_rect(&mut self.fb, bx, by, bw, bh, 2, 20);
        si_padlock(&mut self.fb, bx + 26, by + 30, 20);
        self.set_text(bx + 70, by + 52, 25.0, false, msg, 20);
    }

    fn set_pin_dots(&mut self, cy: i32, n: usize) {
        let (d, gap) = (56, 24);
        let total = 4 * d + 3 * gap;
        let x0 = (self.fb.width - total) / 2;
        for i in 0..4 {
            let cx = x0 + i * (d + gap) + d / 2;
            widgets::ring(&mut self.fb, cx, cy, 28, 3, 20);
            if (i as usize) < n {
                widgets::disc(&mut self.fb, cx, cy, 24, 20);
            }
        }
    }

    /// 数字键盘（1..9 / 0 / Del）。`digit_act + d` 是数字键动作，`del_act` 是删除。
    fn set_keypad(&mut self, y0: i32, digit_act: u32, del_act: u32) {
        let (kw, kh, gap, vgap) = (170, 110, 30, 24);
        let x0 = (self.fb.width - (kw * 3 + gap * 2)) / 2;
        for r in 0..4 {
            for c in 0..3 {
                let (label, digit): (&str, Option<u32>) = match (r, c) {
                    (3, 0) => ("", None),
                    (3, 1) => ("0", Some(0)),
                    (3, 2) => ("Del", None),
                    _ => (
                        ["1", "2", "3", "4", "5", "6", "7", "8", "9"]
                            [(r * 3 + c) as usize],
                        Some((r * 3 + c + 1) as u32),
                    ),
                };
                if label.is_empty() {
                    continue;
                }
                let x = x0 + c * (kw + gap);
                let y = y0 + r * (kh + vgap);
                widgets::outline_rect(&mut self.fb, x, y, kw, kh, 2, 30);
                if digit.is_some() {
                    self.set_center(x + kw / 2, y + kh / 2 + 14, 40.0, false, label, 20);
                } else {
                    self.set_center(x + kw / 2, y + kh / 2 + 10, 28.0, false, "Del", 20);
                }
                self.hits.push(
                    x,
                    y,
                    kw,
                    kh,
                    match digit {
                        Some(d) => digit_act + d,
                        None => del_act,
                    },
                );
            }
        }
    }

    /// 设置页整屏。
    fn draw_settings(&mut self) {
        // 进设置这一帧的耗时拆开看：整屏 GC16 在 ARM 上光是编码就 300 ms 级，
        // 到底是"画"慢还是"刷"慢，只能靠这个日志分辨（`RM_TIMING=1`）。
        let t_all = std::time::Instant::now();
        let s = match &self.screen {
            Screen::Settings(v) => (**v).clone(),
            _ => return,
        };
        let (w, h) = (self.fb.width, self.fb.height);
        self.fb.fill_rect(0, 0, w, h, gray565(255));
        self.hits.clear();
        match s.sub {
            Some(SettingsSub::Account) => {
                self.set_back_sub();
                self.set_page_account(&s);
            }
            Some(SettingsSub::Subscription) => {
                self.set_back_sub();
                self.set_page_subscription(&s);
            }
            Some(SettingsSub::Software) => {
                self.set_back_sub();
                self.set_page_software(&s);
            }
            // === P5 begin: Factory reset 确认框（独占整屏，先于其它分支）===
            None if s.factory_confirm => self.set_factory_confirm(),
            // === P5 end ===
            // === C0W begin: 忘记网络确认框（独占整屏）===
            None if s.wifi_forget.is_some() => self.set_wifi_forget_page(&s),
            // === C0W end ===
            None if s.pin.is_some() => self.set_pin_page(&s),
            None if s.lang_dialog.is_some() => {
                let kind = s.lang_dialog.unwrap();
                self.set_lang_page(&s, kind);
            }
            None if s.help_page.is_some() => {
                let page = s.help_page.unwrap();
                self.set_help_page(&s, page);
            }
            None => {
                self.set_sidebar(s.tab);
                // === P5 begin: tab 7/8 = Developer / Experimental，Help 挪到 9 ===
                match s.tab {
                    0 => self.set_page_general(&s),
                    1 => self.set_page_wifi(&s),
                    2 => self.set_page_battery(&s),
                    3 => self.set_page_storage(&s),
                    4 => self.set_page_security(&s),
                    5 => self.set_page_language(&s),
                    6 => self.set_page_accessibility(&s),
                    7 => self.set_page_developer(),
                    8 => self.set_page_experimental(&s),
                    _ => self.set_page_help(),
                }
                // === P5 end ===
            }
        }
        // === P5 begin: Factory reset 完成提示（顶部黑条，和底部 toast 分开）===
        if let Some((msg, until)) = &s.factory_done {
            if *until > std::time::Instant::now() {
                self.set_top_banner(msg);
            }
        }
        // === P5 end ===
        if let Some((msg, until)) = &s.notice {
            if *until > std::time::Instant::now() {
                self.set_toast(msg);
            }
        }
        // 波形按"这一下改了什么"分两档（官方 swtcon 也是这么分工的）：
        //
        // **整页换内容**（进设置、切 tab、进二级页）→ `GC16 + FULL`。
        //   46 相位、整屏编码走"整块同表"快路径（实测编码 313 + 显示 317 ms）。
        //   和库界面一样，是官方整页切换的做法，会闪一下黑、顺手清残影。
        //
        // **只翻了个开关**（`settings_fast`）→ `A2 + PARTIAL`。
        //   驱动把整屏请求缩成"真正变了的那几块"（一个开关 ≈ 130×50），
        //   A2 只有 12 相位 ≈ 141 ms，点下去是立刻动的。
        //   A2 是二值波形，所以开关本身必须用纯黑画（`set_toggle_full` 里改过）。
        //
        // 为什么不能像以前那样一律 `DU + PARTIAL`：DU/A2 的表里
        // 「目标 = 1/2/8（任何中间灰）」全是 0 码（`dump_pairs` 实测 band 7），
        // 而设置页文字是 `tone 20` = 灰阶 1 —— 一个像素都不驱，整页只剩纯黑
        // 方块，就是那个"设置界面不渲染"的 bug。16 级灰只有 GC16/GL16 给得了；
        // GL16 虽然不闪，但它走逐 cell 查表，整屏编码要 870 ms，太慢。
        let (wave, umode) = if self.settings_fast {
            self.settings_fast = false;
            (WAVEFORM_MODE_A2, rmdev::fb::UPDATE_MODE_PARTIAL)
        } else {
            (WAVEFORM_MODE_GC16, rmdev::fb::UPDATE_MODE_FULL)
        };
        let t_draw = t_all.elapsed();
        self.fb.refresh(0, 0, w, h, wave, umode);
        if env_flag("RM_TIMING") {
            eprintln!(
                "[ui] draw_settings 合计={}ms（画={}ms 刷={}ms）",
                t_all.elapsed().as_millis(),
                t_draw.as_millis(),
                (t_all.elapsed() - t_draw).as_millis()
            );
        }
    }

    // ---------- 各页内容

    fn set_page_general(&mut self, s: &SettingsView) {
        self.set_center(874, 162, Self::SZ_TITLE, true, "General settings", 20);
        let ver = format!("Version {}", s.version);
        // 官方这三行是 Account / Subscription / Software（引导去订阅）。
        // 我们没有任何云账号和订阅：第一行是自建服务器，第二行是所有功能已解锁。
        let server_line = if s.server.configured() {
            s.server
                .url
                .trim_start_matches("http://")
                .trim_end_matches('/')
                .to_string()
        } else {
            "Not configured".to_string()
        };
        let rows: [(&str, &str, u32); 3] = [
            ("Sync server", server_line.as_str(), A_S_ACCOUNT),
            ("Features", "All unlocked", A_S_SUBSCRIPTION),
            ("Software", ver.as_str(), A_S_SOFTWARE),
        ];
        for (i, (label, value, act)) in rows.iter().enumerate() {
            let y = 355 + 214 * i as i32;
            self.set_text(Self::SET_LEFT, y - 25, Self::SZ_LABEL, true, label, 20);
            self.set_box(y, value, *act);
        }
        self.set_toggle(970, "Flight mode", Some(si_plane), s.on(0, 0), A_S_TOGGLE0);
        self.set_button(508, 1582, 312, 90, "Restart", 1, A_S_RESTART);
        self.set_button(904, 1582, 336, 90, "Power off", 2, A_S_POWEROFF);
    }

    fn set_page_wifi(&mut self, s: &SettingsView) {
        // === C0W begin: 这一帧没画当前网络行，长按就不该命中 ===
        self.wifi_cur_row = None;
        // === C0W end ===
        self.set_center(874, 162, Self::SZ_TITLE, true, "Wi-Fi settings", 20);
        if !s.on(1, 0) {
            // 关掉时整页只剩开关，而且置灰（官方零售模式也是这个思路）。
            si_wifi(&mut self.fb, Self::SET_LEFT, 298, 120);
            self.set_text(Self::SET_LEFT + 70, 329, Self::SZ_LABEL, true, "Wi-Fi", 120);
            self.set_switch(Self::SET_PILL_X, 295, false, 120);
            self.hits.push(345, 289, 1059, 90, A_S_TOGGLE0 + 4);
            self.set_center(
                874,
                560,
                Self::SZ_PARA,
                false,
                "Turn on Wi-Fi to see available networks.",
                120,
            );
            return;
        }
        self.set_toggle(329, "Wi-Fi", Some(si_wifi), true, A_S_TOGGLE0 + 4);
        if s.ssid.is_empty() && s.networks.is_empty() {
            self.set_text(
                Self::SET_LEFT,
                449,
                Self::SZ_LABEL,
                true,
                "No Wi-Fi networks available",
                20,
            );
        } else if let Some(name) = &s.connecting {
            // === C0W begin: 连接中（官方 `res_0689b2f.qml`："Connecting to..." + 目标 SSID）===
            // 放最前面：从一个已连的网络切到另一个时旧 `s.ssid` 还在，
            // 但这一帧该显示"正在连新的那个"。
            self.set_text(
                Self::SET_LEFT,
                449,
                Self::SZ_LABEL,
                true,
                "Connecting to...",
                20,
            );
            widgets::disc(&mut self.fb, 427, 511, 7, 20);
            self.set_text(453, 524, Self::SZ_LABEL, true, name, 20);
            // === C0W end ===
        } else if !s.ssid.is_empty() {
            self.set_text(Self::SET_LEFT, 449, Self::SZ_LABEL, true, "Connected to:", 20);
            widgets::disc(&mut self.fb, 427, 511, 7, 20);
            self.set_text(453, 524, Self::SZ_LABEL, true, &s.ssid, 20);
            let bars = s.wifi_signal.map(wifi::signal_bars).unwrap_or(3);
            si_wifi_bars(&mut self.fb, 1254, 492, 20, bars);
            // === C0W begin: 这一行的矩形，长按 → Forget 确认框 ===
            self.wifi_cur_row = Some((345, 410, 1059, 130));
            // === C0W end ===
        }
        if s.networks.is_empty() {
            // 扫不到（或本机没有 iw）：只留已连接那一条，和官方截图一致。
            return;
        }
        // === C0W begin: 连接中也算"状态行已占位"，否则 Networks 标题会压在它上面 ===
        let connected = !s.ssid.is_empty() || s.connecting.is_some();
        // === C0W end ===
        let title = if connected { "Networks" } else { "Choose network" };
        let (title_y, top) = if connected { (620, 650) } else { (540, 570) };
        self.set_text(Self::SET_LEFT, title_y, Self::SZ_LABEL, true, title, 20);
        for (i, n) in s.networks.iter().take(10).enumerate() {
            let y = top + 82 * i as i32;
            self.set_text(420, y + 54, Self::SZ_PARA, false, &n.ssid, 20);
            let bars = n.signal.map(wifi::signal_bars).unwrap_or(0);
            si_wifi_bars(&mut self.fb, 1254, y + 18, 20, bars);
            if n.secure {
                si_padlock(&mut self.fb, 1188, y + 22, 20);
            }
            self.hits.push(345, y, 1059, 82, A_S_NET0 + i as u32);
        }
        // 跟着列表走（官方也是列在列表下面），最多 10 行时仍不会越界。
        let btn_y = (top + 82 * s.networks.len().min(10) as i32 + 30).min(1600);
        self.set_text_button(492, btn_y, 420, 90, "Join other network", 25.0, A_S_JOIN);
    }

    // === C0W begin: Wi-Fi 连接面板 / 忘记网络 ===
    //
    // 规格：
    // - 面板：官方 `res_065b361.qml`（新）/ `res_0687541.qml`（旧）的
    //   `Connect to <b>%1</b>` + `SSID` / `Password` 两个输入 + `Cancel` / `Connect`。
    // - 状态行：官方 `res_0689b2f.qml` 的 `Connected to:` / `Connecting to...`。
    // - 忘记：官方 `res_0688b84.qml` 网络行弹层的 `Forget`（原文就一个词）。
    //
    // 连接本身全部交给 `settings::wifi::connect`（只走 `wpa_cli`），这里只管
    // 输入/绘制/线程，绝不同步等 20 s。

    /// 把一段字按可用宽度截断（面板里的输入框和标题都不能溢出）。
    fn wifi_panel_text(&mut self, size: f32, msg: &str, max_w: i32) -> String {
        match self.text.as_mut() {
            Some(t) => t.ellipsize(size, msg, max_w),
            None => msg.to_string(),
        }
    }

    /// 打开面板前把设置页存一份，关面板时原样放回去（列表/开关都不丢）。
    fn wifi_remember_back(&mut self) {
        if let Screen::Settings(s) = &self.screen {
            self.wifi_back = Some(s.clone());
        }
    }

    /// 点列表里**加密**的网络：只输密码，SSID 只读。
    fn open_wifi_pass(&mut self, ssid: &str) {
        self.wifi_remember_back();
        self.screen = Screen::WifiPass(Box::new(WifiConnect {
            kb: Keyboard::new(),
            ssid: ssid.to_string(),
            stage: 1,
            err: None,
        }));
        self.draw_wifi_connect();
        // `draw_wifi_connect` 先整屏二值画并 FULL，随后 `wifi_connect_boxes`
        // 又补 3 次局部刷新；面板驱动会把紧跟着的 FULL 并掉，屏上只剩那 3 个框
        // （用户报的"只有三个框、没有键盘"）。补一次 FULL 收尾。
        full_refresh(&mut self.fb);
    }

    /// `Join other network`：第一步输 SSID，第二步输密码。
    fn open_wifi_join(&mut self) {
        self.wifi_remember_back();
        self.screen = Screen::WifiJoin(Box::new(WifiConnect {
            kb: Keyboard::new(),
            ssid: String::new(),
            stage: 0,
            err: None,
        }));
        self.draw_wifi_connect();
        full_refresh(&mut self.fb);
    }

    /// 关面板（`Cancel`）：恢复打开面板前那一帧设置页。
    fn wifi_close_panel(&mut self) {
        self.kb_hits.clear();
        match self.wifi_back.take() {
            Some(s) => {
                self.screen = Screen::Settings(s);
                // 连接面板是 1-bit 黑框 + 局部刷新画的：A2 的设置页刷不掉那些框，
                // 会在列表上留下"幽灵框"（用户报的"三个框"）。回设置页时整屏
                // GC16 清一次底。
                self.draw_settings();
                let (w, h) = (self.fb.width, self.fb.height);
                self.fb.refresh(
                    0,
                    0,
                    w,
                    h,
                    WAVEFORM_MODE_GC16,
                    rmdev::fb::UPDATE_MODE_FULL,
                );
            }
            // 理论走不到：面板一定是从设置页打开的
            None => self.open_settings_tab(1),
        }
    }

    /// 面板里的红字（输入校验失败）。不弹 toast：官方也是在输入框下面写一行。
    fn wifi_panel_err(&mut self, msg: &str) {
        if let Screen::WifiPass(p) | Screen::WifiJoin(p) = &mut self.screen {
            p.err = Some(msg.to_string());
        }
        self.draw_wifi_connect();
    }

    /// 面板的下一步：Join 第一步收 SSID；第二步收密码 → 起后台连接。
    fn wifi_panel_advance(&mut self) {
        let Some((join, stage, known, typed)) = (match &self.screen {
            Screen::WifiPass(p) => Some((false, p.stage, p.ssid.clone(), p.kb.text.clone())),
            Screen::WifiJoin(p) => Some((true, p.stage, p.ssid.clone(), p.kb.text.clone())),
            _ => None,
        }) else {
            return;
        };
        if stage == 0 {
            // SSID 1..=32 字节（802.11 上限）。超了/空了在面板里给红字。
            let name = typed.trim().to_string();
            if name.is_empty() || name.len() > 32 {
                self.wifi_panel_err("The network name must be 1 to 32 characters");
                return;
            }
            if let Screen::WifiJoin(p) = &mut self.screen {
                p.ssid = name;
                p.stage = 1;
                p.kb = Keyboard::new();
                p.err = None;
            }
            self.draw_wifi_connect();
            return;
        }
        if typed.is_empty() && join {
            // Join other network 留空密码 = 开放网络（`connect` 下发 `key_mgmt=NONE`）
            self.wifi_start_connect(known, true, String::new());
            return;
        }
        // PSK 8..=63 字节。`Password is too short` 是官方原话（WifiPasswordValidator）。
        if typed.len() < 8 {
            self.wifi_panel_err("Password is too short");
            return;
        }
        if typed.len() > 63 {
            self.wifi_panel_err("Password is too long");
            return;
        }
        self.wifi_start_connect(known, false, typed);
    }

    /// 起后台线程跑 `wifi::connect`（最长 20 s），结果由 `poll_wifi_conn` 收。
    ///
    /// 面板点下去就关（官方也是 `connectToNetwork()` 之后立刻 `close()`），
    /// Wi-Fi 页状态行接着显示 `Connecting to...`。
    fn wifi_start_connect(&mut self, ssid: String, open: bool, psk: String) {
        if self.wifi_conn.is_some() {
            self.notice(NoticeKind::Info, "Please wait for the current task");
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let name = ssid.clone();
        std::thread::Builder::new()
            .name("wifi-conn".into())
            .spawn(move || {
                let t = std::time::Instant::now();
                let r = wifi::connect(&name, open, &psk);
                // 只记 SSID 和成败：密码一个字都不进日志。
                eprintln!(
                    "[wifi] 连接 {}（{} ms）→ {}",
                    name,
                    t.elapsed().as_millis(),
                    if r.is_ok() { "OK" } else { "FAIL" }
                );
                let _ = tx.send(r);
            })
            .ok();
        self.wifi_conn = Some((ssid.clone(), rx));
        self.wifi_conn_at = std::time::Instant::now();
        // 关掉面板的键盘命中表：下一帧可能是别的整屏页在问 `kb_hits`
        self.kb_hits.clear();
        match self.wifi_back.take() {
            Some(mut s) => {
                s.connecting = Some(ssid);
                self.screen = Screen::Settings(s);
            }
            // 从列表直接点开放网络：本来就在设置页，原地标上就行（不重建列表）
            None => {
                if let Screen::Settings(s) = &mut self.screen {
                    s.connecting = Some(ssid);
                } else {
                    self.open_settings_tab(1);
                    if let Screen::Settings(s) = &mut self.screen {
                        s.connecting = Some(ssid);
                    }
                }
            }
        }
        self.draw_settings();
    }

    /// 收后台连接结果（主循环每圈调一次）。
    fn poll_wifi_conn(&mut self) {
        let Some((ssid, rx)) = &self.wifi_conn else {
            return;
        };
        let ssid = ssid.clone();
        // 看门狗：见 `wifi_conn_at` 的注释。45 s = 四档 × 5 s + 关联成功后
        // 自己催 DHCP 的那几秒（`wifi::acquire_ipv4`），留一点余量。
        if self.wifi_conn_at.elapsed() > Duration::from_secs(45) {
            self.wifi_conn = None;
            if let Screen::Settings(s) = &mut self.screen {
                s.connecting = None;
            }
            self.notice(
                NoticeKind::Error,
                &format!("Could not connect to {ssid}: timed out"),
            );
            return;
        }
        let r = match rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("Could not connect: the Wi-Fi task stopped".to_string())
            }
        };
        self.wifi_conn = None;
        if let Screen::Settings(s) = &mut self.screen {
            s.connecting = None;
        }
        match r {
            Ok(()) => {
                // 先把真实状态读回来（`iw link` 只是读驱动，不阻塞），再给提示：
                // 这样这一帧的 `Connected to:` 已经是新 SSID 了。
                self.settings_refresh_wifi();
                self.notice(NoticeKind::Info, &format!("Connected to {ssid}"));
            }
            Err(e) => {
                // `select_network` 会先禁掉其它网络：失败后原来那条可能已经掉了，
                // 读一次真实状态，读不到就把"已连接"清掉，别挂着一个假的 SSID。
                if wifi::current_link().is_none() {
                    if let Screen::Settings(s) = &mut self.screen {
                        s.ssid.clear();
                        s.wifi_signal = None;
                    }
                }
                // 失败照原样显示 `wpa_cli` 的英文，不翻译
                self.notice(NoticeKind::Error, &e)
            }
        }
    }

    /// 连接面板：黑头标题 + SSID/密码两行 + 键盘 + `Cancel`/`Connect`。
    fn draw_wifi_connect(&mut self) {
        // === C20 begin: 这条路径整页二值绘制，见 `wifi_connect_mono_paint` ===
        // A2 只能画黑白，所以先开 `fb.set_mono`、由下面这份原函数体按二值画，
        // 再补输入框的黑框。`true` = 已经画完并刷过。
        if self.wifi_connect_mono_paint() {
            return;
        }
        // === C20 end ===
        let Some((stage, ssid, typed, err, kb)) = (match &self.screen {
            Screen::WifiPass(p) => Some((
                p.stage,
                p.ssid.clone(),
                p.kb.text.clone(),
                p.err.clone(),
                p.kb.clone(),
            )),
            Screen::WifiJoin(p) => Some((
                p.stage,
                p.ssid.clone(),
                p.kb.text.clone(),
                p.err.clone(),
                p.kb.clone(),
            )),
            _ => None,
        }) else {
            return;
        };
        let (w, h) = (self.fb.width, self.fb.height);
        self.fb.fill_rect(0, 0, w, h, gray565(255));
        self.hits.clear();

        // 黑头 + 居中标题。官方是 `Connect to <b>%1</b>`；16 级灰没有富文本，去掉 <b>。
        self.fb.fill_rect(0, 0, w, 118, gray565(0));
        let title = if stage == 0 {
            "Connect to other network".to_string()
        } else {
            format!("Connect to {ssid}")
        };
        let shown = self.wifi_panel_text(32.0, &title, w - 400);
        self.set_center(702, 73, 32.0, true, &shown, 255);

        // 输入行（官方 `TitleTextInputComponent`：一行小标题 + 灰底输入框）
        if stage == 0 {
            self.set_text(262, 250, 29.0, false, "Enter network name (SSID)", 20);
            self.fb.fill_rect(262, 260, 880, 84, gray565(238));
            let t = self.wifi_panel_text(34.0, &typed, 840);
            self.set_text(282, 314, 34.0, false, &t, 20);
        } else {
            self.set_text(262, 250, 29.0, false, "SSID", 20);
            self.fb.fill_rect(262, 260, 880, 84, gray565(238));
            let s2 = self.wifi_panel_text(34.0, &ssid, 840);
            self.set_text(282, 314, 34.0, false, &s2, 20);
            self.set_text(262, 420, 29.0, false, "Password", 20);
            self.fb.fill_rect(262, 430, 880, 84, gray565(238));
            let p2 = self.wifi_panel_text(34.0, &typed, 840);
            self.set_text(282, 484, 34.0, false, &p2, 20);
        }

        // 校验失败的红字（16 级灰没有红，用最深的灰）
        if let Some(e) = &err {
            let t = self.wifi_panel_text(30.0, e, 880);
            self.set_text(262, 570, 30.0, false, &t, 40);
        }

        // 按钮：`Cancel` / `Connect`（Join 第一步是 `Next`）
        let (bw, bh, by) = (420, 90, 612);
        self.fb.fill_rect(262, by, bw, bh, gray565(210));
        self.set_center(262 + bw / 2, by + 56, 34.0, false, "Cancel", 20);
        self.hits.push(262, by, bw, bh, A_C0W_CANCEL);
        self.fb.fill_rect(702, by, bw, bh, gray565(40));
        let ok = if stage == 0 { "Next" } else { "Connect" };
        self.set_center(702 + bw / 2, by + 56, 34.0, true, ok, 250);
        self.hits.push(702, by, bw, bh, A_C0W_CONNECT);

        // 键盘贴屏底（和 Pair 一样）
        let ky = h - kb.height() - 16;
        self.kb_hits = kb.draw(&mut self.fb, &mut self.text, 0, ky, w);
        self.fb
            .refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
    }

    /// 打字后只重画**当前输入框**（1-bit + 局部 GC16）。
    ///
    /// 这一步是用户点名的：进界面先整屏白底 GC16，之后每敲一个字只刷新输入框，
    /// 既不会闪整屏，也不会把上一次的字留在框里（A2/DU 局部刷新会留残影，
    /// 所以这里用 GC16 的 PARTIAL）。
    fn wifi_repaint_field(&mut self) {
        let Some((stage, _ssid, typed, _)) = self.wifi_connect_state() else {
            return;
        };
        self.fb.set_mono(true);
        let r = if stage == 0 {
            self.wifi_connect_box(WIFI_BOX_Y0, "Enter network name (SSID)", &typed)
        } else {
            self.wifi_connect_box(WIFI_BOX_Y1, "Password", &typed)
        };
        self.fb.set_mono(false);
        self.fb.refresh(
            r.0,
            r.1,
            r.2,
            r.3,
            WAVEFORM_MODE_GC16,
            rmdev::fb::UPDATE_MODE_PARTIAL,
        );
    }

    /// 连接面板的点击：键盘先吃，然后 `Cancel` / `Connect`。
    fn tap_wifi_connect(&mut self, x: f32, y: f32) {
        for (r, k) in self.kb_hits.clone() {
            let (xi, yi) = (x as i32, y as i32);
            if xi >= r.0 && xi < r.0 + r.2 && yi >= r.1 && yi < r.1 + r.3 {
                self.handle_key(k);
                return;
            }
        }
        match self.hits.take(x, y) {
            Some(A_C0W_CANCEL) => self.wifi_close_panel(),
            Some(A_C0W_CONNECT) => self.wifi_panel_advance(),
            _ => {}
        }
    }

    /// Wi-Fi 页"当前网络"那一行的长按（`tick` 里按下满 `LONG_PRESS_MS` 才问）：
    /// 落在这一行 → 弹 `Forget` 确认框。
    fn wifi_hold(&mut self, x: f32, y: f32) -> bool {
        let (ok, ssid, row) = match &self.screen {
            Screen::Settings(s) => (
                s.tab == 1 && s.sub.is_none() && !s.ssid.is_empty(),
                s.ssid.clone(),
                self.wifi_cur_row,
            ),
            _ => return false,
        };
        let Some((rx, ry, rw, rh)) = row else {
            return false;
        };
        if !ok {
            return false;
        }
        let (xi, yi) = (x as i32, y as i32);
        if xi < rx || xi >= rx + rw || yi < ry || yi >= ry + rh {
            return false;
        }
        if let Screen::Settings(s) = &mut self.screen {
            s.wifi_forget = Some(ssid);
        }
        self.draw_settings();
        true
    }

    /// 忘记网络确认框（独占整屏）。官方只有一个 `Forget` 按钮（`res_0688b84.qml`），
    /// 这里按"两按钮确认"补一个 `Cancel`。
    fn set_wifi_forget_page(&mut self, s: &SettingsView) {
        let Some(ssid) = s.wifi_forget.clone() else {
            return;
        };
        let (w, h) = (self.fb.width, self.fb.height);
        self.fb.fill_rect(0, 0, w, h, gray565(255));
        self.set_center(702, 420, Self::SZ_TITLE, true, "Forget network", 20);
        let shown = self.wifi_panel_text(Self::SZ_LABEL, &ssid, w - 400);
        self.set_center(702, 520, Self::SZ_LABEL, false, &shown, 20);
        let (bw, bh) = (420, 90);
        let by = 700;
        // `Forget`：黑底白字（主操作）
        self.fb.fill_rect(262, by, bw, bh, gray565(0));
        self.set_center(262 + bw / 2, by + 56, 34.0, true, "Forget", 255);
        self.hits.push(262, by, bw, bh, A_C0W_FORGET);
        // `Cancel`：白底黑框
        widgets::outline_rect(&mut self.fb, 702, by, bw, bh, 2, 30);
        self.set_center(702 + bw / 2, by + 56, 34.0, false, "Cancel", 20);
        self.hits.push(702, by, bw, bh, A_C0W_FORGET_CANCEL);
    }

    /// 确认框的两个按钮。成功只清 `s.ssid` 并重画；失败把 `wpa_cli` 原文塞进 notice。
    fn wifi_forget_action(&mut self, a: u32) {
        let Some(ssid) = (match &self.screen {
            Screen::Settings(s) => s.wifi_forget.clone(),
            _ => None,
        }) else {
            return;
        };
        match a {
            A_C0W_FORGET => {
                let r = wifi::forget(&ssid);
                if let Screen::Settings(s) = &mut self.screen {
                    s.wifi_forget = None;
                    if r.is_ok() {
                        s.ssid.clear();
                        s.wifi_signal = None;
                    }
                }
                match r {
                    Ok(()) => {
                        eprintln!("[wifi] 已忘记 {ssid}");
                        self.settings_refresh_wifi();
                        self.draw_settings();
                    }
                    // `notice` 会自己重画设置页
                    Err(e) => self.notice(NoticeKind::Error, &e),
                }
            }
            A_C0W_FORGET_CANCEL => {
                if let Screen::Settings(s) = &mut self.screen {
                    s.wifi_forget = None;
                }
                self.draw_settings();
            }
            _ => {}
        }
    }
    // === C0W end ===

    fn set_page_battery(&mut self, s: &SettingsView) {
        // === BATT begin: 容量 / 10 分钟平均耗电 / 按该速率的剩余时间估算 ===
        // 三行插在电量行下面，下面的开关和说明整体下移，免得叠在一起。
        const DY: i32 = 126;
        self.set_center(874, 162, Self::SZ_TITLE, true, "Battery settings", 20);
        // 官方 QML 这一页只有 3 个开关；电量/充电/剩余时间放在标题下的一行里，
        // 读不到就完全不画（本机渲染因此和官方截图一致）。
        if let Some(b) = &s.battery {
            if let Some(line) = battery::summary(b) {
                self.set_center(874, 240, 30.0, false, &line, 20);
            }
            // 第 1 行：当前电量（mAh）。没有 charge_now 就不画。
            if let Some(mah) = b.capacity_mah() {
                self.set_center(874, 289, 30.0, false, &battery::fmt_mah(mah), 20);
            }
            // 第 2 行：窗口内平均电流（mA）+ 实际窗口分钟数。
            let avg = self.battery_usage.avg_ma(battery::WINDOW);
            if let Some(avg) = avg {
                let mins = self.battery_usage.span_minutes().round() as u32;
                self.set_center(874, 338, 30.0, false, &battery::fmt_usage(avg, mins), 20);
            }
            // 第 3 行：还能用多久；充电中不给时间，写 Charging。
            let left = if b.is_charging() {
                Some("Charging".to_string())
            } else {
                match (avg, b.capacity_mah()) {
                    (Some(draw), Some(mah)) => battery::estimate(draw, mah)
                        .map(|d| format!("{} left", battery::human_duration(d))),
                    _ => None,
                }
            };
            if let Some(text) = left {
                self.set_center(874, 387, 30.0, false, &text, 20);
            }
        }
        self.set_toggle(
            329 + DY,
            "Show battery percentage",
            None,
            s.on(2, 0),
            A_S_TOGGLE0 + 8,
        );
        self.set_toggle(
            536 + DY,
            "Auto sleep",
            None,
            s.on(2, 1),
            A_S_TOGGLE0 + 9,
        );
        self.set_para(Self::SET_LEFT, 596 + DY, "Puts your reMarkable to sleep");
        self.set_para(Self::SET_LEFT, 645 + DY, "after 20 minutes of inactivity.");
        self.set_toggle(
            777 + DY,
            "Auto power off",
            None,
            s.on(2, 2),
            A_S_TOGGLE0 + 10,
        );
        let y = Self::SET_LEFT;
        self.set_para(
            y,
            838 + DY,
            "Power off after 12 hours of inactivity to save",
        );
        self.set_para(y, 886 + DY, "battery, unless it is manually put to sleep");
        // === BATT end ===
    }

    fn set_page_storage(&mut self, s: &SettingsView) {
        self.set_center(874, 162, Self::SZ_TITLE, true, "Storage settings", 20);
        self.set_text(Self::SET_LEFT, 329, Self::SZ_LABEL, true, "Disk space", 20);
        // 进度条：胶囊 909×45，按已用比例填充
        let (bx, by, bw, bh) = (Self::SET_LEFT, 376, 909, 45);
        let ratio = disk::ratio(s.disk_used as f64, s.disk_total as f64) as f32;
        let fw = ((bw as f32 * ratio).round() as i32).clamp(bh, bw);
        si_fill_capsule(&mut self.fb, bx, by, fw, bh, 20);
        si_capsule(&mut self.fb, bx, by, bw, bh, 4, 20);
        let disk_text = disk::format_used(s.disk_used as f64, s.disk_total as f64);
        self.set_text(422, 480, Self::SZ_VALUE, false, &disk_text, 20);
        self.set_toggle(619, "USB web interface", None, s.on(3, 0), A_S_TOGGLE0 + 12);
        let x = self.set_para(Self::SET_LEFT, 678, "Open the address: ");
        let x = self.set_text(x, 678, Self::SZ_PARA, true, "http://10.11.99.1", 20);
        self.set_text(x, 678, Self::SZ_PARA, false, " in a web", 20);
        self.set_para(Self::SET_LEFT, 727, "browser.");
        self.set_toggle(870, "Sync before sleep", None, s.on(3, 1), A_S_TOGGLE0 + 13);
        self.set_para(Self::SET_LEFT, 930, "Save changes to the cloud when your paper");
        self.set_para(Self::SET_LEFT, 978, "tablet goes to sleep.");
        self.set_text(Self::SET_LEFT, 1122, Self::SZ_LABEL, true, "Cloud connection", 20);
        let msg = s
            .sync_msg
            .as_deref()
            .unwrap_or("See if your reMarkable is syncing\nproperly.");
        for (i, line) in msg.split('\n').enumerate() {
            self.set_para(Self::SET_LEFT, 1181 + 49 * i as i32, line);
        }
        self.set_button(1014, 1088, 315, 90, "Check sync", 0, A_S_CHECK_SYNC);
    }

    fn set_page_security(&mut self, s: &SettingsView) {
        self.set_center(874, 162, Self::SZ_TITLE, true, "Security settings", 20);
        self.set_toggle(329, "Passcode", None, s.on(4, 0), A_S_TOGGLE0 + 16);
        if s.on(4, 0) {
            self.set_text_button(546, 415, 312, 90, "Change passcode", 25.0, A_S_CHANGE_PASS);
        }
    }

    fn set_page_language(&mut self, s: &SettingsView) {
        self.set_center(874, 162, Self::SZ_TITLE, true, "Language settings", 20);
        self.set_text(
            Self::SET_LEFT,
            329,
            Self::SZ_LABEL,
            true,
            "Handwriting conversion language",
            20,
        );
        let hw = language::HANDWRITING[s.lang_hw.min(language::HANDWRITING.len() - 1)].name;
        self.set_box(355, hw, A_S_LANG0);
        self.set_text(Self::SET_LEFT, 542, Self::SZ_LABEL, true, "Keyboard layout", 20);
        let kb = language::KEYBOARDS[s.lang_kb.min(language::KEYBOARDS.len() - 1)].name;
        self.set_box(569, kb, A_S_LANG0 + 1);
    }

    fn set_page_accessibility(&mut self, s: &SettingsView) {
        self.set_center(874, 162, Self::SZ_TITLE, true, "Accessibility settings", 20);
        self.set_text(Self::SET_LEFT, 329, Self::SZ_LABEL, true, "Handedness", 20);
        for (i, (label, cx)) in [("Left", 676), ("Right", 1073)].iter().enumerate() {
            let on = s.right_handed == (i == 1);
            if on {
                widgets::disc(&mut self.fb, *cx, 497, 52, 20);
            }
            widgets::ring(&mut self.fb, *cx, 497, 52, 4, 20);
            self.set_center(*cx, 612, Self::SZ_PARA, on, label, 20);
            self.hits.push(*cx - 55, 440, 110, 180, A_S_RADIO0 + i as u32);
        }
        self.set_text(Self::SET_LEFT, 752, Self::SZ_LABEL, true, "Font size", 20);
        for (i, (label, x, size)) in [("Normal", 521, 24.0f32), ("Large", 917, 36.0)]
            .iter()
            .enumerate()
        {
            let on = s.font_large == (i == 1);
            let (bx, by, bw, bh) = (*x, 838, 312, 90);
            if on {
                self.fb.fill_rect(bx, by, bw, bh, gray565(0));
            } else {
                widgets::outline_rect(&mut self.fb, bx, by, bw, bh, 2, 30);
            }
            let tone = if on { 255 } else { 20 };
            let base = by + bh / 2 + (size * 0.36) as i32;
            self.set_center(bx + bw / 2, base, *size, true, label, tone);
            self.hits.push(bx, by, bw, bh, A_S_RADIO0 + 2 + i as u32);
        }
        // 笔尾橡皮映射。官方没有这一项 —— 官方笔尾永远是一块"刷白"橡皮。
        // 机器上多出来的"擦整笔"模式是灰机上最顺手的擦除方式，很多人希望
        // 笔尾直接就是它，所以给三档：刷白 / 擦整笔 / 跟橡皮菜单上次的选择。
        self.set_text(Self::SET_LEFT, 1005, Self::SZ_LABEL, true, "Pen-tail eraser", 20);
        for (i, (label, want)) in [
            ("Eraser", prefs::TailEraser::Eraser),
            ("Erase stroke", prefs::TailEraser::Stroke),
            ("Use last", prefs::TailEraser::Last),
        ]
        .iter()
        .enumerate()
        {
            let on = s.prefs.tail_eraser == *want;
            let (bx, by, bw, bh) = (Self::SET_LEFT + i as i32 * 280, 1060, 250, 90);
            if on {
                self.fb.fill_rect(bx, by, bw, bh, gray565(0));
            } else {
                widgets::outline_rect(&mut self.fb, bx, by, bw, bh, 2, 30);
            }
            let tone = if on { 255 } else { 20 };
            self.set_center(
                bx + bw / 2,
                by + bh / 2 + (Self::SZ_PARA * 0.36) as i32,
                Self::SZ_PARA,
                true,
                label,
                tone,
            );
            self.hits.push(bx, by, bw, bh, A_S_RADIO0 + 4 + i as u32);
        }
        // 横屏模式（官方没有这一组）：宽页自动进横屏 + 往哪边转。
        // 文案是我们自己的，已登记在 `docs/ACCEPT-gaps.md` 的自造字符串里。
        self.set_text(Self::SET_LEFT, 1205, Self::SZ_LABEL, true, "Auto landscape", 20);
        for (i, label) in ["Off", "On"].iter().enumerate() {
            let on = s.prefs.auto_landscape == (i == 1);
            let (bx, by, bw, bh) = (Self::SET_LEFT + i as i32 * 280, 1260, 250, 90);
            if on {
                self.fb.fill_rect(bx, by, bw, bh, gray565(0));
            } else {
                widgets::outline_rect(&mut self.fb, bx, by, bw, bh, 2, 30);
            }
            self.set_center(
                bx + bw / 2,
                by + bh / 2 + (Self::SZ_PARA * 0.36) as i32,
                Self::SZ_PARA,
                true,
                label,
                if on { 255 } else { 20 },
            );
            self.hits.push(bx, by, bw, bh, A_S_LAND0 + i as u32);
        }
        self.set_text(Self::SET_LEFT, 1395, Self::SZ_LABEL, true, "Landscape rotation", 20);
        for (i, label) in ["Left", "Right"].iter().enumerate() {
            let on = s.prefs.land_dir_left == (i == 0);
            let (bx, by, bw, bh) = (Self::SET_LEFT + i as i32 * 280, 1450, 250, 90);
            if on {
                self.fb.fill_rect(bx, by, bw, bh, gray565(0));
            } else {
                widgets::outline_rect(&mut self.fb, bx, by, bw, bh, 2, 30);
            }
            self.set_center(
                bx + bw / 2,
                by + bh / 2 + (Self::SZ_PARA * 0.36) as i32,
                Self::SZ_PARA,
                true,
                label,
                if on { 255 } else { 20 },
            );
            self.hits.push(bx, by, bw, bh, A_S_LAND0 + 2 + i as u32);
        }
    }

    fn set_page_help(&mut self) {
        self.set_center(874, 162, Self::SZ_TITLE, true, "Help", 20);
        self.set_text(Self::SET_LEFT, 329, Self::SZ_LABEL, true, "About", 20);
        self.set_box(355, "Compliance statements", A_S_HELP0);
        self.set_box(484, "Copyrights and licenses", A_S_HELP0 + 1);
        self.set_text(Self::SET_LEFT, 672, Self::SZ_LABEL, true, "Tutorial", 20);
        self.set_box(698, "Getting started tour and tooltips", A_S_HELP0 + 2);
        self.set_para(
            Self::SET_LEFT,
            881,
            "For troubleshooting, support and FAQ go to https://",
        );
        self.set_para(Self::SET_LEFT, 920, "support.remarkable.com");
    }

    // === P5 begin: Developer 页 / Experimental 页 / Factory reset 确认框 ===

    /// Developer 页（官方 `res_06887bf.qml`，Prod 构建里就三块）。
    ///
    /// 1. `SSH root password`：官方是 xochitl 按序列号派生的，我们没有 → `Not available`
    /// 2. `IP addresses` + `MAC address`：真读（`ip -o -4 addr show` / sysfs）
    /// 3. `Design grid` 开关：官方 UI 的调试网格，没实现 → `Not available`
    ///
    /// 官方第 4 块 `Developer mode` 只在非 Prod 构建显示，我们和 Prod 一致地不画。
    fn set_page_developer(&mut self) {
        self.set_center(874, 162, Self::SZ_TITLE, true, developer::TITLE, 20);

        // 第 1 块：SSH root password（拿不到，灰显）
        self.set_text(
            Self::SET_LEFT,
            329,
            Self::SZ_LABEL,
            true,
            developer::SSH_PASSWORD,
            20,
        );
        self.set_info_box(355, developer::NOT_AVAILABLE, 120);

        // 第 2 块：IP / MAC（真值，读不到才灰显）
        let ips = developer::ip_addresses();
        let ip_text = if ips.is_empty() {
            developer::NOT_AVAILABLE.to_string()
        } else {
            ips.join(", ")
        };
        let ip_grey = ips.is_empty();
        self.set_text(
            Self::SET_LEFT,
            543,
            Self::SZ_LABEL,
            true,
            developer::IP_ADDRESSES,
            20,
        );
        self.set_info_box(569, &ip_text, if ip_grey { 120 } else { 20 });

        let mac = developer::mac_address();
        self.set_text(
            Self::SET_LEFT,
            757,
            Self::SZ_LABEL,
            true,
            developer::MAC_ADDRESS,
            20,
        );
        self.set_info_box(
            783,
            mac.as_deref().unwrap_or(developer::NOT_AVAILABLE),
            if mac.is_some() { 20 } else { 120 },
        );

        // 第 3 块：Design grid（开关行 + 官方副标题，没实现 → 右侧灰字占位）
        self.set_text(
            Self::SET_LEFT,
            1000,
            Self::SZ_LABEL,
            true,
            developer::DESIGN_GRID,
            120,
        );
        self.set_text(
            Self::SET_LEFT,
            1048,
            28.0,
            false,
            developer::DESIGN_GRID_TEXT,
            120,
        );
        let right = Self::SET_LEFT + Self::SET_W - 40;
        let tw = self.set_measure(Self::SZ_LABEL, developer::NOT_AVAILABLE);
        self.set_text(
            right - tw,
            1000,
            Self::SZ_LABEL,
            false,
            developer::NOT_AVAILABLE,
            120,
        );
    }

    /// Experimental 页（官方 `res_0686587.qml`）：标题 + 官方说明 + 7 个开关。
    ///
    /// 版面按 `experimental::ROWS` 循环：带副标题的行高 132，不带的 90。
    /// 开关状态落 `~/.config/rmdev/experimental`（见 `settings::experimental`）。
    fn set_page_experimental(&mut self, s: &SettingsView) {
        self.set_center(874, 162, Self::SZ_TITLE, true, experimental::TITLE, 20);
        self.set_wrap(
            Self::SET_LEFT,
            240,
            28.0,
            experimental::DESCRIPTION,
            Self::SET_W,
            42,
        );
        let mut y = 400;
        for (i, row) in experimental::ROWS.iter().enumerate() {
            let h = if row.text.is_empty() { 90 } else { 132 };
            let base = y + 52;
            self.set_text(
                Self::SET_LEFT,
                base,
                Self::SZ_LABEL,
                true,
                row.header,
                20,
            );
            if !row.text.is_empty() {
                self.set_text(Self::SET_LEFT, base + 44, 28.0, false, row.text, 20);
            }
            // 胶囊/圆点用纯黑：翻开关走 A2 二值快刷（同 `set_toggle_full`）
            self.set_switch(Self::SET_PILL_X, base - 34, s.experimental.get(i), 0);
            self.hits.push(345, y, 1059, h, A_S_EXP0 + i as u32);
            y += h;
        }
    }

    /// Factory reset 确认框（官方 `ResetDeviceDialog`，文案/按钮见 `settings::reset`）。
    ///
    /// 整屏白底：图标 40 + 标题 38 + 正文两行 32 + 小字 28 + 底部两个 351×87 按钮，
    /// 和关机框（`panels::poweroff`）同一套锚点，所以两块弹框看起来是一家的。
    fn set_factory_confirm(&mut self) {
        let (w, h) = (self.fb.width, self.fb.height);
        self.fb.fill_rect(0, 0, w, h, gray565(255));
        si_reload(&mut self.fb, w / 2 - 20, 200, 20);
        self.set_center(w / 2, 340, Self::SZ_TITLE, true, reset::TITLE, 20);
        for (i, line) in reset::DESCRIPTION.iter().enumerate() {
            self.set_center(
                w / 2,
                430 + 49 * i as i32,
                Self::SZ_PARA,
                false,
                line,
                20,
            );
        }
        self.set_center(w / 2, 540, 28.0, false, reset::SUB, 20);

        let (yes, no) = reset::buttons(w, h);
        // Yes：黑底白字（官方 `highlight: false`）
        self.fb.fill_rect(yes.0, yes.1, yes.2, yes.3, gray565(0));
        self.set_center(
            yes.0 + yes.2 / 2,
            yes.1 + yes.3 / 2 + 9,
            25.0,
            true,
            reset::YES,
            255,
        );
        // No：白底黑框（官方 `highlight: true`）
        widgets::outline_rect(&mut self.fb, no.0, no.1, no.2, no.3, 2, 30);
        self.set_center(
            no.0 + no.2 / 2,
            no.1 + no.3 / 2 + 9,
            25.0,
            true,
            reset::NO,
            20,
        );
        self.hits.push(yes.0, yes.1, yes.2, yes.3, A_S_FACTORY_YES);
        self.hits.push(no.0, no.1, no.2, no.3, A_S_FACTORY_NO);
    }

    /// 设置页顶部那条黑 Header（官方 `Header` 组件：黑底 + 居中白字）。
    fn set_top_banner(&mut self, msg: &str) {
        let w = self.fb.width;
        self.fb.fill_rect(0, 0, w, 118, gray565(0));
        self.set_center(w / 2, 73, 32.0, true, msg, 255);
        self.hits.push(0, 0, w, 118, A_S_BANNER);
    }

    /// 值框（同 `set_box`，但没有 ›，也不可点）：给 Developer 页的只读信息行用。
    fn set_info_box(&mut self, y: i32, value: &str, tone: u8) {
        widgets::outline_rect(&mut self.fb, Self::SET_LEFT, y, Self::SET_W, Self::SET_H, 2, 30);
        self.set_text(Self::SET_VALUE_X, y + 58, Self::SZ_VALUE, false, value, tone);
    }

    /// Factory reset：**只清 `~/.config/rmdev`**（`settings::reset::clear_config`），
    /// 用户文档一字节不碰、设备不重启，然后回设置首页，顶部给一条完成提示。
    fn do_factory_reset(&mut self) {
        if let Screen::Settings(s) = &mut self.screen {
            s.factory_confirm = false;
        }
        match reset::clear_config() {
            Ok(()) => {
                eprintln!(
                    "[settings] Factory reset：已清空 {}（用户文档未动）",
                    rmdev::settings::config_dir().display()
                );
                // 按"从没写过配置"的第一跑状态重建设置页（字号回到正常大小）
                set_font_scale(100);
                self.open_settings();
                if let Screen::Settings(s) = &mut self.screen {
                    s.factory_done = Some((
                        reset::DONE.to_string(),
                        std::time::Instant::now() + Duration::from_secs(6),
                    ));
                    // App 里那几份内存副本也跟着回默认值
                    self.auto_sleep = s.on(2, 1);
                    self.show_battery_pct = s.on(2, 0);
                    self.tail_eraser = s.prefs.tail_eraser;
                }
                self.draw_settings();
            }
            Err(e) => {
                eprintln!("[settings] Factory reset 失败：{e}");
                self.set_notice(format!("Could not reset settings: {e}"));
                self.draw_settings();
            }
        }
    }

    // === P5 end ===

    // ---------- 密码 / 语言 / Help 的二级页

    /// 设置/修改/关闭密码（全屏，数字键盘）。密码文件删掉即恢复，见 `settings::security`。
    fn set_pin_page(&mut self, s: &SettingsView) {
        let Some(p) = s.pin.as_ref() else { return };
        let w = self.fb.width;
        // 官方 Header：黑条 + 左 Cancel + 居中标题。
        self.fb.fill_rect(0, 0, w, 118, gray565(0));
        self.set_text(42, 73, Self::SZ_SIDE, true, "Cancel", 255);
        self.hits.push(0, 0, Self::SET_SBW, 118, A_S_PIN_CANCEL);
        self.set_center(702, 73, 32.0, true, p.header(), 255);
        self.set_center(702, 280, Self::SZ_LABEL, true, p.label(), 20);
        self.set_pin_dots(360, p.entered.len());
        if !p.msg.is_empty() {
            self.set_center(702, 470, 30.0, false, &p.msg, 20);
        }
        self.set_keypad(540, A_S_PIN0, A_S_PIN_DEL);
    }

    /// 语言选择（全屏列表 + 选中打勾）。只存偏好，不真的换 UI 语言。
    fn set_lang_page(&mut self, s: &SettingsView, kind: LangKind) {
        self.set_back_row(A_S_LANG_BACK);
        let (title, list, sel) = match kind {
            LangKind::Handwriting => (
                "Handwriting conversion language",
                &language::HANDWRITING[..],
                s.lang_hw,
            ),
            LangKind::Keyboard => ("Keyboard layout", &language::KEYBOARDS[..], s.lang_kb),
        };
        self.set_center(702, 162, Self::SZ_TITLE, true, title, 20);
        for (i, l) in list.iter().enumerate() {
            let (col, row) = (i % 2, i / 2);
            let (x, y) = (144 + 576 * col as i32, 260 + 110 * row as i32);
            if i == sel {
                widgets::outline_rect(&mut self.fb, x, y, 540, 90, 3, 20);
                si_check(&mut self.fb, x + 486, y + 45, 20);
            }
            self.set_text(x + 28, y + 58, Self::SZ_LABEL, i == sel, l.name, 20);
            self.hits.push(x, y, 540, 90, A_S_LANG_ITEM0 + i as u32);
        }
    }

    /// Help 的三个二级页：About（真机信息）/ 许可 / 教程。
    fn set_help_page(&mut self, s: &SettingsView, page: HelpPage) {
        self.set_back_row(A_S_HELP_BACK);
        match page {
            HelpPage::About => {
                self.set_center(702, 162, Self::SZ_TITLE, true, "About this device", 20);
                let build = help::format_build(&s.help.build);
                let rows: [(&str, &str); 4] = [
                    ("Model number", &s.help.model),
                    ("Serial number", &s.help.serial),
                    ("Software version", &s.help.version),
                    ("Build", &build),
                ];
                for (i, (label, value)) in rows.iter().enumerate() {
                    let y = 290 + 84 * i as i32;
                    self.set_text(144, y, Self::SZ_LABEL, true, label, 20);
                    self.set_text(620, y, Self::SZ_PARA, false, value, 20);
                }
                self.set_text(144, 700, Self::SZ_LABEL, true, "Compliance statements", 20);
                self.set_wrap(144, 760, 30.0, help::COMPLIANCE_TEXT, 1100, 42);
                self.set_text(
                    144,
                    1660,
                    26.0,
                    false,
                    "See lower back of the device and visit support.remarkable.com for details.",
                    20,
                );
            }
            HelpPage::Licenses => {
                self.set_center(702, 162, Self::SZ_TITLE, true, "Copyrights and licenses", 20);
                self.set_wrap(144, 290, 30.0, help::LICENSE_TEXT, 1116, 44);
                self.set_text(144, 760, Self::SZ_LABEL, true, "Open source licenses", 20);
                let mut y = 820;
                for name in [
                    "GNU General Public License, version 3",
                    "GNU Lesser General Public License, version 3",
                    "MIT License",
                    "BSD 2-Clause License",
                    "Qt 5.15 (LGPLv3)",
                ] {
                    self.set_text(180, y, Self::SZ_PARA, false, name, 20);
                    y += 54;
                }
            }
            HelpPage::Tutorial => {
                self.set_center(702, 162, Self::SZ_TITLE, true, "Tutorial", 20);
                self.set_wrap(144, 300, 30.0, help::TUTORIAL_TEXT, 1116, 44);
                self.set_button(546, 900, 312, 90, "Restart the tutorial", 0, A_S_TUTORIAL);
            }
        }
    }

    // ---------- General 点进去的三个子页

    /// "Sync server" 页（官方这页是 Account / 配对 reMarkable 云）。
    /// 我们没有任何云账号，所以整页换成自建服务器：状态 + 配对 / 立即同步 / 解除。
    fn set_page_account(&mut self, s: &SettingsView) {
        self.set_center(Self::SET_SUB_CX, 162, Self::SZ_TITLE, true, "Sync server", 20);
        si_server(&mut self.fb, Self::SET_SUB_X, 297, 20);
        let cfg = &s.server;
        if cfg.configured() {
            let host = cfg
                .url
                .trim_start_matches("http://")
                .trim_end_matches('/')
                .to_string();
            self.set_text(
                Self::SET_SUB_X + 71,
                329,
                Self::SZ_LABEL,
                true,
                &format!("Connected to {host}"),
                20,
            );
            self.set_para(
                Self::SET_SUB_X + 70,
                388,
                "Notes, documents and page thumbnails are synced",
            );
            self.set_para(
                Self::SET_SUB_X + 70,
                437,
                "to a server you control. No account, no cloud.",
            );
            let last = if cfg.last_rev > 0 {
                format!("Last sync: revision {}", cfg.last_rev)
            } else {
                "Not synced yet".to_string()
            };
            self.set_text(Self::SET_SUB_X + 70, 530, Self::SZ_PARA, false, &last, 90);
            self.set_button(560, 570, 312, 90, "Sync now", 0, A_S_SYNC_NOW);
            self.set_button(896, 570, 312, 90, "Unpair", 0, A_S_UNPAIR);
        } else {
            self.set_text(
                Self::SET_SUB_X + 71,
                329,
                Self::SZ_LABEL,
                true,
                "Not connected",
                20,
            );
            self.set_para(
                Self::SET_SUB_X + 70,
                388,
                "Pair this device with a server you run",
            );
            self.set_para(
                Self::SET_SUB_X + 70,
                437,
                "yourself. Notes, handwriting and email",
            );
            self.set_para(
                Self::SET_SUB_X + 70,
                486,
                "all go through it.",
            );
            self.set_button(948, 295, 312, 90, "Pair device", 0, A_S_PAIR);
        }

        // 恢复出厂：纯本地操作，和商业逻辑无关，保留。
        // 已配对时上面多两个按钮，整块要往下让开。
        let fy = if cfg.configured() { 800 } else { 636 };
        si_reload(&mut self.fb, 144, fy, 20);
        self.set_text(Self::SET_SUB_X + 71, fy + 30, Self::SZ_LABEL, true, "Factory reset", 20);
        // === P5 begin: 说明文字换成官方 `res_06852b1.qml` 原文 ===
        // 官方是"清整个库"，我们只清 `~/.config/rmdev`（差异见 `settings::reset`）。
        self.set_para(Self::SET_SUB_X + 70, fy + 92, reset::PANEL_TEXT_1);
        self.set_para(Self::SET_SUB_X + 70, fy + 137, reset::PANEL_TEXT_2);
        // === P5 end ===
        self.set_button(916, fy - 4, 344, 90, "Factory reset", 0, A_S_FACTORY);
    }

    /// "Features" 页（官方这页是 Subscription / 卖 Connect）。
    /// 没有订阅：所有功能直接就是可用的，缺的只是自建服务器还没配对。
    fn set_page_subscription(&mut self, s: &SettingsView) {
        self.set_center(Self::SET_SUB_CX, 146, Self::SZ_TITLE, true, "Features", 20);
        self.set_center(
            Self::SET_SUB_CX,
            241,
            Self::SZ_TITLE,
            true,
            "All features unlocked",
            20,
        );
        self.set_center(
            Self::SET_SUB_CX,
            296,
            Self::SZ_PARA,
            false,
            "No subscription and no account. Everything runs on this",
            20,
        );
        self.set_center(
            Self::SET_SUB_CX,
            339,
            Self::SZ_PARA,
            false,
            "device or on a server you run yourself.",
            20,
        );

        let ready = if s.server.configured() { "Ready" } else { "Needs a server" };
        let rows: [(&str, &str); 4] = [
            ("Sync and version history", ready),
            ("Convert handwriting to text", ready),
            ("Send pages by email", ready),
            ("Screen sharing", "Ready"),
        ];
        for (i, (name, status)) in rows.iter().enumerate() {
            let y = 452 + 132 * i as i32;
            self.set_text(Self::SET_SUB_X + 30, y, Self::SZ_LABEL, true, name, 20);
            self.set_text(Self::SET_SUB_X + 30, y + 46, Self::SZ_PARA, false, status, 110);
            self.fb.fill_rect(Self::SET_SUB_X + 30, y + 74, Self::SET_SUB_W - 60, 1, gray565(225));
        }
        self.set_center(
            Self::SET_SUB_CX,
            1010,
            Self::SZ_PARA,
            false,
            "All of these used to require a paid subscription. They are",
            110,
        );
        self.set_center(
            Self::SET_SUB_CX,
            1053,
            Self::SZ_PARA,
            false,
            "implemented locally, so there is nothing to buy.",
            110,
        );
    }

    fn set_page_software(&mut self, s: &SettingsView) {
        self.set_center(Self::SET_SUB_CX, 162, Self::SZ_TITLE, true, "Software", 20);
        si_tablet(&mut self.fb, Self::SET_SUB_X, 297, 20);
        let ver = format!("Version {}", s.version);
        self.set_text(Self::SET_SUB_X + 71, 329, Self::SZ_LABEL, true, &ver, 20);
        self.set_text(Self::SET_SUB_X + 70, 380, 28.0, false, "Release notes", 20);
        self.fb.fill_rect(Self::SET_SUB_X + 70, 385, 176, 2, gray565(0));
        self.hits.push(Self::SET_SUB_X + 60, 350, 420, 80, A_S_RELNOTES);
        si_reload(&mut self.fb, 144, 492, 20);
        self.set_toggle_full(
            1107,
            Self::SET_SUB_X + 70,
            520,
            "Automatic updates",
            None,
            s.on(8, 0),
            A_S_TOGGLE0 + 32,
            (Self::SET_SUB_X, 480, Self::SET_SUB_W, 90),
        );
        si_cogs(&mut self.fb, Self::SET_SUB_X, 620, 20);
        self.set_toggle_full(
            1107,
            Self::SET_SUB_X + 70,
            653,
            "Advanced",
            None,
            s.on(8, 1),
            A_S_TOGGLE0 + 33,
            (Self::SET_SUB_X, 613, Self::SET_SUB_W, 90),
        );
    }

    fn open_settings(&mut self) {
        let (used, total) = disk::read();
        let saved = prefs::load_opt();
        let p = saved.clone().unwrap_or_default();
        // 没写过偏好 = 首次：按官方截图高亮 Large，但字号保持正常（重启后才放大）。
        let font_large = saved.as_ref().map(|p| p.font_large).unwrap_or(true);
        if saved.is_some() {
            set_font_scale(if font_large { FONT_SCALE_LARGE } else { 100 });
        }
        let info = help::read();
        let wifi_on = wifi::is_enabled().unwrap_or(true);
        let ssid = wifi::current().unwrap_or_else(|| "Campus-WLAN".into());
        if env_flag("RM_DEBUG") {
            eprintln!(
                "[settings] passcode={} font_large={font_large} wifi={wifi_on} ssid={ssid}",
                security::is_enabled()
            );
        }
        self.screen = Screen::Settings(Box::new(SettingsView {
            tab: 0,
            // 每个 tab 一份开关状态（group = tab；Software 子页是第 8 组）
            toggles: vec![
                vec![p.flight_mode], // 0 General：Flight mode
                vec![wifi_on],       // 1 Wi-Fi（真实 rfkill 状态）
                vec![p.battery_percentage, p.auto_sleep, p.auto_power_off], // 2 Battery
                vec![p.usb_web, p.sync_before_sleep], // 3 Storage
                vec![security::is_enabled()], // 4 Security：Passcode
                vec![],              // 5 Language
                vec![],              // 6 Accessibility
                vec![],              // 7 Help
                vec![false, false],  // 8 Software 子页
            ],
            right_handed: p.right_handed,
            font_large,
            sub: None,
            version: info.version.clone(),
            disk_used: used,
            disk_total: total,
            ssid: ssid.clone(),
            server: rmdev::net::Config::load(),
            wifi_signal: None,
            networks: Vec::new(),
            // 快照：同步读一次 440 ms，进设置会卡半秒（见 `BATT` 的注释）。
            battery: battery_cached(),
            lang_hw: language::index_of(&language::HANDWRITING, &p.handwriting_lang),
            lang_kb: language::index_of(&language::KEYBOARDS, &p.keyboard_lang),
            lang_dialog: None,
            help_page: None,
            help: info,
            sync_msg: None,
            pin: None,
            notice: None,
            prefs: p,
            // === P5 begin: Experimental 开关从磁盘读；确认框/完成提示每次进设置都清掉 ===
            factory_done: None,
            factory_confirm: false,
            experimental: experimental::load(),
            // === P5 end ===
            // === C0W begin: 确认框/连接中状态每次进设置都清掉 ===
            wifi_forget: None,
            connecting: None,
            // === C0W end ===
        }));
        // === P3 begin ===
        // 状态栏图标直达 Battery / Wi-Fi：调用方先把 tab 号塞进 `pending_settings_tab`，
        // 这里在画第一帧之前套上，免得先闪一下 General 再跳过去。
        if let Some(tab) = self.pending_settings_tab.take() {
            if let Screen::Settings(s) = &mut self.screen {
                s.tab = tab;
                s.sub = None;
            }
        }
        // === P3 end ===
        self.draw_settings();
    }

    /// 重新扫 Wi-Fi（进入 Wi-Fi tab / 打开开关时调用）。会阻塞 1~3 秒，
    /// 调用方先画一帧再进来，至少不会「点了没反应」。
    fn settings_refresh_wifi(&mut self) {
        let on = match &self.screen {
            Screen::Settings(s) => s.on(1, 0),
            _ => return,
        };
        if !on {
            return;
        }
        // 快的那部分（读当前连接）同步做：`iw link` 只是读一次驱动状态。
        let link = wifi::current_link();
        let current = link
            .as_ref()
            .map(|(s, _)| s.clone())
            .or_else(wifi::current);
        if let Screen::Settings(s) = &mut self.screen {
            if let Some((ssid, sig)) = link {
                s.ssid = ssid;
                s.wifi_signal = sig;
            } else if let Some(ssid) = current {
                s.ssid = ssid;
                s.wifi_signal = None;
            }
        }
        // 慢的那部分（主动扫描 1~3 s）扔后台。已经在扫就不再起一个，
        // 否则来回点 tab 会攒一堆扫描线程。
        if self.wifi_scan.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("wifi-scan".into())
            .spawn(move || {
                let t = std::time::Instant::now();
                let nets = wifi::scan();
                eprintln!(
                    "[wifi] 扫描 {} 个网络，{} ms",
                    nets.len(),
                    t.elapsed().as_millis()
                );
                let _ = tx.send(nets);
            })
            .ok();
        self.wifi_scan = Some(rx);
    }

    /// 收后台扫描的结果。每轮空闲都调一次（`tick`）。
    fn poll_wifi_scan(&mut self) {
        let Some(rx) = &self.wifi_scan else { return };
        let nets = match rx.try_recv() {
            Ok(n) => n,
            Err(std::sync::mpsc::TryRecvError::Empty) => return,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.wifi_scan = None;
                return;
            }
        };
        self.wifi_scan = None;
        if let Screen::Settings(s) = &mut self.screen {
            s.networks = nets;
        }
        if matches!(&self.screen, Screen::Settings(s) if s.tab == 1 && s.sub.is_none()) {
            // 这里**不能**走 `settings_fast`（A2）：网络名字是 `tone 20` 的灰阶
            // 文字，二值波形画不出来。扫描结果是一锤子买卖，走整页 GC16。
            self.draw_settings();
        }
    }

    // === P1 begin ===

    /// 开 / 关 USB web 服务（设置页 Storage 那个 `USB web interface` 开关）。
    fn usb_web_apply(&mut self, on: bool) {
        self.usb_web_on = on;
        if !on {
            if let Some(h) = self.usb_web.take() {
                h.stop();
                eprintln!("[webui] USB web interface 关，服务已停");
            }
            self.usb_web_note = None;
            return;
        }
        // 打开时立刻核对一次，别让用户等下一个 5 s。
        self.usb_web_check = std::time::Instant::now() - Duration::from_secs(5);
        self.poll_usb_web();
    }

    /// 每 5 s 对一次账：开关开着 + `10.11.99.1` 在 + 服务没起 → 起；反之停。
    ///
    /// `10.11.99.1` 是 USB gadget 网卡（`usb0`）的固定地址，只有插线才有。
    /// 拔线后地址消失就停服务——否则会一直挂在一个不存在的地址上。
    fn poll_usb_web(&mut self) {
        if self.usb_web_check.elapsed() < Duration::from_secs(5) {
            return;
        }
        self.usb_web_check = std::time::Instant::now();
        if !self.usb_web_on {
            return;
        }
        // UDP bind 到本机不存在的地址会 EADDRNOTAVAIL，比 fork 一个 `ip addr` 便宜。
        let link_up = std::net::UdpSocket::bind("10.11.99.1:0").is_ok();
        if !link_up {
            if let Some(h) = self.usb_web.take() {
                h.stop();
                eprintln!("[webui] USB 线已拔，服务停");
                self.usb_web_note = None;
            }
            return;
        }
        if self.usb_web.is_some() {
            return;
        }
        let Ok(addr) = rmdev::webui::DEFAULT_ADDR.parse() else {
            return;
        };
        let cfg = rmdev::webui::Config {
            // 库根只有一份：`Library::new()` 认 `RMDEV_LIB_ROOT`，和其余模块一致。
            root: self.library.root.clone(),
            addr,
        };
        match rmdev::webui::start(cfg) {
            Ok(h) => {
                eprintln!("[webui] USB web interface http://{}", h.addr().ip());
                self.usb_web = Some(h);
                self.usb_web_note = None;
            }
            Err(e) => {
                // 每 5 s 会重试；只有原因变了才记一行，免得刷屏。
                if self.usb_web_note.as_deref() != Some(e.as_str()) {
                    eprintln!("[webui] 起服务失败：{e}");
                    self.usb_web_note = Some(e);
                }
            }
        }
    }

    // === P1 end ===

    /// 把视图里的开关落成 `~/.config/rmdev/settings`。
    fn persist_settings(s: &SettingsView) {
        let mut p = s.prefs.clone();
        p.font_large = s.font_large;
        p.flight_mode = s.on(0, 0);
        p.battery_percentage = s.on(2, 0);
        p.auto_sleep = s.on(2, 1);
        p.auto_power_off = s.on(2, 2);
        p.usb_web = s.on(3, 0);
        p.sync_before_sleep = s.on(3, 1);
        p.handwriting_lang = language::HANDWRITING
            [s.lang_hw.min(language::HANDWRITING.len() - 1)]
        .code
        .into();
        p.keyboard_lang = language::KEYBOARDS[s.lang_kb.min(language::KEYBOARDS.len() - 1)]
            .code
            .into();
        if let Err(e) = prefs::save(&p) {
            eprintln!("[settings] 保存偏好失败：{e}");
        }
    }

    fn set_notice(&mut self, msg: impl Into<String>) {
        if let Screen::Settings(s) = &mut self.screen {
            s.notice = Some((msg.into(), std::time::Instant::now() + Duration::from_secs(4)));
        }
    }

    /// 打开密码输入全屏页（Set / Change / Remove）。
    fn open_pin(&mut self, mode: PinMode) {
        if let Screen::Settings(s) = &mut self.screen {
            s.pin = Some(PinUi::new(mode));
        }
        self.draw_settings();
    }

    /// 4 位输满后的状态机。成功时落盘并更新 Security 开关。
    fn pin_submit(&mut self) {
        // 局部枚举：借 `s.pin` 的同时还要改 `s.toggles`，所以先算结果再落地。
        enum Outcome {
            Stay,
            Done { enabled: bool, msg: &'static str },
        }
        let now = std::time::Instant::now();
        let outcome = {
            let Screen::Settings(s) = &mut self.screen else {
                return;
            };
            let Some(p) = s.pin.as_mut() else { return };
            if !p.lockout.can_try(now) {
                return;
            }
            let code = p.entered.clone();
            let wrong = |p: &mut PinUi| {
                let locked = p.lockout.record_failure(now);
                p.entered.clear();
                p.msg = if locked {
                    format!("Try again in {} seconds.", security::LOCK_SECS)
                } else {
                    "Wrong passcode. Try again.".into()
                };
            };
            match p.mode {
                PinMode::Remove => {
                    if security::verify(&code) {
                        match security::clear() {
                            Ok(()) => Outcome::Done {
                                enabled: false,
                                msg: "Your passcode has been disabled",
                            },
                            Err(e) => {
                                p.msg = format!("Could not remove passcode: {e}");
                                p.entered.clear();
                                Outcome::Stay
                            }
                        }
                    } else {
                        wrong(p);
                        Outcome::Stay
                    }
                }
                PinMode::Set => match p.stage {
                    0 => {
                        p.first = code;
                        p.entered.clear();
                        p.msg.clear();
                        p.stage = 1;
                        Outcome::Stay
                    }
                    _ => {
                        if code == p.first {
                            match security::set(&code) {
                                Ok(()) => Outcome::Done {
                                    enabled: true,
                                    msg: "Your passcode has been updated",
                                },
                                Err(e) => {
                                    p.msg = format!("Could not save passcode: {e}");
                                    p.entered.clear();
                                    Outcome::Stay
                                }
                            }
                        } else {
                            p.stage = 0;
                            p.first.clear();
                            p.entered.clear();
                            p.msg = "Passcodes did not match. Try again.".into();
                            Outcome::Stay
                        }
                    }
                },
                PinMode::Change => match p.stage {
                    0 => {
                        if security::verify(&code) {
                            p.stage = 1;
                            p.entered.clear();
                            p.msg.clear();
                        } else {
                            wrong(p);
                        }
                        Outcome::Stay
                    }
                    1 => {
                        p.first = code;
                        p.entered.clear();
                        p.msg.clear();
                        p.stage = 2;
                        Outcome::Stay
                    }
                    _ => {
                        if code == p.first {
                            match security::set(&code) {
                                Ok(()) => Outcome::Done {
                                    enabled: true,
                                    msg: "Your passcode has been updated",
                                },
                                Err(e) => {
                                    p.msg = format!("Could not save passcode: {e}");
                                    p.entered.clear();
                                    Outcome::Stay
                                }
                            }
                        } else {
                            p.stage = 1;
                            p.first.clear();
                            p.entered.clear();
                            p.msg = "Passcodes did not match. Try again.".into();
                            Outcome::Stay
                        }
                    }
                },
            }
        };
        if let Outcome::Done { enabled, msg } = outcome {
            if let Screen::Settings(s) = &mut self.screen {
                s.pin = None;
                if let Some(v) = s.toggles.get_mut(4).and_then(|v| v.get_mut(0)) {
                    *v = enabled;
                }
                s.notice =
                    Some((msg.to_string(), std::time::Instant::now() + Duration::from_secs(4)));
            }
        }
        self.draw_settings();
    }

    fn tap_settings(&mut self, x: f32, y: f32) {
        let hit = self.hits.take(x, y);
        let Some(a) = hit else { return };

        // === C0W begin: 忘记网络确认框是模态的，只认 Forget / Cancel ===
        if matches!(&self.screen, Screen::Settings(s) if s.wifi_forget.is_some()) {
            self.wifi_forget_action(a);
            return;
        }
        // === C0W end ===

        // ---- 密码全屏页：只认 Cancel / 数字 / Del
        if matches!(&self.screen, Screen::Settings(s) if s.pin.is_some()) {
            match a {
                A_S_PIN_CANCEL => {
                    if let Screen::Settings(s) = &mut self.screen {
                        s.pin = None;
                    }
                    self.draw_settings();
                }
                A_S_PIN_DEL => {
                    if let Screen::Settings(s) = &mut self.screen {
                        if let Some(p) = s.pin.as_mut() {
                            p.entered.pop();
                        }
                    }
                    self.draw_settings();
                }
                _ if (A_S_PIN0..A_S_PIN0 + 10).contains(&a) => {
                    let d = (a - A_S_PIN0) as u8;
                    let now = std::time::Instant::now();
                    let mut full = false;
                    if let Screen::Settings(s) = &mut self.screen {
                        if let Some(p) = s.pin.as_mut() {
                            if p.lockout.can_try(now) && p.entered.len() < 4 {
                                // 重新输入时把上一次的错误/倒计时提示清掉。
                                p.msg.clear();
                                p.entered.push((b'0' + d) as char);
                                full = p.entered.len() == 4;
                            }
                        }
                    }
                    if full {
                        self.pin_submit();
                    } else {
                        self.draw_settings();
                    }
                }
                _ => {}
            }
            return;
        }

        // === P5 begin: Factory reset 确认框：只认两个按钮 ===
        if matches!(&self.screen, Screen::Settings(s) if s.factory_confirm) {
            match a {
                A_S_FACTORY_YES => self.do_factory_reset(),
                A_S_FACTORY_NO => {
                    if let Screen::Settings(s) = &mut self.screen {
                        s.factory_confirm = false;
                    }
                    self.draw_settings();
                }
                _ => {}
            }
            return;
        }
        // === P5 end ===

        // ---- 语言列表页
        if matches!(&self.screen, Screen::Settings(s) if s.lang_dialog.is_some()) {
            match a {
                A_S_LANG_BACK => {
                    if let Screen::Settings(s) = &mut self.screen {
                        s.lang_dialog = None;
                    }
                }
                _ if (A_S_LANG_ITEM0..A_S_LANG_ITEM0 + 16).contains(&a) => {
                    let idx = (a - A_S_LANG_ITEM0) as usize;
                    if let Screen::Settings(s) = &mut self.screen {
                        match s.lang_dialog {
                            Some(LangKind::Handwriting) => s.lang_hw = idx,
                            Some(LangKind::Keyboard) => s.lang_kb = idx,
                            None => {}
                        }
                        s.lang_dialog = None;
                    }
                    if let Screen::Settings(s) = &self.screen {
                        Self::persist_settings(s);
                    }
                }
                _ => return,
            }
            self.draw_settings();
            return;
        }

        // ---- Help 二级页
        if matches!(&self.screen, Screen::Settings(s) if s.help_page.is_some()) {
            match a {
                A_S_HELP_BACK => {
                    if let Screen::Settings(s) = &mut self.screen {
                        s.help_page = None;
                    }
                    self.draw_settings();
                }
                A_S_TUTORIAL => {
                    if let Screen::Settings(s) = &mut self.screen {
                        s.help_page = None;
                    }
                    self.set_notice("The tutorial will restart next time you open a notebook");
                    self.draw_settings();
                }
                _ => {}
            }
            return;
        }

        // === P5 begin: 顶部完成提示：点一下只收提示，别顺手触发底下的 Back ===
        if a == A_S_BANNER {
            if let Screen::Settings(s) = &mut self.screen {
                s.factory_done = None;
            }
            self.draw_settings();
            return;
        }
        // === P5 end ===

        match a {
            A_SETTINGS_BACK => {
                self.screen = Screen::Library;
                self.view = View::MyFiles;
                self.draw_library();
                return;
            }
            A_S_SUBBACK => {
                if let Screen::Settings(s) = &mut self.screen {
                    s.sub = None;
                }
                self.draw_settings();
                return;
            }
            A_S_ACCOUNT | A_S_SUBSCRIPTION | A_S_SOFTWARE => {
                if let Screen::Settings(s) = &mut self.screen {
                    s.sub = Some(match a {
                        A_S_ACCOUNT => SettingsSub::Account,
                        A_S_SUBSCRIPTION => SettingsSub::Subscription,
                        _ => SettingsSub::Software,
                    });
                }
                self.draw_settings();
                return;
            }
            A_S_CHECK_SYNC => {
                let online = match &self.screen {
                    Screen::Settings(s) => s.on(1, 0) && !s.ssid.is_empty(),
                    _ => false,
                };
                let msg = if online {
                    "No syncing errors found. If you still\nexperience problems, try restarting your device."
                } else {
                    "See if your reMarkable is syncing\nproperly. Requires a Wi-Fi connection."
                };
                if let Screen::Settings(s) = &mut self.screen {
                    s.sync_msg = Some(msg.to_string());
                }
                self.draw_settings();
                return;
            }
            _ if a == A_S_LANG0 || a == A_S_LANG0 + 1 => {
                if let Screen::Settings(s) = &mut self.screen {
                    s.lang_dialog = Some(if a == A_S_LANG0 {
                        LangKind::Handwriting
                    } else {
                        LangKind::Keyboard
                    });
                }
                self.draw_settings();
                return;
            }
            _ if (A_S_HELP0..A_S_HELP0 + 3).contains(&a) => {
                if let Screen::Settings(s) = &mut self.screen {
                    s.help_page = Some(match a - A_S_HELP0 {
                        0 => HelpPage::About,
                        1 => HelpPage::Licenses,
                        _ => HelpPage::Tutorial,
                    });
                }
                self.draw_settings();
                return;
            }
            A_S_CHANGE_PASS => {
                self.open_pin(PinMode::Change);
                return;
            }
            // === C0W begin: 点扫描结果 / Join other network ===
            _ if a == A_S_JOIN || (A_S_NET0..A_S_NET0 + 16).contains(&a) => {
                // 已经在连一个了：后台线程只有一条通道，别叠第二个。
                if self.wifi_conn.is_some() {
                    return;
                }
                if a == A_S_JOIN {
                    self.open_wifi_join();
                    return;
                }
                let net = match &self.screen {
                    Screen::Settings(s) => s.networks.get((a - A_S_NET0) as usize).cloned(),
                    _ => None,
                };
                let Some(n) = net else { return };
                if n.secure {
                    // 加密网络：先输密码（官方 `Connect to <SSID>` 面板）
                    self.open_wifi_pass(&n.ssid);
                } else {
                    // 开放网络：`key_mgmt=NONE` 直接连，UI 走 `Connecting to...`
                    self.wifi_start_connect(n.ssid, true, String::new());
                }
                return;
            }
            // === C0W end ===
            _ => {}
        }
        // === P5 begin: Factory reset 按钮 → 先弹确认框（不再吃掉点击）===
        if a == A_S_FACTORY {
            if let Screen::Settings(s) = &mut self.screen {
                s.factory_confirm = true;
            }
            self.draw_settings();
            return;
        }
        // === P5 end ===
        if (A_SET_TAB0..A_SET_TAB0 + 10).contains(&a) { // === P5: 8 → 10（Developer/Experimental）===
            let tab = (a - A_SET_TAB0) as usize;
            // 点的就是当前这一页（手指抖了一下）→ 什么都不用做。
            // 以前这里无条件整页重绘 + 整屏 GC16（46 相位 ≈ 600 ms）。
            if matches!(&self.screen, Screen::Settings(s) if s.tab == tab && s.sub.is_none()) {
                return;
            }
            if let Screen::Settings(s) = &mut self.screen {
                s.tab = tab;
                s.sub = None;
            }
            self.draw_settings();
            // 进 Wi-Fi 页就起一次后台扫描：页面先画出来，扫描结果到了
            // 由 `poll_wifi_scan` 补一帧。**这里绝不能再同步扫** ——
            // 主动扫描要 1~3 秒，主循环一停，这期间的触摸全排在队列里，
            // 用户看到的就是"从 Wi-Fi 切到 Battery 卡一下"。
            if tab == 1 {
                self.settings_refresh_wifi();
            }
            return;
        }
        match a {
            A_S_PAIR => {
                self.open_pair();
                return;
            }
            A_S_UNPAIR => {
                let cfg = rmdev::net::Config::default();
                if let Err(e) = cfg.save() {
                    // === P3 begin ===
                    self.notice(NoticeKind::Error, &format!("Could not save: {e}"));
                    // === P3 end ===
                    return;
                }
                eprintln!("[net] 已解除配对");
                self.refresh_server_config();
                // === P3 begin ===
                self.notice(NoticeKind::Info, "Server unpaired");
                // === P3 end ===
                self.draw_settings();
                return;
            }
            A_S_SYNC_NOW => {
                self.start_sync();
                return;
            }
            _ => {}
        }
        if (A_S_TOGGLE0..A_S_TOGGLE0 + 36).contains(&a) {
            let slot = a - A_S_TOGGLE0;
            let (g, i) = ((slot / 4) as usize, (slot % 4) as usize);
            // Security 的开关不直接翻：先设/验密码，成功后才变。
            if g == 4 {
                let on = match &self.screen {
                    Screen::Settings(s) => s.on(4, 0),
                    _ => false,
                };
                self.open_pin(if on { PinMode::Remove } else { PinMode::Set });
                return;
            }
            let mut newv = false;
            if let Screen::Settings(s) = &mut self.screen {
                if let Some(v) = s.toggles.get_mut(g).and_then(|v| v.get_mut(i)) {
                    *v = !*v;
                    newv = *v;
                }
            }
            if g == 1 {
                // Wi-Fi：真的去 block/unblock；失败只切 UI 并给英文提示。
                match wifi::set_enabled(newv) {
                    Ok(()) => {
                        if let Screen::Settings(s) = &mut self.screen {
                            if !newv {
                                s.ssid.clear();
                                s.wifi_signal = None;
                                s.networks.clear();
                            }
                        }
                        if newv {
                            self.settings_refresh_wifi();
                        }
                    }
                    Err(e) => {
                        if let Screen::Settings(s) = &mut self.screen {
                            s.notice = Some((
                                format!("Could not change Wi-Fi state: {e}"),
                                std::time::Instant::now() + Duration::from_secs(4),
                            ));
                        }
                    }
                }
                self.draw_settings();
                return;
            }
            if let Screen::Settings(s) = &self.screen {
                Self::persist_settings(s);
                // Auto sleep 是 `App` 自己按空闲计时用的，改完立刻生效
                self.auto_sleep = s.on(2, 1);
                // === P5 begin: 状态栏画不画 NN% 也是立刻生效 ===
                self.show_battery_pct = s.on(2, 0);
                // === P5 end ===
            }
            // === P1 begin ===
            // Storage 页的 `USB web interface`（第 3 组第 0 个）：真起/停服务。
            if (g, i) == (3, 0) {
                let on = matches!(&self.screen, Screen::Settings(s) if s.on(3, 0));
                self.usb_web_apply(on);
            }
            // === P1 end ===
            // 只翻了开关：走 A2 二值快刷（12 相位 ≈ 141 ms），别整屏重绘。
            self.settings_fast = true;
            self.draw_settings();
            return;
        }
        if (A_S_EXP0..A_S_EXP0 + 7).contains(&a) {
            // === P5 begin: Experimental 页的 7 个开关：翻一下 → 立刻落盘 ===
            let i = (a - A_S_EXP0) as usize;
            let mut failed = false;
            if let Screen::Settings(s) = &mut self.screen {
                let v = !s.experimental.get(i);
                s.experimental.set(i, v);
                if let Err(e) = experimental::save(&s.experimental) {
                    eprintln!("[settings] 保存 experimental 失败：{e}");
                    s.notice = Some((
                        format!("Could not save setting: {e}"),
                        std::time::Instant::now() + Duration::from_secs(4),
                    ));
                    failed = true;
                }
            }
            // 只翻了开关 → A2 快刷；要显示灰字提示就得整页重绘
            self.settings_fast = !failed;
            self.draw_settings();
            return;
        }
        // === P5 end ===
        if (A_S_LAND0..A_S_LAND0 + 4).contains(&a) {
            let mut auto_land: Option<bool> = None;
            let mut land_dir_left: Option<bool> = None;
            let mut handed: Option<bool> = None;
            let n = a - A_S_LAND0;
            if let Screen::Settings(s) = &mut self.screen {
                if n < 2 {
                    s.prefs.auto_landscape = n == 1;
                    auto_land = Some(s.prefs.auto_landscape);
                } else {
                    s.prefs.land_dir_left = n == 2;
                    land_dir_left = Some(s.prefs.land_dir_left);
                }
            }
            if let Screen::Settings(s) = &self.screen {
                Self::persist_settings(s);
            }
            if let Some(v) = auto_land {
                self.auto_land = v;
                eprintln!("[settings] 自动横屏 = {v}");
            }
            if let Some(left) = handed {
                // `left` = 左利手 = 镜像
                if rmdev::fb::set_mirrored(left) {
                    eprintln!("[settings] 左右利手 = {}", if left { "左" } else { "右" });
                }
            }
            if let Some(left) = land_dir_left {
                self.land_dir_left = left;
                self.fb.set_land_dir_left(left);
                eprintln!("[settings] 横屏方向 = {}", if left { "左" } else { "右" });
            }
            self.draw_settings();
            return;
        }
        if (A_S_RADIO0..A_S_RADIO0 + 7).contains(&a) {
            // 这两项要同时改 App 自己的缓存（`draw_settings` 之后整页重绘），
            // 不能直接借 `self` —— `s` 已经把 `self.screen` 借走了。
            let mut auto_land: Option<bool> = None;
            let mut land_dir_left: Option<bool> = None;
            let mut handed: Option<bool> = None;
            let n = a - A_S_RADIO0;
            if let Screen::Settings(s) = &mut self.screen {
                match a - A_S_RADIO0 {
                    0 => {
                        s.right_handed = false;
                        s.prefs.right_handed = false;
                        handed = Some(true);
                    }
                    1 => {
                        s.right_handed = true;
                        s.prefs.right_handed = true;
                        handed = Some(false);
                    }
                    2 => {
                        s.font_large = false;
                        set_font_scale(100);
                    }
                    3 => {
                        s.font_large = true;
                        set_font_scale(FONT_SCALE_LARGE);
                    }
                    // 4..6：笔尾橡皮映射（刷白 / 擦整笔 / 跟上次的选择）
                    i => {
                        s.prefs.tail_eraser = match i {
                            4 => prefs::TailEraser::Eraser,
                            5 => prefs::TailEraser::Stroke,
                            _ => prefs::TailEraser::Last,
                        };
                    }
                }
            }
            if let Screen::Settings(s) = &self.screen {
                Self::persist_settings(s);
                // 写笔记那边用的是 `App` 里的内存副本，改完立刻同步，
                // 不用等下一次开设置页/重启。
                self.tail_eraser = s.prefs.tail_eraser;
            }
            if let Some(v) = auto_land {
                self.auto_land = v;
            }
            if let Some(left) = handed {
                // `left` = 左利手 = 镜像
                if rmdev::fb::set_mirrored(left) {
                    eprintln!("[settings] 左右利手 = {}", if left { "左" } else { "右" });
                }
            }
            if let Some(left) = land_dir_left {
                self.land_dir_left = left;
                // 上屏映射变了：整屏要重推一次（`set_land_dir_left` 把脏区设成全屏）
                self.fb.set_land_dir_left(left);
            }
            self.draw_settings();
            return;
        }
        // 其余（Restart / Power off / Pair device / Factory reset）对应的官方弹窗
        // 不在这次的范围内：吃掉点击，不乱跳。
    }


    // ---------- 电源键 / 睡眠

    /// 关机框独占输入：只有两个按钮。
    ///
    /// 走"按下记位置、抬起结算"和库里的点击一致；位移超过 40 px 就当拖动，
    /// 不算点（免得手滑一下就关机）。
    fn poweroff_input(&mut self, ev: InputEvent) {
        match ev {
            InputEvent::Touch(TouchEvent::Down { x, y }) => self.pending_tap = Some((x, y)),
            InputEvent::Touch(TouchEvent::Move { x, y }) => {
                if let Some((px, py)) = self.pending_tap {
                    if (x - px).abs() > 40.0 || (y - py).abs() > 40.0 {
                        self.pending_tap = None;
                    }
                }
            }
            InputEvent::Touch(TouchEvent::Up) => {
                if let Some((x, y)) = self.pending_tap.take() {
                    self.tap_poweroff(x, y);
                }
            }
            InputEvent::Pen(PenEvent::Down(p)) => self.pending_tap = Some((p.x, p.y)),
            InputEvent::Pen(PenEvent::Up) => {
                if let Some((x, y)) = self.pending_tap.take() {
                    self.tap_poweroff(x, y);
                }
            }
            _ => {}
        }
    }

    /// 电源键：睡着时唯一还认的输入。
    fn on_power(&mut self, ev: PowerEvent) {
        let now = std::time::Instant::now();
        // 唤醒那一下的按下/抬起还在队列里，等我们醒过来才被读到。不挡掉就会
        // 被判成"又按了一下"，屏幕闪一下就又睡了。
        if let Some(t) = self.power_mute {
            if now < t {
                return;
            }
            self.power_mute = None;
        }
        match ev {
            PowerEvent::Down => {
                // 关机框里电源键不做事：官方 `Keys` 处理器在 `poweroffDialog.visible`
                // 时连计时器都不启动。
                if self.poweroff {
                    return;
                }
                self.pg.press(now);
            }
            PowerEvent::Up { held_ms } => {
                let act = self.pg.release(now);
                if self.asleep.is_some() {
                    // 睡着时按一下（或按住）都是醒；不弹关机框
                    if act != rmdev::power::Act::None {
                        eprintln!("[power] 唤醒（按了 {held_ms}ms）");
                        self.wake();
                    }
                    return;
                }
                if act != rmdev::power::Act::Short {
                    return;
                }
                eprintln!("[power] 短按 {held_ms}ms → 睡");
                self.sleep_now();
            }
        }
    }

    /// 睡下去。**挂起期间这个函数不返回** —— `suspend()` 要等待唤醒。
    fn sleep_now(&mut self) {
        if self.asleep.is_some() {
            return;
        }
        // 先存盘：睡着前丢一笔是绝对不能接受的
        self.flush_page_now();
        let mode = if rmdev::power::can_suspend() {
            rmdev::power::Mode::Deep
        } else {
            eprintln!("[sleep] 内核不支持 mem 挂起，只能浅睡");
            rmdev::power::Mode::Light
        };
        self.asleep = Some(mode);
        self.draw_sleep_screen();
        // 面板线程要是正卡在波形中间，挂起会把那一相永远停在那里（醒来一块灰）。
        if !self.fb.wait_idle(Duration::from_millis(3000)) {
            eprintln!("[sleep] 面板 3 s 没空闲，仍按当前帧睡");
        }
        if mode != rmdev::power::Mode::Deep {
            return;
        }
        eprintln!("[sleep] 挂起到内存（/sys/power/state ← mem）…");
        let t = std::time::Instant::now();
        match rmdev::power::suspend() {
            Ok(()) => eprintln!("[sleep] 醒了（挂了 {:.1} s）", t.elapsed().as_secs_f64()),
            Err(e) => {
                eprintln!("[sleep] 挂起失败：{e}；退回浅睡（屏幕已停在睡眠图，按电源键唤醒）");
                self.asleep = Some(rmdev::power::Mode::Light);
                return;
            }
        }
        // `suspend()` 返回 = 内核已经恢复
        self.wake();
    }

    /// 醒来：整屏重画（挂着的时候画面是睡眠图）。
    fn wake(&mut self) {
        self.asleep = None;
        self.poweroff = false;
        self.pg.forget();
        self.power_mute = Some(std::time::Instant::now() + Duration::from_millis(600));
        self.last_ev = std::time::Instant::now();
        // 官方 Security 页原话："You will be asked for your passcode when the
        // device is turned on or awakened from sleep mode."
        if security::is_enabled() {
            self.show_lock();
        } else {
            self.repaint_current();
        }
        // === JOB begin: 醒来先同步一次 ===
        // 睡眠时 Wi-Fi 是断的：这期间服务器发布的回答（AI 任务）和设备上的
        // 引用索引全都对不上。醒来立刻拉一次，用户睁眼就是最新的库。
        self.start_sync();
        // === JOB end ===
    }

    pub fn sleeping(&self) -> bool {
        self.asleep.is_some()
    }

    /// 把当前界面整页重画一遍（唤醒、关掉关机框后用）。
    fn repaint_current(&mut self) {
        match &self.screen {
            Screen::Lock(_) => self.draw_lock(),
            Screen::Library => self.draw_library(),
            Screen::Notebook(_) => self.redraw_page(),
            Screen::Search(_) => self.draw_search(),
            Screen::Settings(_) => self.draw_settings(),
            Screen::Dialog(_) => self.draw_dialog(),
            Screen::Rename(_) => self.draw_rename(),
            Screen::Pair(_) => self.draw_pair(),
            Screen::Mail(_) => self.draw_mail(),
            // === C0W begin ===
            Screen::WifiPass(_) | Screen::WifiJoin(_) => self.draw_wifi_connect(),
            // === C0W end ===
        }
    }

    /// 画睡眠屏并推上去。官方 `res_06b6225.qml`：全屏 `suspended.png`，
    /// 加载不出来才画白底 + 40 px 的 "reMarkable is sleeping"。
    fn draw_sleep_screen(&mut self) {
        // 睡眠图是整屏竖图（suspended.png）：横屏画布里会拉变形
        self.force_portrait();
        if !self.sleep_img_loaded {
            self.sleep_img_loaded = true;
            self.sleep_img = sleepscreen::load();
            eprintln!(
                "[sleep] 睡眠图{}",
                if self.sleep_img.is_some() {
                    " suspended.png"
                } else {
                    "缺失，改用兜底文案"
                }
            );
        }
        let img = self.sleep_img.take();
        sleepscreen::draw(&mut self.fb, &mut self.text, img.as_ref());
        self.hits.clear();
        // GL16：睡眠图是白底黑字，16 级灰的波形最干净（A2 会把衬线糊掉）
        let (w, h) = (self.fb.width, self.fb.height);
        self.fb
            .refresh(0, 0, w, h, WAVEFORM_MODE_GL16, rmdev::fb::UPDATE_MODE_PARTIAL);
        self.sleep_img = img;
    }

    /// 立刻把当前页写回磁盘。
    fn flush_page_now(&mut self) {
        if let Screen::Notebook(nb) = &mut self.screen {
            if !nb.dirty || nb.text_edit.is_some() {
                return;
            }
            match self.library.save_page_at(&nb.uuid, nb.page_index, &nb.page) {
                Ok(()) => {
                    nb.dirty = false;
                    // === P11 begin ===
                    // 整页写回把 `RootText` 冲掉了：文字层补回去
                    self.p11_repair_text_layer();
                    // === P11 end ===
                }
                Err(e) => eprintln!("[dev] 睡前存盘失败：{e}"),
            }
        }
    }

    /// 长按 3 s：弹官方关机框。
    fn open_poweroff(&mut self) {
        if self.poweroff || self.asleep.is_some() {
            return;
        }
        self.poweroff = true;
        self.hits.clear();
        eprintln!("[power] 按住 3 s → 关机框");
        let (w, h) = (self.fb.width, self.fb.height);
        poweroff::paint(&mut self.fb, &mut self.text, w, h);
        self.fb
            .refresh(0, 0, w, h, WAVEFORM_MODE_GL16, rmdev::fb::UPDATE_MODE_PARTIAL);
    }

    /// 关机框里的点击。
    fn tap_poweroff(&mut self, x: f32, y: f32) {
        let (w, h) = (self.fb.width, self.fb.height);
        match poweroff::hit(x, y, w, h) {
            Some(poweroff::Act::TurnOff) => {
                eprintln!("[power] Turn off → 关机");
                self.poweroff = false;
                self.flush_page_now();
                power_shutdown();
            }
            Some(poweroff::Act::Cancel) => {
                eprintln!("[power] Cancel");
                self.poweroff = false;
                self.pg.forget();
                self.repaint_current();
            }
            None => {}
        }
    }

    // ---------- 开屏锁屏

    fn show_lock(&mut self) {
        // 锁屏是竖屏整屏版面：横屏里睡着了、醒来弹锁屏，先把画面转回来
        self.force_portrait();
        self.screen = Screen::Lock(Box::new(LockView {
            entered: String::new(),
            msg: String::new(),
            lockout: security::Lockout::default(),
            shown_secs: 0,
            show_digits: false,
        }));
        self.draw_lock();
    }

    /// 锁屏（官方 `res_06b2dac.qml` UnlockScreen 的版面，一项一项对着量）：
    ///
    /// ```text
    /// passCodeInputItem  top = 450、height = 190
    ///   "Enter passcode"  top + fontSizeH2(28)，TextHeaderLarge 38
    ///   圆圈行            label 底 + fontSizeH1(36) 再往下
    ///     72 空白 | 4 个 ⌀60 圆环（间距 25）| 72 宽的 Show / Hide
    /// 通知条（Wrong passcode. Try again.）490×84，top = 640，带挂锁图标
    /// 数字键盘          贴底（`pinKeypadHeight` 690，我们的键位 512）
    /// ```
    fn draw_lock(&mut self) {
        let (entered, msg, show) = match &self.screen {
            Screen::Lock(l) => (l.entered.clone(), l.msg.clone(), l.show_digits),
            _ => return,
        };
        let (w, h) = (self.fb.width, self.fb.height);
        self.fb.fill_rect(0, 0, w, h, gray565(255));
        self.hits.clear();
        // 标题 + 圆圈行（官方 passCodeInputItem 的 y=450 起）
        self.set_center(702, 508, 38.0, true, "Enter passcode", 20);
        self.lock_dots(592, &entered, show);
        if !msg.is_empty() {
            self.lock_notice(&msg);
        }
        self.set_keypad(h - 40 - 4 * 110 - 3 * 24, A_LOCK0, A_LOCK_DEL);
        // 和设置页同一个坑：锁屏的文字也是 `tone 20`（灰阶 1），DU 驱不动，
        // 整屏会只剩键盘的黑框、看不到 "Enter passcode"，等于锁死进不去。
        // 用 GL16（16 级灰、不变像素不驱、不闪）。
        self.fb.refresh(
            0,
            0,
            w,
            h,
            WAVEFORM_MODE_GL16,
            rmdev::fb::UPDATE_MODE_PARTIAL,
        );
    }

    /// 输一位数字；满 4 位就校验。连错 5 次锁 30 秒。
    fn type_lock_digit(&mut self, d: u8) {
        // 恢复路径：密码文件被删掉（`rm ~/.config/rmdev/passcode`）就立刻解锁，
        // 不用重启。
        if !security::is_enabled() {
            eprintln!("[lock] 密码文件已删除，解锁");
            self.screen = Screen::Library;
            self.view = View::MyFiles;
            self.draw_library();
            return;
        }
        let now = std::time::Instant::now();
        let mut unlock = false;
        if let Screen::Lock(l) = &mut self.screen {
            if !l.lockout.can_try(now) || l.entered.len() >= 4 {
                return;
            }
            l.msg.clear();
            l.entered.push((b'0' + d) as char);
            if l.entered.len() == 4 {
                let code = l.entered.clone();
                // 密码文件删掉即恢复：这里 verify 失败就是失败，没有别的后门。
                if security::verify(&code) {
                    unlock = true;
                } else {
                    l.entered.clear();
                    let locked = l.lockout.record_failure(now);
                    l.shown_secs = l.lockout.remaining_secs(now);
                    l.msg = if locked {
                        format!("Try again in {} seconds.", security::LOCK_SECS)
                    } else {
                        "Wrong passcode. Try again.".into()
                    };
                }
            }
        }
        if unlock {
            eprintln!("[lock] 解锁");
            self.screen = Screen::Library;
            self.view = View::MyFiles;
            self.draw_library();
        } else {
            self.draw_lock();
        }
    }

    fn tap_lock(&mut self, x: f32, y: f32) {
        let Some(a) = self.hits.take(x, y) else { return };
        if (A_LOCK0..A_LOCK0 + 10).contains(&a) {
            self.type_lock_digit((a - A_LOCK0) as u8);
        } else if a == A_LOCK_SHOW {
            if let Screen::Lock(l) = &mut self.screen {
                l.show_digits = !l.show_digits;
            }
            self.draw_lock();
        } else if a == A_LOCK_DEL {
            if let Screen::Lock(l) = &mut self.screen {
                l.entered.pop();
            }
            self.draw_lock();
        }
    }

    fn handle_key(&mut self, k: Key) {
        // 文档内搜索 / Notebook settings 的键盘先吃
        if self.nbset.is_open() {
            self.nbset_key(k);
            return;
        }
        if self.search.is_open() {
            self.search_key(k);
            return;
        }
        if matches!(&self.screen, Screen::Notebook(nb) if nb.text_edit.is_some()) {
            if let Screen::Notebook(nb) = &mut self.screen {
                if let Some(e) = nb.text_edit.as_mut() {
                    e.kb.press(k);
                }
            }
            self.repaint_text_editor();
            return;
        }
        // === C20 begin: Wi-Fi 连接面板的按键走 A2 局部刷新 ===
        // 语义和下面 `Screen::WifiPass | Screen::WifiJoin` 那一支完全一样（先喂
        // 键盘、`Enter` 当 `Connect`），只是不再整页 `GC16 + FULL`。命中就 return。
        if self.wifi_connect_key(k) {
            return;
        }
        // === C20 end ===
        match &mut self.screen {
            Screen::Search(s) => {
                s.kb.press(k);
                if k == Key::Hide {
                    self.screen = Screen::Library;
                    self.draw_library();
                    return;
                }
                self.draw_search();
            }
            Screen::Mail(m) => {
                // === P2 begin ===
                // 键盘输入进当前焦点字段的缓冲；回车 = 官方 `Confirm`（收下、收键盘）
                if k == Key::Hide {
                    self.back_to_doc();
                    return;
                }
                if k == Key::Enter {
                    self.mail_commit();
                } else {
                    m.kb.press(k);
                }
                self.draw_mail();
                // === P2 end ===
            }
            // === C0W begin: 连接面板的键盘（回车 = Connect）===
            Screen::WifiPass(p) | Screen::WifiJoin(p) => {
                p.kb.press(k);
                if k == Key::Enter {
                    self.wifi_panel_advance();
                } else {
                    // 用户 2026-11-09：进界面时已经整屏 GC16 刷过一次（白底），
                    // 打字只刷输入框那一小块；整屏重刷会让键盘闪、还会留残影。
                    self.wifi_repaint_field();
                }
            }
            // === C0W end ===
            Screen::Pair(p) => {
                if k == Key::Hide {
                    self.back_to_settings();
                    return;
                }
                p.kb.press(k);
                if k == Key::Enter {
                    self.pair_advance();
                } else {
                    self.draw_pair();
                }
            }
            Screen::Rename(r) => {
                r.kb.press(k);
                if k == Key::Enter {
                    let name = r.kb.text.trim().to_string();
                    if !name.is_empty() {
                        let uuid = r.uuid.clone();
                        if let Err(e) = self.library.rename(&uuid, &name) {
                            eprintln!("[ui] 重命名失败：{e}");
                        }
                    }
                    self.screen = Screen::Library;
                    self.draw_library();
                    return;
                }
                self.draw_rename();
            }
            _ => {}
        }
    }

    fn tap_overlay(&mut self, x: f32, y: f32) {
        // 键盘优先
        for (r, k) in self.kb_hits.clone() {
            let (xi, yi) = (x as i32, y as i32);
            let inside = xi >= r.0 && xi < r.0 + r.2 && yi >= r.1 && yi < r.1 + r.3;
            if inside {
                self.handle_key(k);
                return;
            }
        }
        let act = self.hits.take(x, y);
        match act {
            Some(A_BACK) => {
                self.screen = Screen::Library;
                self.draw_library();
            }
            Some(A_CLEAR) => {
                if let Screen::Search(s) = &mut self.screen {
                    s.kb.text.clear();
                }
                self.draw_search();
            }
            Some(A_CANCEL) => {
                self.screen = Screen::Library;
                self.draw_library();
            }
            Some(A_OK) => self.handle_key(Key::Enter),
            Some(a) if (A_RESULT0..A_RESULT0 + 100).contains(&a) => {
                let i = (a - A_RESULT0) as usize;
                if let Some(b) = self.visible.get(i).cloned() {
                    if b.doc_type == "CollectionType" {
                        // 官方 Collection：navigateTo(entry.id)，在库里进这个文件夹。
                        self.trail.clear();
                        self.enter_folder(&b.uuid, &b.name);
                    } else {
                        // 官方：requestOpenDocumentOnPage(entry, entry.lastOpenedPage)。
                        // PDF 也走这里——open_at 已经接 pdfium 底图。
                        self.open_at(&b.uuid, b.last_opened_page as usize);
                    }
                }
            }
            _ => {}
        }
    }

    /// 翻到第 `idx` 页（会先把当前页存盘，再换页换底图）。
    fn goto_page(&mut self, idx: usize) {
        // === C15 begin ===
        // 1) 当前页要是末尾的虚拟空白页，先把上面的笔画/文字变成真实的一页
        //    （否则下面按 `cur` 存盘会写到一个不存在的页 id 上）；
        // 2) 目标页要是"没有印刷底图"的页（虚拟页 / PDF·电子书自身页数之外的
        //    追加页），也不能走常规路径——那条路会拿最后一页当底图。
        self.c15_commit_virtual();
        // 翻页 = 换一页的引用列表：菜单先收（否则列表内容和页面错位）
        if self.ai_menu {
            self.ai_menu = false;
            self.ai_sync_chrome();
        }
        if self.c15_goto_extra(idx) {
            return;
        }
        // === C15 end ===
        let (uuid, cur, dirty) = match &self.screen {
            Screen::Notebook(nb) => (nb.uuid.clone(), nb.page_index, nb.dirty),
            _ => return,
        };
        let total = self.library.page_ids(&uuid).len().max(1);
        // === C15 begin ===
        // 翻页上界要把末尾那张虚拟页算进去，否则滑到最后一页就到不了它、
        // 从它往后滑还会被夹回上一页。
        let total = if self.c15_has_virtual(&uuid) {
            rmdev::doc::autopage::display_total(self.c15_real_pages(&uuid))
        } else {
            total
        };
        // === C15 end ===
        let idx = idx.min(total.saturating_sub(1));
        if idx == cur {
            return;
        }
        if dirty {
            if let Screen::Notebook(nb) = &self.screen {
                let _ = self.library.save_page_at(&uuid, cur, &nb.page);
            }
        }
        // === P11 begin ===
        // 文字落到官方 `.rm`（放在整页写回之后，见 `Library::save_scene_texts`）
        if matches!(&self.screen, Screen::Notebook(_)) {
            self.p11_save_page_texts(&uuid, cur);
        }
        // === P11 end ===
        let page = self.library.load_page_at(&uuid, idx);
        // PDF 换页要重新渲染底图
        self.pdf_bg = None;
        // 下一页预热的参数（渲染尺寸要跟当前页一致才能命中缓存）
        let mut warm: Option<(String, usize, usize)> = None;
        // 同一次打开里翻页：复用已经解析好的文档，不重新解一遍整本书
        let same = self.doc_uuid.as_deref() == Some(uuid.as_str()) && self.doc.is_some();
        if !same {
            if let Some(b) = self.library.find(&uuid) {
                if b.format.is_known() && b.format != rmdev::doc::Format::Rm {
                    let path = self.document_path(&b);
                    match rmdev::doc::open(path.to_str().unwrap_or("")) {
                        Ok(d) => {
                            self.doc = Some(d);
                            self.doc_uuid = Some(uuid.to_string());
                        }
                        Err(e) => eprintln!("[doc] {e}"),
                    }
                }
            }
        }
        // 翻到宽页（PPT 那种）自动进横屏、翻回竖页自动回来。放在渲染之前：
        // `box_view()` 依赖 `fb` 的新尺寸。
        // （笔记本没有 `doc`：页永远是 1404×1872，横屏只可能是手动开的，不动它）
        let asp = self.doc.as_ref().map(|d| {
            let n = d.pages().saturating_sub(1);
            d.aspect(idx.min(n)).clamp(0.05, 20.0)
        });
        if let Some(a) = asp {
            self.auto_landscape(a);
        }
        let t_render = std::time::Instant::now();
        let rendered: Option<(Vec<u8>, String, usize, usize)> = self.doc.as_ref().and_then(|d| {
            let (fw, fh) = (DOC_W, DOC_H);
            let idx = idx.min(d.pages().saturating_sub(1));
            let asp = d.aspect(idx).clamp(0.05, 20.0);
            let mut ww = (fh as f32 * asp).round() as usize;
            let mut hh = fh;
            if ww > fw {
                ww = fw;
                hh = ((fw as f32) / asp).round() as usize;
            }
            let g = d.render(idx, ww, hh).ok()?;
            let mut bg = vec![255u8; fw * fh];
            let ox = (fw - ww) / 2;
            let oy = (fh - hh.min(fh)) / 2;
            for y in 0..hh.min(fh) {
                let src = y * ww;
                let dst = (oy + y) * fw + ox;
                bg[dst..dst + ww].copy_from_slice(&g[src..src + ww]);
            }
            let path = self
                .library
                .find(&uuid)
                .map(|b| self.document_path(&b).display().to_string())
                .unwrap_or_default();
            Some((bg, path, ww, hh))
        });
        if let Some((bg, path, ww, hh)) = rendered {
            if env_flag("RM_TIMING") {
                eprintln!("[doc] 翻页 render={}ms", t_render.elapsed().as_millis());
            }
            self.pdf_bg = Some(bg);
            self.page_box = Some(((DOC_W - ww) / 2, (DOC_H - hh.min(DOC_H)) / 2, ww, hh.min(DOC_H)));
            self.bg_gen = self.bg_gen.wrapping_add(1);
            warm = Some((path, ww, hh));
        }
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.page = page;
            nb.page_index = idx;
            nb.page_count = total;
            nb.dirty = false;
            nb.active = None;
            // === P11 begin ===
            // 读文字：官方 `.rm` 优先，没有文字层再看 sidecar
            let p11_v = self.library.load_scene_texts(&uuid, idx);
            nb.texts = p11_v.iter().map(|t| t.to_item()).collect();
            nb.p11_styles = p11_v.iter().map(|t| t.style).collect();
            // === P11 end ===
            nb.text_edit = None;
            // 换页：选区下标和撤销栈都是针对上一页的层，一律作废
            nb.clear_sel();
            nb.undo.clear();
            nb.redo.clear();
        }
        // 新页的框可能换了宽高比（横屏自动切过），视图要重算
        if self.landscape() {
            let v = self.box_view();
            if let Screen::Notebook(nb) = &mut self.screen {
                nb.view = v;
            }
        }
        eprintln!("[dev] 翻到第 {} 页", idx + 1);
        self.redraw_page();
        // 一眼看到当前页之后再预热下一页：一次整页渲染几十毫秒，放在刷新**之后**
        // 用户感觉不到；不预热的话下一次翻页要等它渲完才出图。
        if let Some((path, ww, hh)) = warm {
            if idx + 1 < total {
                self.prefetch_job = Some((path, idx + 1, ww, hh));
            }
        }
    }

    fn open_search(&mut self) {
        // 库界面可能还挂着一条 toast，别带到搜索页上来
        self.banner = None;
        self.screen = Screen::Search(Box::new(Search {
            kb: Keyboard::new(),
        }));
        self.draw_search();
    }

    fn open_rename(&mut self, uuid: &str, name: &str) {
        let mut kb = Keyboard::new();
        kb.text = name.to_string();
        self.screen = Screen::Rename(Box::new(Rename {
            uuid: uuid.to_string(),
            kb,
        }));
        self.draw_rename();
    }

    fn handle(&mut self, ev: InputEvent) {
        // 空闲时跳过的那一轮：现在有人在用了，2 s 后补上（不打断这一下操作）。
        if self.sync_skip_idle && self.asleep.is_none() {
            self.sync_skip_idle = false;
            self.sync_auto_at = std::time::Instant::now() + Duration::from_secs(2);
        }
        self.last_ev = std::time::Instant::now();
        if env_flag("RM_DEBUG") {
            eprintln!("[ev] {ev:?}");
        }
        // 电源键：睡着时唯一还认的输入
        if let InputEvent::Power(p) = ev {
            self.on_power(p);
            return;
        }
        // === JOB begin: Job status 整屏优先吃输入（笔和手指都算） ===
        //
        // 为什么放这么靠前：这层是盖在库上的整屏 sheet，`Screen` 仍然是
        // `Library`，中间那些"按 screen 分派"的关卡（长按判定、菜单折叠、
        // 触摸锁……）都会先把事件分走，实测放后面笔点根本到不了。
        if self.jobs_open {
            match &ev {
                InputEvent::Pen(PenEvent::Down(q)) => {
                    self.last_touch = Some((q.x, q.y, 1));
                    return;
                }
                InputEvent::Pen(PenEvent::Up) => {
                    self.jobs_activate();
                    return;
                }
                InputEvent::Touch(TouchEvent::Down { x, y }) => {
                    self.last_touch = Some((*x, *y, 1));
                    return;
                }
                InputEvent::Touch(TouchEvent::Up { .. }) => {
                    self.jobs_activate();
                    return;
                }
                _ => {}
            }
        }
        // === JOB end ===
        // 睡着 / 关机框：手指和笔都不往下发。睡着时更不能落墨——
        // 官方这时候连 `documentView` 都是不可见的。
        if self.asleep.is_some() || self.poweroff {
            if self.poweroff {
                self.poweroff_input(ev);
            }
            return;
        }
        // === E1 begin: 错题档案是整屏 sheet，开着头所有输入都归它 ===
        // （笔和手指同一个入口；返回箭头/`Learned`/拖动滚动都在 `e1_input` 里）
        if self.mistakes.is_open() {
            self.e1_input(ev);
            return;
        }
        // === E1 end ===
        // === D1 begin: 提问键盘开着 = 整屏模态输入 ===
        if self.d1_capture(&ev) {
            return;
        }
        // === D1 end ===
        // 延迟测量：每个输入事件的时间戳，和 `[fb] ioctl 前 t=` 相减就是
        // "笔/手指到我们提交刷新"这一段（面板自己那十几毫秒测不到，但这段能测）。
        if env_flag("RM_TIMING") {
            match &ev {
                InputEvent::Pen(PenEvent::Down(p)) => {
                    eprintln!("[pen] t={:.1} down {:.0},{:.0} p={}", rmdev::fb::now_ms(), p.x, p.y, p.pressure)
                }
                InputEvent::Pen(PenEvent::Move(p)) => {
                    eprintln!("[pen] t={:.1} move {:.0},{:.0} p={}", rmdev::fb::now_ms(), p.x, p.y, p.pressure)
                }
                InputEvent::Pen(PenEvent::Up) => {
                    eprintln!("[pen] t={:.1} up", rmdev::fb::now_ms())
                }
                InputEvent::Touch(TouchEvent::Down { x, y }) => {
                    eprintln!("[touch] t={:.1} down {x:.0},{y:.0}", rmdev::fb::now_ms())
                }
                _ => {}
            }
        }
        // 新的按下说明上一次"按住"已经结束：长按的 skip_up 只挡紧跟着的那一次抬笔
        let down = match &ev {
            InputEvent::Touch(TouchEvent::Down { .. }) => true,
            InputEvent::Pen(PenEvent::Down(_)) => true,
            _ => false,
        };
        if down {
            self.skip_up = false;
        }
        // 笔记本里那条 toast 黑条是直接贴在屏幕上的：下一次按下先整页重绘擦掉它，
        // 否则笔迹刷上去会留下一块花掉的残影（库里那条由 tap_library 自己收）。
        if down && self.banner.is_some() && matches!(self.screen, Screen::Notebook(_)) {
            self.banner = None;
            self.redraw_page();
        }
        if let InputEvent::Pen(_) = ev {
            self.pen_ts = std::time::Instant::now();
            // 笔一落下，就把"手指按住"那套状态清掉：手掌压着屏 550ms 会触发
            // 长按弹页面总览，写字时这是灾难。
            self.press = None;
            self.drag = None;
            self.multi_touch = false;
            self.gesture_armed = false;
            self.gesture_sum = (0.0, 0.0, 1.0);
            if let Screen::Notebook(nb) = &mut self.screen {
                nb.swipe = None;
            }
        }
        // 目录是盖在页总览上的整页面板：开着头，所有输入都只归它
        // （点条目/拖滚动），不再往下发给总览或纸面。
        if self.toc.is_open() {
            self.toc_input(ev);
            return;
        }
        // === D6 begin ===
        if self.d6_input(&ev) {
            return;
        }
        // === D6 end ===
        match ev {
            // 手指只点 UI，不落墨——这是官方的分工
            InputEvent::Touch(t) => self.handle_touch(t),
            InputEvent::Pen(p) => {
                if matches!(self.screen, Screen::Notebook(_)) {
                    self.handle_notebook(p)
                } else {
                    self.handle_library_pointer(p)
                }
            }
            // 电源键在上面就整条处理掉了（睡着时它是唯一认的输入）
            InputEvent::Power(_) => {}
        }
    }

    /// 屏幕坐标上的一次纯 UI 点击。返回 true = 已被 UI 消费
    /// （笔就不用落墨；手指就不用当翻页/手势）。
    ///
    /// 笔和手指共用这一条：顺序就是原来笔 Down 分支里的顺序 ——
    /// 面板画得越靠上、越先拿到点击。
    fn ui_tap(&mut self, x: f32, y: f32) -> bool {
        // === AIREF begin: 引用菜单开着时，点行 = 跳转，点别处 = 收起 ===
        // 放在最前面：这是人主动点开的浮层，任何输入都先给它。
        if self.ai_menu && matches!(&self.screen, Screen::Notebook(nb) if !nb.overview) {
            let marks = self.ai_marks_here();
            match rmdev::airefs::menu_row_at(marks.len(), self.fb.height, x as i32, y as i32) {
                Some(i) => self.ai_pick_row(i),
                None => self.ai_toggle_menu(),
            }
            // 笔的那一下别再落墨：抬起事件也让它当"点过 UI"处理
            if let Screen::Notebook(nb) = &mut self.screen {
                nb.tap_consumed = true;
            }
            return true;
        }
        // === AIREF end ===
        // === D1 begin: 第 13 格的浮层（两项） ===
        if self.d1_ui_tap(x, y) {
            return true;
        }
        // === D1 end ===
        // 文档内搜索 / Notebook settings 是盖在文档视图上的浮层：最优先，
        // 而且开着时吞掉所有点击（官方搜索态/设置页都禁画布手势）。
        if self.tcfg.is_open() {
            return self.tcfg_tap(x, y);
        }
        if self.nbset.is_open() {
            return self.nbset_tap(x, y);
        }
        if self.search.is_open() {
            return self.search_tap(x, y);
        }
        // === P13 begin ===
        // 剪贴板提示条盖在页面最下面：点右端 = `Clear clipboard`，点别处吞掉
        // （官方提示条上那个 MouseArea 也是 accepted: true，不让点击漏到纸面）。
        let (has_clip, selecting) = match &self.screen {
            Screen::Notebook(nb) => (!self.clipboard.is_empty(), nb.chrome.selecting),
            _ => (false, false),
        };
        if rmdev::chrome::clip_toast_visible(has_clip, selecting)
            && matches!(&self.screen, Screen::Notebook(nb) if !nb.overview)
        {
            let (w, h) = (self.fb.width, self.fb.height);
            let (tx, ty, tw, th) = rmdev::chrome::clip_toast_rect(w, h);
            let (xi, yi) = (x as i32, y as i32);
            if xi >= tx && xi < tx + tw && yi >= ty && yi < ty + th {
                let (cx, cy, cw, ch) = rmdev::chrome::clip_clear_rect(w, h);
                if xi >= cx && xi < cx + cw && yi >= cy && yi < cy + ch {
                    self.clipboard.clear();
                    eprintln!("[sel] Clear clipboard → 剪贴板清空");
                    self.repaint_rects(&[(tx, ty, tw, th)], WAVEFORM_MODE_GC16);
                }
                if let Screen::Notebook(nb) = &mut self.screen {
                    nb.tap_consumed = true;
                }
                return true;
            }
        }
        // === P13 end ===
        // 页面总览是另一套界面（返回/缩略图/加页/长按菜单），整屏交给它
        if matches!(&self.screen, Screen::Notebook(nb) if nb.overview) {
            self.tap_overview(x, y);
            return true;
        }
        // 右上角的 × 关闭文档：必须**先于**工具条判断，
        // 否则会被 chrome.hit 当成"点空白"吃掉。
        // 工具条收起时 × 也是隐藏的，别让盲点把它当成"关文档"。
        let chrome_on = matches!(&self.screen, Screen::Notebook(nb) if nb.chrome.visible);
        if env_flag("RM_DEBUG") {
            eprintln!("[tap] ui_tap {x:.0},{y:.0} chrome_on={chrome_on} close={:?}", close_rect());
        }
        let (clx, cly, clw, clh) = close_rect();
        if chrome_on
            && (clx as f32..(clx + 78) as f32).contains(&x)
            && (cly as f32 + 4.0..(cly + 84) as f32).contains(&y)
        {
            self.close_notebook();
            return true;
        }
        // 文档设置面板画在最上面（工具条、其它面板之后），所以最先给它
        if self.ds_tap(x, y) {
            if let Screen::Notebook(nb) = &mut self.screen {
                nb.tap_consumed = true;
            }
            return true;
        }
        // 标签/分享/图层三个面板盖在页面上，先给它们
        if let Some(a) = self.tag_hit(x, y) {
            if let Screen::Notebook(nb) = &mut self.screen {
                nb.tap_consumed = true;
            }
            self.tag_click(a);
            return true;
        }
        if let Some(a) = self.share_hit(x, y) {
            if let Screen::Notebook(nb) = &mut self.screen {
                nb.tap_consumed = true;
            }
            self.share_click(a);
            return true;
        }
        if let Some(act) = self.layer_hit(x, y) {
            if let Screen::Notebook(nb) = &mut self.screen {
                nb.tap_consumed = true;
            }
            self.layer_click(act);
            return true;
        }
        let (hit, in_bar) = match &self.screen {
            Screen::Notebook(nb) => (
                nb.chrome.hit(x, y),
                nb.chrome.visible && rmdev::chrome::in_band(x as i32),
            ),
            _ => (None, false),
        };
        // 工具条那一列（含隐藏后的把手）里的空白：不能当纸面
        if in_bar && hit.is_none() {
            return true;
        }
        if let Some(msg) = hit {
            let was_layers =
                matches!(&self.screen, Screen::Notebook(nb) if nb.chrome.menu == Menu::Layers);
            if let Screen::Notebook(nb) = &mut self.screen {
                nb.tap_consumed = true;
                nb.exit_taps = 0;
            }
            self.on_chrome(msg);
            // 图层面板开着时在纸上落笔：先收面板，这一笔照画（不然会"点了没反应"）
            if was_layers && msg == Msg::Dismiss {
                if let Screen::Notebook(nb) = &mut self.screen {
                    nb.tap_consumed = false;
                }
                return false;
            }
            return true;
        }
        false
    }

    // === P7 begin ===
    /// 文档页滑块的触摸入口：返回 `true` 表示这个事件被滑块吃掉了。
    ///
    /// 只认**手指**（笔走 `handle_notebook`，根本不经过这里，所以笔照常落墨）、
    /// 只在文档正文视图。命中区就是官方那条 `MouseArea`：贴底 100 px
    /// （`y >= slider::bar(h).0`）。**从底部上滑**才拉出滑块（官方 `fastScroll`
    /// 也是往上滑；横着划仍然是翻页）；条已经开着时按上去直接拖。
    /// 按着往右拖改页号，往上拖过 [`slider::CANCEL`] 就取消（页码冻住、松手不跳页）。
    fn p7_touch(&mut self, t: TouchEvent) -> bool {
        let (total, page_now) = match &self.screen {
            Screen::Notebook(nb) if !nb.overview => (nb.page_count.max(1), nb.page_index),
            _ => return false,
        };
        let (w, h) = (self.fb.width, self.fb.height);
        let (bar_top, _) = slider::bar(h);
        // 手指落在哪一页（1-based 转 0-based；`page_at_x` 自己会夹住边界）
        let at = |x: f32| slider::page_at_x(x as i32, w, total).min(total) - 1;
        match t {
            TouchEvent::Down { x, y } => {
                self.p7_arm = None;
                if (y as i32) >= bar_top {
                    if self.slider.is_some() {
                        // 条已经开着：按上去直接跳页 + 开拖（官方 `onPressed`）
                        self.press = None;
                        let old = self.slider.map(|(_, _, s)| s);
                        self.slider =
                            Some((std::time::Instant::now(), at(x), slider::State::Pressed));
                        self.p7_repaint(old);
                        return true;
                    }
                    // 还没开：只记锚点。横着划（翻页）不该被滑块抢走。
                    self.p7_arm = Some((x, y));
                    return false;
                }
                // 官方那个铺满屏幕的 MouseArea：滑块开着时点别处 = 收起，
                // 而且 `mouse.accepted = false`，事件照常往下传。
                if self.slider.is_some() {
                    self.p7_hide();
                }
                false
            }
            TouchEvent::Move { x, y } => {
                let Some((_, cur, st)) = self.slider else {
                    // 条还没开：从底边往上滑够了才拉出来
                    let Some((_, y0)) = self.p7_arm else {
                        return false;
                    };
                    if !slider::opens_from(y0, y) {
                        return false;
                    }
                    self.p7_arm = None;
                    self.press = None;
                    self.slider =
                        Some((std::time::Instant::now(), at(x), slider::State::Pressed));
                    self.p7_repaint(None);
                    return true;
                };
                let ns = slider::next_state(y as i32, bar_top);
                // 取消以后页号冻住，不再跟着手指走（官方 `value` 也是被
                // `pageThumbnailVisible` 挡着才更新）
                let page = if ns == slider::State::Pressed {
                    at(x)
                } else {
                    cur
                };
                if ns == st && page == cur {
                    // 页码和状态都没变就不用刷屏（e-ink 一次 partial 也是几百毫秒）
                    return true;
                }
                self.slider = Some((std::time::Instant::now(), page, ns));
                self.p7_repaint(Some(st));
                true
            }
            TouchEvent::Up => {
                self.p7_arm = None;
                let Some((_, page, st)) = self.slider else {
                    return false;
                };
                // 松手：条留着（官方松手后还要等 `hidePageSliderTimer` 才收）
                self.slider = Some((std::time::Instant::now(), page, slider::State::Idle));
                if st == slider::State::Pressed && page != page_now {
                    // 只有没被取消、而且真换了页才跳；`goto_page` 会整屏重画一次
                    self.goto_page(page);
                } else {
                    // 取消 / 没换页：把预览框擦掉，游标留在原地等自动收
                    self.p7_repaint(Some(st));
                }
                true
            }
        }
    }

    /// 只刷滑块自己那两块（底部条 + 拖动时的预览框），正文一笔都不重画。
    ///
    /// 走 `repaint_rects` 那套：先把纸面贴回来、再补浮层，最后只推这两块 partial。
    fn p7_repaint(&mut self, old: Option<slider::State>) {
        let Some((_, _, st)) = self.slider else {
            return;
        };
        if !matches!(&self.screen, Screen::Notebook(nb) if !nb.overview) {
            return;
        }
        let (w, h) = (self.fb.width, self.fb.height);
        let mut rects = vec![slider::band_rect(w, h)];
        // 预览框：这一帧要画（按着），或者上一帧画过（现在得擦掉）
        if st == slider::State::Pressed || old == Some(slider::State::Pressed) {
            rects.push(slider::thumb_rect(w, h));
        }
        self.repaint_rects(&rects, WAVEFORM_MODE_GC16);
    }

    /// 收起滑块（静置到点 / 点别处）：先清状态，再让 `repaint_rects` 从纸面把
    /// 底部那条（和预览框）补回来 —— 只刷这两块，不整屏。
    fn p7_hide(&mut self) {
        let Some((_, _, st)) = self.slider.take() else {
            return;
        };
        if !matches!(&self.screen, Screen::Notebook(nb) if !nb.overview) {
            return;
        }
        let (w, h) = (self.fb.width, self.fb.height);
        let mut rects = vec![slider::band_rect(w, h)];
        if st == slider::State::Pressed {
            rects.push(slider::thumb_rect(w, h));
        }
        self.repaint_rects(&rects, WAVEFORM_MODE_GC16);
    }

    /// 预览框里的缩略图：直接吃页总览那份缓存（306×392），2× 放大到 612×784。
    /// 没开过页总览就没有 —— 那时 `paint` 画一个大号 `Page N`（官方那边是
    /// `Loading` + `Page %1`）。拖动期间**绝不**现渲：渲一页要几百毫秒。
    fn p7_thumb(&self, uuid: &str, page: usize) -> Option<(Vec<u8>, i32, i32)> {
        const TW: usize = 306;
        const TH: usize = 392;
        let e = self
            .page_thumbs_cache
            .iter()
            .find(|((u, i), _, _)| u == uuid && *i == page)?;
        if e.2.len() < TW * TH {
            return None;
        }
        Some((p7_scale2(&e.2, TW, TH), (TW * 2) as i32, (TH * 2) as i32))
    }
    // === P7 end ===

    fn handle_touch(&mut self, t: TouchEvent) {
        // 笔优先：笔动过 250ms 内，所有触摸一律丢掉。
        // 写字时握笔的手掌/手指会一直压着屏，官方在这块也只认笔。
        if self.pen_ts.elapsed() < Duration::from_millis(250) {
            self.pending_tap = None;
            self.press = None;
            self.drag = None;
            if let Screen::Notebook(nb) = &mut self.screen {
                nb.swipe = None;
            }
            return;
        }
        // === C18 begin: 笔还按在屏上（哪怕停着不动）→ 触摸照样不算数 ===
        // 上面那条只看"笔刚动过 250 ms"：笔停住描线/思考时一个事件都不发，
        // 时间窗一过，手掌就算成了手指（点 UI、翻页、长按都能被它触发）。
        // 这里补上状态判据：正在写（`nb.active`）就一直只认笔。
        if matches!(&self.screen, Screen::Notebook(nb) if nb.active.is_some()) {
            if rmdev::input::touch::tlog() {
                eprintln!("[tlog] G 笔正按在屏上 → 丢弃触摸 {t:?}");
            }
            self.pending_tap = None;
            self.press = None;
            self.drag = None;
            self.multi_touch = false;
            if let Screen::Notebook(nb) = &mut self.screen {
                nb.swipe = None;
            }
            return;
        }
        // === C18 end ===
        // 官方那条"慢拖不能被吃成长按"：手指一动过 15 px，这次长按候选就作废。
        // 放在所有分支之前 —— 库、页总览、标签、图层名都走同一套规则。
        if let TouchEvent::Move { x, y } = t {
            self.cancel_long_press_if_moved(x, y);
        }
        // === LOCKBTN begin ===
        // 右上角锁图标：点一下切换触控锁（锁着也能点，否则没法解锁）。
        if matches!(&self.screen, Screen::Notebook(nb) if !nb.overview) {
            match t {
                TouchEvent::Down { x, y }
                    if (lock_rect().0..=lock_rect().0 + 80).contains(&(x as i32)) && y <= 90.0 =>
                {
                    self.lock_press = true;
                    return;
                }
                TouchEvent::Up if self.lock_press => {
                    self.lock_press = false;
                    self.touch_lock = !self.touch_lock;
                    eprintln!("[lock] 触控锁 = {}", self.touch_lock);
                    self.redraw_page();
                    return;
                }
                _ => {}
            }
        }
        // === LOCKBTN end ===
        // === AIREF begin ===
        // 引用菜单开着时，它优先吃触摸：点行 = 跳过去，点别处 = 收起。
        //
        // 必须放在触摸锁**前面**：锁把纸面手势全吃掉（防掌压），但菜单是人
        // 主动点开的浮层，锁着也得点得动 —— 否则"看引用"在默认锁定下根本用不了。
        if self.ai_menu && matches!(&self.screen, Screen::Notebook(nb) if !nb.overview) {
            match t {
                TouchEvent::Down { x, y } => {
                    self.ai_last = Some((x, y));
                    return;
                }
                TouchEvent::Move { x, y } => {
                    self.ai_last = Some((x, y));
                    return;
                }
                TouchEvent::Up if self.ai_menu_fresh => {
                    // 就是刚点开菜单的那次抬指：不算"点别处"
                    self.ai_menu_fresh = false;
                    return;
                }
                TouchEvent::Up => {
                    let (x, y) = self.ai_last.unwrap_or((0.0, 0.0));
                    let marks = self.ai_marks_here();
                    let h = self.fb.height;
                    match rmdev::airefs::menu_row_at(marks.len(), h, x as i32, y as i32) {
                        Some(i) => self.ai_pick_row(i),
                        None => self.ai_toggle_menu(),
                    }
                    return;
                }
                _ => {}
            }
        }
        // === AIREF end ===
        // === TLOCK begin ===
        // 触控锁（默认开，用户 2026-11-09 要求）：编辑时手指只认
        // ① 左边工具栏 ② 右上角 × ③ 横向翻页；工具栏收起时只认左上角圆钮。
        // 手掌/手指蹭到纸面不再触发任何操作 —— 之前"手一边写一边动"会把画面
        // 弄成灰块和残影，就是因为掌心的触点被当成了 UI 操作。
        if self.touch_lock {
            if let Screen::Notebook(nb) = &self.screen {
                if !nb.overview {
                    let collapsed = !nb.chrome.visible;
                    match t {
                        TouchEvent::Down { x, y } => {
                            let on_toolbar = rmdev::chrome::in_band(x as i32);
                            let on_close = !collapsed && x >= close_rect().0 as f32 && y <= 100.0;
                            // AI 引用（框/角标）和 `View` 弹层要放行，否则触控锁
                            // 会把"长按看回答"一起锁掉。
                            let on_airef = self.ai_view.is_some()
                                || self.ai_ref_hit(x, y).is_some();
                            if !(on_toolbar || on_close || on_airef) {
                                self.tlock = Some((x, y, x, y));
                                self.pending_tap = None;
                                self.press = None;
                                return;
                            }
                            if collapsed && !(rmdev::chrome::in_band(x as i32) && y < CHROME_W as f32) {
                                self.tlock = Some((x, y, x, y));
                                self.pending_tap = None;
                                self.press = None;
                                return;
                            }
                        }
                        TouchEvent::Move { x, y } => {
                            if let Some((x0, y0, _, _)) = self.tlock {
                                self.tlock = Some((x0, y0, x, y));
                            }
                            return;
                        }
                        TouchEvent::Up if self.ai_view.is_some() => {}
                        TouchEvent::Up => {
                            if let Some((x0, y0, x1, y1)) = self.tlock.take() {
                                let dx = x1 - x0;
                                let dy = y1 - y0;
                                if dx.abs() > 300.0 && dy.abs() < dx.abs() * 0.6 {
                                    let cur = match &self.screen {
                                        Screen::Notebook(nb) => nb.page_index,
                                        _ => 0,
                                    };
                                    if dx < 0.0 {
                                        self.goto_page(cur + 1);
                                    } else if cur > 0 {
                                        self.goto_page(cur - 1);
                                    }
                                }
                            }
                            return;
                        }
                        _ => {}
                    }
                }
            }
        }
        // === TLOCK end ===
        // AI 引用框：**手指长按**才弹 `View`，点 `View` 才跳转。
        // 笔尖/短按都不算（用户 2026-11-09 的要求，怕写字时误触）。
        if matches!(&self.screen, Screen::Notebook(nb) if !nb.overview) {
            match t {
                TouchEvent::Down { x, y } => {
                    self.ai_last = Some((x, y));
                    if self.ai_view.is_some() {
                        return; // 弹层开着：只认这一下，别穿透去写字/翻页
                    }
                    if let Some(m) = self.ai_ref_hit(x, y) {
                        self.ai_hold = Some((x, y, std::time::Instant::now(), m));
                        return;
                    }
                }
                TouchEvent::Move { x, y } => {
                    self.ai_last = Some((x, y));
                    if let Some((x0, y0, _, _)) = &self.ai_hold {
                        if (x - x0).abs() > 15.0 || (y - y0).abs() > 15.0 {
                            self.ai_hold = None; // 动了 = 不是长按
                        } else {
                            return;
                        }
                    }
                }
                TouchEvent::Up => {
                    if self.ai_hold.take().is_some() {
                        return; // 短按：什么都不做
                    }
                    if self.ai_view.is_some() && self.ai_view_fresh {
                        // 长按弹层那一下的抬指：只清标记，别当成点弹层
                        self.ai_view_fresh = false;
                        return;
                    }
                    if let Some((_m, r)) = self.ai_view.take() {
                        let (x, y) = self.ai_last.unwrap_or((0.0, 0.0));
                        if rmdev::airefs::view_hit(r, x as i32, y as i32) {
                            // 点 `View` = 打开这一页的引用菜单（2026-11-09 改的：
                            // 一页上可能好几个引用，先看清是哪一条再选）
                            self.ai_menu = true;
                            self.ai_sync_chrome();
                            self.redraw_page();
                        } else {
                            self.redraw_page();
                        }
                        return;
                    }
                }
                _ => {}
            }
        }
        // === AIREF end ===
        // 先把落点记下来并画个标记：这样"手指有没有被收到"在屏幕上就能看见
        match t {
            TouchEvent::Down { x, y } => {
                let n = self.last_touch.map(|(_, _, n)| n + 1).unwrap_or(1);
                self.last_touch = Some((x, y, n));
                let _ = n;
            }
            TouchEvent::Up => {}
            _ => {}
        }
        // 双指期间位移全归手势识别：不翻页、不滚动（否则捏合完会顺手翻一页）
        if self.multi_touch {
            if let TouchEvent::Move { .. } = t {
                return;
            }
        }
        match &self.screen {
            Screen::Library => match t {
                TouchEvent::Down { x, y } => {
                    // === P12 begin ===
                    // 顶部 120px 内按下才算"往下拉抽屉"的起点
                    self.drawer.down(x, y);
                    // === P12 end ===
                    self.pending_tap = Some((x, y));
                    self.press = Some((x, y, std::time::Instant::now()));
                    self.drag = Some((x, y, x, y));
                }
                TouchEvent::Move { x, y } => {
                    // === P12 begin ===
                    self.drawer.moving(x, y);
                    // === P12 end ===
                    if let Some((px, py)) = self.pending_tap {
                        if (x - px).abs() > 40.0 || (y - py).abs() > 40.0 {
                            self.pending_tap = None;
                        }
                    }
                    if let Some((x0, y0, _, _)) = self.drag {
                        self.drag = Some((x0, y0, x, y));
                    }
                }
                TouchEvent::Up => {
                    // === P12 begin ===
                    // 顶部区往下拖超过 200px：拉开抽屉。这一下不再算滚动/点击。
                    // Tags 视图有自己的胶囊交互，不从顶部下拉（`View all` 切过去时
                    // 抽屉已经关了）；其它库视图都认。
                    if self.view != View::Tags && self.drawer.up() {
                        self.reload_drawer();
                        self.pending_tap = None;
                        self.press = None;
                        self.drag = None;
                        self.menu_open = false;
                        self.sort_open = false;
                        self.draw_library();
                        return;
                    }
                    // === P12 end ===
                    // 双指松手：只清场，不结算成"点了一下"
                    if self.multi_touch {
                        self.multi_touch = false;
                        self.pending_tap = None;
                        self.drag = None;
                        return;
                    }
                    if self.skip_up {
                        self.skip_up = false;
                        self.drag = None;
                        self.pending_tap = None;
                        return;
                    }
                    // 竖向拖动 = 滚动（手指往下拖 → 内容跟着往下 → 滚动量减少）
                    if let Some((x0, y0, x1, y1)) = self.drag.take() {
                        let (dx, dy) = (x1 - x0, y1 - y0);
                        if !self.menu_open
                            && dy.abs() > 120.0
                            && dy.abs() > dx.abs() * 1.2
                        {
                            self.press = None;
                            self.pending_tap = None;
                            self.scroll_by(dy as i32);
                            return;
                        }
                    }
                    if let Some((x, y)) = self.pending_tap.take() {
                        if self.maybe_long_press(x, y) {
                            return;
                        }
                        self.tap_library(x, y);
                    }
                }
            },
            Screen::Settings(_) => {
                match t {
                    TouchEvent::Down { x, y } => {
                        self.pending_tap = Some((x, y));
                        // === C0W begin: 记一次按下时刻，`tick` 里判长按（当前网络行）===
                        self.press = Some((x, y, std::time::Instant::now()));
                        // === C0W end ===
                    }
                    TouchEvent::Up => {
                        // === C0W begin: 长按候选一定要清（双指那条早退也不能漏）===
                        self.press = None;
                        // === C0W end ===
                        if self.multi_touch {
                            self.multi_touch = false;
                            self.pending_tap = None;
                            self.drag = None;
                            return;
                        }
                        // === C0W begin: 长按已经把确认框弹出来了，抬指别再算一次点击 ===
                        if self.skip_up {
                            self.skip_up = false;
                            self.pending_tap = None;
                            return;
                        }
                        // === C0W end ===
                        if let Some((x, y)) = self.pending_tap.take() {
                            self.tap_settings(x, y);
                        }
                    }
                    _ => {}
                }
            }
            Screen::Lock(_) => {
                match t {
                    TouchEvent::Down { x, y } => self.pending_tap = Some((x, y)),
                    TouchEvent::Up => {
                        if let Some((x, y)) = self.pending_tap.take() {
                            self.tap_lock(x, y);
                        }
                    }
                    _ => {}
                }
            }
            Screen::Dialog(_) => {
                if let TouchEvent::Down { x, y } = t {
                    self.tap_dialog(x, y);
                }
            }
            // === C0W begin: Wi-Fi 连接面板（键盘 + Cancel/Connect，整屏）===
            Screen::WifiPass(_) | Screen::WifiJoin(_) => match t {
                TouchEvent::Down { x, y } => self.pending_tap = Some((x, y)),
                TouchEvent::Up => {
                    if self.multi_touch {
                        self.multi_touch = false;
                        self.pending_tap = None;
                        self.drag = None;
                        return;
                    }
                    if let Some((x, y)) = self.pending_tap.take() {
                        self.tap_wifi_connect(x, y);
                    }
                }
                _ => {}
            },
            // === C0W end ===
            Screen::Search(_) | Screen::Rename(_) | Screen::Pair(_) | Screen::Mail(_) => {
                let kind = match self.screen {
                    Screen::Pair(_) => 1u8,
                    Screen::Mail(_) => 2,
                    _ => 0,
                };
                match t {
                    TouchEvent::Down { x, y } => self.pending_tap = Some((x, y)),
                    TouchEvent::Up => {
                        if self.multi_touch {
                            self.multi_touch = false;
                            self.pending_tap = None;
                            self.drag = None;
                            return;
                        }
                        if let Some((x, y)) = self.pending_tap.take() {
                            match kind {
                                1 => self.tap_pair(x, y),
                                2 => self.tap_mail(x, y),
                                _ => self.tap_overlay(x, y),
                            }
                        }
                    }
                    _ => {}
                }
            }
            Screen::Notebook(_) => {
                // 页面总览是另一套界面：点缩略图跳页、长按弹菜单（和笔同一条路），
                // 手指在这里不翻页、不滑动手势。
                if matches!(&self.screen, Screen::Notebook(nb) if nb.overview) {
                    match t {
                        TouchEvent::Down { x, y } => {
                            self.pending_tap = Some((x, y));
                            self.press = Some((x, y, std::time::Instant::now()));
                        }
                        TouchEvent::Move { x, y } => {
                            // 和笔一样：指头挪开 30px 就不算"点了一下缩略图"
                            if let Some((x0, y0, _)) = self.press {
                                if (x - x0).abs() > 30.0 || (y - y0).abs() > 30.0 {
                                    self.pending_tap = None;
                                    self.press = None;
                                }
                            }
                        }
                        TouchEvent::Up => {
                            // 长按已经由 tick 弹出菜单：这次抬指别再点一次
                            if self.skip_up {
                                self.skip_up = false;
                                self.pending_tap = None;
                                self.press = None;
                                return;
                            }
                            let held = self
                                .press
                                .map(|(_, _, t0)| {
                                    t0.elapsed() >= std::time::Duration::from_millis(550)
                                })
                                .unwrap_or(false);
                            let at = self
                                .press
                                .take()
                                .map(|(x, y, _)| (x, y))
                                .or(self.pending_tap.take());
                            if let Some((x, y)) = at {
                                if held && self.open_page_select(x, y) {
                                    return;
                                }
                                self.ui_tap(x, y);
                            }
                        }
                    }
                    return;
                }
                // === D1 begin: 内联回答卡片（手指 = 滚动 / Close；笔不走这里） ===
                if self.d1_touch(&t) {
                    return;
                }
                // === D1 end ===
                // 图层改名键盘（整屏）：先给键盘
                if matches!(&self.screen, Screen::Notebook(nb) if nb.layer_rename.is_some()) {
                    if let TouchEvent::Down { x, y } = t {
                        self.tap_layer_rename(x, y);
                    }
                    return;
                }
                // 标签改名键盘（整屏）：先给键盘
                if matches!(&self.screen, Screen::Notebook(nb) if nb.tag_edit.is_some()) {
                    if let TouchEvent::Down { x, y } = t {
                        self.tap_tag_editor(x, y);
                    }
                    return;
                }
                if matches!(&self.screen, Screen::Notebook(nb) if nb.text_edit.is_some()) {
                    if let TouchEvent::Down { x, y } = t {
                        self.tap_text_editor(x, y);
                    }
                    return;
                }
                // 纯 UI 点击（× / 文档设置 / 标签 / 分享 / 图层 / 工具条 + 菜单）：
                // 被 UI 吃掉就不设 swipe，抬指也就不会结算成翻页。
                // 图层面板另记一个按下点：按住 550ms 由 `tick` 弹改名键盘
                // （官方是层名 `onPressAndHold`，面板动作在按下时已经做掉了）。
                if let TouchEvent::Down { x, y } = t {
                    let lpx = panel_x(LAYER_PANEL_X, LAYER_PANEL_W) as f32;
                    if matches!(&self.screen, Screen::Notebook(nb) if nb.chrome.menu == Menu::Layers)
                        && (x >= lpx && x < lpx + LAYER_PANEL_W as f32)
                    {
                        self.press = Some((x, y, std::time::Instant::now()));
                    }
                    if self.ui_tap(x, y) {
                        return;
                    }
                }
                // === P7 begin ===
                // 文档页滑块（官方 `pageSlider` / fastScroll）：只在**手指**、
                // 只在正文视图、只在最右 60 px 里生效。放在纯 UI 命中**之后** ——
                // 右上角那个 × 在 x 1300..1380，先给滑块的话手指就关不掉文档了。
                if self.p7_touch(t) {
                    return;
                }
                // === P7 end ===
                match t {
                    TouchEvent::Down { x, y } => {
                        if let Screen::Notebook(nb) = &mut self.screen {
                            nb.swipe = Some((x, y, x));
                        }
                    }
                    TouchEvent::Move { x, .. } => {
                        if let Screen::Notebook(nb) = &mut self.screen {
                            if let Some((x0, y0, _)) = nb.swipe {
                                nb.swipe = Some((x0, y0, x));
                            }
                        }
                    }
                    TouchEvent::Up => {
                        // 图层面板的"按住改名"只认按着的那段时间，抬指就作废
                        self.press = None;
                        // 双指松手：只清场，不把这一下结算成翻页
                        if self.multi_touch {
                            self.multi_touch = false;
                            self.pending_tap = None;
                            self.drag = None;
                            return;
                        }
                        let sw = match &mut self.screen {
                            Screen::Notebook(nb) => nb.swipe.take(),
                            _ => None,
                        };
                        // 放大后单指是"平移"（on_gesture 里做），不能顺手翻页
                        let zoomed = matches!(&self.screen, Screen::Notebook(nb) if nb.view.scale > 1.01);
                        if let Some((x0, _y0, x1)) = sw.filter(|_| !zoomed) {
                            let dx = x1 - x0;
                            if dx.abs() > 300.0 {
                                let cur = match &self.screen {
                                    Screen::Notebook(nb) => nb.page_index,
                                    _ => 0,
                                };
                                if dx < 0.0 {
                                    self.goto_page(cur + 1);
                                } else if cur > 0 {
                                    self.goto_page(cur - 1);
                                }
                                return;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn handle_library_pointer(&mut self, ev: PenEvent) {
        // === JOB begin: Job status 整屏开着时笔也归它 ===

        // === JOB end ===
        match ev {
            PenEvent::Down(ip) => {
                self.pending_tap = Some((ip.x, ip.y));
                self.press = Some((ip.x, ip.y, std::time::Instant::now()));
            }
            PenEvent::Up => {
                if self.skip_up {
                    self.skip_up = false;
                    self.pending_tap = None;
                    return;
                }
                if let Some((x, y)) = self.pending_tap.take() {
                    if self.maybe_long_press(x, y) {
                        return;
                    }
                    match &self.screen {
                        Screen::Library => self.tap_library(x, y),
                        Screen::Settings(_) => self.tap_settings(x, y),
                        Screen::Lock(_) => self.tap_lock(x, y),
                        Screen::Dialog(_) => self.tap_dialog(x, y),
                        _ => self.tap_overlay(x, y),
                    }
                }
            }
            PenEvent::Move(ip) => {
                if let Some((x, y)) = self.pending_tap {
                    if (ip.x - x).abs() > 24.0 || (ip.y - y).abs() > 24.0 {
                        self.pending_tap = None;
                    }
                }
            }
            PenEvent::Out => {}
        }
    }

    fn handle_notebook(&mut self, ev: PenEvent) {
        // === AIREF begin: 引用菜单开着时，菜单矩形里的笔点击也认 ===
        if self.ai_menu && matches!(&self.screen, Screen::Notebook(nb) if !nb.overview) {
            // 点开菜单的那一次抬笔（来自工具条第 14 格）到此为止：
            // 它不在菜单矩形里，按"矩形外照常写字"会落到纸上。
            if self.ai_menu_fresh && matches!(ev, PenEvent::Up) {
                self.ai_menu_fresh = false;
                return;
            }
            if self.ai_menu_pen(&ev) {
                return;
            }
        }
        // === AIREF end ===
        // === D1 begin: 框选 AI 的矩形拖动（工具开着时笔不落墨） ===
        if self.d1_pen(&ev) {
            return;
        }
        // === D1 end ===
        let in_overview = matches!(&self.screen, Screen::Notebook(nb) if nb.overview);
        if in_overview {
            match ev {
                PenEvent::Down(ip) => {
                    self.pending_tap = Some((ip.x, ip.y));
                    self.press = Some((ip.x, ip.y, std::time::Instant::now()));
                }
                PenEvent::Up => {
                    if self.skip_up {
                        self.skip_up = false;
                        self.pending_tap = None;
                        return;
                    }
                    // 用**按下点**做命中：注入的抬笔事件常常是 (0,1871)
                    let down = self.press.take();
                    let held = down
                        .map(|(_, _, t)| t.elapsed() >= std::time::Duration::from_millis(550))
                        .unwrap_or(false);
                    let at = down.map(|(x, y, _)| (x, y)).or(self.pending_tap.take());
                    if let Some((x, y)) = at {
                        if held && self.open_page_select(x, y) {
                            return;
                        }
                        self.tap_overview(x, y);
                    }
                }
                PenEvent::Move(ip) => {
                    if let Some((x0, y0)) = self.pending_tap {
                        if (ip.x - x0).abs() > 30.0 || (ip.y - y0).abs() > 30.0 {
                            self.pending_tap = None;
                            self.press = None;
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        if matches!(&self.screen, Screen::Notebook(nb) if nb.layer_rename.is_some()) {
            if let PenEvent::Down(ip) = ev {
                self.tap_layer_rename(ip.x, ip.y);
            }
            return;
        }
        if matches!(&self.screen, Screen::Notebook(nb) if nb.tag_edit.is_some()) {
            if let PenEvent::Down(ip) = ev {
                self.tap_tag_editor(ip.x, ip.y);
            }
            return;
        }
        if matches!(&self.screen, Screen::Notebook(nb) if nb.text_edit.is_some()) {
            if let PenEvent::Down(ip) = ev {
                self.tap_text_editor(ip.x, ip.y);
            }
            return;
        }
        match ev {
            PenEvent::Down(ip) => {
                // 纯 UI 命中（× / 文档设置 / 标签 / 分享 / 图层 / 工具条 + 菜单）：
                // 和手指共用同一条 `ui_tap`。被 UI 吃掉就到此为止，不落墨。
                if self.ui_tap(ip.x, ip.y) {
                    return;
                }
                // 选区模式：笔事件全归选区，一个墨点都不落（工具条/面板上面的分支已处理）
                if matches!(&self.screen, Screen::Notebook(nb) if nb.chrome.selecting) {
                    if let Screen::Notebook(nb) = &mut self.screen {
                        nb.tap_consumed = true;
                    }
                    self.sel_down(ip.x, ip.y);
                    return;
                }
                let nb = match &mut self.screen {
                    Screen::Notebook(nb) => nb,
                    _ => return,
                };
                nb.tap_consumed = false;
                // 文字工具：落笔不画线，抬笔时在这一点开键盘
                if !ip.rubber && nb.chrome.tool() == Tool::Undefined {
                    nb.pending_text = Some((ip.x, ip.y));
                    nb.tap_consumed = true;
                    return;
                }
                // 笔尾（BTN_TOOL_RUBBER）永远是橡皮，与当前选的工具无关；
                // 橡皮菜单里选了"擦选区"时，笔身也走 EraseSection
                let tool = if ip.rubber {
                    rmdev::pen::Tool::Eraser
                } else if nb.chrome.erasing && nb.chrome.erase_section {
                    rmdev::pen::Tool::EraseSection
                } else {
                    nb.chrome.tool()
                };
                // 擦选区（6d）：不是"更粗的路径橡皮"，是拖一个矩形：
                // 按下只记起点，Move 画虚线框，抬笔删与框相交的整条笔迹。
                if tool == rmdev::pen::Tool::EraseSection {
                    let lim = self.canvas.clip_l as f32 + 1.0;
                    nb.erase_rect = Some(((ip.x.max(lim), ip.y), (ip.x.max(lim), ip.y)));
                    self.sel_dirty = None;
                    eprintln!("[erase] 框选开始 ({:.0},{:.0})", ip.x, ip.y);
                    return;
                }
                let ctx = Ctx {
                    p: ip.pressure,
                    tilt: width::tilt_magnitude(ip.tilt_x, ip.tilt_y),
                    tilt_x: ip.tilt_x,
                    tilt_y: ip.tilt_y,
                    ..Ctx::default()
                };
                let w0 = width::width(tool, &ctx, nb.chrome.thickness as f32).w;
                let thickness = if ip.rubber { 2.4f32 } else { nb.chrome.thickness as f32 };
                // 笔宽按视图一起放大：屏幕上的线宽才跟笔尖一致
                let ink_scale = self.pixel_scale * nb.view.scale;
                let mut builder = StrokeBuilder::new(
                    tool,
                    tool_color(tool, nb.chrome.color),
                    thickness,
                    ink_scale,
                );
                // 铅笔硬度：硬铅的粗细不随倾角/压力起伏（软铅保留官方的全部变化）
                if tool.is_pencil() {
                    builder
                        .set_hard_var(rmdev::pen::pencil::grade(nb.chrome.pencil_grade).var);
                }
                // 屏幕 → 页面（视图逆变换）→ 场景（减页面原点）
                let pg = nb.view.screen_to_page((ip.x, ip.y));
                let mut sp = ip;
                sp.x = pg.0 - PAGE_DX;
                sp.y = pg.1 - PAGE_DY;
                builder.begin(sp);
                nb.active = Some(Active {
                    builder,
                    prev: (sp.x, sp.y, w0 * 0.5 * ink_scale),
                    moved: false,
                    tap_x: ip.x,
                    tap_y: ip.y,
                    // 笔尾橡皮按设置页那一档走：固定刷白 / 固定擦整笔 /
                    // 跟橡皮菜单上一次的选择（默认刷白，和官方行为一致）。
                    // 笔身橡皮就是菜单里选的那一档。
                    stroke_erase: if ip.rubber {
                        tail_stroke_erase(self.tail_eraser, self.last_erase_stroke)
                    } else {
                        nb.chrome.erase_stroke
                    },
                    erase: if matches!(
                        tool,
                        rmdev::pen::Tool::Eraser | rmdev::pen::Tool::EraseSection
                    ) {
                        vec![(sp.x, sp.y, w0 * 0.5 * ink_scale)]
                    } else {
                        Vec::new()
                    },
                });
            }
            PenEvent::Move(ip) => {
                // 选区模式：套索加点 / 拖动选区，都不落墨
                if matches!(&self.screen, Screen::Notebook(nb) if nb.chrome.selecting) {
                    self.sel_move(ip.x, ip.y);
                    return;
                }
                // 擦选区拖框（6d）：只更新框 + 画虚线，纸面一个像素都不动
                let preview = match &mut self.screen {
                    Screen::Notebook(nb) => match nb.erase_rect.as_mut() {
                        Some((start, cur)) => {
                            let old = *cur;
                            let lim = self.canvas.clip_l as f32 + 1.0;
                            let now = (ip.x.max(lim).min(self.fb.width as f32 - 1.0), ip.y);
                            if (now.0 - old.0).abs() < 2.0 && (now.1 - old.1).abs() < 2.0 {
                                None
                            } else {
                                *cur = now;
                                Some((*start, old, now))
                            }
                        }
                        None => None,
                    },
                    _ => None,
                };
                if let Some((start, old, now)) = preview {
                    self.repaint_erase_box(Some((start, old)), (start, now));
                    return;
                }
                let nb = match &mut self.screen {
                    Screen::Notebook(nb) => nb,
                    _ => return,
                };
                // 笔宽跟着视图一起放大（画布是页面坐标，放大后要更粗才跟手）
                let view = nb.view;
                let scale = self.pixel_scale * view.scale;
                let pg = view.screen_to_page((ip.x, ip.y));
                let (canvas, fb) = (&mut self.canvas, &mut self.fb);
                let Some(active) = nb.active.as_mut() else {
                    return;
                };
                let mut sp = ip;
                sp.x = pg.0 - PAGE_DX;
                sp.y = pg.1 - PAGE_DY;
                let p = active.builder.add(sp);
                active.moved = true;
                let r = p.width * 0.5 * scale;
                let (px, py, pr) = active.prev;
                let erasing = matches!(
                    active.builder.tool(),
                    rmdev::pen::Tool::Eraser | rmdev::pen::Tool::EraseSection
                );
                if erasing {
                    canvas.erase_segment(
                        px + PAGE_DX,
                        py + PAGE_DY,
                        pr,
                        p.x + PAGE_DX,
                        p.y + PAGE_DY,
                        r,
                    );
                    active.erase.push((p.x, p.y, r));
                } else {
                    // 铅笔走网点（浓度看硬度档 + 压力），其余笔走实心/颜色
                    canvas.set_ink(
                        active.builder.tool(),
                        active.builder.color(),
                        nb.chrome.pencil_grade,
                        p.pressure,
                    );
                    canvas.ink_segment(
                        px + PAGE_DX,
                        py + PAGE_DY,
                        pr,
                        p.x + PAGE_DX,
                        p.y + PAGE_DY,
                        r,
                    );
                }
                // 调试：一条笔迹只看前 3 个点的宽度，够判断 NaN/0 宽
                if env_flag("RM_TIMING") && active.builder.line().points.len() <= 3 {
                    eprintln!(
                        "[ink] {:?} tone={} p={:.3} tilt={:.3} w={:.3} r={:.3} ink_scale={:.3}",
                        active.builder.tool(),
                        tool_color(active.builder.tool(), nb.chrome.color) as u8,
                        p.pressure,
                        width::tilt_magnitude(ip.tilt_x, ip.tilt_y),
                        p.width,
                        r,
                        scale
                    );
                }
                active.prev = (p.x, p.y, r);
                nb.dirty = true;
                let _ = fb;
            }
            PenEvent::Up => {
                // 选区模式：抬笔结算（粘贴 / 拖动 / 闭合套索），不落墨
                if matches!(&self.screen, Screen::Notebook(nb) if nb.chrome.selecting) {
                    self.sel_up();
                    return;
                }
                let Screen::Notebook(nb) = &mut self.screen else {
                    return;
                };
                // 擦选区（6d）：抬笔按矩形删，撤销记一步；只重刷框 + 被删笔迹的范围
                if let Some((a, b)) = nb.erase_rect.take() {
                    let (p, q) = (nb.view.screen_to_page(a), nb.view.screen_to_page(b));
                    let scene = RectF {
                        x: p.0.min(q.0) - PAGE_DX,
                        y: p.1.min(q.1) - PAGE_DY,
                        w: (p.0 - q.0).abs(),
                        h: (p.1 - q.1).abs(),
                    };
                    let (n, bounds) = nb.commit_erase_rect(scene);
                    // 影响范围 = 拖框 ∪ 被删笔迹的包围盒（它们可能探出框外），都换成屏幕坐标
                    let mut scr = (
                        (a.0.min(b.0) - 8.0).floor() as i32,
                        (a.1.min(b.1) - 8.0).floor() as i32,
                        (a.0 - b.0).abs().ceil() as i32 + 16,
                        (a.1 - b.1).abs().ceil() as i32 + 16,
                    );
                    if let Some(g) = bounds {
                        let s0 = nb
                            .view
                            .page_to_screen((g.x + PAGE_DX - 12.0, g.y + PAGE_DY - 12.0));
                        let s1 = nb.view.page_to_screen((
                            g.x + g.w + PAGE_DX + 12.0,
                            g.y + g.h + PAGE_DY + 12.0,
                        ));
                        let u = (
                            s0.0.min(s1.0).floor() as i32,
                            s0.1.min(s1.1).floor() as i32,
                            (s0.0 - s1.0).abs().ceil() as i32,
                            (s0.1 - s1.1).abs().ceil() as i32,
                        );
                        scr = union_rect(scr, u);
                    }
                    eprintln!("[erase] 框选删掉 {n} 笔");
                    self.sel_dirty = None;
                    // 一笔都没删也要刷：纸面没动，但虚线框得擦掉
                    self.repaint_page_rect(scr);
                    return;
                }
                if let Some((x, y)) = nb.pending_text.take() {
                    nb.tap_consumed = false;
                    self.open_text_editor(x, y);
                    return;
                }
                if nb.tap_consumed {
                    nb.tap_consumed = false;
                    return;
                }
                let Some(active) = nb.active.take() else {
                    return;
                };
                self.force_flush = true;
                // 橡皮：把它真正扫到的笔迹从模型里删掉（可撤销）。之前只在 canvas 上
                // 擦像素，屏幕看着擦掉了，翻页回来又全回来。
                // 只有"擦整笔"才删线条。普通橡皮走下面的落墨路径：它把
                // 扫过的像素刷白，并留下一条 `Tool::Eraser` 的橡皮线 ——
                // 官方也是这么存的（渲染时按顺序做减法），否则翻页回来
                // 擦掉的东西会全部复活。
                if !active.erase.is_empty() && active.stroke_erase {
                    let path = active.erase;
                    // 影响范围 = 橡皮扫过的包围盒（外扩最大笔半径），用它局部重刷
                    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
                    let mut rmax = 0.0f32;
                    for &(x, y, r) in &path {
                        x0 = x0.min(x);
                        y0 = y0.min(y);
                        x1 = x1.max(x);
                        y1 = y1.max(y);
                        rmax = rmax.max(r);
                    }
                    let (n, del_bounds) = nb.commit_erase(&path);
                    if n == 0 {
                        eprintln!("[erase] 整笔擦：{} 点，没碰到笔迹", path.len());
                    }
                    if n > 0 {
                        eprintln!("[dev] 橡皮擦掉 {n} 笔");
                        let pad = rmax + 12.0;
                        let sx = (x0 + PAGE_DX - pad).floor() as i32;
                        let sy = (y0 + PAGE_DY - pad).floor() as i32;
                        let ex = (x1 + PAGE_DX + pad).ceil() as i32;
                        let ey = (y1 + PAGE_DY + pad).ceil() as i32;
                        // 橡皮路径的包围盒 ∪ 被删笔迹的包围盒。
                        //
                        // 只刷路径的话，一条横贯半页的笔画被"点擦"删掉之后，
                        // 探出路径的那一大截会留在屏上 —— 用户看到的就是
                        // "擦不干净，只消掉了一部分"。
                        let (mut sx, mut sy, mut ex, mut ey) = (sx, sy, ex, ey);
                        if let Some(b) = del_bounds {
                            sx = sx.min((b.x + PAGE_DX - pad).floor() as i32);
                            sy = sy.min((b.y + PAGE_DY - pad).floor() as i32);
                            ex = ex.max((b.x + b.w + PAGE_DX + pad).ceil() as i32);
                            ey = ey.max((b.y + b.h + PAGE_DY + pad).ceil() as i32);
                        }
                        self.repaint_page_rect((sx, sy, ex - sx, ey - sy));
                    }
                    return;
                }
                // 荧光笔：画的时候用 1px 网点（二值波形，立刻出来、不闪），
                // 抬笔后按**真灰度**重涂这一条带 —— 和原版存/显示的平涂浅灰一致。
                // 只刷这条带子，面板时间几毫秒，闪一下也只在带子上。
                let mut hl_rect: Option<(i32, i32, i32, i32)> = None;
                if active.moved {
                    let line = active.builder.into_line();
                    if line.tool.is_highlighter() {
                        if let Some(b) = line_bbox(&line) {
                            // 包围盒只是**中心线**的范围，横向一条线高度是 0——
                            // 必须再按笔宽撑开，否则重涂只覆盖到中间 9 像素。
                            let half = line
                                .points
                                .iter()
                                .fold(0.0f32, |m, p| m.max(p.width * 0.5));
                            let pad = half + 4.0;
                            hl_rect = Some((
                                (b.x + PAGE_DX - pad) as i32,
                                (b.y + PAGE_DY - pad) as i32,
                                (b.w + 2.0 * pad) as i32,
                                (b.h + 2.0 * pad) as i32,
                            ));
                        }
                    }
                    if !line.points.is_empty() {
                        let layer = nb.page.current;
                        let index = nb.page.lines_mut().len();
                        nb.page.lines_mut().push(line.clone());
                        // 新笔迹 = 可撤销的一步；同时让重做栈作废
                        nb.undo.push(Op::Add {
                            layer,
                            index,
                            lines: vec![line],
                        });
                        nb.redo.clear();
                        nb.dirty = true;
                    }
                    // 抬笔后只更新"可能变了"的那两格：撤销/重做的可用态。
                    // 以前这里调 `chrome.draw`，它在内部整列 120x1872 来一发 GC16 ——
                    // 每写一笔都要在左侧闪一条竖条，还平白多花三四十毫秒。
                    let undo_before = nb.chrome.undo_ok;
                    nb.sync_undo();
                    let undo_changed = undo_before != nb.chrome.undo_ok;
                    nb.chrome.draw_in(&mut self.fb, &mut self.text);
                    if undo_changed {
                        self.repaint_rects(&[(rmdev::chrome::band_x(), 600, CHROME_W, 240)], WAVEFORM_MODE_GC16);
                    }
                } else {
                    // 纯点击：只有落在左下角才计入退出；点别处清零。
                    let (x, y) = (active.tap_x, active.tap_y);
                    if x < 64.0 && y > (self.fb.height - 64) as f32 {
                        nb.exit_taps += 1;
                        eprintln!("[dev] 退出手势 {}/3", nb.exit_taps);
                        if nb.exit_taps >= 3 {
                            eprintln!("[dev] 退出");
                            std::process::exit(0);
                        }
                    } else {
                        nb.exit_taps = 0;
                    }
                }
                if let Some(r) = hl_rect {
                    self.repaint_page_rect(r);
                }
                // === C15 begin ===
                // 这一笔要是落在末尾的虚拟空白页上：立刻把它变成真页（末尾再挂一张），
                // 顺手刷一下屏底页码——M 变了一页。
                self.c15_on_pen_up();
                // === C15 end ===
            }
            PenEvent::Out => {}
        }
    }

    // ------------------------------------------------------------ 选区（套索）

    /// 选区模式下笔按下。优先级：悬浮条 > 拖动选区 > 粘贴 > 新套索。
    ///
    /// 坐标全是**屏幕**坐标：`select` 模块要的也是屏幕坐标，只有 `Line.points`
    /// 是场景坐标，所以进 `to_scene`、出 `rect_to_view`。
    fn sel_down(&mut self, x: f32, y: f32) {
        // 1) 悬浮条
        let bars = match &self.screen {
            Screen::Notebook(nb) => nb.sel.as_ref().map(|s| select::bar_layout(rect_to_view(s.bounds))),
            _ => None,
        };
        if let Some(bars) = bars {
            if let Some(act) = select::bar_hit(&bars, x, y) {
                self.sel_act(act);
                return;
            }
        }
        // 2) 落在选框里 → 拖动（只改模型，抬笔才重画：EPDC 没有"区域恢复"的 API）
        let in_sel = match &self.screen {
            Screen::Notebook(nb) => nb
                .sel
                .as_ref()
                .is_some_and(|s| select::inside(s.bounds, to_scene((x, y)))),
            _ => false,
        };
        if in_sel {
            if let Screen::Notebook(nb) = &mut self.screen {
                nb.drag_sel = Some(DragSel {
                    from: (x, y),
                    last: (x, y),
                    moved: false,
                });
            }
            eprintln!("[sel] 拖动开始：按下 ({x:.0},{y:.0})");
            return;
        }
        // 3) 空白处：剪贴板非空就先记成"点一下粘贴"，笔一动就改回套索
        let has_clip = !self.clipboard.is_empty();
        let had_sel = matches!(&self.screen, Screen::Notebook(nb) if nb.sel.is_some());
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.sel = None;
            nb.drag_sel = None;
            nb.lasso = Some(vec![(x, y)]);
            nb.pending_paste = if has_clip { Some((x, y)) } else { None };
        }
        eprintln!(
            "[sel] 套索开始 ({x:.0},{y:.0}){}",
            if has_clip { "（剪贴板非空：抬笔没动就粘贴）" } else { "" }
        );
        // 旧选框还留在屏上，而套索是增量画的，只能整页重画一次把它擦掉
        if had_sel {
            self.redraw_page();
        }
    }

    /// 套索加一个点（2px 内抽稀），返回新画的这一段
    fn lasso_step(&mut self, x: f32, y: f32) -> Option<((f32, f32), (f32, f32))> {
        let Screen::Notebook(nb) = &mut self.screen else {
            return None;
        };
        let lasso = nb.lasso.as_mut()?;
        let prev = *lasso.last()?;
        // 笔一动，"点一下粘贴"就不成立了
        if let Some((px, py)) = nb.pending_paste {
            if (x - px).hypot(y - py) > 24.0 {
                nb.pending_paste = None;
                eprintln!("[sel] 笔动了 → 不做粘贴，改成套索");
            }
        }
        if (x - prev.0).hypot(y - prev.1) < 2.0 {
            return None;
        }
        // 套索不许画进工具条那一条（画进去就会把图标啃掉，且它不属于页面）
        let x = clamp_paper_f(x);
        lasso.push((x, y));
        Some((prev, (x, y)))
    }

    /// 拖动选区：按增量平移，`sel.bounds` 跟着走（整体平移不用重算）
    fn drag_step(&mut self, x: f32, y: f32) {
        let Screen::Notebook(nb) = &mut self.screen else {
            return;
        };
        let Some(d) = nb.drag_sel.as_mut() else {
            return;
        };
        let (dx, dy) = (x - d.last.0, y - d.last.1);
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        d.last = (x, y);
        d.moved = true;
        let Some(idx) = nb.sel.as_ref().map(|s| s.idx.clone()) else {
            return;
        };
        select::translate(nb.page.lines_mut(), &idx, dx, dy);
        if let Some(s) = nb.sel.as_mut() {
            s.bounds.x += dx;
            s.bounds.y += dy;
        }
    }

    /// 选区模式下笔移动
    fn sel_move(&mut self, x: f32, y: f32) {
        if matches!(&self.screen, Screen::Notebook(nb) if nb.drag_sel.is_some()) {
            self.drag_step(x, y);
            return;
        }
        let Some((a, b)) = self.lasso_step(x, y) else {
            return;
        };
        // 增量：只画新的一段，脏区交给 flush_pending 按节奏推
        draw_lasso_seg(&mut self.fb, a, b);
        let r = seg_box(a, b);
        self.sel_dirty = Some(match self.sel_dirty {
            Some(u) => union_rect(u, r),
            None => r,
        });
    }

    /// 选区模式下抬笔：点一下空白 = 粘贴；拖过 = 移动；否则闭合套索选笔迹。
    fn sel_up(&mut self) {
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.tap_consumed = false;
        }
        // 1) 粘贴（按下后没动过）
        let paste_at = match &mut self.screen {
            Screen::Notebook(nb) => nb.pending_paste.take(),
            _ => None,
        };
        if let Some((x, y)) = paste_at {
            if let Screen::Notebook(nb) = &mut self.screen {
                nb.lasso = None;
            }
            self.sel_paste(x, y);
            return;
        }
        // 2) 拖动结算
        let drag = match &mut self.screen {
            Screen::Notebook(nb) => nb.drag_sel.take(),
            _ => None,
        };
        if let Some(d) = drag {
            if !d.moved {
                // 点在选框里但没动：选区保留，什么都不做
                return;
            }
            let (dx, dy) = (d.last.0 - d.from.0, d.last.1 - d.from.1);
            let (layer, idx) = match &self.screen {
                Screen::Notebook(nb) => (
                    nb.page.current,
                    nb.sel.as_ref().map(|s| s.idx.clone()).unwrap_or_default(),
                ),
                _ => return,
            };
            if !idx.is_empty() {
                if let Screen::Notebook(nb) = &mut self.screen {
                    // 一步撤销：把同样的位移搬回去
                    nb.undo.push(Op::Move {
                        layer,
                        idx: idx.clone(),
                        dx,
                        dy,
                    });
                    nb.redo.clear();
                    nb.dirty = true;
                }
                eprintln!("[sel] 拖动完成 (dx={dx:.0}, dy={dy:.0})，{} 笔", idx.len());
                self.redraw_page();
            }
            return;
        }
        // 3) 套索结算
        let poly = match &mut self.screen {
            Screen::Notebook(nb) => nb.lasso.take(),
            _ => None,
        };
        let Some(poly) = poly else {
            return;
        };
        let scene: Vec<(f32, f32)> = poly.iter().copied().map(to_scene).collect();
        let idx = match &self.screen {
            Screen::Notebook(nb) => select::lasso_select(nb.page.lines(), &scene),
            _ => Vec::new(),
        };
        let bounds = match &self.screen {
            Screen::Notebook(nb) => select::bounds_of(nb.page.lines(), &idx),
            _ => None,
        };
        match bounds {
            Some(b) => {
                if let Screen::Notebook(nb) = &mut self.screen {
                    nb.sel = Some(Sel {
                        idx: idx.clone(),
                        bounds: b,
                    });
                }
                eprintln!(
                    "[sel] 套索闭合 {} 点 → 选中 {} 笔，bbox ({:.0},{:.0}) {:.0}x{:.0}（场景坐标）",
                    poly.len(),
                    idx.len(),
                    b.x,
                    b.y,
                    b.w,
                    b.h
                );
            }
            None => {
                if let Screen::Notebook(nb) = &mut self.screen {
                    nb.sel = None;
                }
                eprintln!("[sel] 套索闭合 {} 点 → 没选中，清空", poly.len());
            }
        }
        // 套索折线还在屏上，选框/悬浮条也要画出来
        self.redraw_page();
    }

    /// 悬浮条上的动作。条上只有 copy / cut（`select.rs` 的规格），别的组合留着备用。
    fn sel_act(&mut self, act: SelAct) {
        match act {
            SelAct::Copy => {
                let picked: Vec<Line> = match &self.screen {
                    Screen::Notebook(nb) => match &nb.sel {
                        Some(s) => s
                            .idx
                            .iter()
                            .filter_map(|&i| nb.page.lines().get(i).cloned())
                            .collect(),
                        None => Vec::new(),
                    },
                    _ => Vec::new(),
                };
                if picked.is_empty() {
                    return;
                }
                eprintln!("[sel] 复制 {} 笔（剪贴板 {} 笔）", picked.len(), picked.len());
                self.clipboard = picked;
                // 官方 copy 之后选区还在，画面不用动
                // === P13 begin ===
                // 但底部要立刻出现剪贴板提示条（官方 `Clipboard.onItemsChanged`）
                let (w, h) = (self.fb.width, self.fb.height);
                self.repaint_rects(&[rmdev::chrome::clip_toast_rect(w, h)], WAVEFORM_MODE_GC16);
                // === P13 end ===
            }
            SelAct::Cut => {
                let (layer, mut idx) = match &self.screen {
                    Screen::Notebook(nb) => match &nb.sel {
                        Some(s) => (nb.page.current, s.idx.clone()),
                        None => return,
                    },
                    _ => return,
                };
                idx.sort_unstable();
                idx.dedup();
                if idx.is_empty() {
                    return;
                }
                let (taken, rest) = match &mut self.screen {
                    Screen::Notebook(nb) => {
                        let taken = select::take(nb.page.lines_mut(), &idx);
                        (taken, nb.page.total_lines())
                    }
                    _ => return,
                };
                if taken.is_empty() {
                    return;
                }
                eprintln!(
                    "[sel] 剪切 {} 笔（页面剩 {} 笔，剪贴板 {} 笔）",
                    taken.len(),
                    rest,
                    taken.len()
                );
                self.clipboard = taken.clone();
                // === P13 begin ===
                // 剪切也算"复制过了"：底部马上给提示条
                let (w, h) = (self.fb.width, self.fb.height);
                self.repaint_rects(&[rmdev::chrome::clip_toast_rect(w, h)], WAVEFORM_MODE_GC16);
                // === P13 end ===
                if let Screen::Notebook(nb) = &mut self.screen {
                    // 一步撤销：按原下标放回去
                    nb.undo.push(Op::Remove {
                        layer,
                        items: idx.into_iter().zip(taken).collect(),
                    });
                    nb.redo.clear();
                    nb.dirty = true;
                    nb.sel = None;
                    nb.lasso = None;
                }
                self.redraw_page();
            }
            other => eprintln!("[sel] 悬浮条上没有 {other:?}（规格只有 copy/cut）"),
        }
    }

    /// 把剪贴板贴到屏幕点 `(x, y)`：内容包围盒的中心对齐笔尖。
    fn sel_paste(&mut self, x: f32, y: f32) {
        if self.clipboard.is_empty() {
            return;
        }
        let mut lines = self.clipboard.clone();
        let all: Vec<usize> = (0..lines.len()).collect();
        let (tx, ty) = to_scene((x, y));
        if let Some(b) = select::bounds_of(&lines, &all) {
            let (dx, dy) = (tx - (b.x + b.w * 0.5), ty - (b.y + b.h * 0.5));
            select::translate(&mut lines, &all, dx, dy);
        }
        let n = lines.len();
        let (layer, at) = match &mut self.screen {
            Screen::Notebook(nb) => {
                let at = nb.page.lines_mut().len();
                // put_back：追加到当前层末尾（坐标是绝对的，页内位置不变）
                select::put_back(nb.page.lines_mut(), lines.clone());
                (nb.page.current, at)
            }
            _ => return,
        };
        eprintln!("[sel] 粘贴 {n} 笔 → 屏幕 ({x:.0},{y:.0})，第 {} 层末尾（下标 {at} 起）", layer + 1);
        if let Screen::Notebook(nb) = &mut self.screen {
            // 一步撤销：把刚插进去的这一段删掉
            nb.undo.push(Op::Add {
                layer,
                index: at,
                lines,
            });
            nb.redo.clear();
            nb.dirty = true;
            nb.sel = None;
            nb.lasso = None;
        }
        // === C15 begin ===
        // 贴在末尾的虚拟空白页上：立刻把它变成真页（贴图也是一次文档改动）
        self.c15_commit_virtual();
        // === C15 end ===
        self.redraw_page();
    }

    /// 把这一批攒下的墨一次推上屏。
    /// 局部刷新统一入口：**永远不刷左侧工具条那一条**（`x < canvas.clip_l`）。
    ///
    /// 用 DU 波形去刷一块"内容没变"的静态图标（左上圆环、撤销/重做箭头）会把像素
    /// 啃花，表现就是"图标缺一块 / 变透明"。页面内容本来也不会画进这一条
    /// （`canvas.clip_l` 就是为这个设的），所以裁掉不影响正确性。
    fn refresh_part(&mut self, x: i32, y: i32, w: i32, h: i32) {
        let x0 = x.max(self.canvas.clip_l);
        let x1 = x + w;
        if x1 <= x0 {
            return;
        }
        self.fb.refresh(
            x0,
            y,
            x1 - x0,
            h,
            WAVEFORM_MODE_DU,
            rmdev::fb::UPDATE_MODE_PARTIAL,
        );
    }

    fn flush_pending(&mut self, min_interval: Duration) -> bool {
        // EPDC 一次 update 要几十毫秒，推得比面板快只会把队列堆起来 → 手感发飘。
        // 所以按固定节奏推，中间攒下来的脏区合并成一次。
        if !self.force_flush && self.last_flush.elapsed() < min_interval {
            return false;
        }
        self.force_flush = false;
        // 键盘开着时画布是静态的：刷一次就会把键盘盖掉
        let view = match &self.screen {
            Screen::Notebook(nb) if nb.text_edit.is_none() => Some(nb.view),
            _ => None,
        };
        // 灰色墨（铅笔/荧光笔）不能用二值波形推：中间灰在面板上根本显示不出来
        // （帧缓冲里有、屏幕上一片空白）。黑墨走 **A2**（最快的那档二值波形）。
        //
        // 以前是 DU：25 相位 ≈ 294 ms，写完一笔要小半秒才完全黑到位，手感是
        // "跟手但发飘"。A2 只有 12 相位 ≈ 141 ms，同一套二值语义（实测
        // `A2 黑←白` = 推黑 10 帧、`A2 白←白` / `黑←黑` 全 0，不乱推），
        // 官方 rM2 的笔就是 A2。
        let wf = if self.canvas.dirty_gray() {
            rmdev::fb::WAVEFORM_MODE_GL16
        } else {
            rmdev::fb::WAVEFORM_MODE_A2
        };
        let mut rect = match view {
            Some(_) => self.canvas.flush(&mut self.fb, wf),
            None => None,
        };
        // 放大/平移之后，`flush` 刷的那块"页面矩形"和屏幕上该更新的区域不是一回事：
        // 那块页面矩形刚被按 1:1 写进帧缓冲（内容不对），要按变换重画；再算出真正的
        // 屏幕矩形一起刷。scale==1 且 rot==0 时 `flush` 自己刷的就是对的，走原来的
        // 快路径（一块都不重贴）。
        if let (Some(v), Some(r)) = (view, rect) {
            if !view_identity(&v) {
                let sr = self.canvas.screen_rect_of(&v, r);
                self.canvas.paint_view_rect(&mut self.fb, &v, r);
                self.canvas.paint_view_rect(&mut self.fb, &v, sr);
                let u = union_rect(r, sr);
                self.refresh_part(u.0, u.1, u.2, u.3);
                if env_flag("RM_DEBUG") {
                    eprintln!("[view] 增量刷新 页面 {r:?} → 屏幕 {sr:?}，一起刷 {u:?}");
                }
                rect = Some(u);
            }
        }
        // 套索/选框不走 canvas（纸面没动，canvas 脏区是空的），脏区自己攒
        let sel = self.sel_dirty.take();
        if let Some((sx, sy, sw, sh)) = sel {
            self.refresh_part(sx, sy, sw, sh);
        }
        let rect = match (rect, sel) {
            (Some(a), Some(b)) => Some(union_rect(a, b)),
            (a, b) => a.or(b),
        };
        if let Some((x, y, w, h)) = rect {
            self.last_flush = std::time::Instant::now();
            self.repaint_overlays(x, y, w, h);
            return true;
        }
        false
    }

    /// 纸面重刷之后，把被盖住的浮层补回来。
    ///
    /// canvas 只在 `x >= CHROME_W` 的范围内画，所以左侧工具条本来就不会被盖；
    /// 但**右上角的关闭按钮**和**底部页码**在纸面里，笔迹刷过去就没了。
    /// 这里只在脏区真的碰到它们时才补画，平时零开销。
    /// 图层面板占地（没开或不在笔记本视图时 None）
    fn layers_panel_rect(&self) -> Option<(i32, i32, i32, i32)> {
        let (n, has_text) = match &self.screen {
            // === P11 begin ===
            // 高度要跟 `paint_layers_panel` 用同一套算法（含文字层那一行），
            // 否则浮层补画会盖不住面板顶边
            Screen::Notebook(nb) if nb.chrome.menu == Menu::Layers => {
                (nb.page.layers.len() as i32, !nb.texts.is_empty())
            }
            // === P11 end ===
            _ => return None,
        };
        // === P11 begin ===
        let h = p11_layers_height(n, has_text);
        // === P11 end ===
        Some((
            panel_x(LAYER_PANEL_X, LAYER_PANEL_W),
            LAYER_BTN_BOTTOM - h,
            LAYER_PANEL_W,
            h,
        ))
    }

    fn repaint_overlays(&mut self, x: i32, y: i32, w: i32, h: i32) {
        let hit = |bx: i32, by: i32, bw: i32, bh: i32| {
            x < bx + bw && bx < x + w && y < by + bh && by < y + h
        };
        let mut need_close = false;
        let mut need_footer = false;
        // 工具条收起时，页面上任何一笔都可能画到左上角那个圆环上（canvas 的
        // clip_l 这时是 0，整屏都可以画）。碰到就把圆环补回去。
        let collapsed = matches!(&self.screen, Screen::Notebook(nb) if !nb.chrome.visible);
        if collapsed && hit(rmdev::chrome::band_x(), 0, CHROME_W, CHROME_W) {
            // 先把这一格从纸面重画回去（笔迹可能正好从圆环下面穿过），再补圆环
            let view = match &self.screen {
                Screen::Notebook(nb) => nb.view,
                _ => PageView::identity(),
            };
            self.canvas
                .paint_view_rect(&mut self.fb, &view, band_rect(0, CHROME_W));
            if let Screen::Notebook(nb) = &self.screen {
                nb.chrome.draw_in(&mut self.fb, &mut self.text);
            }
            self.fb.refresh(
                rmdev::chrome::band_x(),
                0,
                CHROME_W,
                CHROME_W,
                WAVEFORM_MODE_DU,
                rmdev::fb::UPDATE_MODE_PARTIAL,
            );
        }
        let (clx, cly, clw, clh) = close_rect();
        if hit(clx, cly, clw, clh) {
            self.paint_close();
            need_close = true;
        }
        let (ftx, fty, ftw, fth) = footer_rect();
        if hit(ftx, fty, ftw, fth) {
            self.paint_footer();
            need_footer = true;
        }
        // 画布的局部刷新也会把页面上的文字盖掉：脏区碰到哪段就补画哪段。
        // 位置和 `redraw_page` 一样要过视图变换，否则放大后会画回原位。
        let (texts, view) = match &self.screen {
            Screen::Notebook(nb) => (nb.texts.clone(), nb.view),
            _ => (Vec::new(), PageView::identity()),
        };
        for it in &texts {
            let p = view.page_to_screen((it.x as f32 + PAGE_DX, it.y as f32 + PAGE_DY));
            let px = p.0 as i32;
            let py = p.1 as i32;
            if under_band(px) {
                continue;
            }
            let tw = self
                .text
                .as_mut()
                .map(|t| t.width(it.size, &it.text) as i32)
                .unwrap_or(0);
            let (bx, by, bw, bh) = (px - 4, py - (it.size * 1.1) as i32, tw + 12, (it.size * 1.5) as i32);
            if hit(bx, by, bw, bh) {
                self.fb.fill_rect(bx, by, bw, bh, gray565(255));
                if let Some(t) = self.text.as_mut() {
                    t.draw(&mut self.fb, px, py, it.size, &it.text, 20);
                }
                self.refresh_part(bx, by, bw, bh);
            }
        }

        // 套索/选框/悬浮条也是画在纸面上的，canvas 局部刷新会把它们盖掉。
        // （套索是整条重画：它只有几十上百段，比精确裁剪简单，也不会画错。）
        if let Some((lx, ly, lw, lh)) = self.lasso_rect() {
            if hit(lx, ly, lw, lh) {
                let poly = match &self.screen {
                    Screen::Notebook(nb) => nb.lasso.clone(),
                    _ => None,
                };
                if let Some(poly) = poly {
                    for w in poly.windows(2) {
                        draw_lasso_seg(&mut self.fb, w[0], w[1]);
                    }
                }
                self.refresh_part(lx, ly, lw, lh);
            }
        }
        if let Some((sx, sy, sw, sh)) = self.sel_overlay_rect() {
            if hit(sx, sy, sw, sh) {
                let view = match &self.screen {
                    Screen::Notebook(nb) => nb.sel.as_ref().map(|s| rect_to_view(s.bounds)),
                    _ => None,
                };
                if let Some(view) = view {
                    dashed_rect(&mut self.fb, view, 2, 20);
                    self.paint_sel_bar(view);
                }
                self.refresh_part(sx, sy, sw, sh);
            }
        }

        // 画布的局部刷新会把面板整块盖掉，所以只要面板开着就无条件重画它
        if let Some((px, py, pw, ph)) = self.tags_panel_rect() {
            self.paint_tags_panel();
            self.fb.refresh(px, py, pw, ph, WAVEFORM_MODE_DU, rmdev::fb::UPDATE_MODE_PARTIAL);
        }
        if let Some((px, py, pw, ph)) = self.share_panel_rect() {
            self.paint_share_panel();
            self.fb.refresh(px, py, pw, ph, WAVEFORM_MODE_DU, rmdev::fb::UPDATE_MODE_PARTIAL);
        }
        if let Some((px, py, pw, ph)) = self.layers_panel_rect() {
            self.paint_layers_panel();
            self.fb.refresh(px, py, pw, ph, WAVEFORM_MODE_DU, rmdev::fb::UPDATE_MODE_PARTIAL);
        }
        // 文档设置面板同理：笔迹刷过它就得补回来（只在真的碰到时补，省一次 EPDC）
        if self.ds.is_open() {
            let kind = self.doc_kind();
            if let Some((px, py, pw, ph)) = self.ds.rect(kind) {
                if hit(px, py, pw, ph) {
                    self.paint_docsettings();
                    self.fb.refresh(px, py, pw, ph, WAVEFORM_MODE_DU, rmdev::fb::UPDATE_MODE_PARTIAL);
                }
            }
        }
        if need_close {
            let (clx, cly, clw, clh) = close_rect();
            self.fb
                .refresh(clx, cly, clw, clh, WAVEFORM_MODE_DU, rmdev::fb::UPDATE_MODE_PARTIAL);
        }
        if need_footer {
            let (ftx, fty, ftw, fth) = footer_rect();
            self.fb
                .refresh(ftx, fty, ftw, fth, WAVEFORM_MODE_DU, rmdev::fb::UPDATE_MODE_PARTIAL);
        }
        // === D1 begin: 墨迹的局部刷新别把卡片/浮层啃掉 ===
        self.d1_repaint_overlays(x, y, w, h);
        // === D1 end ===
    }

    fn on_chrome(&mut self, msg: Msg) {
        // 同一个人手快连点同一个工具按钮：只认第一次。
        // （笔点击没这个问题，但手指双击太常见；官方是纯 toggle，我们这里多一层去抖，
        //   差别只体现在 350ms 内的连点，正常使用感觉不出来。）
        if matches!(
            msg,
            Msg::PenMenu | Msg::Eraser | Msg::SettingsMenu | Msg::Selection
        ) {
            let now = std::time::Instant::now();
            if let Some((m, t)) = self.last_tb {
                if m == msg && now.duration_since(t) < std::time::Duration::from_millis(350) {
                    return;
                }
            }
            self.last_tb = Some((msg, now));
        }
        // 点工具条上别的按钮就算离开文档设置面板。面板是直接画进帧缓冲的，
        // 收起来必须整页重绘，否则纸上会留着它的残影（所以只在函数末尾补一次）。
        let ds_was_open = self.ds.is_open();
        if ds_was_open && msg != Msg::DocSettings {
            self.ds.close();
        }
        // === D1 begin: 第 13 格（框选问 AI）与别的工具互斥 ===
        if self.d1_on_chrome(msg) {
            return;
        }
        // === D1 end ===
        // 先处理不需要改工具条状态的
        match msg {
            Msg::Back => {
                self.close_notebook();
                return;
            }
            Msg::Undo => {
                let mut did = false;
                let mut log = String::new();
                let mut rect = None;
                if let Screen::Notebook(nb) = &mut self.screen {
                    // 撤销会让 sel.idx 全部失效，先清掉
                    nb.clear_sel();
                    if let Some(op) = nb.undo.pop() {
                        // 影响范围要在 revert 之前算（Move 要用改动前的坐标）
                        rect = op_screen_rect(&op, &nb.page.layers);
                        if op.revert(&mut nb.page.layers) {
                            nb.redo.push(op.clone());
                            nb.dirty = true;
                            did = true;
                            log = format!(
                                "[dev] undo {} → {} 笔（可重做 {}）",
                                op.label(),
                                nb.page.total_lines(),
                                nb.redo.len()
                            );
                        } else {
                            eprintln!("[sel] undo 失败：{} 的下标对不上了", op.label());
                        }
                    }
                }
                if did {
                    eprintln!("{log}");
                    if let Some(r) = rect {
                        self.repaint_page_rect(r);
                    } else {
                        self.redraw_page();
                    }
                }
                return;
            }
            Msg::Redo => {
                let mut did = false;
                let mut log = String::new();
                let mut rect = None;
                if let Screen::Notebook(nb) = &mut self.screen {
                    nb.clear_sel();
                    if let Some(op) = nb.redo.pop() {
                        let label = op.label();
                        rect = op_screen_rect(&op, &nb.page.layers);
                        if op.apply(&mut nb.page.layers) {
                            nb.undo.push(op.clone());
                            nb.dirty = true;
                            did = true;
                            log = format!(
                                "[dev] redo {} → {} 笔（可撤销 {}）",
                                label,
                                nb.page.total_lines(),
                                nb.undo.len()
                            );
                        } else {
                            eprintln!("[sel] redo 失败：{label} 的下标对不上了");
                        }
                    }
                }
                if did {
                    eprintln!("{log}");
                    if let Some(r) = rect {
                        self.repaint_page_rect(r);
                    } else {
                        self.redraw_page();
                    }
                }
                return;
            }
            Msg::Hide => {
                let menu_rect = match &self.screen {
                    Screen::Notebook(nb) => nb.chrome.menu_rect(),
                    _ => None,
                };
                if let Screen::Notebook(nb) = &mut self.screen {
                    nb.chrome.visible = false;
                    nb.chrome.menu = Menu::None;
                }
                // 只刷受影响的几条：工具条那一列归纸面了、× 和页码要收起来。
                // 以前这里整屏 GC16，收个工具条都要闪一下。
                let mut rects = vec![
                    (rmdev::chrome::band_x(), 0, CHROME_W, self.fb.height),
                    close_rect(),
                    footer_rect(),
                ];
                if let Some(r) = menu_rect {
                    rects.push(r);
                }
                self.repaint_rects(&rects, WAVEFORM_MODE_GC16);
                return;
            }
            Msg::Show => {
                if let Screen::Notebook(nb) = &mut self.screen {
                    nb.chrome.visible = true;
                }
                self.repaint_rects(
                    &[
                        (rmdev::chrome::band_x(), 0, CHROME_W, self.fb.height),
                        close_rect(),
                        footer_rect(),
                    ],
                    WAVEFORM_MODE_GC16,
                );
                return;
            }
            Msg::Eraser | Msg::EraseSection | Msg::Selection | Msg::ClearLayer => {}
            Msg::PageOverview => {
                self.open_overview();
                return;
            }
            Msg::DocRename => {
                let (uuid, name) = match &self.screen {
                    Screen::Notebook(nb) => (nb.uuid.clone(), nb.name.clone()),
                    _ => return,
                };
                self.open_rename(&uuid, &name);
                return;
            }
            // 图层改名（6a）：长按层名 / 双击层名都走这条
            Msg::LayerRename(i) => {
                self.open_layer_rename(i);
                return;
            }
            Msg::DocTrash => {
                if let Screen::Notebook(nb) = &mut self.screen {
                    let uuid = nb.uuid.clone();
                    let _ = self.library.delete_book(&uuid);
                    eprintln!("[dev] 已移到回收站 {uuid}");
                }
                self.close_notebook();
                return;
            }
            Msg::TextTool => {
                // "A" 按钮 = 文字工具（TOOLS 最后一项是 Text）
                if let Screen::Notebook(nb) = &mut self.screen {
                    nb.chrome.selected = TOOLS.len() - 1;
                    nb.chrome.erasing = false;
                    nb.chrome.selecting = false;
                    nb.clear_sel();
                    nb.chrome.menu = Menu::None;
                }
                self.redraw_page();
                return;
            }
            Msg::DocMenu => {
                if let Screen::Notebook(nb) = &mut self.screen {
                    nb.chrome.menu = if nb.chrome.menu == Menu::Doc {
                        Menu::None
                    } else {
                        Menu::Doc
                    };
                }
                self.redraw_page();
                return;
            }
            Msg::Layers => {
                if let Screen::Notebook(nb) = &mut self.screen {
                    nb.chrome.menu = if nb.chrome.menu == Menu::Layers {
                        Menu::None
                    } else {
                        Menu::Layers
                    };
                }
                self.redraw_page();
                return;
            }
            Msg::Share => {
                if let Screen::Notebook(nb) = &mut self.screen {
                    nb.chrome.menu = if nb.chrome.menu == Menu::Share {
                        Menu::None
                    } else {
                        Menu::Share
                    };
                }
                self.redraw_page();
                return;
            }
            Msg::Tags => {
                if let Screen::Notebook(nb) = &mut self.screen {
                    nb.chrome.menu = if nb.chrome.menu == Menu::Tags {
                        Menu::None
                    } else {
                        Menu::Tags
                    };
                }
                self.redraw_page();
                return;
            }
            Msg::DocSettings => {
                // 工具条最下面那格：开/关文档设置面板。面板的行/命中/绘制都在
                // `panels::docsettings` 里，这里只给类型（PDF 还是笔记本）。
                let kind = self.doc_kind();
                // 面板矩形要在 toggle 之前取：关掉以后 rect() 就没了
                let panel = self.ds.rect(kind);
                self.ds.toggle(kind);
                eprintln!(
                    "[ds] 面板{}（{kind:?}）",
                    if self.ds.is_open() { "开" } else { "关" }
                );
                if let Screen::Notebook(nb) = &mut self.screen {
                    // 别的菜单让位，免得两块面板叠在一起
                    nb.chrome.menu = Menu::None;
                }
                let mut rects = vec![(rmdev::chrome::band_x(), 0, CHROME_W, self.fb.height), footer_rect()];
                if let Some(r) = panel {
                    rects.push(r);
                }
                self.repaint_rects(&rects, WAVEFORM_MODE_GC16);
                return;
            }
            Msg::Selection => {}
            _ => {}
        }

        let Screen::Notebook(nb) = &mut self.screen else {
            return;
        };
        // "清空图层"会动整页内容，光刷工具条那几格等于什么都没发生（用户看到的就是
        // "点了 erase all 卡住"）。这个标志让收尾走整页重绘。
        let mut page_cleared = false;
        // 重画范围靠"改动前/改动后"对比决定：只是切工具就只刷工具条那一列
        // （`Chrome::draw` 只刷 120 宽），菜单收起或模式切换才整页重绘。
        let was = (nb.chrome.menu, nb.chrome.erasing, nb.chrome.selecting);
        // 设置菜单里改"粗细/颜色"时菜单不关，只有两行反白条变了，得单独记下来
        let feather_before = nb.chrome.feather;
        let th_before = nb.chrome.thickness_idx();
        let col_before = nb.chrome.color_idx();
        let grade_before = nb.chrome.pencil_grade;
        // 菜单收起时要把它盖住的纸面补回来，rect 必须在改 menu 之前取
        let menu_rect_before = nb.chrome.menu_rect();
        match msg {
            // 笔：官方的两段式 —— 先选中笔，**已经是笔了再点**才开/收那 9 支笔的菜单
            Msg::PenMenu => {
                if nb.chrome.menu == Menu::Pens {
                    nb.chrome.menu = Menu::None;
                } else if !nb.chrome.erasing
                    && !nb.chrome.selecting
                    && nb.chrome.selected + 1 < TOOLS.len()
                {
                    nb.chrome.menu = Menu::Pens;
                } else {
                    // 从橡皮/选区/文字切回笔：官方是 `currentBrush = lastWritingTool`
                    nb.chrome.selected = nb.last_pen.min(TOOLS.len() - 2);
                    nb.chrome.erasing = false;
                    nb.chrome.selecting = false;
                    nb.chrome.erase_section = false;
                    nb.chrome.menu = Menu::None;
                    nb.clear_sel();
                }
            }
            Msg::SettingsMenu => {
                nb.chrome.menu = if nb.chrome.menu == Menu::Settings {
                    Menu::None
                } else {
                    Menu::Settings
                };
            }
            // 橡皮：同样两段式
            Msg::Eraser => {
                if !nb.chrome.erasing {
                    nb.chrome.erasing = true;
                    nb.chrome.erase_section = false;
                    // 从别的工具切回橡皮：恢复上一次用的那一档，而不是每次都
                    // 重置成刷白橡皮（"从其他工具切换回橡皮的时候也会记住"）。
                    nb.chrome.erase_stroke = self.last_erase_stroke;
                    nb.chrome.selecting = false;
                    nb.chrome.menu = Menu::None;
                    nb.clear_sel();
                    eprintln!(
                        "[dev] 橡皮 = {}（上一次那一档）",
                        if nb.chrome.erase_stroke { "擦整笔" } else { "刷白" }
                    );
                } else if nb.chrome.erase_section {
                    nb.chrome.erase_section = false;
                    nb.chrome.menu = Menu::None;
                } else {
                    nb.chrome.menu = if nb.chrome.menu == Menu::Erase {
                        Menu::None
                    } else {
                        Menu::Erase
                    };
                }
            }
            // 橡皮菜单第 1 行：切回"只刷白"的普通橡皮（并记住这一档）
            Msg::EraseBrush => {
                nb.chrome.erasing = true;
                nb.chrome.erase_stroke = false;
                nb.chrome.erase_section = false;
                nb.chrome.selecting = false;
                nb.chrome.menu = Menu::None;
                nb.clear_sel();
                self.last_erase_stroke = false;
                persist_last_eraser(false);
                eprintln!("[dev] 橡皮 = 刷白（已记住）");
            }
            Msg::EraseStroke => {
                nb.chrome.erasing = true;
                nb.chrome.erase_stroke = true;
                nb.chrome.erase_section = false;
                nb.chrome.selecting = false;
                nb.chrome.menu = Menu::None;
                nb.clear_sel();
                self.last_erase_stroke = true;
                persist_last_eraser(true);
                eprintln!("[dev] 橡皮 = 擦整笔（已记住）");
            }
            Msg::EraseSection => {
                nb.chrome.erasing = true;
                nb.chrome.erase_section = true;
                nb.chrome.erase_stroke = false;
                nb.chrome.selecting = false;
                nb.chrome.menu = Menu::None;
                nb.clear_sel();
            }
            // 橡皮菜单第 3 行：清空当前图层（可撤销）
            Msg::ClearLayer => {
                let li = nb.page.current;
                let items: Vec<(usize, Line)> = nb
                    .page
                    .layers
                    .get(li)
                    .map(|l| l.lines.iter().cloned().enumerate().collect())
                    .unwrap_or_default();
                if !items.is_empty() {
                    if let Some(l) = nb.page.layers.get_mut(li) {
                        l.lines.clear();
                    }
                    let n = items.len();
                    nb.undo.push(Op::Remove { layer: li, items });
                    nb.redo.clear();
                    nb.dirty = true;
                    nb.clear_sel();
                    eprintln!("[dev] 清空图层 {li}：{n} 笔");
                }
                page_cleared = true;
                nb.chrome.menu = Menu::None;
            }
            // 选区：不是开关，点一下就是"选中选区工具"（官方当一支笔用）
            Msg::Selection => {
                nb.chrome.selecting = true;
                nb.chrome.erasing = false;
                nb.chrome.erase_section = false;
                nb.chrome.erase_stroke = false;
                nb.chrome.menu = Menu::None;
                nb.clear_sel();
            }
            Msg::Tool(i) => {
                nb.chrome.selected = i.min(TOOLS.len() - 1);
                // 官方只有一个 currentColor：换到荧光笔时自动切到荧光色，
                // 换回普通笔再切回黑色（否则会拿荧光色去画圆珠笔/或者反过来）。
                if let Some(t) = TOOLS.get(nb.chrome.selected).map(|(t, _)| *t) {
                    if t.is_highlighter() && !nb.chrome.color.is_highlighter() {
                        nb.chrome.color = rmdev::pen::line::Color::HighlighterYellow;
                    } else if !t.is_highlighter() && nb.chrome.color.is_highlighter() {
                        nb.chrome.color = rmdev::pen::line::Color::Black;
                    }
                }
                // 记住最后一支真笔：从文字工具切回来时要用它
                if nb.chrome.selected + 1 < TOOLS.len() {
                    nb.last_pen = nb.chrome.selected;
                }
                nb.chrome.erasing = false;
                nb.chrome.selecting = false;
                nb.chrome.erase_section = false;
                nb.clear_sel();
                nb.chrome.menu = Menu::None;
                eprintln!("[dev] tool={:?}", nb.chrome.tool());
            }
            Msg::Thickness(t) => {
                nb.chrome.thickness = t;
                eprintln!("[dev] thickness={t}");
            }
            Msg::Color(c) => {
                nb.chrome.color = c;
                eprintln!("[dev] color={c:?}");
            }
            Msg::Feather => {
                nb.chrome.feather = !nb.chrome.feather;
                eprintln!("[dev] feather={}", nb.chrome.feather);
            }
            Msg::Grade(i) => {
                nb.chrome.pencil_grade = i.min(rmdev::pen::pencil::GRADES.len() - 1);
                eprintln!(
                    "[dev] pencil={}",
                    rmdev::pen::pencil::grade(nb.chrome.pencil_grade).name
                );
            }
            Msg::Dismiss => {
                nb.chrome.menu = Menu::None;
            }
            _ => {}
        }
        self.last_tool = nb.chrome.selected;
        self.feather = nb.chrome.feather;
        self.canvas.feather = nb.chrome.feather;
        let (undo_before, redo_before) = (nb.chrome.undo_ok, nb.chrome.redo_ok);
        nb.sync_undo();
        let undo_changed = undo_before != nb.chrome.undo_ok;
        let redo_changed = redo_before != nb.chrome.redo_ok;
        let now = (nb.chrome.menu, nb.chrome.erasing, nb.chrome.selecting);
        // 菜单开/收两个方向都要刷：开的时候要刷新菜单那一块，收的时候要刷被盖住的纸面
        let menu_rect_after = nb.chrome.menu_rect();
        // 进出选区：纸面上那层套索/选框要整块重画，只能走整页
        if was.2 != now.2 || ds_was_open || page_cleared {
            self.redraw_page();
            return;
        }
        // 只有"会变样的那几格"要重刷：笔/粗细/橡皮/选区 四格（y 120..600）+
        // 撤销重做两格（可用态变了才刷）。整列刷一遍是 120x1872，
        // 这四格只有 120x480，快 4 倍，而且不会有整条竖条闪一下。
        let mut rects = vec![(rmdev::chrome::band_x(), TOP_CELLS_Y, CHROME_W, 4 * 120)];
        // 设置菜单内部换档：只刷"旧反白行 + 新反白行"，别整块 400×1080 重来
        if now.0 == Menu::Settings && was.0 == Menu::Settings {
            let th_after = nb.chrome.thickness_idx();
            if th_after != th_before {
                if let Some(r) = nb.chrome.settings_row(Sec::Stroke, th_before) {
                    rects.push(r);
                }
                if let Some(r) = nb.chrome.settings_row(Sec::Stroke, th_after) {
                    rects.push(r);
                }
                rects.retain(|r| *r != (rmdev::chrome::band_x(), TOP_CELLS_Y, CHROME_W, 4 * 120));
            }
            if nb.chrome.feather != feather_before {
                if let Some(r) =
                    nb.chrome
                        .settings_row(Sec::Stroke, rmdev::chrome::THICKNESSES.len())
                {
                    rects.push(r);
                }
                rects.retain(|r| *r != (rmdev::chrome::band_x(), TOP_CELLS_Y, CHROME_W, 4 * 120));
            }
            let col_after = nb.chrome.color_idx();
            // 颜色段/硬度段：改档只刷旧/新两行
            if col_after != col_before || nb.chrome.pencil_grade != grade_before {
                let sec = nb.chrome.color_sec();
                let old = if sec == Sec::Pencil { grade_before } else { col_before };
                let new = if sec == Sec::Pencil {
                    nb.chrome.pencil_grade
                } else {
                    col_after
                };
                if let Some(r) = nb.chrome.settings_row(sec, old) {
                    rects.push(r);
                }
                if let Some(r) = nb.chrome.settings_row(sec, new) {
                    rects.push(r);
                }
            }
        }
        if undo_changed || redo_changed {
            rects.push((rmdev::chrome::band_x(), 600, CHROME_W, 240));
        }
        if was.0 != now.0 {
            if let Some(r) = menu_rect_before {
                rects.push(r);
            }
            if let Some(r) = menu_rect_after {
                rects.push(r);
            }
        }
        self.repaint_rects(&rects, WAVEFORM_MODE_GC16);
    }
}



/// 灰度缓冲 → 24 位 BMP。行序自底向上，BGR，每行 4 字节对齐——这是 BMP 规范，
/// 服务器端任何图像库都能直接读。
fn gray_to_bmp(gray: &[u8], w: usize, h: usize) -> Vec<u8> {
    let stride = (w * 3 + 3) & !3;
    let data_len = stride * h;
    let mut out = Vec::with_capacity(54 + data_len);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + data_len) as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&2835i32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    let mut row = vec![0u8; stride];
    for y in (0..h).rev() {
        for x in 0..w {
            let v = gray[y * w + x];
            row[x * 3] = v;
            row[x * 3 + 1] = v;
            row[x * 3 + 2] = v;
        }
        out.extend_from_slice(&row);
    }
    out
}

// ---------------------------------------------------------------- 设置页图标
//
// 全部照官方 20-settings*.png 临摹（40×40 框，量下来的行列都写在注释里）。
// 选中行底色是黑的，需要挖空的地方按 `tone` 反推底色。

fn si_bg(tone: u8) -> u8 {
    if tone > 128 {
        0
    } else {
        255
    }
}

fn si_arc(fb: &mut Fb, cx: i32, cy: i32, r: i32, a0: f32, a1: f32, t: i32, tone: u8) {
    let c = gray565(tone);
    let n = (r * 8).max(24);
    for k in 0..=n {
        let a = a0 + (a1 - a0) * k as f32 / n as f32;
        let px = cx + (a.cos() * r as f32) as i32;
        let py = cy + (a.sin() * r as f32) as i32;
        fb.fill_rect(px - t / 2, py - t / 2, t, t, c);
    }
}

fn si_fill_round(fb: &mut Fb, x: i32, y: i32, w: i32, h: i32, r: i32, tone: u8) {
    let c = gray565(tone);
    fb.fill_rect(x + r, y, w - 2 * r, h, c);
    fb.fill_rect(x, y + r, w, h - 2 * r, c);
    widgets::disc(fb, x + r, y + r, r, tone);
    widgets::disc(fb, x + w - r - 1, y + r, r, tone);
    widgets::disc(fb, x + r, y + h - r - 1, r, tone);
    widgets::disc(fb, x + w - r - 1, y + h - r - 1, r, tone);
}

/// 胶囊描边（左右半圆 + 上下直线）。`widgets::capsule` 会把两个整圆都画出来，
/// 45 高、90 宽时看起来是 ∞，所以设置页开关/进度条自己画一版。
fn si_capsule(fb: &mut Fb, x: i32, y: i32, w: i32, h: i32, t: i32, tone: u8) {
    let r = h / 2;
    let ri = (r - t / 2).max(1);
    let (c0, c1) = (x + r, x + w - r - 1);
    let (u0, u1) = (-std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2);
    si_arc(fb, c0, y + r, ri, u0 + std::f32::consts::PI, u1 + std::f32::consts::PI, t, tone);
    si_arc(fb, c1, y + r, ri, u0, u1, t, tone);
    let cc = gray565(tone);
    fb.fill_rect(x + r, y, w - 2 * r, t, cc);
    fb.fill_rect(x + r, y + h - t, w - 2 * r, t, cc);
}

fn si_fill_capsule(fb: &mut Fb, x: i32, y: i32, w: i32, h: i32, tone: u8) {
    let r = h / 2;
    fb.fill_rect(x + r, y, w - 2 * r, h, gray565(tone));
    widgets::disc(fb, x + r, y + r, r, tone);
    widgets::disc(fb, x + w - r - 1, y + r, r, tone);
}

/// General：笔记本（外框 36×39 @x+2，框宽约 5）
fn si_general(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    si_fill_round(fb, x + 2, y + 1, 36, 39, 7, tone);
    si_fill_round(fb, x + 8, y + 6, 25, 29, 3, si_bg(tone));
}

/// Wi-Fi：三段弧 + 圆点（弧心 x+20,y+30，半径 26/18/10）
fn si_wifi(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let (cx, cy) = (x + 20, y + 30);
    for (r, t) in [(26i32, 5i32), (18, 5), (10, 4)] {
        si_arc(fb, cx, cy, r, -2.44, -0.70, t, tone);
    }
    widgets::disc(fb, cx, y + 31, 4, tone);
}

/// 按信号强度只画 `bars` 段弧（0..3）+ 圆点。
fn si_wifi_bars(fb: &mut Fb, x: i32, y: i32, tone: u8, bars: u8) {
    let (cx, cy) = (x + 20, y + 30);
    for (i, (r, t)) in [(26i32, 5i32), (18, 5), (10, 4)].iter().enumerate() {
        // 第 1 段最外圈：bars=3 全画；bars=2 画内两圈；bars=1 画最内圈。
        if (3 - i as u8) <= bars {
            si_arc(fb, cx, cy, *r, -2.44, -0.70, *t, tone);
        }
    }
    widgets::disc(fb, cx, y + 31, 4, tone);
}

/// 小挂锁（列表行的加密标记），约 22×28。
fn si_padlock(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    si_arc(fb, x + 11, y + 12, 6, std::f32::consts::PI, std::f32::consts::PI * 2.0, 3, tone);
    fb.fill_rect(x + 5, y + 11, 3, 6, c);
    fb.fill_rect(x + 14, y + 11, 3, 6, c);
    widgets::outline_rect(fb, x + 2, y + 13, 18, 15, 3, tone);
}

/// 列表选中用的对勾。
fn si_check(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    widgets::line(fb, x, y, x + 7, y + 9, 5, tone);
    widgets::line(fb, x + 7, y + 9, x + 22, y - 12, 5, tone);
}

/// Battery：圆角电池 36×33 @x+1,y+4 + 内芯 + 电极
fn si_battery(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    si_fill_round(fb, x + 1, y + 4, 36, 33, 5, tone);
    si_fill_round(fb, x + 5, y + 8, 28, 25, 3, si_bg(tone));
    fb.fill_rect(x + 7, y + 10, 24, 21, gray565(tone));
    fb.fill_rect(x + 37, y + 15, 4, 9, gray565(tone));
}

/// Storage：带盖收纳箱（盖 y+0..y+11，箱体 y+11..y+40，把手 x14..26）
fn si_storage(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    fb.fill_rect(x + 1, y, 39, 4, c);
    fb.fill_rect(x + 1, y + 4, 4, 7, c);
    fb.fill_rect(x + 36, y + 4, 4, 7, c);
    fb.fill_rect(x + 1, y + 7, 39, 4, c);
    widgets::outline_rect(fb, x + 3, y + 11, 36, 29, 4, tone);
    fb.fill_rect(x + 14, y + 16, 12, 4, c);
}

/// Security：挂锁（锁梁弧心 x+20,y+13 r7；锁体 26×25 @x+7,y+15）
fn si_lock(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    let pi = std::f32::consts::PI;
    si_arc(fb, x + 20, y + 13, 7, pi, pi * 2.0, 4, tone);
    fb.fill_rect(x + 11, y + 12, 4, 6, c);
    fb.fill_rect(x + 25, y + 12, 4, 6, c);
    widgets::outline_rect(fb, x + 7, y + 15, 26, 25, 4, tone);
    widgets::disc(fb, x + 20, y + 24, 4, tone);
    fb.fill_rect(x + 18, y + 25, 4, 8, c);
}

/// Language：地球（整 40×40，圈粗 4）
fn si_globe(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    widgets::ring(fb, x + 20, y + 20, 18, 4, tone);
    fb.fill_rect(x + 2, y + 18, 36, 4, c);
    for rx in [9i32, 15] {
        for yy in -17..=17 {
            let t = yy as f32 / 18.0;
            let dx = (rx as f32 * (1.0 - t * t).max(0.0).sqrt()) as i32;
            fb.fill_rect(x + 20 + dx - 1, y + 20 + yy, 3, 1, c);
            fb.fill_rect(x + 20 - dx - 1, y + 20 + yy, 3, 1, c);
        }
    }
}

/// Accessibility：小 A + 大 A
fn si_font(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    widgets::line(fb, x + 8, y + 7, x + 1, y + 29, 3, tone);
    widgets::line(fb, x + 8, y + 7, x + 15, y + 29, 3, tone);
    widgets::line(fb, x + 4, y + 21, x + 12, y + 21, 3, tone);
    widgets::line(fb, x + 31, y, x + 21, y + 29, 3, tone);
    widgets::line(fb, x + 31, y, x + 41, y + 29, 3, tone);
    widgets::line(fb, x + 25, y + 19, x + 37, y + 19, 3, tone);
}

/// Help：圆圈问号
fn si_help(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    widgets::ring(fb, x + 20, y + 20, 18, 4, tone);
    si_arc(fb, x + 20, y + 17, 8, -2.75, 0.55, 4, tone);
    fb.fill_rect(x + 18, y + 19, 4, 9, gray565(tone));
    fb.fill_rect(x + 18, y + 31, 4, 4, gray565(tone));
}

/// 飞行模式：俯视飞机（机翼竖直、机身水平、尾翼在左）
fn si_plane(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let fuse = [(0, 17), (40, 17), (40, 24), (0, 24)];
    let wing = [
        (13, 0),
        (26, 16),
        (26, 24),
        (13, 40),
        (10, 32),
        (12, 20),
    ];
    let tail_t = [(0, 12), (7, 15), (7, 17), (0, 17)];
    let tail_b = [(0, 29), (7, 26), (7, 24), (0, 24)];
    for poly in [&fuse[..], &wing[..], &tail_t[..], &tail_b[..]] {
        let pts: Vec<(i32, i32)> = poly.iter().map(|(a, b)| (x + a, y + b)).collect();
        widgets::poly(fb, &pts, tone);
    }
}

/// 人像（订阅页 / 账户页）
fn si_person(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let pi = std::f32::consts::PI;
    widgets::ring(fb, x + 20, y + 12, 8, 3, tone);
    for k in 0..=60 {
        let a = pi + pi * k as f32 / 60.0;
        let px = x + 20 + (a.cos() * 14.0) as i32;
        let py = y + 38 + (a.sin() * 12.0) as i32;
        fb.fill_rect(px - 1, py - 1, 3, 3, gray565(tone));
    }
}

/// 圆形箭头（重启 / 恢复出厂 / 自动更新）
fn si_reload(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    // 圆弧：从顶部逆时针绕一圈，缺口在右上
    si_arc(fb, x + 19, y + 22, 14, -1.62, -5.45, 4, tone);
    // 右上箭头 + 竖杆
    let pts = [(x + 29, y + 9), (x + 39, y + 11), (x + 33, y + 19)];
    widgets::poly(fb, &pts, tone);
    fb.fill_rect(x + 31, y + 15, 4, 16, c);
}

/// 服务器（Sync server 页）。三层圆角机箱 + 每层右侧一个指示灯。
/// 官方没有这个图标（官方这页是"账号"），我们自己画的，风格对齐 si_storage。
fn si_server(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    for i in 0..3 {
        let yy = y + 2 + i * 13;
        si_fill_round(fb, x + 2, yy, 36, 10, 2, tone);
        widgets::disc(fb, x + 31, yy + 5, 2, si_bg(tone));
    }
}

/// 平板（Software 的 Version 行）
fn si_tablet(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    si_fill_round(fb, x + 2, y, 36, 40, 5, tone);
    si_fill_round(fb, x + 6, y + 4, 28, 32, 3, si_bg(tone));
}

fn si_gear(fb: &mut Fb, cx: i32, cy: i32, r: i32, tone: u8) {
    let c = gray565(tone);
    widgets::ring(fb, cx, cy, r, 4, tone);
    for k in 0..8 {
        let a = k as f32 * std::f32::consts::TAU / 8.0;
        let px = cx + (a.cos() * (r as f32 + 2.0)) as i32;
        let py = cy + (a.sin() * (r as f32 + 2.0)) as i32;
        fb.fill_rect(px - 2, py - 2, 5, 5, c);
    }
}

/// 两个齿轮（Software 的 Advanced 行）
fn si_cogs(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    si_gear(fb, x + 14, y + 14, 10, tone);
    si_gear(fb, x + 28, y + 29, 6, tone);
}

// === P5 begin ===
/// Experimental：锥形瓶（40×40 填充式，和 si_lock/si_tablet 同一画法）。
///
/// 官方侧栏图标在 C++ 的 `SettingsModel` 里，QML dump 拿不到；"实验"的通行
/// 画法就是锥形瓶，风格对齐现有手绘图标。
fn si_flask(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    fb.fill_rect(x + 12, y, 16, 4, c); // 瓶口
    fb.fill_rect(x + 16, y + 4, 8, 10, c); // 瓶颈
    // 瓶身：从 8 宽扩到 36 宽，逐行画（没有画线的库，和 si_globe 一个路子）
    for i in 0..=22 {
        let half = 4 + (14 * i) / 22;
        fb.fill_rect(x + 20 - half, y + 14 + i, half * 2, 1, c);
    }
    fb.fill_rect(x + 2, y + 36, 36, 4, c); // 瓶底
}
// === P5 end ===

/// 订阅页黑块里的“登录窗口”插图 240×180（逐像素量自官方截图）
fn si_browser(fb: &mut Fb, x: i32, y: i32) {
    let w = gray565(255);
    widgets::outline_rect(fb, x, y, 240, 180, 5, 255);
    fb.fill_rect(x + 5, y + 32, 230, 5, w);
    for i in 0..3 {
        widgets::disc(fb, x + 30 + i * 24, y + 16, 6, 255);
    }
    // 人像：白圆 + 黑脸心 + 底部咬口
    widgets::disc(fb, x + 121, y + 70, 15, 255);
    widgets::disc(fb, x + 121, y + 68, 6, 0);
    widgets::disc(fb, x + 121, y + 89, 10, 0);
    // 两个输入框 102×22（2px 边框）
    widgets::outline_rect(fb, x + 69, y + 96, 102, 22, 2, 255);
    fb.fill_rect(x + 78, y + 105, 84, 2, w);
    widgets::outline_rect(fb, x + 69, y + 127, 102, 22, 2, 255);
    for i in 0..5 {
        widgets::disc(fb, x + 80 + i * 15, y + 137, 3, 255);
    }
}

/// 环境变量开关，**只读一次**。
///
/// 原来每个输入事件都 `std::env::var("RM_DEBUG")`：那是一次堆分配 + 全局环境锁，
/// 笔在 100~200Hz 下就是每秒几百次无谓开销。开关是启动时定的，缓存完全等价。
fn env_flag(name: &str) -> bool {
    fn get(name: &str) -> bool {
        std::env::var(name).is_ok()
    }
    static DEBUG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    static TIMING: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    match name {
        "RM_DEBUG" => *DEBUG.get_or_init(|| get("RM_DEBUG")),
        "RM_TIMING" => *TIMING.get_or_init(|| get("RM_TIMING")),
        other => get(other),
    }
}

/// 电池快照，后台线程每 10 s 刷一次。
///
/// **别在绘制路径里直接调 `battery::read()`**：它读
/// `/sys/class/power_supply/*/uevent`，每次都要走 I2C，设备上实测
/// **一次 440 ms**（`time cat /sys/class/power_supply/*/uevent` = 0.44 s）。
/// `draw_status` 是每帧都画的、`SettingsView::new` 是每次打开设置都跑的，
/// 同步读等于**每次重绘库／每次进设置都卡半秒**——就是那个"整个界面像冻住
/// 一会儿，然后才动"。读快照只是一次 `Mutex` 加锁 + `Copy`。
static BATT: std::sync::Mutex<Option<battery::Battery>> = std::sync::Mutex::new(None);

/// 状态栏的 Wi-Fi 信号格数 + 云是否连得上。
///
/// 两件事都不能在绘制路径里做：`iw dev wlan0 link` 要起进程（约 15 ms），
/// `net::health` 是一次 HTTP（超时 5 s）。所以同样走后台快照：
///
/// * `.0` = Wi-Fi 格数 0..3（0 = 没连上，只画那个点）；
/// * `.1` = `Some(true)` 服务器连得上 / `Some(false)` 连不上 / `None` 还没测出来。
static NETSTATE: std::sync::Mutex<(u8, Option<bool>)> = std::sync::Mutex::new((0, None));

fn net_state() -> (u8, Option<bool>) {
    *NETSTATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 起后台线程维护 [`NETSTATE`]：Wi-Fi 格数每 10 s、云连通性每 20 s 一次。
///
/// 云这次探测走 `net::health`（`GET /api/v1/health`，不需要 token，超时 5 s）。
/// 没配置服务器时不发请求，直接判定"连不上"——状态栏画带叉的云朵，和以前一样。
fn spawn_net_reader() {
    let _ = std::thread::Builder::new()
        .name("netstate".into())
        .spawn(|| {
            let mut cloud_ok: Option<bool> = None;
            // 前 3 轮每秒抓一次信号，让刚进库的那一屏尽快是准的；之后放慢。
            let mut tick: u32 = 0;
            loop {
                let bars = wifi::current_link()
                    .and_then(|(_, sig)| sig)
                    .map(wifi::signal_bars)
                    .unwrap_or(0);
                let cfg = rmdev::net::Config::load();
                if !cfg.configured() {
                    cloud_ok = Some(false);
                } else if rmdev::cloud::events_up() {
                    // 事件长连接还在 = 服务器可达。**零网络 I/O** 就能点亮那颗云。
                    cloud_ok = Some(true);
                } else if tick % 60 == 0 {
                    // 通道断了才探活，而且 5 分钟最多一次：这条路径以前是每
                    // 20 s 一个 GET（一小时 180 个连接），全走 tailscale 用户态
                    // 栈 —— 无线和 CPU 被反复叫醒，纯耗电。
                    cloud_ok = Some(rmdev::net::health(&cfg.url).unwrap_or(false));
                }
                *NETSTATE.lock().unwrap_or_else(|e| e.into_inner()) = (bars, cloud_ok);
                tick = tick.wrapping_add(1);
                std::thread::sleep(Duration::from_secs(5));
            }
        });
}

fn battery_cached() -> Option<battery::Battery> {
    *BATT.lock().unwrap_or_else(|e| e.into_inner())
}

/// 起后台线程维护 [`BATT`]。启动时先同步读一次，保证第一帧状态栏就有真值
/// （那 440 ms 在还没出画面的时候花，用户看不到）。
fn spawn_battery_reader() {
    *BATT.lock().unwrap_or_else(|e| e.into_inner()) = battery::read();
    let _ = std::thread::Builder::new()
        .name("batt".into())
        .spawn(|| loop {
            std::thread::sleep(Duration::from_secs(10));
            let b = battery::read();
            *BATT.lock().unwrap_or_else(|e| e.into_inner()) = b;
        });
}

/// 读电池电量与充电状态（状态栏用）。rM2 只有 uevent，没有 capacity 文件；
/// 读不到就用 100/Unknown 兜底。
fn read_battery() -> (u8, String) {
    match battery_cached() {
        Some(b) => (b.capacity.unwrap_or(100), b.state.label().to_string()),
        None => (100, "Unknown".into()),
    }
}

/// 墙钟毫秒（任务列表按时间排；`rmdev::fb::now_ms` 是"开机到现在"，不能用）。
fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn fmt_date(ms: u64) -> String {
    if ms == 0 {
        return "—".into();
    }
    let secs = (ms / 1000) as i64;
    let days = secs / 86400;
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Howard Hinnant 的 civil_from_days，避免引入时间库。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `--gate`：常驻监听笔，右下角连点 5 下就切到我们的 app。
///
/// 为什么要单独一个进程：官方 UI（remux）跑着的时候屏幕归它，
/// 我们只能从旁路偷听 input 事件，攒够手势再去抢屏。
/// 起一个独立单元做切换，避免监视进程被 systemd 重启时把子进程一起收掉。
fn spawn_switch() {
    let _ = std::process::Command::new("systemd-run")
        .args(["--unit=rmdev-switch", "--collect", "/bin/sh", "/home/root/dev-switch.sh"])
        .stdin(std::process::Stdio::null())
        .spawn();
}

/// DEV 入口文档的 uuid，`--install-entry` 时写进去。
fn entry_file() -> std::path::PathBuf {
    std::path::PathBuf::from("/home/root/.config/rmdev/entry.uuid")
}

fn read_entry_uuid() -> Option<String> {
    std::fs::read_to_string(entry_file())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| s.len() == 36)
}

/// `--watch`：盯 remux 的 journal，等它打开 DEV 条目。
///
/// 为什么不用文件系统：实测打开文档时 `.metadata`/`.content` **一个字节都不变**
/// （xochitl 不把 lastOpened 写回去）。但它会打日志：
/// `rm.docworker  -> worker on <uuid> now running`
/// 这条又精确又便宜，于是改成盯 journal。
fn watch() -> ! {
    use std::io::BufRead;
    let uuid = match read_entry_uuid() {
        Some(u) => u,
        None => {
            eprintln!("[watch] 没有 DEV 入口（先跑 rmdev --install-entry）");
            std::process::exit(1);
        }
    };
    let needle = format!("worker on {uuid} now running");
    eprintln!("[watch] 盯 DEV 入口 {uuid}");

    loop {
        let mut child = match std::process::Command::new("journalctl")
            .args(["-f", "-u", "remux", "-o", "cat", "--since", "now"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[watch] journalctl 起不来：{e}");
                std::thread::sleep(Duration::from_secs(3));
                continue;
            }
        };
        if let Some(out) = child.stdout.take() {
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                if line.contains(&needle) {
                    eprintln!("[watch] DEV 条目被打开 → 切换");
                    spawn_switch();
                    break;
                }
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        // 等切换脚本把 remux 停掉再重新挂上 journal，避免重复触发
        std::thread::sleep(Duration::from_secs(6));
    }
}

/// `--install-entry`：在官方库里建/更新那个 DEV 条目。
fn install_entry() {
    let lib = rmdev::doc::Library::new();
    let name = "DEV ─ 打开 rmdev";
    let existing = read_entry_uuid()
        .filter(|u| std::path::Path::new(&format!("{}/{}.metadata", rmdev::doc::library::DEFAULT_ROOT, u)).exists());
    let uuid = match existing {
        Some(u) => {
            // 保证名字正确（用户可能改过）
            let meta_path = format!("{}/{}.metadata", rmdev::doc::library::DEFAULT_ROOT, u);
            if let Ok(text) = std::fs::read_to_string(&meta_path) {
                if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) {
                    v["visibleName"] = serde_json::json!(name);
                    if let Ok(out) = serde_json::to_string_pretty(&v) {
                        let _ = std::fs::write(&meta_path, out);
                    }
                }
            }
            u
        }
        None => match lib.create_book(name) {
            Ok(u) => u,
            Err(e) => {
                eprintln!("[entry] 建条目失败：{e}");
                std::process::exit(1);
            }
        },
    };
    if let Some(dir) = entry_file().parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(entry_file(), &uuid);
    eprintln!("[entry] DEV 入口 = {uuid}（在官方库的「My files」里，点它即可进入）");
}

/// `--render <what> <out.pgm>`：本机无头渲染一屏，用来和官方截图对比。
///
/// 只在本机用（帧缓冲在堆上，不碰面板）。`what` 的取值：
/// `library` / `sidebar` / `favorites` / `tags` / `trash` / `notebooks` /
/// `pdfs` / `ebooks` / `settings[:<tab>]` / `search[:<query>]` / `doc:<uuid>[:<page>]` /
/// `docsearch:<uuid>[:<page>]:<query>[:<cmd>]` / `nbsettings:<uuid>[:<page>][:<cmd>]` /
/// `pagesel:<uuid>[:<page>][:<cmd>]`（页总览多选，cmd = open/all/none/delete/move/tag）；
/// 库和侧栏视图还可以加 `:list`（列表模式）或 `:sort`（排序下拉展开）。
///
/// `--bench-ui`：本机给几个热路径计时（库重绘 / 卡片 / 文字 / 整页重绘）。
/// e-ink 设备上的绝对值没意义，但同一台机器上前后对比能说明优化有没有用。
/// `lock`。`<tab>` 可以是 0..7，也可以是 `wifi` / `battery` / `storage` / `security` /
/// `language` / `accessibility` / `help`（以及 `about` / `licenses` / `langlist` / `pin`）。
fn bench_ui() {
    let fb = Fb::open_mem();
    let mut app = App::new(fb);
    let n = 20;
    let t = std::time::Instant::now();
    for _ in 0..n {
        app.thumbs.clear();
        app.view = View::MyFiles;
        app.draw_library();
    }
    println!("draw_library（含缩略图冷渲染）: {:.2} ms/帧", t.elapsed().as_secs_f64() * 1000.0 / n as f64);

    let t = std::time::Instant::now();
    for _ in 0..n {
        app.view = View::MyFiles;
        app.draw_library();
    }
    println!("draw_library（缩略图命中缓存）: {:.2} ms/帧", t.elapsed().as_secs_f64() * 1000.0 / n as f64);

    let uuid = app
        .library
        .books()
        .iter()
        .find(|b| b.doc_type == "DocumentType")
        .map(|b| b.uuid.clone());
    if let Some(uuid) = uuid {
        let t = std::time::Instant::now();
        app.open_at(&uuid, 0);
        println!("open_at 首次（冷）: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0);
        let t = std::time::Instant::now();
        for _ in 0..n {
            // 清掉"整屏刷新限速"，否则量到的是 250ms 的 sleep，不是 CPU
            app.last_full = None;
            app.open_at(&uuid, 0);
        }
        println!("open_at 再次（光栅缓存）: {:.2} ms/帧", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
        let t = std::time::Instant::now();
        for _ in 0..n {
            app.last_full = None;
            app.redraw_page();
        }
        println!("redraw_page（内容没变）: {:.2} ms/帧", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
    }
    // 贴图：逐像素 put vs 整行 blit_gray（同一个 426×510 缩略图 × 6 张卡）
    let img = vec![200u8; 426 * 510];
    let t = std::time::Instant::now();
    for _ in 0..20 {
        for k in 0..6 {
            let (cx, cy) = (33 + 458 * k, 120);
            for yy in 0..510 {
                for xx in 0..426 {
                    app.fb.put(cx + xx, cy + yy, gray565(img[(yy * 426 + xx) as usize]));
                }
            }
        }
    }
    println!("6 张卡逐像素 put: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0 / 20.0);
    let t = std::time::Instant::now();
    for _ in 0..20 {
        for k in 0..6 {
            let (cx, cy) = (33 + 458 * k, 120);
            app.fb.blit_gray(cx, cy, 426, 510, &img);
        }
    }
    println!("6 张卡 blit_gray : {:.2} ms", t.elapsed().as_secs_f64() * 1000.0 / 20.0);

    // 文字：库界面一屏大约 30 段（标题/按钮/卡片名/副标题）
    let t = std::time::Instant::now();
    for _ in 0..20 {
        if let Some(t2) = app.text.as_mut() {
            for k in 0..30 {
                t2.draw(&mut app.fb, 40, 200 + k * 40, 32.0, "Quick sheets", 20);
            }
        }
    }
    println!("30 段文字（缓存命中）: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0 / 20.0);

    let t = std::time::Instant::now();
    let mut c = Canvas::new(1404, 1872);
    c.clear();
    for _ in 0..n {
        c.paint_all(&mut app.fb);
    }
    println!("canvas paint_all 整屏 1:1: {:.2} ms/次", t.elapsed().as_secs_f64() * 1000.0 / n as f64);
}

// === D1 begin ===
// ---------------------------------------------------------------------------
// 框选问 AI（D1）
//
// 工具条第 13 格 → 两项浮层（`Rectangle` / `Current page`）→ 拖矩形 →
// 打问题（可空）→ 上传「整页 PNG + 裁图 PNG」→ `POST /api/v1/agent/ask`（SSE）
// → 内联回答卡片（钉在上半部，**下面照样写字**）。
//
// 纯逻辑/绘制在 `panels::selectai`（矩形归一化、最小尺寸、裁剪、请求体、
// 浮层/提问条/框）和 `panels::ai` 的内联模式（SSE / tick / paint / 公式）。
// 这一段只做接线：事件、上传线程、卡片刷新。
// ---------------------------------------------------------------------------
use rmdev::panels::selectai as sai;

/// `--render c9:*` 默认打开的文档 = 设备上装的测试 PDF（Exercise 5.3）。
/// 本机库不是设备那份时用 `c9:<what>:<uuid>[:<page>]` 显式指定。
const C9_DOC: &str = "a1b2c3d4-0000-4000-8000-000000000001";

impl App {
    // ------------------------------------------------------------ 工具态

    /// 工具态写回 `Chrome`（图标反白用）。
    fn d1_sync_chrome(&mut self) {
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.chrome.ask_ai = self.selai.on;
        }
    }

    /// 点第 13 格：进工具（顺手弹两项浮层）/ 再点一次退出（回普通笔）。
    fn d1_toggle_tool(&mut self) {
        let on = !self.selai.on;
        self.selai.reset();
        if on {
            // 进工具 = 重新问一轮：上一张卡片收掉
            self.d1_close_card();
            self.selai.on = true;
            self.selai.popover = true;
            if let Screen::Notebook(nb) = &mut self.screen {
                // 和别的工具互斥，顺手把菜单收了（否则两层面板叠着）
                nb.chrome.erasing = false;
                nb.chrome.selecting = false;
                nb.chrome.erase_section = false;
                nb.chrome.erase_stroke = false;
                nb.chrome.menu = Menu::None;
                nb.clear_sel();
            }
            eprintln!("[d1] 框选问 AI = 开（浮层：Rectangle / Current page）");
        } else {
            eprintln!("[d1] 框选问 AI = 关（回普通笔）");
        }
        self.d1_sync_chrome();
        // 浮层/框/格子一起换样子：一次整页重绘最省心（工具切换是低频动作）
        self.redraw_page();
    }

    /// 退出工具（`close_card` = 连回答卡片一起收）。
    fn d1_leave_tool(&mut self, close_card: bool) {
        let had = self.selai.on || self.selai.popover || self.selai.rect.is_some();
        self.selai.reset();
        if had {
            self.d1_sync_chrome();
        }
        if close_card {
            self.d1_close_card();
        }
    }

    // ------------------------------------------------------------ 浮层

    /// 浮层重绘（只刷它自己那块，纸面补回来）。
    fn d1_repaint_popover(&mut self) {
        self.repaint_rects(&[sai::popover_rect()], WAVEFORM_MODE_GC16);
    }

    /// `ui_tap` 里的浮层：命中两项 / 点外面收起。
    /// 返回 `true` = 这一下被浮层吃了。
    fn d1_ui_tap(&mut self, x: f32, y: f32) -> bool {
        if !matches!(self.screen, Screen::Notebook(_)) || !self.selai.popover {
            return false;
        }
        let (xi, yi) = (x as i32, y as i32);
        if let Some(p) = sai::popover_hit(xi, yi) {
            self.selai.popover = false;
            match p {
                sai::Pick::Rect => {
                    // 工具1：画矩形。清掉上一轮的框，等下一次拖
                    self.selai.rect = None;
                    self.selai.stage = sai::Stage::Idle;
                    self.d1_repaint_popover();
                    eprintln!("[d1] 浮层 Rectangle：拖一个矩形（不依赖笔迹）");
                }
                sai::Pick::Page => {
                    // 工具2：发送指定的页，默认就是当前页 —— 点一下直接发
                    self.d1_repaint_popover();
                    eprintln!("[d1] 浮层 Current page：整页直接发");
                    self.d1_ask(sai::Rect::full_page(), String::new());
                }
            }
            return true;
        }
        // 点浮层外面：收起，但这一下照常往下走（可能是别的工具格）
        self.selai.popover = false;
        self.d1_repaint_popover();
        false
    }

    // ------------------------------------------------------------ 笔：拖矩形

    /// `handle_notebook` 最前面：工具开着时笔全归矩形，一个墨点都不落。
    fn d1_pen(&mut self, ev: &PenEvent) -> bool {
        let ready = matches!(
            &self.screen,
            Screen::Notebook(nb)
                if !nb.overview
                    && nb.text_edit.is_none()
                    && nb.layer_rename.is_none()
                    && nb.tag_edit.is_none()
        );
        if !ready {
            return false;
        }
        // 卡片开着：笔**先看按钮热区**（`Close` / 滚动条 → 点按钮，BUG 3），
        // 其他位置照常落墨（`selectai::route` 的策略）。
        if self.ask_ai.open {
            return self.d1_card_pen(ev);
        }
        match ev {
            PenEvent::Down(ip) => {
                if self.selai.popover {
                    // 笔点浮层里的项 = 选中它；点别处 = 收起浮层。两种都不落墨。
                    let _ = self.d1_ui_tap(ip.x, ip.y);
                    return true;
                }
                // 框已定稿：笔按在左上/右下角 = 调大小（键盘保持开着，不落墨）
                if self.selai.stage == sai::Stage::Ask {
                    if let Some(r) = self.selai.rect {
                        if let Some(h) = sai::handle_hit(r, ip.x as i32, ip.y as i32) {
                            self.selai.handle = Some(h);
                            self.selai.drag = Some((ip.x as i32, ip.y as i32));
                            return true;
                        }
                    }
                    return false;
                }
                if !self.selai.on || self.selai.stage != sai::Stage::Idle || self.selai.kb.is_some()
                {
                    return false;
                }
                // 工具条/×/面板优先：命中了就按 UI 办
                if self.ui_tap(ip.x, ip.y) {
                    return true;
                }
                let x = clamp_paper_f(ip.x);
                let y = ip.y.clamp(0.0, (self.fb.height - 1) as f32);
                self.selai.drag = Some((x as i32, y as i32));
                self.selai.rect = None;
                true
            }
            PenEvent::Move(ip) => {
                // 调框：拖哪个角就动哪个角，另一个角钉住
                if let Some(h) = self.selai.handle {
                    let x = clamp_paper_i(ip.x as i32);
                    let y = (ip.y as i32).clamp(0, self.fb.height - 1);
                    if let Some(r) = self.selai.rect {
                        // 拖角同样不画，松手再画（见上）。
                        self.selai.rect = Some(sai::resize(r, h, x, y));
                    }
                    return true;
                }
                let Some(start) = self.selai.drag else {
                    return false;
                };
                let x = clamp_paper_f(ip.x) as i32;
                let y = (ip.y as i32).clamp(0, self.fb.height - 1);
                let now = sai::clamp_page(start, (x, y));
                if self.selai.rect == Some(now) {
                    return true;
                }
                // 用户 2026-11-09：拖动过程中**不绘制**（e-ink 追不上，越画越卡），
                // 只更新状态；松手那一下一次性把框画出来（`redraw_page`）。
                self.selai.rect = Some(now);
                true
            }
            PenEvent::Up => {
                if self.selai.handle.take().is_some() {
                    self.selai.drag = None;
                    eprintln!("[d1] 框已调整 {:?}", self.selai.rect.map(|r| r.arr()));
                    // 松手才画：整页一次（含键盘/提问条），之后不再跟随。
                    self.redraw_page();
                    return true;
                }
                let Some(start) = self.selai.drag.take() else {
                    return false;
                };
                let last = self.selai.rect;
                self.selai.rect = None;
                // 预览框擦掉：纸面按 canvas 补回来
                if let Some(r) = last {
                    self.repaint_rects(&[rect_tuple(r.expand(6))], WAVEFORM_MODE_GC16);
                }
                // 太小（< 40×40）= 误触：取消，留在工具态。
                // 用**已经归一化**的矩形再走一遍 `finish_drag`（只会重排+钳制+判最小），
                // 这样反方向拖（右下 → 左上）也是同一份结果。
                let (a, b) = match last {
                    Some(r) => ((r.x0, r.y0), (r.x1, r.y1)),
                    None => (start, start),
                };
                let Some(r) = sai::finish_drag(a, b) else {
                    eprintln!("[d1] 矩形太小（< 40×40），当误触取消");
                    return true;
                };
                // 框定稿 → 立刻进"问问题"
                self.selai.rect = Some(r);
                self.selai.stage = sai::Stage::Ask;
                self.selai.kb = Some(Keyboard::new());
                eprintln!("[d1] 框好 {:?} → 输入问题（空 = 解释这一块）", r.arr());
                self.redraw_page();
                true
            }
            PenEvent::Out => false,
        }
    }

    /// 卡片开着时的笔：热区命中就点按钮/拖滚动条，其余位置返回 `false`（落墨）。
    ///
    /// 真机 BUG 3：以前 `Pen → Ink` 一律不看热区，笔点 `Close`（1191,110）只会
    /// 在卡片底下写一笔。现在和"笔点浮层优先于落墨"同一条规则。
    fn d1_card_pen(&mut self, ev: &PenEvent) -> bool {
        let card = self.selai.card.unwrap_or_else(sai::card_rect);
        match ev {
            PenEvent::Down(ip) => {
                let (xi, yi) = (ip.x as i32, ip.y as i32);
                match sai::card_hot_in(card, xi, yi) {
                    Some(sai::CardHit::Close) => {
                        eprintln!("[d1] 卡片 Close（笔）");
                        self.d1_close_card();
                        true
                    }
                    Some(sai::CardHit::Scroll) => {
                        self.selai.card_drag = Some((yi, yi));
                        true
                    }
                    None => false,
                }
            }
            PenEvent::Move(ip) => {
                let Some((_, last)) = self.selai.card_drag else {
                    return false;
                };
                let now = ip.y as i32;
                let dy = last - now;
                if dy.abs() >= 6 {
                    self.ask_ai.scroll_by(dy);
                    self.selai.card_drag = Some((0, now));
                    self.d1_paint_card();
                }
                true
            }
            PenEvent::Up => self.selai.card_drag.take().is_some(),
            PenEvent::Out => false,
        }
    }

    /// 拖框的增量重绘：先补纸面，再补浮层，最后画新框；刷新交给 `flush_pending`。
    fn d1_repaint_box(&mut self, old: Option<sai::Rect>, now: sai::Rect) {
        let view = match &self.screen {
            Screen::Notebook(nb) => nb.view,
            _ => PageView::identity(),
        };
        // **只补四条边**，不补整个包围盒。
        //
        // 以前这里把 old∪new 的整块面积从 canvas 贴回来：框越大越慢，拖到半屏
        // 时每次 move 要重贴 ~1.5 MB，实测 200 ms/事件 → 拖动像冻住（用户报的
        // "拖出来就卡死"）。橡皮筋只有 3 px 边 + 12 px 角，包围盒内部根本没动过，
        // 所以按 10 px 的边条补就够：`paint_view_rect` 在 scale=1 时是 `blit_1to1`
        // （纯 memcpy），8 条加起来不到 40 KB。
        // 只刷**变化的那两条边**：拖一个角时另外两条边一个像素都没动，
        // 刷它们纯属浪费（每次 refresh 面板要 11~20 ms，8 条就是 100 ms+）。
        let strips = changed_strips(old, now);
        for s in &strips {
            self.canvas.paint_view_rect(&mut self.fb, &view, *s);
        }
        sai::draw_rect(&mut self.fb, now);
        // 画完再推：先推会把还没画上的旧边框又显示一遍。
        for s in &strips {
            self.refresh_part(s.0, s.1, s.2, s.3);
        }
        self.sel_dirty = None;
    }

    // ------------------------------------------------------------ 提问键盘

    /// `handle()` 最前面：提问键盘开着时吃掉全部触摸/笔（整屏模态输入）。
    fn d1_capture(&mut self, ev: &InputEvent) -> bool {
        if self.selai.kb.is_none() {
            return false;
        }
        match ev {
            InputEvent::Touch(TouchEvent::Down { x, y }) => {
                self.d1_prompt_down(*x, *y);
                true
            }
            InputEvent::Pen(PenEvent::Down(p)) => {
                self.d1_prompt_down(p.x, p.y);
                true
            }
            InputEvent::Touch(_) | InputEvent::Pen(_) => true,
            _ => false,
        }
    }

    fn d1_prompt_down(&mut self, x: f32, y: f32) {
        let (xi, yi) = (x as i32, y as i32);
        match sai::prompt_tap(xi, yi) {
            Some(sai::PromptAct::Send) => {
                let text = self
                    .selai
                    .kb
                    .as_ref()
                    .map(|k| k.text.trim().to_string())
                    .unwrap_or_default();
                let Some(r) = self.selai.rect else {
                    self.d1_cancel_input();
                    return;
                };
                self.d1_ask(r, text);
            }
            Some(sai::PromptAct::Cancel) => self.d1_cancel_input(),
            Some(sai::PromptAct::Key) => {
                let Some(k) = sai::prompt_key(&self.selai.kb_hits, xi, yi) else {
                    return;
                };
                let mut layer_changed = false;
                let mut done = false;
                if let Some(kb) = self.selai.kb.as_mut() {
                    let before = kb.layer;
                    kb.press(k);
                    layer_changed = kb.layer != before;
                    done = kb.done;
                }
                if done {
                    let text = self
                        .selai
                        .kb
                        .as_ref()
                        .map(|k| k.text.trim().to_string())
                        .unwrap_or_default();
                    let r = self.selai.rect.unwrap_or_else(sai::Rect::full_page);
                    self.d1_ask(r, text);
                    return;
                }
                // 打字只换提问条那一行；换档（⇧/123）才刷整块键盘
                let mut rects = vec![sai::prompt_rect()];
                if layer_changed {
                    rects.push(sai::kb_rect());
                }
                self.repaint_rects(&rects, WAVEFORM_MODE_GC16);
            }
            None => {}
        }
    }

    /// 取消输入：键盘收掉，框留着（还能再拖一个），回工具态。
    fn d1_cancel_input(&mut self) {
        self.selai.kb = None;
        self.selai.kb_hits.clear();
        self.selai.stage = sai::Stage::Idle;
        let mut rects = vec![sai::prompt_rect(), sai::kb_rect()];
        if let Some(r) = self.selai.rect {
            rects.push(rect_tuple(r.expand(6)));
        }
        self.selai.rect = None;
        self.repaint_rects(&rects, WAVEFORM_MODE_GC16);
        eprintln!("[d1] 取消提问");
    }

    // ------------------------------------------------------------ 发送

    /// 把整页（印刷底图 + 墨迹 + 文字层）渲成灰度图。
    ///
    /// 走的是和屏幕上一模一样的合成路径：canvas（底图 × 墨迹）→ 文字层。
    /// **尺寸就是 1404×1872，不做 2x**：真机实测 2x 那版模型要读 1 分 47 秒。
    fn d1_page_gray(&mut self) -> Vec<u8> {
        let (w, h) = (self.fb.width as usize, self.fb.height as usize);
        let mut tmp = Fb::open_mem();
        // 底图必须在画之前喂给 canvas：`pdf_bg` / `bg_gen` 是每页先渲好的
        // 印刷页（PDF/EPUB），正好就是用户说的"底下的书"。
        self.canvas.set_bg(self.pdf_bg.clone());
        let saved = self.canvas.clip_l;
        self.canvas.clip_l = 0;
        self.canvas.paint_all(&mut tmp);
        self.canvas.clip_l = saved;
        // 文字层：坐标和 `paint_overlays` 里画在纸上的完全一致
        let (texts, view) = match &self.screen {
            Screen::Notebook(nb) => (nb.texts.clone(), nb.view),
            _ => (Vec::new(), PageView::identity()),
        };
        if let Some(t) = self.text.as_mut() {
            for it in &texts {
                let p = view.page_to_screen((it.x as f32 + PAGE_DX, it.y as f32 + PAGE_DY));
                t.draw(&mut tmp, p.0 as i32, p.1 as i32, it.size, &it.text, 20);
            }
        }
        let mut gray = vec![255u8; w * h];
        for y in 0..h {
            for x in 0..w {
                gray[y * w + x] = tmp.get(x as i32, y as i32);
            }
        }
        gray
    }

    /// 发一次：先立卡片（`Thinking…`），PNG + 上传扔后台，回来再开流。
    fn d1_ask(&mut self, r: sai::Rect, text: String) {
        self.d1_ask_as(r, text, None);
    }

    /// `d1_ask` 的本体。`reuse_ai` = 沿用已有回答文档 uuid（Job status 的
    /// Retry 走这条：索引里那一条 `ai` 就不用改，框不会变孤儿）。
    fn d1_ask_as(&mut self, r: sai::Rect, text: String, reuse_ai: Option<String>) {
        let (uuid, page) = match &self.screen {
            Screen::Notebook(nb) => (nb.uuid.clone(), nb.page_index + 1),
            _ => return,
        };
        let page_gray = self.d1_page_gray();
        let (w, h) = (sai::PAGE_W as usize, sai::PAGE_H as usize);
        let (cw, ch, cut) = sai::crop(&page_gray, w, h, r);
        if cut.is_empty() {
            eprintln!("[d1] 裁图失败：矩形 {:?}", r.arr());
            return;
        }
        eprintln!(
            "[d1] 发送：doc={uuid} page={page} rect={:?} 裁图 {cw}×{ch} 问题={:?}",
            r.arr(),
            text
        );
        eprintln!(
            "[d1] 请求体（select）= {}",
            sai::ask_body(&uuid, page as u32, r, "<整页 sha>", "<裁图 sha>", &text)
        );
        // 键盘/框/浮层收掉，工具退出（卡片开着时笔要能写字）
        self.selai.reset();
        self.d1_sync_chrome();
        // 卡片**立刻**立起来：大图模型要 1~2 分钟，不能让人盯着没反应的屏幕。
        // 高度按内容自适应（"Thinking…" 时最矮），页面在卡片周围一直看得见。
        self.ask_ai.show_pending();
        let keep = sai::card_rect_for(self.ask_ai.inline_needed_height());
        self.selai.card = Some(keep);
        self.ask_ai.set_inline(Some(keep));
        self.selai.stage = sai::Stage::Uploading;
        self.selai.up_at = std::time::Instant::now();
        // 这一轮发布用的回答文档 id：设备先铸好，服务器按它建 `aiN.pdf`。
        // Retry 沿用上一次那个 id（服务器按 `ai` 覆盖条目，不会多出一条）。
        let ai_doc = reuse_ai.unwrap_or_else(|| rmdev::doc::library::new_uuid("ai"));
        let n = self.ai_refs.items.len() as u32 + 1;
        let parent = self.ai_parent(&uuid);
        eprintln!("[ai] 本轮发布：ai={ai_doc} parent={parent:?} n={n}");
        // Job status：这一轮记一条 `running`。关掉卡片之后它还在这儿，跑完
        // （服务器发布 + 同步）主循环把它改成 `done`，失败改成 `failed`。
        let doc_name = self
            .library
            .find(&uuid)
            .map(|b| b.name.clone())
            .unwrap_or_default();
        self.jobs.start(
            &ai_doc,
            &uuid,
            &doc_name,
            &text,
            page as u32,
            Some(r.arr().map(|v| v as f64)),
            epoch_ms(),
        );
        self.jobs.save();
        self.ai_job = Some(ai_doc.clone());
        if self.jobs_open {
            self.paint_jobs();
        }
        self.selai.pending = Some(sai::PendingAsk {
            doc: uuid,
            page: page as u32,
            rect: r,
            text,
            ai_doc,
            parent,
            n,
        });
        self.redraw_page();
        // 编码 + 上传在后台（设备上 2.6 MB 的 PNG 要几秒）
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(c9_upload(&page_gray, cw, ch, &cut));
        });
        self.selai.up = Some(rx);
    }

    /// 上传回来了：补上 `rect`/`crop_blob`，开 SSE 流。
    fn d1_open_card(&mut self, image: String, crop: String) {
        let Some(p) = self.selai.pending.take() else {
            return;
        };
        // 上传跑着的时候用户可能已经翻页/回库了：这一轮丢掉，别在库界面上开流
        let same = matches!(&self.screen, Screen::Notebook(nb) if nb.uuid == p.doc);
        if !same {
            eprintln!("[d1] 上传回来时已经不在这一本，丢弃这一轮");
            self.selai.stage = sai::Stage::Idle;
            self.selai.card = None;
            self.ask_ai.set_inline(None);
            return;
        }
        let keep = self.selai.card.unwrap_or_else(sai::card_rect);
        self.ask_ai.set_inline(Some(keep));
        self.ask_ai.set_sel(p.rect.arr(), Some(crop));
        self.ask_ai
            .set_publish(p.ai_doc.clone(), p.parent.clone(), p.n);
        self.ask_ai
            .open_with(ai::Mode::Select, &p.text, &p.doc, p.page as usize, Some(image));
        self.selai.stage = sai::Stage::Idle;
        eprintln!("[d1] 上传完成 → 开流（doc={} page={}）", p.doc, p.page);
        self.d1_paint_card();
    }

    /// 卡片重绘 + 一次局部刷新（GC16 局部，字才不糊）。
    ///
    /// 卡片高度跟着内容走（见 `selectai::card_height`）：
    /// * 高度变了 → 旧/新两块都按纸面重画（`repaint_rects` 会先贴 canvas
    ///   再画浮层），这样长高/缩短都不会在页面上留白条或残字；
    /// * 高度没变（流式增长中的绝大多数帧）→ 只画卡片、只刷卡片那一块。
    fn d1_paint_card(&mut self) {
        if !self.ask_ai.open {
            return;
        }
        let want = sai::card_rect_for(self.ask_ai.inline_needed_height());
        let old = self.selai.card;
        if old != Some(want) {
            self.selai.card = Some(want);
            self.ask_ai.set_inline(Some(want));
            let mut rects = vec![want];
            if let Some(o) = old {
                if o != want {
                    rects.push(o);
                }
            }
            self.repaint_rects(&rects, WAVEFORM_MODE_GC16);
            self.selai.card_at = std::time::Instant::now();
            return;
        }
        self.ask_ai.paint_inline(&mut self.fb);
        let (x, y, w, h) = want;
        self.fb.refresh(
            x,
            y,
            w,
            h,
            WAVEFORM_MODE_GC16,
            rmdev::fb::UPDATE_MODE_PARTIAL,
        );
        self.selai.card_at = std::time::Instant::now();
    }

    /// 关卡片：把卡片盖住的纸面补回来（**刚写的字一笔都不丢**，都在 canvas 里）。
    fn d1_close_card(&mut self) {
        if !self.ask_ai.open {
            self.ask_ai.set_inline(None);
            self.selai.card = None;
            return;
        }
        let card = self.selai.card.unwrap_or_else(sai::card_rect);
        self.ask_ai.apply(ai::Act::Close);
        self.ask_ai.set_inline(None);
        self.selai.card = None;
        // 只有还在纸面上才需要把那块补回来；已经回库了就别拿旧页面往上涂。
        // 外扩 2 px：卡片边框/底边线正好压在边界上，少补一圈会留一条灰线。
        if matches!(self.screen, Screen::Notebook(_)) {
            self.repaint_rects(&[expand_rect(card, 2)], WAVEFORM_MODE_GC16);
        }
        eprintln!("[d1] 卡片关闭");
    }

    // ------------------------------------------------------------ 卡片触摸

    /// `handle_touch` 的 Notebook 分支：手指在卡片里 = 滚动 / Close。
    /// **笔不经过这里**（`d1_pen` 直接放行到落墨路径）。
    fn d1_touch(&mut self, t: &TouchEvent) -> bool {
        let Some((cx, cy, cw, ch)) = self.selai.card.filter(|_| self.ask_ai.open) else {
            return false;
        };
        let q = sai::Quad(cx, cy, cw, ch);
        match t {
            TouchEvent::Down { x, y } => {
                let (xi, yi) = (*x as i32, *y as i32);
                if !q.has(xi, yi) {
                    return false;
                }
                if self.ask_ai.inline_hit(*x, *y) == Some(ai::Act::Close) {
                    eprintln!("[d1] 卡片 Close（手指）");
                    self.d1_close_card();
                    return true;
                }
                self.selai.card_drag = Some((yi, yi));
                true
            }
            TouchEvent::Move { y, .. } => {
                let Some((_, last)) = self.selai.card_drag else {
                    return false;
                };
                let now = *y as i32;
                let dy = last - now; // 手指往上 = 内容往上走 = scroll 变大
                if dy.abs() >= 6 {
                    self.ask_ai.scroll_by(dy);
                    self.selai.card_drag = Some((0, now));
                    self.d1_paint_card();
                }
                true
            }
            TouchEvent::Up => self.selai.card_drag.take().is_some(),
        }
    }

    // ------------------------------------------------------------ 工具条

    /// `on_chrome` 开头：第 13 格 + 别的工具格让位。
    fn d1_on_chrome(&mut self, msg: Msg) -> bool {
        match msg {
            Msg::AiSelect => {
                self.d1_toggle_tool();
                true
            }
            // 第 14 格：这一页的 AI 引用列表（开/收）
            Msg::AiRefs => {
                self.ai_toggle_menu();
                true
            }
            // 第 15 格：横屏 / 竖屏
            Msg::Landscape => {
                self.toggle_landscape();
                true
            }
            // 切到别的工具/收起工具条：AI 工具必须退出，否则笔还在画框
            Msg::PenMenu
            | Msg::Tool(_)
            | Msg::Eraser
            | Msg::EraseBrush
            | Msg::EraseStroke
            | Msg::EraseSection
            | Msg::Selection
            | Msg::TextTool
            | Msg::SettingsMenu
            | Msg::Hide => {
                if self.selai.on || self.selai.popover {
                    self.selai.reset();
                    self.d1_sync_chrome();
                    self.repaint_rects(
                        &[band_rect(rmdev::chrome::ai_y(), rmdev::chrome::geom().cell)],
                        WAVEFORM_MODE_GC16,
                    );
                }
                false
            }
            _ => false,
        }
    }

    // ------------------------------------------------------------ 绘制 / tick

    /// `paint_overlays` 最后一步：浮层 / 定稿的框 / 卡片 / 提问键盘（**最上层**）。
    fn d1_paint(&mut self) {
        if !matches!(&self.screen, Screen::Notebook(nb) if !nb.overview) {
            return;
        }
        if self.selai.popover {
            sai::draw_popover(&mut self.fb, &mut self.text);
        }
        // 框（拖动中和定稿都画；拖动时的增量刷新由 `d1_repaint_box` 负责）
        if let Some(r) = self.selai.rect {
            sai::draw_rect(&mut self.fb, r);
        }
        if self.ask_ai.open {
            self.ask_ai.paint_inline(&mut self.fb);
        }
        if let Some(kb) = self.selai.kb.take() {
            self.selai.kb_hits = sai::draw_prompt(&mut self.fb, &mut self.text, &kb);
            self.selai.kb = Some(kb);
        }
    }

    /// `repaint_overlays` 最后一步：墨迹的局部刷新别把卡片/浮层啃掉。
    fn d1_repaint_overlays(&mut self, x: i32, y: i32, w: i32, h: i32) {
        let hit = |bx: i32, by: i32, bw: i32, bh: i32| {
            x < bx + bw && bx < x + w && y < by + bh && by < y + h
        };
        if self.selai.popover {
            let (px, py, pw, ph) = sai::popover_rect();
            if hit(px, py, pw, ph) {
                sai::draw_popover(&mut self.fb, &mut self.text);
                self.fb.refresh(
                    px,
                    py,
                    pw,
                    ph,
                    WAVEFORM_MODE_DU,
                    rmdev::fb::UPDATE_MODE_PARTIAL,
                );
            }
        }
        if self.ask_ai.open {
            let (cx, cy, cw, ch) = sai::card_rect();
            if hit(cx, cy, cw, ch) {
                // 笔在卡片里写过（我们允许，只是被盖住）→ 卡片重画一次盖回去
                self.ask_ai.paint_inline(&mut self.fb);
                self.fb.refresh(
                    cx,
                    cy,
                    cw,
                    ch,
                    WAVEFORM_MODE_DU,
                    rmdev::fb::UPDATE_MODE_PARTIAL,
                );
            }
        }
    }

    /// 主循环的空闲 tick：上传结果 / SSE 增量 / 限速刷卡。
    fn d1_tick(&mut self) {
        // 1) 换文档/离开笔记本：工具、框、卡片全部归零（别把上一本的框发出去）
        let uuid = match &self.screen {
            Screen::Notebook(nb) => Some(nb.uuid.clone()),
            _ => None,
        };
        // 页总览是另一套界面：卡片/工具都收起来
        if matches!(&self.screen, Screen::Notebook(nb) if nb.overview) {
            if self.ask_ai.open {
                self.d1_close_card();
            }
            if self.selai.on || self.selai.popover || self.selai.kb.is_some() {
                self.d1_leave_tool(false);
            }
            return;
        }
        match uuid {
            Some(u) => {
                if self.selai.doc != u {
                    if !self.selai.doc.is_empty() {
                        self.d1_leave_tool(true);
                    }
                    self.selai.doc = u;
                }
            }
            None => {
                if self.selai.on || self.selai.popover || self.selai.kb.is_some() {
                    self.d1_leave_tool(true);
                } else if self.ask_ai.open {
                    self.d1_close_card();
                }
            }
        }
        // 2) 上传结果
        if let Some(r) = self.selai.poll_upload() {
            match r {
                Ok((image, crop)) => self.d1_open_card(image, crop),
                Err(e) => {
                    eprintln!("[d1] 上传失败：{e}");
                    self.selai.stage = sai::Stage::Idle;
                    self.selai.pending = None;
                    self.ask_ai.fail(&format!("Upload failed: {e}"));
                    self.d1_paint_card();
                }
            }
        }
        // 3) SSE：收增量 → 限速重画
        //
        // **卡片关掉也要继续收**（用户 2026-11-09）：难题跑几十秒到几分钟，人
        // 可以先去干别的；流在后台收完、服务器发布 PDF，主循环这边顺手同步一次。
        if self.ask_ai.running() || self.ask_ai.open {
            self.ask_ai.tick();
            // === AIREF begin: 服务器已发布 PDF → 立刻同步一次，框和文档很快就位 ===
            if self.ask_ai.take_published() {
                eprintln!("[ai] 回答已发布，触发同步");
                if let Some(ai) = self.ai_job.clone() {
                    if self.jobs.finish(&ai, rmdev::aijobs::DONE) {
                        self.jobs.save();
                        if self.jobs_open {
                            self.paint_jobs();
                        }
                    }
                }
                self.start_sync();
            } else if let Some(err) = self.ask_ai.take_failed() {
                eprintln!("[ai] 这一轮失败：{err}");
                if let Some(ai) = self.ai_job.take() {
                    if self.jobs.finish(&ai, rmdev::aijobs::FAILED) {
                        self.jobs.save();
                        if self.jobs_open {
                            self.paint_jobs();
                        }
                    }
                }
            }
            // === AIREF end ===
            // 笔优先：笔刚动过 300ms 内不刷卡，流式内容等手停下来再补
            let pen_busy = self.pen_ts.elapsed() < Duration::from_millis(300);
            if self.ask_ai.open
                && self.ask_ai.needs_redraw()
                && !pen_busy
                && self.selai.card_at.elapsed() >= Duration::from_millis(350)
            {
                self.d1_paint_card();
            }
        }
    }
}

/// `sai::Rect` → `(x, y, w, h)`。
/// 两次框之间真正变了的边条（每条覆盖整条边的高度/宽度，带 6 px 余量）。
///
/// 拖一个角时只会动两条边（比如右下角 → 右竖条 + 下横条），所以一般返回 2 条。
/// `old` 为 `None`（第一帧）时退回四条边。
fn changed_strips(old: Option<sai::Rect>, now: sai::Rect) -> Vec<(i32, i32, i32, i32)> {
    let Some(o) = old else {
        return border_strips(rect_tuple(now.expand(6))).to_vec();
    };
    let (x0, y0, x1, y1) = (now.x0, now.y0, now.x1, now.y1);
    let mut v = Vec::new();
    let ylo = o.y0.min(y0) - 6;
    let yhi = o.y1.max(y1) + 6;
    let xlo = o.x0.min(x0) - 6;
    let xhi = o.x1.max(x1) + 6;
    if o.x0 != x0 {
        let a = o.x0.min(x0) - 6;
        let b = o.x0.max(x0) + 6;
        v.push((a, ylo, b - a, yhi - ylo));
    }
    if o.x1 != x1 {
        let a = o.x1.min(x1) - 6;
        let b = o.x1.max(x1) + 6;
        v.push((a, ylo, b - a, yhi - ylo));
    }
    if o.y0 != y0 {
        let a = o.y0.min(y0) - 6;
        let b = o.y0.max(y0) + 6;
        v.push((xlo, a, xhi - xlo, b - a));
    }
    if o.y1 != y1 {
        let a = o.y1.min(y1) - 6;
        let b = o.y1.max(y1) + 6;
        v.push((xlo, a, xhi - xlo, b - a));
    }
    v
}

/// 一个矩形的四条边条（厚 10 px），顺序：上、下、左、右。
///
/// 橡皮筋补图只用边条：框内部是纸面，从来没被覆盖过（见 `d1_repaint_box`）。
fn border_strips(r: (i32, i32, i32, i32)) -> [(i32, i32, i32, i32); 4] {
    let (x, y, w, h) = r;
    let t = 10;
    [
        (x, y, w, t),
        (x, y + h - t, w, t),
        (x, y, t, h),
        (x + w - t, y, t, h),
    ]
}

fn rect_tuple(r: sai::Rect) -> (i32, i32, i32, i32) {
    (r.x0, r.y0, r.w(), r.h())
}

/// 屏幕矩形外扩 `p` 像素（不夹边界，`repaint_rects` 自己会裁）。
fn expand_rect(r: (i32, i32, i32, i32), p: i32) -> (i32, i32, i32, i32) {
    (r.0 - p, r.1 - p, r.2 + 2 * p, r.3 + 2 * p)
}

/// 后台线程：两张 PNG + 两次 `put_blob`。
///
/// 返回 `(整页 sha, 裁图 sha)`。服务器不在线/没配对就把错误原样带给卡片。
fn c9_upload(page: &[u8], cw: usize, ch: usize, cut: &[u8]) -> Result<(String, String), String> {
    let cfg = rmdev::net::Config::load();
    if !cfg.configured() {
        return Err("Not paired".to_string());
    }
    let full = rmdev::share::png_gray(page, sai::PAGE_W as usize, sai::PAGE_H as usize);
    let crop = rmdev::share::png_gray(cut, cw, ch);
    if full.is_empty() || crop.is_empty() {
        return Err("PNG encode failed".to_string());
    }
    let a = rmdev::net::put_blob(&cfg, &full)?;
    let b = rmdev::net::put_blob(&cfg, &crop)?;
    Ok((a, b))
}

// ---------------------------------------------------------------- 出图

/// `--render c9:<what>[:<uuid>[:<page>]]`：框选 AI 的三个状态出图。
///
/// * `c9:tool`   —— 工具态：第 13 格反白 + 两项浮层（Rectangle / Current page）
/// * `c9:rect`   —— 矩形拖到一半：实线框 + 点阵浅影（印刷底图上，零笔迹）
/// * `c9:ask`    —— 框定稿 + 提问条 + 键盘（"问问题"那一步）
/// * `c9:pending` —— 刚 Send、还没开流：矮卡片 + `Thinking…`（BUG 1 那一帧）
/// * `c9:answer` —— 回答卡片在上半部、下半部有手写笔迹的合成态（假数据）
fn c9_render(spec: &str, out: &str) {
    let mut it = spec.split(':');
    let what = it.next().unwrap_or("tool").to_string();
    let uuid = it.next().unwrap_or(C9_DOC).to_string();
    let page = it.next().and_then(|p| p.parse::<usize>().ok()).unwrap_or(0);
    let fb = Fb::open_mem();
    let mut app = App::new(fb);
    app.open_at(&uuid, page);
    let bg_from_thumb = app.c9_bg_fallback(&uuid);
    match what.as_str() {
        "tool" => {
            app.selai.on = true;
            app.selai.popover = true;
            app.selai.doc = uuid.clone();
            app.d1_sync_chrome();
        }
        "rect" => {
            app.selai.on = true;
            app.selai.popover = false;
            // 拖到一半的样子：左上 (260, 300) → 右下 (1150, 1080)
            app.selai.rect = Some(sai::Rect::new(260, 300, 1150, 1080));
            app.selai.doc = uuid.clone();
            app.d1_sync_chrome();
            // 手指还按着（`drag` 有值 = 框由 d1_repaint_box 画）
            app.selai.drag = Some((260, 300));
        }
        "ask" => {
            // 框定稿 + 提问条（键盘）：「问问题」那一步
            app.selai.on = true;
            app.selai.popover = false;
            app.selai.rect = Some(sai::Rect::new(260, 300, 1150, 1080));
            app.selai.stage = sai::Stage::Ask;
            let mut kb = Keyboard::new();
            kb.text = "第 1 题错在哪？".to_string();
            app.selai.kb = Some(kb);
            app.selai.doc = uuid.clone();
            app.d1_sync_chrome();
        }
        "pending" => {
            // 刚 Send、还没开流：矮卡片 + `Thinking…`（真机 BUG 1 的那一帧）
            app.selai.on = false;
            app.selai.doc = uuid.clone();
            app.ask_ai.show_pending();
            let keep = sai::card_rect_for(app.ask_ai.inline_needed_height());
            app.selai.card = Some(keep);
            app.ask_ai.set_inline(Some(keep));
            app.d1_sync_chrome();
        }
        "answer" => {
            app.c9_seed_answer(&uuid);
        }
        other => {
            eprintln!("[render] c9 不认识 {other}（要 tool / rect / answer）");
            return;
        }
    }
    app.redraw_page();
    match app.fb.dump_pgm(out) {
        Ok(()) => eprintln!(
            "[render] c9:{} → {out}（底图{}）",
            what,
            if bg_from_thumb {
                "= PDF 缩略图放大"
            } else if app.pdf_bg.is_some() {
                "= 文档真渲染"
            } else {
                "= 白纸（本机没有这份库）"
            }
        ),
        Err(e) => eprintln!("[render] 写 {out} 失败：{e}"),
    }
}

impl App {
    /// 本机没有 libpdfium 时的兜底：拿 PDF 第一页的缩略图（`.thumbnails/*.jpg`）
    /// 放大成 1404×1872 当印刷底图。**只给 `--render c9:*` 用**，真机上
    /// `pdf_bg` 是 pdfium 渲的，根本走不到这里。
    fn c9_bg_fallback(&mut self, uuid: &str) -> bool {
        if self.pdf_bg.is_some() {
            return false;
        }
        let ids = self.library.page_ids(uuid);
        let Some(pid) = ids.first() else {
            return false;
        };
        let p = self
            .library
            .root
            .join(format!("{uuid}.thumbnails"))
            .join(format!("{pid}.jpg"));
        let Ok(data) = std::fs::read(&p) else {
            return false;
        };
        let Some((tw, th, gray)) = c9_jpeg_gray(&data) else {
            return false;
        };
        let (pw, ph) = (sai::PAGE_W as f32, sai::PAGE_H as f32);
        let s = (pw / tw as f32).min(ph / th as f32);
        let (dw, dh) = ((tw as f32 * s) as usize, (th as f32 * s) as usize);
        let (dw, dh) = (dw.min(sai::PAGE_W as usize), dh.min(sai::PAGE_H as usize));
        let mut bg = vec![255u8; sai::PAGE_W as usize * sai::PAGE_H as usize];
        let ox = (sai::PAGE_W as usize - dw) / 2;
        let oy = (sai::PAGE_H as usize - dh) / 2;
        for y in 0..dh {
            let sy = (y as f32 * th as f32 / dh as f32) as usize;
            let sy = sy.min(th - 1);
            for x in 0..dw {
                let sx = (x as f32 * tw as f32 / dw as f32) as usize;
                let sx = sx.min(tw - 1);
                bg[(oy + y) * sai::PAGE_W as usize + ox + x] = gray[sy * tw + sx];
            }
        }
        self.pdf_bg = Some(bg);
        self.bg_gen += 1;
        true
    }

    /// `c9:answer` 的假数据：一张够长的回答 + 下半部几行手写。
    ///
    /// 手写是真的写进 `.rm` 页面模型（`Layer::lines`）再走正常光栅化，
    /// 所以出图里那些字和真机上写出来的是同一条路径。
    fn c9_seed_answer(&mut self, uuid: &str) {
        self.selai.on = false;
        self.selai.popover = false;
        self.selai.doc = uuid.to_string();
        self.d1_sync_chrome();

        // ---- 回答（假数据；流式面板的 seed 不连服务器）----
        //
        // **真机上的真实回答长这样**：模型按 `$…$` 输出，但每个公式都单独占一行。
        // 这张图就是"公式回流"的证据：这 11 行应该被并成 3~4 行、公式排在行内。
        let answer = "验证：被积函数\n$xe^{-x}$\n在\n$[0,1]$\n上从\n$0$\n增到\n$e^{-1} \\approx 0.368$\n再回落，平均值约\n$0.264$\n，与上述结果一致。";
        self.ask_ai.set_inline(Some(sai::card_rect()));
        self.ask_ai.seed(ai::Mode::Select, "第 1 题错在哪？", answer);
        // 公式图：和真机同一条解码路径（`png_to_gray`），尺寸照服务端 scale=2 的样子
        for (k, w, h) in [
            ("xe^{-x}", 130usize, 60usize),
            ("[0,1]", 90, 50),
            ("0", 30, 60),
            ("e^{-1} \\approx 0.368", 220, 55),
            ("0.264", 100, 50),
        ] {
            let g = c9_formula_gray(w, h);
            self.ask_ai
                .seed_png(k, &rmdev::share::png_gray(&g, w, h));
        }
        // 卡片高度跟着内容走（和真机一致）
        let keep = sai::card_rect_for(self.ask_ai.inline_needed_height());
        self.selai.card = Some(keep);
        self.ask_ai.set_inline(Some(keep));
        self.selai.card_at = std::time::Instant::now();

        // ---- 下半部的手写（页面上真实存在，关掉卡片就在）----
        //
        // 三"行字"，每行四个"词"：每个词是一小段带尖的折线。形状不重要，
        // 重点是这些笔迹走的是**和真机完全一样**的模型 → 光栅化路径。
        for row in 0..3 {
            let y0 = 1230.0 + row as f32 * 132.0;
            for w in 0..4 {
                let x0 = 260.0 + w as f32 * 196.0;
                let n = 7 + (row + w) % 3;
                let pts: Vec<(f32, f32)> = (0..=n)
                    .map(|k| {
                        let t = k as f32 / n as f32;
                        let x = x0 + t * (128.0 + (w as f32) * 6.0) - PAGE_DX;
                        let y = y0 + (t * 9.0 + row as f32).sin() * 10.0
                            + if k % 3 == 1 { 14.0 } else { 0.0 }
                            - if k % 4 == 2 { 11.0 } else { 0.0 };
                        (x, y)
                    })
                    .collect();
                if let Screen::Notebook(nb) = &mut self.screen {
                    if nb.page.layers.is_empty() {
                        break;
                    }
                    nb.page.layers[0].lines.push(c9_line(&pts));
                    nb.dirty = true;
                }
            }
        }
    }
}

/// 造一张"公式图"占位：白底 + 一圈黑边 + 中间一条黑杠（一眼能看出边界）。
fn c9_formula_gray(w: usize, h: usize) -> Vec<u8> {
    let mut g = vec![255u8; w * h];
    for x in 0..w {
        g[x] = 0;
        g[(h - 1) * w + x] = 0;
    }
    for y in 0..h {
        g[y * w] = 0;
        g[y * w + w - 1] = 0;
    }
    for x in (w / 6)..(w * 5 / 6) {
        for y in (h / 2 - 2)..(h / 2 + 2) {
            g[y * w + x] = 0;
        }
    }
    g
}

/// 一串屏幕坐标 → 一条圆珠笔笔迹（走 `StrokeBuilder`，和真机同一条路）。
fn c9_line(pts: &[(f32, f32)]) -> Line {
    let mut b = StrokeBuilder::new(
        Tool::BallpointV2,
        rmdev::pen::line::Color::Black,
        2.0,
        1.0,
    );
    let mut ip = rmdev::pen::line::InputPoint {
        x: pts[0].0,
        y: pts[0].1,
        pressure: 0.55,
        tilt_x: 0.0,
        tilt_y: 0.0,
        rubber: false,
    };
    b.begin(ip);
    for &(x, y) in &pts[1..] {
        ip.x = x;
        ip.y = y;
        b.add(ip);
    }
    b.line().clone()
}

/// 灰度 JPEG → `(w, h, 灰度)`。用 zune-jpeg（纯 Rust，和 `doc::comic` 同一个后端）。
fn c9_jpeg_gray(data: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    use zune_jpeg::zune_core::bytestream::ZCursor;
    let mut dec = zune_jpeg::JpegDecoder::new(ZCursor::new(data));
    let px = dec.decode().ok()?;
    let (w, h) = dec.dimensions()?;
    let n = w * h;
    if px.len() != n && px.len() != n * 3 {
        return None;
    }
    let gray = if px.len() == n {
        px
    } else {
        (0..n)
            .map(|i| {
                let (r, g, b) = (px[i * 3] as u32, px[i * 3 + 1] as u32, px[i * 3 + 2] as u32);
                ((r * 299 + g * 587 + b * 114) / 1000) as u8
            })
            .collect()
    };
    Some((w, h, gray))
}
// === D1 end ===

// === C8 begin ===
/// `--render ai:<mode>:<问题文本>`：AI 回答面板的离屏出图（本机假数据，不连服务器）。
///
/// 回答里放一段带 `$x^2+y^2=1$` 的中文，公式图用 `share::png_gray` 现造一张
/// 200×80 的占位图（画个框 + 两道横线），走的是和真机完全一样的 `paint` /
/// `png_to_gray` 路径——出图能看见，真机上就画得出来。
fn c8_render_ai(spec: &str, out: &str) {
    let mut it = spec.splitn(2, ':');
    let mode = ai::Mode::from_wire(it.next().unwrap_or("live"));
    let question = it.next().unwrap_or("为什么单位圆这么重要？");
    let mut panel = ai::Ai::new();
    // 够一屏以上的假回答：顺带把滚动条和"超出可滚"也画出来
    let mut answer = String::new();
    answer.push_str("单位圆是 $x^2+y^2=1$ 的图像：圆心在原点、半径是 1。\n\n");
    for _ in 0..16 {
        answer.push_str(
            "因为半径恒等于 1，圆上的点可以写成横纵坐标各一个三角函数，三角函数于是变成了坐标。",
        );
        answer.push_str(
            "这一步把代数和几何接在一起，后面求导、积分、傅里叶展开都要靠它。\n\n",
        );
    }
    answer.push_str("先把方程看成一类点的集合，再去看参数怎么跑，整个问题就展开了。");
    panel.seed(mode, question, &answer);

    // 200×80 的占位公式图：一圈黑边 + 两道横线（像个分式）
    let (iw, ih) = (200usize, 80usize);
    let mut gray = vec![255u8; iw * ih];
    for x in 0..iw {
        gray[x] = 0;
        gray[(ih - 1) * iw + x] = 0;
    }
    for y in 0..ih {
        gray[y * iw] = 0;
        gray[y * iw + iw - 1] = 0;
    }
    for x in 20..180 {
        for y in 26..29 {
            gray[y * iw + x] = 0;
        }
    }
    for x in 60..140 {
        for y in 48..51 {
            gray[y * iw + x] = 0;
        }
    }
    let png = rmdev::share::png_gray(&gray, iw, ih);
    let ok = panel.seed_png("x^2+y^2=1", &png);

    let mut fb = Fb::open_mem();
    panel.paint(&mut fb);
    if let Err(e) = fb.dump_pgm(out) {
        eprintln!("[render] 写 {out} 失败：{e}");
    } else {
        eprintln!(
            "[render] ai:{} → {out}（公式图{}）",
            mode.wire(),
            if ok { "已解码" } else { "解码失败" }
        );
    }
}
// === C8 end ===

// === D5 begin ===
// 手写转文字（C14）：文档设置面板的 `Convert to text` → 当前页（印刷底图 + 墨迹）
// 渲染成灰度 PNG → `POST /api/v1/ocr` → 结果写进这一页的 `.rm` 文字层。
//
// * 网络在后台线程（`d5_convert_text`），主循环每帧只 `try_recv`（`d5_poll_ocr`）；
// * 结果回主线程写盘：`Library::save_scene_texts`（官方 `.rm`，同目录临时文件 +
//   rename；失败自动退回 sidecar），**只写 RootText 文字层，不碰墨迹/图层**；
// * 文字位置：`ocr::scene_texts` 接在已有文字层下面，行距 `size * 1.45`
//   （和 `library.rs::p11_layout` 同一条，落盘再读回来不跳）。

/// 后台那一趟 OCR：目标页 + 结果通道（结果只可能是文字或英文错误原因）。
struct D5Job {
    doc: String,
    page: usize,
    rx: std::sync::mpsc::Receiver<Result<String, String>>,
}

/// 一次只跑一趟（服务器也是串行跑 dsh）。主循环用 `try_recv` 收结果。
static D5_JOB: std::sync::Mutex<Option<D5Job>> = std::sync::Mutex::new(None);

/// 非阻塞取结果：还在跑返回 None；拿到了就把句柄清掉（`Busy` 跟着结束）。
fn d5_take_result() -> Option<(String, usize, Result<String, String>)> {
    let mut slot = D5_JOB.lock().unwrap_or_else(|e| e.into_inner());
    let (doc, page, res) = {
        let job = slot.as_ref()?;
        match job.rx.try_recv() {
            Ok(r) => (job.doc.clone(), job.page, r),
            Err(std::sync::mpsc::TryRecvError::Empty) => return None,
            // 线程没了却没发结果（panic / 被杀）：当成断线，别把 UI 挂在"转换中"
            Err(std::sync::mpsc::TryRecvError::Disconnected) => (
                job.doc.clone(),
                job.page,
                Err("Connection lost".to_string()),
            ),
        }
    };
    *slot = None;
    Some((doc, page, res))
}

impl App {
    /// 文档设置 → `Convert to text`。判定全部走 `rmdev::ocr` 的纯函数。
    fn d5_convert_text(&mut self) {
        // 只处理笔记本的当前页（面板这一行也只给 Notebook 画）。
        let target = match &self.screen {
            Screen::Notebook(nb) if !nb.overview => Some((nb.uuid.clone(), nb.page_index)),
            _ => None,
        };
        let cfg = rmdev::net::Config::load();
        let busy = D5_JOB
            .lock()
            .map(|g| g.is_some())
            .unwrap_or_else(|e| e.into_inner().is_some());
        let gate = rmdev::ocr::gate(cfg.configured(), busy, target.is_some());
        if let Some(msg) = rmdev::ocr::gate_notice(gate) {
            eprintln!("[ocr] 不转：{msg}");
            self.notice(NoticeKind::Info, msg);
            return;
        }
        if !rmdev::ocr::can_send(gate) {
            return;
        }
        let Some((doc, page)) = target else { return };
        // 当前页 = 印刷底图（PDF/电子书才有）+ 墨迹；文字层不进图（不然会越转越多）。
        let Some((gray, w, h)) = self.d5_page_gray() else {
            self.notice(NoticeKind::Error, "Could not render page");
            return;
        };
        self.notice(NoticeKind::Info, rmdev::ocr::PROGRESS);
        let (tx, rx) = std::sync::mpsc::channel();
        let thread_doc = doc.clone();
        std::thread::spawn(move || {
            // PNG 编码也在后台做：1404×1872 的 deflate 在设备上要几十毫秒。
            let png = rmdev::share::png_gray(&gray, w, h);
            let r = rmdev::ocr::run(&cfg, &thread_doc, page, &png);
            let _ = tx.send(r);
        });
        *D5_JOB.lock().unwrap_or_else(|e| e.into_inner()) = Some(D5Job { doc, page, rx });
        eprintln!("[ocr] 已发：第 {} 页", page + 1);
    }

    /// 当前页的整页灰度图（1:1 页面坐标，255 白 / 0 黑）。
    ///
    /// 合成方式和 `Canvas::paint_all` 的 `blit_1to1` 一样：`bg * (255 - ink) / 255`。
    /// 用**内存里**的 `nb.page`（可能有还没落盘的笔画），不重新读盘。
    fn d5_page_gray(&mut self) -> Option<(Vec<u8>, usize, usize)> {
        let (page, w, h) = match &self.screen {
            Screen::Notebook(nb) if !nb.overview => (nb.page.clone(), self.canvas.w, self.canvas.h),
            _ => return None,
        };
        if w == 0 || h == 0 {
            return None;
        }
        let mut c = Canvas::new(w, h);
        c.clip_l = 0;
        c.set_bg(self.pdf_bg.clone());
        let grade = rmdev::pen::pencil::DEFAULT;
        for l in page.visible_lines() {
            c.ink_line(l, self.pixel_scale, (PAGE_DX, PAGE_DY), grade);
        }
        let has_bg = c.bg.len() == w * h;
        let bg = &c.bg;
        let dark = c.dark_buf();
        let mut gray = vec![255u8; w * h];
        for i in 0..w * h {
            let base = if has_bg { bg[i] } else { 255 };
            gray[i] = (base as u32 * (255 - dark[i] as u32) / 255) as u8;
        }
        Some((gray, w, h))
    }

    /// 把识别结果写进第 `page` 页的文字层：先读盘上的文字层，接在最后一行下面，
    /// 再整份原子写回。只动 `RootText`，墨迹/图层原样（`rmtext::serialize_texts`
    /// 只重写文字块；`tests::writing_text_does_not_touch_ink` 逐字段验过）。
    fn d5_apply_text(&mut self, doc: &str, page: usize, text: &str) -> Result<usize, String> {
        let mut scene = self.library.load_scene_texts(doc, page);
        let new = rmdev::ocr::scene_texts(&scene, text);
        if new.is_empty() {
            return Err("No text found".to_string());
        }
        scene.extend(new.iter().cloned());
        let store = self
            .library
            .save_scene_texts(doc, page, &scene)
            .map_err(|e| e.to_string())?;
        eprintln!(
            "[ocr] 第 {} 页写入 {} 行文字（{:?}）",
            page + 1,
            new.len(),
            store
        );
        // 还停在这一页：内存里的文字层一起换掉，下一次局部刷新才画得出来。
        // 用户已经切走：只落盘，翻回来时 `load_scene_texts` 会读出来。
        if let Screen::Notebook(nb) = &mut self.screen {
            if nb.uuid == doc && nb.page_index == page {
                nb.texts = scene.iter().map(|t| t.to_item()).collect();
                nb.p11_styles = scene.iter().map(|t| t.style).collect();
            }
        }
        Ok(new.len())
    }

    /// 每帧一次（`tick` 里）：后台结果回主线程写盘 + 重绘 + 提示。
    fn d5_poll_ocr(&mut self) {
        let Some((doc, page, res)) = d5_take_result() else {
            return;
        };
        match res {
            Ok(text) => match self.d5_apply_text(&doc, page, &text) {
                Ok(n) => {
                    eprintln!("[ocr] 完成：第 {} 页 +{n} 行", page + 1);
                    self.notice(NoticeKind::Info, rmdev::ocr::DONE);
                }
                Err(e) => {
                    eprintln!("[ocr] 写文字层失败：{e}");
                    self.notice(NoticeKind::Error, &format!("Could not add text: {e}"));
                }
            },
            Err(e) => {
                // 没配对 / 没网 / 服务器 5xx / 返回空文字：一句英文原因，文件一个字节没动
                eprintln!("[ocr] 失败：{e}");
                self.notice(NoticeKind::Error, &e);
            }
        }
    }
}

// === D6 begin ===
// Live Note 实时模式（C11）的接线。实现在 `src/panels/live.rs`（几何、折行、
// 防抖/单飞/限频状态机、SSE 解析、整页 PNG 全在那边，带单测），这一段只做三件事：
//
//   1. `d6_tick()`  —— 每帧：收流事件（try_recv，绝不阻塞）、判防抖、到点重画；
//   2. `d6_input()` —— 把顶栏 `Live Note` 开关 / 文字带的按下、拖动、抬笔分过来；
//   3. `d6_render_live()` —— `--render live:on` 的出图（假数据，不连服务器）。
//
// 文字带走**正常 overlay 路径**：`paint_overlays` 里一行 `self.d6_paint_overlays();`
// （在工具条、页脚之后，D1 的卡片/键盘之前）。**不能**只当浮层自己补画 ——
// 真机上就是这么露的 bug：`Chrome::draw_in` 会把 x<120 的一整列刷白（全屏高），
// 文字带横跨整屏，工具条后画就把带子左边 120px 啃掉（`Live N` 只剩 `ote`）。
//
// 流式 token 仍然由 `d6_tick` 直接补画并**只刷带子那一块**（A2 12 相位 ≈ 141 ms，
// 不整屏重绘）；整屏/局部重绘后带子由 `paint_overlays` 自己回来，两条路画的是
// 同一份像素。
//
// 规矩：别人的段一个字不动、import 不重排，所以要的东西都走 `d6::` 全路径。

mod d6 {
    pub use rmdev::panels::{live, live::BandHit};
}

impl App {
    /// 有浮层盖着笔记页：这时候不画文字带、也不吃输入（画了也白画）。
    ///
    /// D1 的框选/提问键盘、C8 的内联回答卡片都压在下半屏，**带子必须让路**：
    /// 否则提问键盘的按键会被带子吃掉（点了没反应）。
    fn d6_covered(&self) -> bool {
        self.toc.is_open()
            || self.tcfg.is_open()
            || self.nbset.is_open()
            || self.search.is_open()
            || self.ds.is_open()
            || self.ask_ai.open
            || self.selai.card.is_some()
            || self.selai.kb.is_some()
            || matches!(
                &self.screen,
                Screen::Notebook(nb)
                    if nb.layer_rename.is_some()
                        || nb.tag_edit.is_some()
                        || nb.text_edit.is_some()
            )
    }

    /// 把文字带 / 顶栏按钮画进帧缓冲 —— `paint_overlays` 末尾一行调用。
    ///
    /// **必须画在工具条之后**：`Chrome::draw_in` 会 `fill_rect(0, 0, 120, 屏高)`
    /// 把工具条那一列刷白；文字带横跨整屏，先画就会被啃掉左边 120 px（真机上
    /// `Live N` 只剩 `ote` 就是这么来的）。也不画到 D1 的卡片/提问键盘、C8 的
    /// 整屏 sheet 上 —— 那些在 `paint_overlays` 后半段，天然压在带子上面。
    fn d6_paint_overlays(&mut self) {
        // Live Note 已下线（用户 2026-11-09）：按钮和文字带一个像素都不画。
        // 之前只让 `d6_tick` 早退，按钮还会被画出来、点了还能开带子 ——
        // 带子没有 tick 维护就会在纸上留下灰块和残影。
        return;
        #[allow(unreachable_code)]
        if !matches!(&self.screen, Screen::Notebook(nb) if !nb.overview) {
            return;
        }
        let (w, h) = (self.fb.width, self.fb.height);
        let chrome_on = matches!(&self.screen, Screen::Notebook(nb) if nb.chrome.visible);
        let on = d6::live::is_on();
        // **顶栏按钮和文字带要分开对待**：`d6_covered()`（AI 卡片 / 提问键盘 /
        // 别的整屏面板开着）只说明"文字带别画、别抢输入"，按钮在右上角
        // (952..1300, 10..85)，和卡片 (90..930) 根本不重叠 —— 把按钮一起藏掉
        // 会让"打开 AI 卡片"顺手擦掉 Live Note，D1 的像素不变量测试就是这么挂的。
        if self.d6_covered() {
            // 顶栏按钮是 chrome 的一部分，被卡片/键盘盖住的那只是**文字带**；
            // 按钮照画（`on` 决定它的反白状态）。注意：`is_on()` 是进程级全局，
            // 并行跑的测试会互相影响，所以这里不能写 `!on` 这种条件。
            if chrome_on {
                d6::live::paint_btn(&mut self.fb, &mut self.text, w, on);
                d6::live::set_btn(true);
            }
            return;
        }
        if chrome_on {
            d6::live::paint_btn(&mut self.fb, &mut self.text, w, on);
            d6::live::set_btn(true);
        }
        if on {
            d6::live::paint_band(&mut self.fb, &mut self.text, w, h);
            d6::live::mark_painted(true);
        }
    }

    /// 每帧一次（`tick()` 里、睡着/空闲判断之后）。
    fn d6_tick(&mut self) {
        let (w, h) = (self.fb.width, self.fb.height);
        let now = rmdev::fb::now_ms() as u64;
        // 收后台事件永远做：不阻塞、不动屏幕
        // === LIVE-OFF begin ===
        // Live Note 按用户要求下线（2026-11-09）：按钮和文字带都不再出现。
        // 实现代码先留着（`panels/live.rs`），彻底删除放到下一轮，
        // 免得这次动的面太大。
        if d6::live::is_on() {
            d6::live::set_on(false);
            d6::live::mark_painted(false);
        }
        d6::live::set_btn(false);
        return;
        // === LIVE-OFF end ===
        #[allow(unreachable_code)]
        let in_note = matches!(&self.screen, Screen::Notebook(nb) if !nb.overview);
        if !in_note {
            // 离开笔记正文（回库 / 页总览 / 换屏）就关掉模式
            if d6::live::is_on() {
                d6::live::set_on(false);
                d6::live::mark_painted(false);
            }
            d6::live::set_btn(false);
            return;
        }
        let chrome_on = matches!(&self.screen, Screen::Notebook(nb) if nb.chrome.visible);
        // 顶栏按钮跟着工具条走（官方全屏时顶栏整个收走）
        let want_btn = chrome_on;
        if d6::live::btn_shown() != want_btn {
            d6::live::set_btn(want_btn);
            if want_btn {
                let on = d6::live::is_on();
                d6::live::paint_btn(&mut self.fb, &mut self.text, w, on);
                let (bx, by, bw, bh) = d6::live::btn_rect(w);
                self.fb.refresh(
                    bx,
                    by,
                    bw,
                    bh,
                    d6::live::WF_STREAM,
                    d6::live::UPD_PARTIAL,
                );
            } else {
                // 收走之后那一块归纸面
                let r = d6::live::btn_rect(w);
                self.repaint_rects(&[r], WAVEFORM_MODE_GC16);
            }
        }
        if self.d6_covered() {
            // 浮层盖着：内容先攒着，收起来那一下补一帧
            if d6::live::is_on() {
                d6::live::force_repaint();
            }
            return;
        }
        // 防抖到点 + 内容相对上次请求变了 → 起一条（单飞，旧的会被取消）。
        // 先问"有没有抬笔在等"，没有就不算页面指纹（那是每帧 292 笔的循环）。
        if d6::live::start_pending() && d6::live::take_start(now, self.page_fingerprint()) {
            self.d6_start_live();
        }
        // 顶栏按钮：被整屏重绘刷掉了就补一个（一个像素的探针）
        let on = d6::live::is_on();
        if want_btn && !d6::live::btn_looks_painted(&self.fb, w, on) {
            d6::live::paint_btn(&mut self.fb, &mut self.text, w, on);
            let (bx, by, bw, bh) = d6::live::btn_rect(w);
            self.fb.refresh(bx, by, bw, bh, d6::live::WF_STREAM, d6::live::UPD_PARTIAL);
        }
        if !on {
            return;
        }
        // 文字带：被盖掉 / 第一次画 → 记脏（60ms 限频由 `want_paint` 管）
        if !d6::live::should_be_painted() || !d6::live::looks_painted(&self.fb, w, h) {
            d6::live::force_repaint();
        }
        let Some(full) = d6::live::want_paint(now) else {
            return;
        };
        d6::live::paint_band(&mut self.fb, &mut self.text, w, h);
        let (_, by, _, bh) = d6::live::band_rect(w, h);
        let wf = if full {
            d6::live::WF_FULL
        } else {
            d6::live::WF_STREAM
        };
        self.fb
            .refresh(0, by, w, bh, wf, d6::live::UPD_PARTIAL);
        d6::live::mark_painted(true);
    }

    /// 起一条流：整页灰度 + 页码 + 最后一行文字交给后台线程。
    ///
    /// 整页图**必须带印刷底图**（PDF/EPUB 页的 `pdf_bg`）和文字层：用户是
    /// "边写边问"，底下的书才是题目。只用 `visible_lines()` 画笔迹等于把题目
    /// 删了 —— 真机上 PDF 习题页被模型读成「这页基本是空白，只有几笔很淡的
    /// 随手划痕」。合成走 `d1_page_gray`（D1 框选问 AI 的同一条路）：
    /// canvas（底图 × 墨迹）→ 内存帧缓冲 → 文字层 → 1404×1872 灰度。
    fn d6_start_live(&mut self) {
        let Some((uuid, page, text)) = (match &self.screen {
            Screen::Notebook(nb) => Some((
                nb.uuid.clone(),
                nb.page_index,
                nb.texts.last().map(|t| t.text.clone()).unwrap_or_default(),
            )),
            _ => None,
        }) else {
            return;
        };
        let t0 = std::time::Instant::now();
        let gray = self.d1_page_gray();
        eprintln!(
            "[live] 整页灰度 {}ms（底图 {}，文字层 {} 条，工具条外{}）",
            t0.elapsed().as_millis(),
            if self.pdf_bg.is_some() { "有" } else { "无" },
            match &self.screen {
                Screen::Notebook(nb) => nb.texts.len(),
                _ => 0,
            },
            if gray.len() == (self.fb.width * self.fb.height) as usize {
                ""
            } else {
                "（尺寸不对！）"
            }
        );
        d6::live::start(&uuid, page, &text, gray);
    }

    /// 发出去的那张整页 PNG（和后台线程同一套编码；出图/单测用）。
    fn d6_page_png(&mut self) -> Vec<u8> {
        let gray = self.d1_page_gray();
        rmdev::share::png_gray(&gray, self.fb.width as usize, self.fb.height as usize)
    }

    /// 顶栏按钮 / 文字带的 `Close`：关掉模式，把纸面补回来。
    fn d6_live_off(&mut self) {
        let (w, h) = (self.fb.width, self.fb.height);
        if !d6::live::is_on() {
            return;
        }
        d6::live::set_on(false);
        d6::live::mark_painted(false);
        // 带子盖住的那块归纸面（工具条、页码、× 都由 `paint_overlays` 重画）
        let r = d6::live::band_rect(w, h);
        self.repaint_rects(&[r], WAVEFORM_MODE_GC16);
        if d6::live::btn_shown() {
            d6::live::paint_btn(&mut self.fb, &mut self.text, w, false);
            let (bx, by, bw, bh) = d6::live::btn_rect(w);
            self.fb
                .refresh(bx, by, bw, bh, d6::live::WF_STREAM, d6::live::UPD_PARTIAL);
        }
    }

    /// 顶栏按钮：开/关模式。
    fn d6_live_toggle(&mut self) {
        let (w, h) = (self.fb.width, self.fb.height);
        let on = if d6::live::is_on() {
            false
        } else {
            d6::live::set_on(true);
            true
        };
        if !on {
            self.d6_live_off();
            return;
        }
        // 打开：文字带是白底不透明的，直接盖在纸面上（一次 GC16 画全）
        d6::live::paint_band(&mut self.fb, &mut self.text, w, h);
        let (_, by, _, bh) = d6::live::band_rect(w, h);
        self.fb
            .refresh(0, by, w, bh, d6::live::WF_FULL, d6::live::UPD_PARTIAL);
        d6::live::mark_painted(true);
        if d6::live::btn_shown() {
            d6::live::paint_btn(&mut self.fb, &mut self.text, w, true);
            let (bx, by2, bw, bh2) = d6::live::btn_rect(w);
            self.fb
                .refresh(bx, by2, bw, bh2, d6::live::WF_FULL, d6::live::UPD_PARTIAL);
        }
    }

    /// 出图用：往当前页上补几笔假手写（fixture 页上原本有没有墨都不影响）。
    fn d6_demo_ink(&mut self) {
        let Screen::Notebook(nb) = &mut self.screen else {
            return;
        };
        let wave = |x0: f32, y0: f32, amp: f32, n: usize, tool: Tool| {
            let mut l = Line::new(tool, rmdev::pen::Color::Black, 2.0);
            for i in 0..n {
                let t = i as f32 / (n - 1) as f32;
                l.push(rmdev::pen::Point {
                    x: x0 + t * 470.0,
                    y: y0 + (t * 11.0).sin() * amp,
                    speed: 0.0,
                    dir: 0.0,
                    width: 3.4,
                    pressure: 0.95,
                });
            }
            l
        };
        nb.page
            .lines_mut()
            .push(wave(-430.0, 250.0, 30.0, 140, Tool::BallpointV2));
        nb.page
            .lines_mut()
            .push(wave(-380.0, 430.0, 22.0, 120, Tool::FinelinerV2));
        nb.page
            .lines_mut()
            .push(wave(-410.0, 640.0, 40.0, 160, Tool::MarkerV2));
        nb.page
            .lines_mut()
            .push(wave(-360.0, 850.0, 26.0, 110, Tool::BallpointV2));
        nb.dirty = true;
    }

    /// 出图用：顶栏按钮（选中态）+ 文字带，只写帧缓冲（走和真机同一条 overlay 路径）。
    fn d6_paint_live(&mut self) {
        self.d6_paint_overlays();
    }

    /// 出图用：把当前页换成一张"印刷习题页"（`pdf_bg`）。
    ///
    /// 本机没有 pdfium，真 PDF 渲不出来；但**底图这条路和真机完全一样**：
    /// `pdf_bg` → `canvas.set_bg` → `paint_all` → 整页灰度 → PNG。用它验
    /// "抓出去的图里有印刷内容"，也用来解释真机上那句"这页基本是空白"。
    fn d6_fake_print_page(&mut self) {
        let (w, h) = (self.fb.width, self.fb.height);
        let mut fb = Fb::open_mem();
        fb.fill_rect(0, 0, w, h, gray565(255));
        // 左边一条装订线 + 标题下面一条分隔线（看起来像印出来的页）
        fb.fill_rect(84, 110, 3, h - 260, gray565(205));
        if let Some(t) = self.text.as_mut() {
            t.draw(
                &mut fb,
                130,
                220,
                44.0,
                "Exercise 5.3    Integration by parts",
                0,
            );
            t.draw(
                &mut fb,
                130,
                278,
                28.0,
                "Evaluate each integral. Show every step.",
                60,
            );
        }
        fb.fill_rect(130, 310, w - 260, 2, gray565(90));
        if let Some(t) = self.text.as_mut() {
            for (i, q) in [
                "1.    x e^x dx",
                "2.    x^2 cos x dx",
                "3.    ln x dx",
                "4.    x sin 2x dx",
            ]
            .iter()
            .enumerate()
            {
                t.draw(&mut fb, 150, 400 + i as i32 * 92, 34.0, q, 0);
            }
            t.draw(
                &mut fb,
                130,
                830,
                26.0,
                "Hint: integrate by parts first, then substitute.",
                60,
            );
        }
        let mut gray = vec![255u8; (w * h) as usize];
        for y in 0..h {
            for x in 0..w {
                gray[(y * w + x) as usize] = fb.get(x, y);
            }
        }
        self.pdf_bg = Some(gray);
        self.canvas.set_bg(self.pdf_bg.clone());
        self.bg_gen = self.bg_gen.wrapping_add(1);
        // 这一页的笔迹/文字清掉，只留 `d6_demo_ink` 补的几笔
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.page = RmPage::default();
            nb.texts.clear();
            nb.dirty = true;
        }
    }

    /// 一个按下点是不是被 Live Note 吃了。
    fn d6_down(&mut self, x: f32, y: f32) -> bool {
        // Live 下线：不再吃任何按下（按钮已经不存在了）。
        return false;
        #[allow(unreachable_code)]
        let (w, h) = (self.fb.width, self.fb.height);
        // 顶栏按钮（工具条收起时不存在）
        if d6::live::btn_shown() && d6::live::in_btn(x, y, w) {
            d6::live::grab(); // 跟着的抬指也归我们
            self.d6_live_toggle();
            return true;
        }
        match d6::live::hit_band(x, y, w, h) {
            Some(d6::BandHit::Close) => {
                d6::live::grab();
                self.d6_live_off();
                true
            }
            Some(d6::BandHit::Scroll) => {
                // 带内拖动 = 上下滚（手指和笔都行；笔在这里不落墨，带子是 UI 不是纸）
                d6::live::drag_start(y);
                true
            }
            None => false,
        }
    }

    /// 拖动：正在拖文字带就滚它。
    fn d6_move(&mut self, x: f32, y: f32) -> bool {
        return false;
        #[allow(unreachable_code)]
        if !d6::live::grabbed() {
            return false;
        }
        d6::live::drag_move(y, self.fb.width, self.fb.height);
        true
    }

    /// 抬指/抬笔。返回 true = 这一下归文字带（不落墨、也不算点击）。
    ///
    /// `pen` = 这是笔抬起来的。**只有笔**才起防抖：手指只是点 UI/翻页，
    /// 不该顺手问一次 AI。
    fn d6_up(&mut self, pen: bool) -> bool {
        // Live 下线：抬笔不再起防抖、不再问 AI。
        return false;
        #[allow(unreachable_code)]
        if d6::live::drag_end() {
            return true;
        }
        if pen && matches!(&self.screen, Screen::Notebook(_)) {
            // 写完一笔：起防抖计时（期间再写会重置）
            d6::live::pen_up(rmdev::fb::now_ms() as u64);
        }
        false
    }

    /// 输入总入口：只有笔记正文页、而且没被整屏浮层盖住时才管。
    fn d6_input(&mut self, ev: &InputEvent) -> bool {
        if !matches!(&self.screen, Screen::Notebook(nb) if !nb.overview) || self.d6_covered() {
            return false;
        }
        match ev {
            InputEvent::Pen(PenEvent::Down(p)) => self.d6_down(p.x, p.y),
            InputEvent::Pen(PenEvent::Move(p)) => self.d6_move(p.x, p.y),
            InputEvent::Pen(PenEvent::Up) => self.d6_up(true),
            InputEvent::Touch(TouchEvent::Down { x, y }) => self.d6_down(*x, *y),
            InputEvent::Touch(TouchEvent::Move { x, y }) => self.d6_move(*x, *y),
            InputEvent::Touch(TouchEvent::Up) => self.d6_up(false),
            _ => false,
        }
    }
}

/// `--render "ocr:done[:<uuid>[:<page>]]"`：把结果文字层**真写进**一页再出图。
///
/// 库先整份拷到临时目录再打开（`RMDEV_LIB_ROOT` 指过去），因为这条路径会写
/// `.rm`——不能把 `~/re_xochitl/fixture` 里的真样本改了。
fn d5_render_ocr(spec: &str, out: &str) {
    let mut it = spec.split(':');
    let _mode = it.next().unwrap_or("done");
    let want = it.next().filter(|s| !s.is_empty()).map(|s| s.to_string());
    let page = it.next().and_then(|s| s.parse::<usize>().ok()).unwrap_or(0);

    let src = std::path::PathBuf::from(
        std::env::var("RMDEV_LIB_ROOT")
            .unwrap_or_else(|_| rmdev::doc::library::DEFAULT_ROOT.to_string()),
    );
    let tmp = std::env::temp_dir().join(format!("rmdev-d5-ocr-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    if let Err(e) = d5_copy_tree(&src, &tmp) {
        eprintln!("[render] 拷库 {} 失败：{e}", src.display());
    }
    std::env::set_var("RMDEV_LIB_ROOT", &tmp);

    let mut app = App::new(Fb::open_mem());
    let uuid = want
        .or_else(|| {
            app.library
                .books()
                .into_iter()
                .find(|b| b.format == rmdev::doc::Format::Rm)
                .map(|b| b.uuid)
        })
        .unwrap_or_default();
    if uuid.is_empty() {
        eprintln!("[render] ocr: 库里没有笔记（RMDEV_LIB_ROOT={}）", src.display());
        return;
    }
    app.open_at(&uuid, page);
    // 假识别结果：多行、带空行和公式，够看行距/换行。
    let text = "Meeting notes\n\nThe unit circle is x^2 + y^2 = 1\n\
                Radius stays equal to one\n\nNext: Fourier series\n\
                Converted by the server OCR";
    match app.d5_apply_text(&uuid, page, text) {
        Ok(n) => {
            let path = app.library.page_id_at(&uuid, page).unwrap_or_default();
            let rm = tmp.join(&uuid).join(format!("{path}.rm"));
            eprintln!(
                "[render] ocr: 第 {} 页写入 {n} 行文字 → {}",
                page + 1,
                rm.display()
            );
        }
        Err(e) => eprintln!("[render] ocr: 写文字层失败：{e}"),
    }
    app.redraw_page();
    if let Err(e) = app.fb.dump_pgm(out) {
        eprintln!("[render] 写 {out} 失败：{e}");
    } else {
        eprintln!("[render] ocr:done → {out}");
    }
    let _ = std::fs::remove_dir_all(&tmp);
}

/// 递归拷贝目录（出图把 fixture 库拷到 /tmp 用；库很小，不值得引 walkdir）。
fn d5_copy_tree(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    let rd = match std::fs::read_dir(src) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for e in rd {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            d5_copy_tree(&e.path(), &to)?;
        } else {
            std::fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}
// === D5 end ===

/// `--render live:on[:<uuid>[:<页>]]`：Live Note 实时模式出图。
///
/// 假数据：真库里挑一本笔记（`RMDEV_LIB_ROOT`，本机一般指 `~/re_xochitl/fixture`），
/// 打开那一页，再补四笔正弦"手写"（页上原本有没有墨都不影响），最后把文字带
/// 用 [`d6::live::seed_demo`] 摆成"AI 已经流了一段"的样子 —— 不连服务器。
fn d6_render_live(spec: &str, out: &str) {
    let mut it = spec.split(':');
    let mode = it.next().unwrap_or("on");
    let want_uuid = it
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());
    let page = it.next().and_then(|p| p.parse::<usize>().ok()).unwrap_or(0);
    let mut app = App::new(Fb::open_mem());
    let uuid = want_uuid.or_else(|| {
        app.library
            .books()
            .into_iter()
            .find(|b| {
                b.doc_type == "DocumentType"
                    && b.format == rmdev::doc::Format::Rm
                    && !app.library.page_ids(&b.uuid).is_empty()
            })
            .map(|b| b.uuid)
    });
    let Some(uuid) = uuid else {
        eprintln!("[render] live: 库里没有可用的笔记（用 RMDEV_LIB_ROOT 指到 fixture）");
        return;
    };
    app.open_at(&uuid, page);
    let printed = mode == "pdf";
    let thinking = mode == "think";
    if printed {
        // `live:pdf`：假印刷习题页（本机没 pdfium，但合成路径和真机一致）
        app.d6_fake_print_page();
    }
    app.d6_demo_ink();
    app.redraw_page();
    if printed {
        // 把**真要发出去的那张 PNG** 落盘：肉眼确认印刷底图在（真机上就是它被
        // 读成"这页基本是空白"）
        let png = app.d6_page_png();
        let p = format!("{out}.page.png");
        match std::fs::write(&p, &png) {
            Ok(()) => eprintln!(
                "[render] live 整页 PNG（发出去那张，{} 字节）→ {p}",
                png.len()
            ),
            Err(e) => eprintln!("[render] 写 {p} 失败：{e}"),
        }
    }
    if thinking {
        // `live:think`：等首字 9 秒时的样子（Thinking… + 浅一点的 Still working…）
        d6::live::seed_thinking(d6::live::THINKING_MS + 1000);
    } else {
        d6::live::seed_demo(d6::live::DEMO_TEXT);
    }
    app.d6_paint_live();
    if let Err(e) = app.fb.dump_pgm(out) {
        eprintln!("[render] 写 {out} 失败：{e}");
    } else {
        eprintln!(
            "[render] live:{} → {out}",
            if printed {
                "pdf"
            } else if thinking {
                "think"
            } else {
                "on"
            }
        );
    }
}

// ---------------------------------------------------------------- 测试
//
// 两条真机回归：
//   1. 整页重绘（工具条会把 x<120 整列刷白）之后，文字带必须还是完整的；
//   2. 抓出去的那张整页 PNG 必须带**印刷底图**（只画笔迹 = 把题目删了）。
/// `live` 的开关是进程级全局：bin 测试里凡是翻它的都要先拿这把锁，
/// 否则并行跑会互相踩（真踩过：`d6_band_*` 断言"分隔线断了"）。
#[cfg(test)]
static LIVE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
fn live_guard() -> std::sync::MutexGuard<'static, ()> {
    LIVE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod d6_tests {
    use super::*;

    // 闸门在文件作用域（见下面的 `LIVE_LOCK`）：`mod tests` 里的 d1 那几个
    // 用例也要翻 `live` 的开关，两处共用同一把锁才串得起来。

    fn d6_test_nb() -> Notebook {
        Notebook {
            uuid: "d6-test-doc".into(),
            name: "D6 test".into(),
            page: RmPage::default(),
            page_index: 0,
            page_count: 1,
            overview: false,
            page_thumbs: None,
            swipe: None,
            chrome: Chrome::default(),
            active: None,
            exit_taps: 0,
            dirty: false,
            tap_consumed: false,
            texts: Vec::new(),
            p11_styles: Vec::new(),
            text_edit: None,
            pending_text: None,
            sel: None,
            lasso: None,
            drag_sel: None,
            pending_paste: None,
            undo: Vec::new(),
            redo: Vec::new(),
            tag_edit: None,
            layer_rename: None,
            erase_rect: None,
            view: PageView::identity(),
            last_pen: 0,
            landscape_back: None,
        }
    }

    /// 文字带走正常 overlay 路径：整页重绘 → 带子（含左边被工具条刷掉的那一列）回来。
    #[test]
    /// 关掉模式：纸面（含印刷底图）补回来，带子不留残影。
    #[test]
    /// D1 的框选/提问键盘压在下半屏：带子必须让路，否则键盘按键点不动。
    #[test]
    /// 发出去的整页 PNG **必须**带印刷底图 + 文字层，不能只有笔迹。
    #[test]
    fn d6_live_png_carries_the_printed_page() {
        let mut app = App::new(Fb::open_mem());
        let (w, h) = (app.fb.width as usize, app.fb.height as usize);
        // 印刷页：整页浅灰 + 每 40 行一道 12px 的"字行"
        let mut bg = vec![222u8; w * h];
        for y in (0..h).step_by(40) {
            for dy in 0..12 {
                for x in 200..1200 {
                    bg[(y + dy) * w + x] = 60;
                }
            }
        }
        app.pdf_bg = Some(bg.clone());
        app.canvas.set_bg(Some(bg.clone()));
        // 笔迹：一条黑线
        app.canvas
            .ink_segment(300.0, 1700.0, 3.0, 900.0, 1700.0, 3.0);
        let png = app.d6_page_png();
        assert!(!png.is_empty(), "整页要能编出 PNG");
        let (pw, ph, gray) = rmdev::panels::ai::png_to_gray(&png).expect("PNG 要能解回来");
        assert_eq!((pw, ph), (w, h), "整页图就是 1404×1872，不做 2x");
        // 印刷字行（第 80..91 行）必须在 —— 老实现（只画 visible_lines）这里是白的
        assert!(
            gray[85 * w + 700] < 120,
            "印刷底图丢了（{}）—— 这就是真机说「这页基本是空白」的原因",
            gray[85 * w + 700]
        );
        // RGB565 往返有 ±2 的量化误差
        assert!(
            (gray[85 * w + 700] as i32 - bg[85 * w + 700] as i32).abs() <= 3,
            "像素要和底图一致（{} vs {}）",
            gray[85 * w + 700],
            bg[85 * w + 700]
        );
        // 字行之间的浅灰也在（不是只剩笔迹的白纸）
        assert!((way_white(&gray, w) as i32 - 222).abs() < 20, "底色丢了");
        // 墨迹在
        assert!(gray[1700 * w + 600] < 80, "墨迹丢了");
    }

    /// 一页里出现最多的灰度（= 纸/底色的主色）。
    fn way_white(gray: &[u8], w: usize) -> u8 {
        let mut hist = [0u32; 256];
        for y in (0..gray.len() / w).step_by(7) {
            for x in (0..w).step_by(7) {
                hist[gray[y * w + x] as usize] += 1;
            }
        }
        let mut best = (0usize, 0u32);
        for (v, n) in hist.iter().enumerate() {
            if *n > best.1 {
                best = (v, *n);
            }
        }
        best.0 as u8
    }
}
// === D6 end ===

// === C15 begin: Live Note 的回答写成纸上的墨迹 ===
// 用户要的不是"底下弹一条带子显示聊天"，而是回答就长在手写旁边：跟在笔迹下面、
// 在**独立图层**（`AI`，和 C12 批改同一层）、能被人用橡皮擦掉、导出时也在。
//
// 分工：
// * `panels::live` 只留一条 96 px 的状态条（`Thinking…` / `Writing to page…` /
//   `Done`），流式正文不再显示在带子里；`done` 时把整段回答交给这里；
// * `panels::inknote` 在**后台线程**里把回答排版成位图（文字 + LaTeX 公式 +
//   插图），再把位图栅格成墨迹 op（`annotate::Op::Stroke`）；
// * 这里只做三件事：算锚点（这次手写的包围盒）→ 把 op 追加进 `AI` 图层并
//   画进画布 → 局部刷新那一块。
//
// 为什么"位图 → 墨迹"而不是直接排笔画：屏幕上的文字/公式/插图已经有一套画法
// （`ui::text` + 服务器 pdflatex 出来的公式图），纸上要长得和屏幕一模一样，
// 只有"先渲染再栅格"这一条路不会出现两套排版互相漂移。
impl App {
    /// 每帧一次（`tick` 里、`d6_tick` 之后）：回答到齐就去排墨迹，墨迹回来就落纸。
    fn c15_live_ink(&mut self) {
        use rmdev::panels::{inknote, live};
        if let Some(reply) = live::take_reply() {
            match self.c15_anchor() {
                Some((anchor, page, dx)) => {
                    let width =
                        (page.0 - inknote::PAGE_LEFT - inknote::PAGE_MARGIN - 20).max(320);
                    eprintln!(
                        "[inknote] 回答 {} 字，锚点 {:?}，宽 {}",
                        reply.chars().count(),
                        anchor,
                        width
                    );
                    live::clear_last_ink();
                    inknote::spawn(reply, anchor, width, page, dx);
                }
                // 不在笔记正文里（用户已经翻走了）：回答只留在日志里，不落纸
                None => eprintln!("[inknote] 没有可落的页面，丢弃这一轮回答"),
            }
        }
        let Some(res) = inknote::poll() else {
            return;
        };
        match res {
            Ok(done) => {
                let (added, rect) = self.c15_apply_ink(&done.ops);
                eprintln!(
                    "[inknote] {}，落点 ({}, {})，写进 AI 图层 {} 条",
                    done.what, done.at.0, done.at.1, added
                );
                rmdev::panels::live::set_last_ink(added);
                match rect {
                    Some(r) => self.repaint_rects(&[r], WAVEFORM_MODE_GC16),
                    None => self.redraw_page(),
                }
                // 状态条要变成 `Done`
                rmdev::panels::live::force_repaint();
            }
            Err(e) => eprintln!("[inknote] 没落墨：{e}"),
        }
    }

    /// 这次手写的包围盒（页面像素坐标）+ 页面尺寸 + 场景坐标原点。
    ///
    /// 取**当前层最后 3 条笔迹**的并集：用户刚写完的那几句就在这儿，回答落在
    /// 它们下面最自然。跳过橡皮线（它们的包围盒不是"写的东西"）。
    fn c15_anchor(&self) -> Option<([i32; 4], (i32, i32), i32)> {
        let Screen::Notebook(nb) = &self.screen else {
            return None;
        };
        let layer = nb.page.layers.get(nb.page.current)?;
        let mut acc: Option<RectF> = None;
        let mut taken = 0usize;
        for l in layer.lines.iter().rev() {
            if matches!(
                l.tool,
                rmdev::pen::Tool::Eraser
                    | rmdev::pen::Tool::EraseSection
                    | rmdev::pen::Tool::ClearPage
            ) {
                continue;
            }
            let Some(b) = line_bbox(l) else { continue };
            acc = Some(match acc {
                None => b,
                Some(a) => RectF {
                    x: a.x.min(b.x),
                    y: a.y.min(b.y),
                    w: (a.x + a.w).max(b.x + b.w) - a.x.min(b.x),
                    h: (a.y + a.h).max(b.y + b.h) - a.y.min(b.y),
                },
            });
            taken += 1;
            if taken >= 3 {
                break;
            }
        }
        let (w, h) = (self.fb.width, self.fb.height);
        let dx = w / 2;
        // 一页一个笔迹都没有（刚翻到空白页）：锚点放在页首，回答从上面开始写
        let b = acc.unwrap_or(RectF {
            x: 120.0 - dx as f32,
            y: 200.0,
            w: 300.0,
            h: 80.0,
        });
        Some((
            [
                (b.x + dx as f32) as i32,
                b.y as i32,
                (b.x + b.w + dx as f32) as i32,
                (b.y + b.h) as i32,
            ],
            (w, h),
            dx,
        ))
    }

    /// 把墨迹 op 追加进页面的 `AI` 图层，同时画进画布并算出要刷新的屏幕矩形。
    ///
    /// 返回 `(写进去几条, 屏幕矩形)`。矩形是 `None` 表示算不出来（调用方退回整页重画）。
    fn c15_apply_ink(
        &mut self,
        ops: &[rmdev::panels::annotate::Op],
    ) -> (usize, Option<(i32, i32, i32, i32)>) {
        use rmdev::panels::annotate::Op;
        let Screen::Notebook(nb) = &mut self.screen else {
            return (0, None);
        };
        let st = match rmdev::panels::annotate::append(&mut nb.page, ops) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[inknote] 写 AI 图层失败：{e}");
                return (0, None);
            }
        };
        nb.dirty = true;
        let view = nb.view;
        // 画布坐标 = 场景坐标 + (PAGE_DX, PAGE_DY)
        let (mut bx0, mut by0) = (f32::MAX, f32::MAX);
        let (mut bx1, mut by1) = (f32::MIN, f32::MIN);
        for op in ops {
            let Op::Stroke { pts, w } = op else { continue };
            let r = (*w * 0.5).max(0.5);
            for pair in pts.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                self.canvas.ink_segment(
                    a.0 + PAGE_DX,
                    a.1 + PAGE_DY,
                    r,
                    b.0 + PAGE_DX,
                    b.1 + PAGE_DY,
                    r,
                );
                bx0 = bx0.min(a.0.min(b.0) + PAGE_DX);
                bx1 = bx1.max(a.0.max(b.0) + PAGE_DX);
                by0 = by0.min(a.1.min(b.1) + PAGE_DY);
                by1 = by1.max(a.1.max(b.1) + PAGE_DY);
            }
        }
        if bx0 > bx1 || by0 > by1 {
            return (st.lines, None);
        }
        let pad = 6.0;
        let page_rect = (
            (bx0 - pad).floor() as i32,
            (by0 - pad).floor() as i32,
            ((bx1 - bx0) + pad * 2.0).ceil() as i32,
            ((by1 - by0) + pad * 2.0).ceil() as i32,
        );
        let r = self.canvas.screen_rect_of(&view, page_rect);
        (st.lines, Some(r))
    }
}
// === C15 end ===

// === E2 begin ===
// 输入线程的两条兜底（笔 10 s / 手指 5 s 按下超时强制复位、事件循环重建）在
// `rmdev::input::guard`（纯状态机 + 单测），接线在 `src/input/mod.rs` /
// `src/input/touch.rs` 的 `run_with` / `run`。这一段只做 C14 的持久状态条。
//
// 为什么另起一条：`notice` 的 TTL 是 4 s，而整页 OCR 实测 **115 s** —— 4 秒之后
// 屏幕上一点反馈都没有，用户只能以为卡死。**不动 `notice` 的语义**（那条 toast
// 照旧弹），只加一份独立状态：转换中在屏幕底部固定位置显示 `Converting to text`
// + 三个点轮流实心；结束/失败换成结果，4 s 后消失。状态机在
// `panels::docsettings::OcrStrip`（带单测，时间由调用方注入）。
//
// 成败怎么判：不往 D5 的代码里加分支，只看现成的 `D5_JOB` 单飞槽位 ——
// `None → Some` 是开始（顺便拿到目标页），`Some → None` 是结束；再看目标页
// 文字层行数有没有变多（`d5_apply_text` 落的盘，`load_scene_texts` 读得到）。
impl App {
    /// 每帧一次（`tick` 里，紧跟 `d5_poll_ocr`）。
    fn e2_ocr_tick(&mut self) {
        let now = rmdev::fb::now_ms();
        // 后台任务现在什么样：在不在跑、跑的是哪一页
        let job = {
            let slot = D5_JOB.lock().unwrap_or_else(|e| e.into_inner());
            slot.as_ref().map(|j| (j.doc.clone(), j.page))
        };
        match job {
            Some((doc, page)) => {
                if self.ds.ocr.target() != Some((doc.as_str(), page)) {
                    let before = self.library.load_scene_texts(&doc, page).len();
                    if self.ds.ocr.start(now, &doc, page, before) {
                        eprintln!("[ocr] 状态条：第 {} 页转换中", page + 1);
                        self.e2_ocr_repaint();
                    }
                }
            }
            None => {
                // 目标还挂着 = 这一趟刚结束
                let target = self.ds.ocr.target().map(|(d, p)| (d.to_string(), p));
                if let Some((doc, page)) = target {
                    let after = self.library.load_scene_texts(&doc, page).len();
                    if let Some(ok) = self.ds.ocr.finish(now, after) {
                        eprintln!(
                            "[ocr] 状态条：第 {} 页{}",
                            page + 1,
                            if ok { "转换完成" } else { "转换失败" }
                        );
                        self.e2_ocr_repaint();
                    }
                }
            }
        }
        // 动画步进 / 结果到期：只重画底部那一块
        if self.ds.ocr.tick(now) {
            self.e2_ocr_repaint();
        }
    }

    /// 状态条那一块重画 + 局部刷新。
    ///
    /// 只在笔记**正文页**做：别的界面上 `canvas` 里没有对应的纸面，
    /// 硬刷一块矩形会把界面擦花（状态条本身也不该盖在库/设置页上）。
    fn e2_ocr_repaint(&mut self) {
        if !matches!(&self.screen, Screen::Notebook(nb) if !nb.overview) {
            return;
        }
        let rect = docsettings::ocr_strip_rect(self.fb.width);
        self.repaint_rects(&[rect], WAVEFORM_MODE_GC16);
    }

    /// 把状态条画进帧缓冲（`paint_overlays` 末尾；只画不刷）。
    fn e2_paint_ocr(&mut self) {
        if !matches!(&self.screen, Screen::Notebook(nb) if !nb.overview) {
            return;
        }
        let now = rmdev::fb::now_ms();
        let Some(view) = self.ds.ocr.view(now) else {
            return;
        };
        let w = self.fb.width;
        if let Some(t) = self.text.as_mut() {
            docsettings::paint_ocr_strip(&mut self.fb, t, view, w);
        }
    }
}

/// `--render "ocr:converting"`：假的"转换进行中"，只为出图。
///
/// 真的 OCR 要 115 s、还得连服务器，出图不可能等它。这里挑库里第一本笔记，
/// 直接把状态条推到"转换中"的第 2 格再整页重绘。
fn e2_render_ocr(what: &str, out: &str) -> bool {
    let Some(spec) = what.strip_prefix("ocr:") else {
        return false;
    };
    // `ocr:done` 还是 D5 那条（把结果文字层真写进一页）
    if spec.split(':').next() != Some("converting") {
        return false;
    }
    let mut app = App::new(Fb::open_mem());
    let uuid = app
        .library
        .books()
        .into_iter()
        .find(|b| b.format == rmdev::doc::Format::Rm)
        .map(|b| b.uuid)
        .unwrap_or_default();
    if uuid.is_empty() {
        eprintln!("[render] ocr:converting：库里没有笔记");
        return true;
    }
    app.open_at(&uuid, 0);
    // 停在"转换中"的第 2 格：出图上看得出三个点在走
    let now = rmdev::fb::now_ms();
    app.ds
        .ocr
        .start(now - docsettings::OCR_DOT_MS * 1.4, &uuid, 0, 0);
    app.redraw_page();
    match app.fb.dump_pgm(out) {
        Ok(()) => eprintln!("[render] ocr:converting → {out}"),
        Err(e) => eprintln!("[render] 写 {out} 失败：{e}"),
    }
    true
}
// === E2 end ===

// === C12 begin ===
// AI 批改并**写回页面**（C12）。入口 = 文档设置面板的 `Grade page` 一行。
//
// 流程：当前页渲成灰度 PNG（含印刷底图 + 墨迹 + 文字层，复用 D1 那条合成路径）
//   → `POST /api/v1/agent/ask`，`mode=grade`（服务器 prompt 只让模型回
//     `{"kind":"annotate","ops":[…]}`，坐标 1404×1872）
//   → 从回答里抠出那段 JSON → `panels::annotate::apply_at` 写进**独立的 `AI` 图层**
//     （原笔迹一个字节不动，这一条由 `panels/annotate.rs` 的单测担保）
//   → 重新读页 + 整页重绘。
//
// 网络在后台线程（`c12_run`），主循环每帧只 `try_recv`（`c12_poll`），绝不阻塞。
// 结果经 `C12_RX` 静态槽回传（和 C4 用 mpsc 的理由一样：不想动 `App` 的构造）。
mod c12 {
    use std::sync::mpsc::{self, Receiver, TryRecvError};
    use std::sync::Mutex;

    /// 后台跑完的一件事。
    pub struct Done {
        pub doc: String,
        pub page: usize,
        /// 成功 = 写进 AI 层的条数；失败 = 一句英文原因（直接给用户看）。
        pub result: Result<usize, String>,
    }

    static RX: Mutex<Option<Receiver<Done>>> = Mutex::new(None);

    /// 有没有一件事正在跑。
    pub fn busy() -> bool {
        RX.lock().map(|g| g.is_some()).unwrap_or(false)
    }

    /// 占位：把接收端挂上去（真正的事件由后台线程 `send`）。
    pub fn arm(rx: Receiver<Done>) {
        if let Ok(mut g) = RX.lock() {
            *g = Some(rx);
        }
    }

    /// 取一次结果；没到就放回去。
    pub fn poll() -> Option<Done> {
        let mut g = RX.lock().ok()?;
        let rx = g.as_ref()?;
        match rx.try_recv() {
            Ok(d) => {
                *g = None;
                Some(d)
            }
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => {
                *g = None;
                None
            }
        }
    }

    /// 后台线程用：跑完把结果塞进来。
    pub fn spawn(doc: String, page: usize, png: Vec<u8>) {
        let (tx, rx) = mpsc::channel();
        arm(rx);
        std::thread::spawn(move || {
            let r = run(&doc, page, &png);
            let _ = tx.send(Done {
                doc,
                page,
                result: r,
            });
        });
    }

    /// 后台那一趟：传图 → 问（grade）→ 读 SSE 攒出完整回答 → 抠 JSON。
    fn run(doc: &str, page: usize, png: &[u8]) -> Result<usize, String> {
        use std::io::BufRead;
        let cfg = rmdev::net::Config::load();
        if !cfg.configured() {
            return Err("Pair a server first".into());
        }
        let sha = rmdev::net::put_blob(&cfg, png)?;
        let body = serde_json::json!({
            "mode": "grade",
            "doc": doc,
            "page": page,
            "image_blob": sha,
            "text": "",
        })
        .to_string();
        eprintln!("[c12] 发批改请求 doc={doc} page={page} 图 {} KB", png.len() / 1024);
        let mut r = rmdev::net::open_stream(
            "POST",
            "/api/v1/agent/ask",
            body.as_bytes(),
            std::time::Duration::from_secs(300),
        )?;
        let mut answer = String::new();
        let mut head = String::new();
        let mut line = String::new();
        loop {
            line.clear();
            let n = r
                .read_line(&mut line)
                .map_err(|e| format!("读流失败：{e}"))?;
            if n == 0 {
                break;
            }
            let t = line.trim_end();
            if let Some(e) = t.strip_prefix("event:") {
                head = e.trim().to_string();
                continue;
            }
            let Some(data) = t.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim_start();
            match head.as_str() {
                "token" => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(data) {
                        if let Some(t) = v.get("t").and_then(|x| x.as_str()) {
                            answer.push_str(t);
                        }
                    }
                }
                "done" => break,
                "error" => return Err(format!("agent error: {data}")),
                _ => {}
            }
        }
        eprintln!("[c12] 回答 {} 字", answer.chars().count());
        apply_answer(doc, page, &answer)
    }

    /// 从回答里抠出那段 annotate JSON 并落到页面上。返回写进去的条数。
    pub fn apply_answer(doc: &str, page: usize, answer: &str) -> Result<usize, String> {
        let json = extract_json(answer).ok_or_else(|| {
            let head: String = answer.trim().chars().take(40).collect();
            if head.is_empty() {
                "No answer from the server".to_string()
            } else {
                format!("Bad response from server: {head}…")
            }
        })?;
        let batch = rmdev::panels::annotate::parse_event(&json)?;
        if batch.valid_len() == 0 {
            return Ok(0);
        }
        let root = std::path::PathBuf::from(
            std::env::var("RMDEV_LIB_ROOT")
                .unwrap_or_else(|_| rmdev::doc::library::DEFAULT_ROOT.to_string()),
        );
        let lib = rmdev::doc::library::Library::with_root(&root);
        let pid = lib
            .page_id_at(doc, page)
            .ok_or_else(|| "No such page".to_string())?;
        let path = root.join(doc).join(format!("{pid}.rm"));
        let stats = rmdev::panels::annotate::apply_at(&path, &batch.ops)?;
        Ok(stats.lines)
    }

    /// 抠 JSON：从第一个 `{` 到最后一个 `}`（模型爱在前面加一句话）。
    pub fn extract_json(s: &str) -> Option<String> {
        let a = s.find('{')?;
        let b = s.rfind('}')?;
        if b <= a {
            return None;
        }
        Some(s[a..=b].to_string())
    }
}

impl App {
    /// 文档设置面板点了 `Grade page`：起后台批改。
    fn c12_grade_page(&mut self) {
        if c12::busy() {
            self.notice(NoticeKind::Info, "Please wait for the current task");
            return;
        }
        let Some((doc, page)) = (match &self.screen {
            Screen::Notebook(nb) => Some((nb.uuid.clone(), nb.page_index)),
            _ => None,
        }) else {
            return;
        };
        if !rmdev::net::Config::load().configured() {
            self.notice(NoticeKind::Error, "Pair a server first");
            return;
        }
        // 先把这一页的墨迹存盘：`apply_at` 会整页重写，内存里没存的笔迹会丢
        if let Screen::Notebook(nb) = &mut self.screen {
            if nb.dirty {
                if let Err(e) = self.library.save_page_at(&nb.uuid, nb.page_index, &nb.page) {
                    eprintln!("[c12] 先存页失败：{e}");
                } else {
                    nb.dirty = false;
                }
            }
        }
        let png = {
            let gray = self.d1_page_gray();
            let (w, h) = (self.fb.width as usize, self.fb.height as usize);
            rmdev::share::png_gray(&gray, w, h)
        };
        if png.is_empty() {
            self.notice(NoticeKind::Error, "Image encode failed");
            return;
        }
        eprintln!("[c12] 批改第 {} 页（图 {} KB）", page + 1, png.len() / 1024);
        self.notice(NoticeKind::Info, "Grading...");
        c12::spawn(doc, page, png);
    }

    /// 每帧一次（`tick` 里）：收后台结果。
    fn c12_poll(&mut self) {
        let Some(done) = c12::poll() else {
            return;
        };
        match done.result {
            Ok(0) => {
                eprintln!("[c12] 没有发现问题");
                self.notice(NoticeKind::Info, "No mistakes found");
            }
            Ok(n) => {
                eprintln!("[c12] 批注 {} 条已写进 AI 图层", n);
                self.notice(NoticeKind::Info, "Annotations added");
                // 磁盘上的页已经变了：让 C4 那条"重新读页 + 重绘"的路径来收尾
                self.c4_reload_open_page();
                self.repaint_current();
            }
            Err(e) => {
                eprintln!("[c12] 失败：{e}");
                self.notice(NoticeKind::Error, &e);
            }
        }
    }
}
// === C12 end ===

// === E1 begin: 错题档案（C13 设备端）===
// 侧栏 ☰ → `Mistakes` → 整屏列表。布局/绘制/网络全在 `panels::mistakes`
// （纯逻辑 + 单测），这里只做三件事：把侧栏动作翻译成"打开"、把笔/手指
// 事件翻译成面板输入、把后台回复翻译成通知 + 重绘。
//
// 数据流：`open()` 起后台线程 `GET /api/v1/profile` → `tick()` 里
// `e1_poll()` 非阻塞收 → 有列表就重画；点行右端 `Learned` → 后台
// `POST /api/v1/profile/<id>/learned` → 成功后就地变淡 + `Marked as learned`。
// UI 线程一次网络都不碰。

/// 侧栏 `Mistakes` 的动作号（9450 段，避开现有全部号段，grep 过没被占）
const A_E1_MISTAKES: u32 = 9450;

// === JOB begin: Job status（AI 异步任务列表）===
//
// 用户 2026-11-09：难题问出去可以把卡片关掉，任务继续跑；这里看还剩哪些、
// 哪些失败了。**只显示列表 + 来源文档**（用户明确要求），状态用图形表示，
// 不再造一串英文词。失败那行右边给一个 `Retry`（沿用同一个回答 uuid）。
const A_E1_JOBS: u32 = 9460;
const A_E1_JOBS_CLOSE: u32 = 9461;
const A_E1_JOB0: u32 = 9470;
const A_E1_JOBRETRY0: u32 = 9600;
/// 列表行高与首行位置。
const JOB_ROW: i32 = 128;
const JOB_TOP: i32 = 220;
// === JOB end ===

impl App {
    // === JOB begin ===

    /// 侧栏 `Job status`：打开整屏任务列表。
    fn e1_open_jobs(&mut self) {
        self.menu_open = false;
        self.filters_open = false;
        self.sort_open = false;
        self.jobs = rmdev::aijobs::Jobs::load();
        self.jobs_reconcile();
        self.jobs_open = true;
        self.paint_jobs();
    }

    /// 自愈：`running` 但回答文档已经在库里 = 其实已经发布完了。
    ///
    /// 为什么需要：卡片的流只有一条，用户关掉卡片又问了下一题时旧流就被换掉了，
    /// 那一轮永远收不到 `done`。但服务器照跑照发布，sync 之后文档就在库里 ——
    /// 按"文档在不在"判，比追着事件判结实。
    fn jobs_reconcile(&mut self) {
        let _ = self.jobs_reconcile_and_report();
    }

    /// 同 [`App::jobs_reconcile`]，但告诉调用方这次有没有条目真的变了
    /// （自动同步靠它判断"要不要重绘"）。
    fn jobs_reconcile_and_report(&mut self) -> bool {
        let mut changed = false;
        for i in 0..self.jobs.items.len() {
            if self.jobs.items[i].status != rmdev::aijobs::RUNNING {
                continue;
            }
            if self.library.find(&self.jobs.items[i].ai).is_some() {
                self.jobs.items[i].status = rmdev::aijobs::DONE.to_string();
                changed = true;
            }
        }
        if changed {
            self.jobs.save();
        }
        changed
    }

    /// 整屏重画（进列表 / 列表内容变了）。
    fn paint_jobs(&mut self) {
        let (w, h) = (self.fb.width, self.fb.height);
        self.hits.clear();
        self.tiles.clear();
        self.fb.fill_rect(0, 0, w, h, gray565(255));

        // 顶栏：返回箭头 + 标题。标题是我们自己这一屏的名字，不是官方原文。
        // 返回箭头：横杆 + 左上/左下两撇（指尖在左边）
        self.fb.fill_rect(56, 58, 56, 5, gray565(20));
        for i in 0..22 {
            self.fb.fill_rect(56 + i, 58 - i, 5, 5, gray565(20));
            self.fb.fill_rect(56 + i, 58 + i + 2, 5, 5, gray565(20));
        }
        self.hits.push(24, 20, 120, 96, A_E1_JOBS_CLOSE);
        if let Some(t) = self.text.as_mut() {
            t.draw_bold(&mut self.fb, 140, 76, 40.0, "Job status", 20);
        }

        if self.jobs.items.is_empty() {
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, 60, JOB_TOP + 40, 30.0, "No AI jobs yet", 120);
            }
            self.fb
                .refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
            return;
        }

        for (i, j) in self.jobs.items.iter().enumerate() {
            let y = JOB_TOP + i as i32 * JOB_ROW;
            if y + JOB_ROW > h - 40 {
                break;
            }
            // 行分隔线
            self.fb.fill_rect(40, y + JOB_ROW - 2, w - 80, 2, gray565(215));
            // 状态标记（不写英文词）：跑着 = 三个点、成功 = 勾、失败 = 叉
            let c = gray565(20);
            match j.status.as_str() {
                rmdev::aijobs::DONE => {
                    // 勾：短的那笔往下，长的往右上
                    for k in 0..15 {
                        self.fb.fill_rect(50 + k, y + 48 + k, 6, 6, c);
                    }
                    for k in 0..24 {
                        self.fb.fill_rect(64 + k, y + 62 - k, 6, 6, c);
                    }
                }
                rmdev::aijobs::FAILED => {
                    for k in 0..26 {
                        self.fb.fill_rect(52 + k, y + 40 + k, 5, 5, c);
                        self.fb.fill_rect(78 - k, y + 40 + k, 5, 5, c);
                    }
                }
                _ => {
                    for k in 0..3 {
                        widgets::disc(&mut self.fb, 58 + k * 22, y + 56, 6, 20);
                    }
                }
            }
            // 来源文档（列表的主角）
            let name = if j.doc_name.is_empty() { &j.doc } else { &j.doc_name };
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, 130, y + 66, 30.0, name, 20);
            }
            // 行本身可点：打开来源文档（回到当时的页，框就在纸上）
            self.hits.push(40, y, w - 80, JOB_ROW - 4, A_E1_JOB0 + i as u32);
            // 失败的行右边给 `Retry`
            if j.status == rmdev::aijobs::FAILED {
                let (bx, bw) = (w - 300, 220);
                self.fb.fill_rect(bx, y + 30, bw, 68, gray565(255));
                rect_border(&mut self.fb, bx, y + 30, bw, 68, 2, 20);
                if let Some(t) = self.text.as_mut() {
                    t.draw(&mut self.fb, bx + 60, y + 76, 30.0, "Retry", 20);
                }
                self.hits.push(bx, y + 30, bw, 68, A_E1_JOBRETRY0 + i as u32);
            }
        }
        self.fb
            .refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
    }

    /// Job status 的触摸输入：按下记位置，抬起按命中表分发。
    fn jobs_input(&mut self, ev: &InputEvent) {
        // 通用那段（记 `last_touch`）在这条路径上是跳过的，自己记一下
        if let InputEvent::Touch(TouchEvent::Down { x, y }) = ev {
            self.last_touch = Some((*x, *y, 1));
            return;
        }
        let InputEvent::Touch(TouchEvent::Up { .. }) = ev else {
            return;
        };
        self.jobs_activate();
    }

    /// 抬手/抬笔那一下的分发（笔和手指共用）。
    fn jobs_activate(&mut self) {
        let (x, y) = match self.last_touch {
            Some((x, y, _)) => (x, y),
            None => return,
        };
        self.jobs_activate_at(x, y);
    }

    /// 按屏幕坐标分发（笔、手指、触摸注入都走这里）。
    fn jobs_activate_at(&mut self, x: f32, y: f32) {
        let Some(a) = self.hits.take(x, y) else {
            return;
        };
        if a == A_E1_JOBS_CLOSE {
            self.jobs_open = false;
            self.draw_library();
            return;
        }
        let i = if (A_E1_JOB0..A_E1_JOB0 + 40).contains(&a) {
            (a - A_E1_JOB0) as usize
        } else if (A_E1_JOBRETRY0..A_E1_JOBRETRY0 + 40).contains(&a) {
            let i = (a - A_E1_JOBRETRY0) as usize;
            self.e1_retry_job(i);
            return;
        } else {
            return;
        };
        let Some(j) = self.jobs.items.get(i).cloned() else {
            return;
        };
        self.jobs_open = false;
        // 回到来源文档、当时那一页（引用框就在纸上）
        eprintln!("[jobs] 打开来源 {} 第 {} 页", j.doc, j.page);
        self.open_at(&j.doc, j.page.saturating_sub(1) as usize);
    }

    /// 失败任务重试：**沿用同一个回答 uuid**（索引里那条 `ai` 就不会变孤儿）。
    ///
    /// 需要原问题的文字和框，所以先把来源文档打到那一页，再走一遍 `d1_ask`；
    /// 现在不在那本里就先打开它 —— 打开是异步的（要渲染 PDF），所以这里只
    /// 负责开文档，重试交给 `tick` 里那条"回到文档后再发"的路径。
    fn e1_retry_job(&mut self, i: usize) {
        let Some(j) = self.jobs.items.get(i).cloned() else {
            return;
        };
        eprintln!("[jobs] Retry {} → {}", j.ai, j.doc);
        self.jobs_open = false;
        // `open_at` 是同步的：回来时文档已经在这一页上，可以直接重发。
        self.open_at(&j.doc, j.page.saturating_sub(1) as usize);
        if !matches!(&self.screen, Screen::Notebook(nb) if nb.uuid == j.doc) {
            eprintln!("[jobs] 打不开来源文档，重试放弃");
            return;
        }
        let rect = match j.rect {
            Some(r) => sai::Rect::new(r[0] as i32, r[1] as i32, r[2] as i32, r[3] as i32),
            None => sai::Rect::full_page(),
        };
        self.d1_ask_as(rect, j.question.clone(), Some(j.ai.clone()));
    }

    // === JOB end ===

    /// 侧栏 `Mistakes`：收起侧栏，打开整屏列表（拉列表在后台线程里）。
    fn e1_open_mistakes(&mut self) {
        self.menu_open = false;
        self.filters_open = false;
        self.sort_open = false;
        let cfg = rmdev::net::Config::load();
        if !cfg.configured() {
            eprintln!("[mistakes] 没配对，列表只显示 `Pair a server first`");
        }
        self.mistakes.open(cfg);
        self.draw_mistakes();
    }

    /// 错题列表整屏重画 + 一次全屏刷新（整页 sheet 的惯例，同 `toc`）。
    fn draw_mistakes(&mut self) {
        self.e1_paint_mistakes(true);
    }

    /// 滚动时只刷列表那一块（同 `toc::repaint_toc(false)`）：
    /// 拖动中一串整屏 GC16 会把 EPDC 堵死，手感就没了。
    fn draw_mistakes_rows(&mut self) {
        self.e1_paint_mistakes(false);
    }

    fn e1_paint_mistakes(&mut self, full: bool) {
        let (w, h) = (self.fb.width, self.fb.height);
        self.hits.clear();
        self.tiles.clear();
        self.mistakes.draw(&mut self.fb, &mut self.text);
        // 屏幕还是 `Screen::Library`，`notice()` 画的这条 toast 得补在面板上，
        // 否则 `Marked as learned` 只进队列、肉眼看不到。
        self.draw_notice_bar();
        if full {
            self.fb
                .refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
        } else {
            // 行 + 底部滚动提示；黑头（条数没变）不动
            self.fb.refresh(
                0,
                mistakes::LIST_TOP,
                w,
                h - mistakes::LIST_TOP - mistakes::SAFE_BOTTOM,
                WAVEFORM_MODE_GC16,
                rmdev::fb::UPDATE_MODE_PARTIAL,
            );
        }
    }

    /// 列表开着时所有输入都归它（笔和手指都走这条）：
    /// 点左上返回箭头回库、点行右端 `Learned` 发请求、上下拖滚动。
    fn e1_input(&mut self, ev: InputEvent) {
        // 0 = 按下，1 = 移动，2 = 抬起。抬笔/抬指事件不带坐标，用按下点当点击位置。
        let (phase, x, y) = match ev {
            InputEvent::Touch(TouchEvent::Down { x, y }) => (0, x, y),
            InputEvent::Touch(TouchEvent::Move { x, y }) => (1, x, y),
            InputEvent::Touch(TouchEvent::Up) => (2, 0.0, 0.0),
            InputEvent::Pen(PenEvent::Down(p)) => (0, p.x, p.y),
            InputEvent::Pen(PenEvent::Move(p)) => (1, p.x, p.y),
            InputEvent::Pen(PenEvent::Up) => (2, 0.0, 0.0),
            _ => return,
        };
        match phase {
            0 => self.mistakes.down(x, y),
            1 => {
                // 每 90px 滚一行；真滚了才局部重画（整屏刷新留给打开/进结果）
                if self.mistakes.moved(x, y) {
                    self.draw_mistakes_rows();
                }
            }
            _ => {
                let Some(act) = self.mistakes.up() else {
                    return;
                };
                match act {
                    mistakes::Act::Back => {
                        self.mistakes.close();
                        self.draw_library();
                    }
                    mistakes::Act::Learned(i) => {
                        // 只对命中的那一行发 POST；在跑别的请求时 `learn` 返回 None
                        if let Some(id) = self.mistakes.learn(i) {
                            eprintln!("[mistakes] 标记学会 id={id}");
                        }
                    }
                }
            }
        }
    }

    /// 每帧一次（`tick` 里）：收后台回复。成功 → toast；失败 → 一行英文通知，
    /// 不 panic、不弹模态。
    fn e1_poll(&mut self) {
        // 公式图是异步取的：落地一张就补一帧（`needs_repaint` 自带去重）
        if self.mistakes.is_open() && self.mistakes.needs_repaint() {
            self.draw_mistakes_rows();
        }
        let Some(ev) = self.mistakes.poll() else {
            return;
        };
        match ev {
            mistakes::Event::Loaded => {
                eprintln!("[mistakes] 列表到了");
                if self.mistakes.is_open() {
                    self.draw_mistakes();
                }
            }
            mistakes::Event::LoadFailed(e) => {
                // 细节只进日志；界面固定一行 `Could not load mistakes`
                eprintln!("[mistakes] 拉列表失败：{e}");
                if self.mistakes.is_open() {
                    self.draw_mistakes();
                }
            }
            mistakes::Event::Learned { id, ok: true, .. } => {
                eprintln!("[mistakes] id={id} 已标记学会");
                self.notice(NoticeKind::Info, mistakes::MARKED);
            }
            mistakes::Event::Learned {
                id,
                ok: false,
                why,
            } => {
                eprintln!("[mistakes] id={id} 标记失败：{why}");
                self.notice(NoticeKind::Error, mistakes::MARK_FAILED);
            }
        }
    }
}
// === E1 end ===

// === C20 begin: 输 Wi-Fi 密码界面走 A2 二值局部刷新 ===
//
// 用户原话：「Wi-Fi 链接输入密码的时候帮我弄成 A2 刷新，要不然太闪了」。
//
// 改之前：每敲一个字符 = `draw_wifi_connect` 整页重画 + `GC16 + FULL`
// （46 相位 ≈ 540 ms，整块先黑再亮），打字像在闪灯。
//
// 改之后：进面板那一帧还是整页（一次 `GC16 + FULL`，建立干净基线），之后
// **每次按键只重画输入框和键盘**，两块各自 `A2 + PARTIAL`：
// * 面积：输入框 888×132 + 键盘 1404×601 ≈ 0.96 Mpx，比整屏 2.6 Mpx 少 63%；
// * 波形：A2 只有 12 相位 ≈ 141 ms，二值语义不驱没变的像素，不闪。
//
// 「二值」是硬约束，不是美化：A2 对**目标为中间灰**的像素一个驱动码都没有
// （`panel/encode.rs` 的注释），屏上保持原样 —— 灰底输入框 + 抗锯齿文字放进
// A2 区域里，退格时旧字会被钉在屏上（残留），这正是"宁可少闪，不要脏屏"
// 要避免的那类问题。所以这条路径整页在 `fb.set_mono(true)` 下重画：
// 灰底 → 纯白 + 4 px 黑框，文字走 1-bit 阈值（`ui::text::draw` 的 mono 分支）。
//
// 想亲眼看刷了什么：`RM_PANEL_JOBS=1` 会打出每次 `refresh` 的
// 「宽×高@x,y wf=4 mode=0 ← 文件:行号」（4 = A2，0 = PARTIAL）。

/// 输入框矩形：和 `draw_wifi_connect` 里的 `fill_rect(262, y, 880, 84)` 一致。
/// 两处不一致的话，按下第一个键时文字会跳一下。
const WIFI_BOX_X: i32 = 262;
const WIFI_BOX_W: i32 = 880;
const WIFI_BOX_H: i32 = 84;
/// 第一行（SSID / 网络名）和密码行的 y。
const WIFI_BOX_Y0: i32 = 260;
const WIFI_BOX_Y1: i32 = 430;
/// 1-bit 边框粗细。A2 只有黑白，靠这圈黑边把输入框和空白区分开。
const WIFI_FRAME: i32 = 4;

impl App {
    /// 面板的一次整页绘制：`draw_wifi_connect` 的前半段（见上面那段说明）。
    ///
    /// 返回 `true` = 已经画完并刷过；`false` = 交给原函数体。第二次进来时
    /// `fb.mono` 已经是开的，原函数体就是"按二值规则画一遍"的那份实现 ——
    /// 标题/按钮/键盘的布局、省略号、字号都只有那一份，这里不复制。
    fn wifi_connect_mono_paint(&mut self) -> bool {
        if self.fb.mono() {
            return false;
        }
        if !matches!(self.screen, Screen::WifiPass(_) | Screen::WifiJoin(_)) {
            return false;
        }
        self.fb.set_mono(true);
        // 原函数体：整页按二值画，最后 `refresh(0, 0, w, h, GC16, FULL)`。
        self.draw_wifi_connect();
        // 输入框的黑框得在整屏填白**之后**补（原函数体第一件事就是整屏填白）。
        self.wifi_connect_boxes();
        self.fb.set_mono(false);
        true
    }

    /// 连接面板的当前状态：`(stage, ssid, 已输入的文本, 键盘)`。
    fn wifi_connect_state(&self) -> Option<(u8, String, String, Keyboard)> {
        match &self.screen {
            Screen::WifiPass(p) => Some((p.stage, p.ssid.clone(), p.kb.text.clone(), p.kb.clone())),
            Screen::WifiJoin(p) => Some((p.stage, p.ssid.clone(), p.kb.text.clone(), p.kb.clone())),
            _ => None,
        }
    }

    /// 画一个输入框（1-bit：4 px 黑框 + 白底 + 一行字），返回要刷新的矩形。
    ///
    /// 调用方必须已经把 `fb.set_mono(true)` 打开。
    fn wifi_connect_box(&mut self, y0: i32, label: &str, value: &str) -> (i32, i32, i32, i32) {
        // 标签基线 = 框顶 - 10（原函数体是 250 / 420，正好对上 260 / 430）。
        self.set_text(WIFI_BOX_X, y0 - 10, 29.0, false, label, 20);
        // 先铺黑再挖白：反过来边框会被 interior 吃掉。
        self.fb.fill_rect(
            WIFI_BOX_X - WIFI_FRAME,
            y0 - WIFI_FRAME,
            WIFI_BOX_W + WIFI_FRAME * 2,
            WIFI_BOX_H + WIFI_FRAME * 2,
            gray565(0),
        );
        self.fb.fill_rect(WIFI_BOX_X, y0, WIFI_BOX_W, WIFI_BOX_H, gray565(255));
        // 文字基线 = 框顶 + 54（原函数体是 314 / 484）。
        let shown = self.wifi_panel_text(34.0, value, 840);
        self.set_text(WIFI_BOX_X + 20, y0 + 54, 34.0, false, &shown, 20);
        // 刷新矩形带上标签那一行（标签不变，但多带一点不会有副作用）。
        (
            WIFI_BOX_X - WIFI_FRAME,
            y0 - 44,
            WIFI_BOX_W + WIFI_FRAME * 2,
            WIFI_BOX_H + WIFI_FRAME * 2 + 46,
        )
    }

    /// `Cancel` 那个次级按钮：mono 把它的灰底（210）压成纯白之后，按钮就没
    /// 轮廓了 —— 补一圈 4 px 黑框（官方二级按钮就是描边样式，见 `Forget` 弹层）。
    ///
    /// 调用方必须已经把 `fb.set_mono(true)` 打开。
    fn wifi_cancel_outline(&mut self) -> (i32, i32, i32, i32) {
        // 和 `draw_wifi_connect` 里的 `fill_rect(262, 612, 420, 90, …)` 对齐。
        let (x, y, w, h) = (262, 612, 420, 90);
        let (fx, fy, fw, fh) = (
            x - WIFI_FRAME,
            y - WIFI_FRAME,
            w + WIFI_FRAME * 2,
            h + WIFI_FRAME * 2,
        );
        self.fb.fill_rect(fx, fy, fw, fh, gray565(0));
        self.fb.fill_rect(x, y, w, h, gray565(255));
        self.set_center(x + w / 2, y + 56, 34.0, false, "Cancel", 20);
        (fx, fy, fw, fh)
    }

    /// 补输入框的 1-bit 边框，并按 A2 局部刷一遍（只有边框那圈像素会真翻）。
    ///
    /// 调用方必须已经把 `fb.set_mono(true)` 打开。
    fn wifi_connect_boxes(&mut self) {
        let Some((stage, ssid, typed, _)) = self.wifi_connect_state() else {
            return;
        };
        let mut rects = Vec::with_capacity(3);
        if stage == 0 {
            rects.push(self.wifi_connect_box(WIFI_BOX_Y0, "Enter network name (SSID)", &typed));
        } else {
            // 只读的 SSID 框也要有框：它和密码框长得一样，只是没有光标。
            rects.push(self.wifi_connect_box(WIFI_BOX_Y0, "SSID", &ssid));
            rects.push(self.wifi_connect_box(WIFI_BOX_Y1, "Password", &typed));
        }
        rects.push(self.wifi_cancel_outline());
        for (x, y, w, h) in rects {
            self.fb.refresh(
                x,
                y,
                w,
                h,
                WAVEFORM_MODE_A2,
                rmdev::fb::UPDATE_MODE_PARTIAL,
            );
        }
    }

    /// 面板里的一个按键：只重画输入框和键盘，两块各自 A2 局部刷。
    ///
    /// 返回 `true` = 这个键被面板吃掉了（`handle_key` 直接 return）；`false`
    /// = 当前不是面板，交回 `handle_key` 的原有分支。
    ///
    /// 语义和 `handle_key` 里 `WifiPass | WifiJoin` 那一段完全一致：先喂键盘，
    /// `Enter` 当成 `Connect`。
    fn wifi_connect_key(&mut self, k: Key) -> bool {
        if !matches!(self.screen, Screen::WifiPass(_) | Screen::WifiJoin(_)) {
            return false;
        }
        match &mut self.screen {
            Screen::WifiPass(p) | Screen::WifiJoin(p) => p.kb.press(k),
            _ => return false,
        }
        if k == Key::Enter {
            // 阶段推进 / 校验失败都会整页重画（那条路径保持不变）。
            self.wifi_panel_advance();
            return true;
        }
        let Some((stage, _ssid, typed, kb)) = self.wifi_connect_state() else {
            return true;
        };
        let (w, h) = (self.fb.width, self.fb.height);
        self.fb.set_mono(true);
        // 1) 输入框：只画当前这一步那一行，另一行（stage 1 的只读 SSID 框）不动。
        let (label, y0) = if stage == 0 {
            ("Enter network name (SSID)", WIFI_BOX_Y0)
        } else {
            ("Password", WIFI_BOX_Y1)
        };
        let r = self.wifi_connect_box(y0, label, &typed);
        self.fb
            .refresh(r.0, r.1, r.2, r.3, WAVEFORM_MODE_A2, rmdev::fb::UPDATE_MODE_PARTIAL);
        // 2) 键盘：换大小写/换符号档时整块都变，整块重画最省事（面积也只有
        //    整屏的 32%）。`kb.draw` 自己会先把这块填黑。
        let ky = h - kb.height() - 16;
        self.kb_hits = kb.draw(&mut self.fb, &mut self.text, 0, ky, w);
        self.fb.refresh(
            0,
            ky,
            w,
            kb.height(),
            WAVEFORM_MODE_A2,
            rmdev::fb::UPDATE_MODE_PARTIAL,
        );
        self.fb.set_mono(false);
        true
    }
}

/// `--render wifi:pass[:<已输入>]` / `--render wifi:join[:<已输入>]`：
/// 连接面板的离屏出图（本机假数据，不碰 `wpa_cli`）。
///
/// 带 `<已输入>` 时走的是**真实按键路径**（`wifi_connect_key`），所以出的是
/// "敲了几个字之后"的画面 —— 用来确认局部重画没有把框/键盘擦坏。
fn wifi_render(what: &str, out: &str) -> bool {
    let Some(spec) = what.strip_prefix("wifi:") else {
        return false;
    };
    let (kind, typed) = match spec.split_once(':') {
        Some((k, t)) => (k, t.to_string()),
        None => (spec, String::new()),
    };
    let mut app = App::new(Fb::open_mem());
    let mut c = WifiConnect {
        kb: Keyboard::new(),
        ssid: "My Phone".to_string(),
        stage: 1,
        err: None,
    };
    match kind {
        "pass" | "" => app.screen = Screen::WifiPass(Box::new(c)),
        "join" => {
            c.stage = 0;
            c.ssid.clear();
            app.screen = Screen::WifiJoin(Box::new(c));
        }
        other => {
            eprintln!("[render] wifi 不认识 {other}（要 pass / join）");
            return true;
        }
    }
    app.draw_wifi_connect();
    for ch in typed.chars() {
        app.wifi_connect_key(Key::Ch(ch));
    }
    if let Err(e) = app.fb.dump_pgm(out) {
        eprintln!("[render] 写 {out} 失败：{e}");
    }
    eprintln!("[render] wifi:{kind} → {out}");
    true
}
// === C20 end ===

fn render_screen(what: &str, out: &str) {
    // === C20 begin ===
    // `--render wifi:pass|join[:<已输入>]`：热点的连接面板（见 `wifi_render`）。
    if wifi_render(what, out) {
        return;
    }
    // === C20 end ===
    // === E1 begin ===
    // `--render mistakes:demo[:<top>]`：错题档案整屏出图（假数据，不连网、不读库）。
    if let Some(spec) = what.strip_prefix("mistakes:") {
        rmdev::panels::mistakes::render_demo(spec, out);
        return;
    }
    // `--render inknote:demo|diagram|long`：AI 回答变墨迹后长什么样（不连库）
    if let Some(spec) = what.strip_prefix("inknote:") {
        rmdev::panels::inknote::render_demo(spec, out);
        return;
    }
    // === E1 end ===
    // === E2 begin ===
    if e2_render_ocr(what, out) { return; }
    // === E2 end ===
    // === D5 begin ===
    // `--render ocr:done[:<uuid>[:<page>]]`：自己拷库、自己落盘，不走下面这套。
    if let Some(spec) = what.strip_prefix("ocr:") {
        d5_render_ocr(spec, out);
        return;
    }
    // === D5 end ===
    // === D6 begin ===
    // `--render live:on[:<uuid>[:<页>]]`：Live Note 实时模式出图（假数据，不连网）
    if let Some(spec) = what.strip_prefix("live:") {
        d6_render_live(spec, out);
        return;
    }
    // === D6 end ===
    // === D1 begin ===
    // `--render c9:<tool|rect|answer>[:<uuid>[:<page>]]`：框选 AI 的三个状态
    if let Some(spec) = what.strip_prefix("c9:") {
        c9_render(spec, out);
        return;
    }
    // === D1 end ===
    // === C8 begin ===
    // `--render ai:<mode>:<问题>` 自己造数据、自己落盘，不走 App。
    if let Some(spec) = what.strip_prefix("ai:") {
        c8_render_ai(spec, out);
        return;
    }
    // === C8 end ===
    // === D2 begin ===
    // `--render annot:<spec>`：AI 批注写回页的离屏出图（假 ops，不读库、不连网）。
    // 实现在 `panels::annotate`，这里只留一行路由。
    if let Some(spec) = what.strip_prefix("annot:") {
        rmdev::panels::annotate::render_demo(spec, out);
        return;
    }
    // === D2 end ===
    let fb = Fb::open_mem();
    let mut app = App::new(fb);
    let (name, arg) = match what.split_once(':') {
        Some((n, a)) => (n, Some(a.to_string())),
        None => (what, None),
    };
    match name {
        "library" => app.view = View::MyFiles,
        "sidebar" => {
            app.menu_open = true;
            app.view = View::MyFiles;
        }
        "filters" => {
            app.menu_open = true;
            app.filters_open = true;
            app.view = View::MyFiles;
        }
        "favorites" => app.view = View::Favorites,
        // `tags` / `tags:pages` / `tags:documents` / `tags:<tag>[,<tag>]`
        // 出图用的弹层状态：`tags:hold:<tag>` / `tags:delete:<tag>` / `tags:rename:<tag>`
        "tags" => {
            app.view = View::Tags;
            app.tags.on_open();
            let spec = arg.clone().unwrap_or_default();
            let parts: Vec<&str> = spec.split([':', ',']).filter(|p| !p.is_empty()).collect();
            let mut i = 0;
            while i < parts.len() {
                match parts[i] {
                    "all" => app.tags.tab = tagsview::Tab::All,
                    "pages" => app.tags.tab = tagsview::Tab::Pages,
                    "documents" => app.tags.tab = tagsview::Tab::Documents,
                    "hold" => {
                        i += 1;
                        let tag = parts.get(i).map(|s| s.to_string());
                        app.tags.selected = tag.clone().into_iter().collect();
                        app.tags.hold = tag;
                    }
                    "delete" => {
                        i += 1;
                        let tag = parts.get(i).map(|s| s.to_string());
                        app.tags.selected = tag.clone().into_iter().collect();
                        app.tags.confirm = tag.clone();
                        app.tags.hold = tag;
                    }
                    "rename" => {
                        i += 1;
                        let tag = parts.get(i).map(|s| s.to_string()).unwrap_or_default();
                        let mut kb = Keyboard::new();
                        kb.text = tag.clone();
                        app.tags.selected = vec![tag.clone()];
                        app.tags.hold = Some(tag.clone());
                        app.tags.rename = Some((tag, kb));
                    }
                    other => app.tags.selected.push(other.to_string()),
                }
                i += 1;
            }
        }
        "trash" => app.view = View::Trash,
        "notebooks" => app.view = View::Notebooks,
        "pdfs" => app.view = View::Pdfs,
        "ebooks" => app.view = View::Ebooks,
        "settings" => {
            app.open_settings();
            let mut scan_wifi = false;
            match arg.as_deref() {
                Some("account") => {
                    if let Screen::Settings(s) = &mut app.screen {
                        s.sub = Some(SettingsSub::Account);
                    }
                }
                Some("subscription") => {
                    if let Screen::Settings(s) = &mut app.screen {
                        s.sub = Some(SettingsSub::Subscription);
                    }
                }
                Some("software") => {
                    if let Screen::Settings(s) = &mut app.screen {
                        s.sub = Some(SettingsSub::Software);
                    }
                }
                Some(a) => {
                    // 名字入口：settings:wifi / battery / storage / security /
                    // language / accessibility / help，另外几个二级页也能直接出图。
                    let tab = match a {
                        "wifi" => Some(1),
                        "battery" => Some(2),
                        "storage" => Some(3),
                        "security" => Some(4),
                        "language" => Some(5),
                        "accessibility" => Some(6),
                        // === P5: 7 = Developer，8 = Experimental，Help 挪到 9 ===
                        "developer" => Some(7),
                        "experimental" => Some(8),
                        "help" => Some(9),
                        _ => a.parse::<usize>().ok(),
                    };
                    if let Screen::Settings(s) = &mut app.screen {
                        if let Some(n) = tab {
                            s.tab = n;
                        }
                        match a {
                            "about" => s.help_page = Some(HelpPage::About),
                            "licenses" => s.help_page = Some(HelpPage::Licenses),
                            "tutorial" => s.help_page = Some(HelpPage::Tutorial),
                            "langlist" => s.lang_dialog = Some(LangKind::Handwriting),
                            "kblist" => s.lang_dialog = Some(LangKind::Keyboard),
                            "pin" => s.pin = Some(PinUi::new(PinMode::Set)),
                            // === P5: 本机出图 settings:factory → Factory reset 确认框 ===
                            "factory" => s.factory_confirm = true,
                            _ => {}
                        }
                        scan_wifi = s.tab == 1 && s.lang_dialog.is_none() && s.help_page.is_none();
                    }
                }
                None => {}
            }
            if scan_wifi {
                app.settings_refresh_wifi();
                // 本机出图要的是"最终样子"，这里同步等一次（真机上绝不等）
                if let Some(rx) = app.wifi_scan.take() {
                    if let Ok(n) = rx.recv_timeout(std::time::Duration::from_secs(6)) {
                        if let Screen::Settings(s) = &mut app.screen {
                            s.networks = n;
                        }
                    }
                }
            }
            app.draw_settings();
        }
        "lock" => app.show_lock(),
        // 睡眠屏 / 关机框：和真机同一个函数，只是帧缓冲在内存里
        "sleep" => app.draw_sleep_screen(),
        "poweroff" => app.open_poweroff(),
        "search" => {
            app.open_search();
            if let Some(q) = arg.as_deref() {
                if let Screen::Search(s) = &mut app.screen {
                    for ch in q.chars() {
                        s.kb.text.push(ch);
                    }
                }
                app.draw_search();
            }
        }
        // 自建服务器：配对向导 / 发邮件 / Features 页（本机出图用）
        "pair" => app.open_pair(),
        "mail" => {
            let uuid = arg.clone().unwrap_or_default();
            app.open_at(&uuid, 0);
            app.open_mail();
        }
        "features" => {
            app.open_settings();
            if let Screen::Settings(sv) = &mut app.screen {
                sv.sub = Some(SettingsSub::Subscription);
            }
            app.draw_settings();
        }
        "new-notebook" => app.open_dialog(DialogKind::NewNotebook),
        "new-folder" => app.open_dialog(DialogKind::NewFolder),
        "doc" => {
            let spec = arg.clone().unwrap_or_default();
            let mut it = spec.split(':');
            let uuid = it.next().unwrap_or("").to_string();
            let page = it.next().and_then(|p| p.parse::<usize>().ok()).unwrap_or(0);
            app.open_at(&uuid, page);
        }
        // 本机出图专用（真机上没有这条路径）：打开文档并把「文档设置」面板弹出来，
        // 用来和官方 `35-docsettings-nb.png` / `41-docsettings-pdf.png` 对照。
        // `docpanel:<uuid>[:<page>]`
        "docpanel" => {
            let spec = arg.clone().unwrap_or_default();
            let mut it = spec.split(':');
            let uuid = it.next().unwrap_or("").to_string();
            let page = it.next().and_then(|p| p.parse::<usize>().ok()).unwrap_or(0);
            app.open_at(&uuid, page);
            let kind = app.doc_kind();
            let title = match &app.screen {
                Screen::Notebook(nb) => nb.name.clone(),
                _ => String::new(),
            };
            app.ds.set_title(&title);
            app.ds.toggle(kind);
            app.redraw_page();
        }
        // 文档内搜索：`docsearch:<uuid>[:<page>]:<query>[:<cmd>]`
        // cmd = search（默认）/ type / next / prev / pageonly / close，走后端的真实
        // 点击/按键分发，方便无头复现每个状态。
        "docsearch" => {
            let spec = arg.clone().unwrap_or_default();
            let mut it = spec.split(':');
            let uuid = it.next().unwrap_or("").to_string();
            let a = it.next();
            let b = it.next();
            let cmd = it.next().unwrap_or("search").to_string();
            let (page, query) = match (a, b) {
                (Some(p), Some(q)) => (p.parse::<usize>().unwrap_or(0), q.to_string()),
                (Some(q), None) => (0, q.to_string()),
                _ => (0, String::new()),
            };
            app.open_at(&uuid, page);
            // 走真实入口：打开文档设置面板 → 点 `Search` 行
            app.open_ds_panel();
            app.ds_tap(300.0, 1572.0);
            let mid = |r: (i32, i32, i32, i32)| ((r.0 + r.2 / 2) as f32, (r.1 + r.3 / 2) as f32);
            let bar = app.search.bar(app.fb.width);
            match cmd.as_str() {
                "type" | "enter" => {
                    for ch in query.chars() {
                        app.search_key(Key::Ch(ch));
                    }
                    if cmd == "enter" {
                        app.search_key(Key::Enter);
                    }
                }
                "pageonly" => {
                    let (x, y) = mid(bar.page_only);
                    app.search_tap(x, y);
                    app.search.kb.text = query.clone();
                    app.submit_doc_search();
                }
                _ => {
                    if !query.is_empty() {
                        app.search.kb.text = query.clone();
                        app.submit_doc_search();
                    }
                    match cmd.as_str() {
                        "next" => {
                            let (x, y) = mid(bar.next);
                            app.search_tap(x, y);
                        }
                        "prev" => {
                            let (x, y) = mid(bar.previous);
                            app.search_tap(x, y);
                        }
                        "close" => {
                            let (x, y) = mid(bar.cancel);
                            app.search_tap(x, y);
                        }
                        _ => {}
                    }
                }
            }
        }
        // Notebook settings 页：`nbsettings:<uuid>[:<page>][:<cmd>]`
        // cmd = open（默认）/ name / template / save，同样走真实点击。
        "nbsettings" => {
            let spec = arg.clone().unwrap_or_default();
            let mut it = spec.split(':');
            let uuid = it.next().unwrap_or("").to_string();
            let page = it.next().and_then(|p| p.parse::<usize>().ok()).unwrap_or(0);
            let cmd = it.next().unwrap_or("open").to_string();
            app.open_at(&uuid, page);
            // 走真实入口：打开文档设置面板 → 点 `Notebook settings` 行
            app.open_ds_panel();
            app.ds_tap(300.0, 1452.0);
            let steps: Vec<(f32, f32)> = match cmd.as_str() {
                "name" => vec![(400.0, 260.0)],
                "template" => vec![(1205.0, 1125.0)],
                "save" => vec![
                    (870.0, 1125.0),  // 第 3 张卡：Grid medium
                    (300.0, 550.0),   // Notebook cover: First page
                    (300.0, 810.0),   // Page orientation: Landscape
                ],
                _ => Vec::new(),
            };
            for (x, y) in steps {
                app.nbset_tap(x, y);
            }
            if cmd == "save" {
                app.nbset.kb.text = "Renamed notebook".to_string();
                app.nbset_tap(1350.0, 60.0);
            }
        }
        // Text settings sheet：`textcfg:<uuid>[:large|apply|confirm]`
        // 用来本机出图核对布局（真机上走文档设置 → Text size）。
        "textcfg" => {
            let spec = arg.clone().unwrap_or_default();
            let mut it = spec.split(':');
            let uuid = it.next().unwrap_or("").to_string();
            let cmd = it.next().unwrap_or("open").to_string();
            let tap = |app: &mut App, act: textcfg::Act| {
                let (w, h) = (app.fb.width, app.fb.height);
                if let Some((r, _)) = app.tcfg.hits(w, h).into_iter().find(|(_, a)| *a == act) {
                    app.tcfg_tap((r.0 + r.2 / 2) as f32, (r.1 + r.3 / 2) as f32);
                }
            };
            app.open_at(&uuid, 0);
            app.open_text_settings();
            match cmd.as_str() {
                // 只选不生效：看档位高亮
                "large" => {
                    tap(&mut app, textcfg::Act::Size(4));
                }
                // 走真实点击：第 5 档 + Done → 应用（重排 + 落盘）
                "apply" => {
                    tap(&mut app, textcfg::Act::Size(4));
                    tap(&mut app, textcfg::Act::Done);
                }
                // 有墨迹时的确认框（只为出图，不改 fixture）
                "confirm" => {
                    let cfg = app.reading_cfg;
                    app.tcfg.show(cfg, true);
                    tap(&mut app, textcfg::Act::Size(4));
                    tap(&mut app, textcfg::Act::Done);
                    app.redraw_page();
                }
                _ => {}
            }
        }
        // 页总览多选：`pagesel:<uuid>[:<page>][:<cmd>]`
        // cmd = open（默认）/ all / none / delete / move / tag，出图对照用。
        "pagesel" => {
            let spec = arg.clone().unwrap_or_default();
            let mut it = spec.split(':');
            let uuid = it.next().unwrap_or("").to_string();
            let rest: Vec<&str> = it.collect();
            let page = rest.first().and_then(|p| p.parse::<usize>().ok()).unwrap_or(0);
            let cmd = rest.get(1).copied().unwrap_or("open");
            app.open_at(&uuid, page);
            app.open_overview();
            let total = match &app.screen {
                Screen::Notebook(nb) => nb.page_count,
                _ => 0,
            };
            // 等同长按第 `page` 页：进入多选并选中它
            app.pagesel.show(total, page);
            // 第二个参数是命令额外的输入（Move 的目标页号 / Tag 的名字）
            let val = rest.get(2).copied().unwrap_or("");
            match cmd {
                "all" => app.pagesel.select_all(),
                "none" => app.pagesel.select_none(),
                "delete" => app.pagesel.ask_delete(),
                "move" => {
                    app.open_move_to();
                    if let Some(kb) = app.move_to.as_mut() {
                        kb.text = if val.is_empty() { "3" } else { val }.to_string();
                    }
                }
                "tag" => app.open_page_tag(),
                // 真跑一遍动作，用来无头验证落盘结果（真机上没有这条路径）
                "movecommit" => {
                    app.open_move_to();
                    if let Some(kb) = app.move_to.as_mut() {
                        kb.text = val.to_string();
                    }
                    app.commit_move_to();
                }
                "deleteall" => {
                    app.pagesel.select_all();
                    app.pagesel.ask_delete();
                    app.delete_selected_pages();
                }
                "tagcommit" => {
                    app.open_page_tag();
                    if let Screen::Notebook(nb) = &mut app.screen {
                        if let Some(kb) = nb.tag_edit.as_mut() {
                            kb.text = val.to_string();
                        }
                    }
                    app.commit_tag();
                }
                // 选中额定的几页再移动：`pagesel:<uuid>:0:movesel:4:1,3`
                "movesel" => {
                    app.pagesel.select_none();
                    for part in rest.get(3).copied().unwrap_or("").split(',') {
                        if let Ok(i) = part.trim().parse::<usize>() {
                            app.pagesel.toggle(i);
                        }
                    }
                    app.open_move_to();
                    if let Some(kb) = app.move_to.as_mut() {
                        kb.text = val.to_string();
                    }
                    app.commit_move_to();
                }
                "duplicate" => app.duplicate_selected_pages(),
                _ => {}
            }
            if cmd == "move" {
                app.paint_move_to();
            } else {
                app.draw_overview();
            }
        }
        // === P8 begin ===
        // 库多选出图：`libsel` / `libsel:two` / `libsel:folder` / `libsel:tag` /
        // `libsel:move` / `libsel:trash`（本机无头对照用）。
        "libsel" => {
            let cmd = arg.clone().unwrap_or_default();
            app.view = if cmd == "trash" { View::Trash } else { View::MyFiles };
            // 先画一帧把 `visible` 填出来（动作条只认当前视图里的条目）
            app.draw_library();
            if cmd == "trash" {
                if let Some(b) = app.visible.first().cloned() {
                    app.trash_sel.push(b.uuid);
                }
            } else {
                let docs: Vec<String> = app
                    .visible
                    .iter()
                    .filter(|b| b.doc_type == "DocumentType")
                    .map(|b| b.uuid.clone())
                    .collect();
                let folders: Vec<String> = app
                    .visible
                    .iter()
                    .filter(|b| b.doc_type == "CollectionType")
                    .map(|b| b.uuid.clone())
                    .collect();
                match cmd.as_str() {
                    "two" => {
                        for u in docs.iter().take(2) {
                            app.lib_sel.insert(u.clone());
                        }
                    }
                    "folder" => {
                        if let Some(u) = folders.first() {
                            app.lib_sel.insert(u.clone());
                        }
                    }
                    "move" => {
                        if let Some(u) = docs.first() {
                            app.lib_sel.insert(u.clone());
                            app.lib_moving = true;
                        }
                    }
                    "movein" => {
                        // 先选中一个文档，再进第一个文件夹：出 Move here 的顶栏
                        if let Some(u) = docs.first() {
                            app.lib_sel.insert(u.clone());
                        }
                        let folder = app
                            .visible
                            .iter()
                            .find(|b| b.doc_type == "CollectionType")
                            .cloned();
                        if let Some(b) = folder {
                            app.lib_moving = true;
                            app.enter_folder(&b.uuid, &b.name);
                        }
                    }
                    "tag" => {
                        if let Some(u) = docs.first() {
                            app.lib_sel.insert(u.clone());
                            app.lib_tag_begin();
                        }
                    }
                    _ => {
                        if let Some(u) = docs.first() {
                            app.lib_sel.insert(u.clone());
                        }
                    }
                }
            }
            app.draw_library();
        }
        // === P8 end ===
        other => eprintln!("[render] 未知界面 {other}"),
    }
    // 库/侧栏视图支持两个后缀：`:list` = 列表模式，`:sort` = 排序下拉展开。
    // 例：`library:list` / `favorites:list` / `library:sort`
    if matches!(
        name,
        "library"
            | "sidebar"
            | "filters"
            | "favorites"
            | "tags"
            | "trash"
            | "notebooks"
            | "pdfs"
            | "ebooks"
    ) {
        match arg.as_deref() {
            Some("list") => app.list_mode = true,
            Some("sort") => app.sort_open = true,
            _ => {}
        }
    }
    if !matches!(
        app.screen,
        Screen::Settings(_)
            | Screen::Search(_)
            | Screen::Dialog(_)
            | Screen::Pair(_)
            | Screen::Mail(_)
            // === C0W begin ===
            | Screen::WifiPass(_)
            | Screen::WifiJoin(_)
            // === C0W end ===
            | Screen::Lock(_)
    ) {
        if name == "doc"
            || name == "docpanel"
            || name == "docsearch"
            || name == "nbsettings"
            || name == "textcfg"
            || name == "pagesel"
            || name == "sleep"
            || name == "poweroff"
        {
            // open_at / 浮层/睡眠屏/关机框已经把整页画好了
        } else {
            app.draw_library();
        }
    }
    // === P6 begin ===
    // 无头出图：电子书封面是后台渲的，等它回来再重画一次（最多 2 秒）。
    // 真机上主循环的 `tick()` 会做同样的事。
    if matches!(app.screen, Screen::Library) && (p6_has_pending_covers() || p6_has_covers()) {
        let mut redraw = p6_has_covers();
        for _ in 0..100 {
            if p6_poll_covers() {
                redraw = true;
                break;
            }
            if !p6_has_pending_covers() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if redraw {
            app.draw_library();
        }
    }
    // === P6 end ===
    if let Err(e) = app.fb.dump_pgm(out) {
        eprintln!("[render] 写 {out} 失败：{e}");
    } else {
        eprintln!("[render] {name} → {out}");
    }
}

/// Auto sleep 的空闲阈值：官方 Battery settings 写的就是 20 分钟。
/// `RM_AUTO_SLEEP_SECS=0` 关掉（上机测试时用，免得测到一半睡着了）。
fn auto_sleep_delay() -> Duration {
    let secs = std::env::var("RM_AUTO_SLEEP_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(20 * 60);
    Duration::from_secs(secs)
}

/// 真关机（关机框里点了 `Turn off`）。
///
/// 官方是 `Qt.quit()`：xochitl 退出，系统随之断电。我们是 systemd 里的普通
/// 服务，退出不会断电，所以直接叫 `poweroff`（busybox / systemctl 两条路都试）。
/// 正常情况第一个命令就把机器关掉了，`status()` 根本不会返回。
fn power_shutdown() {
    use std::process::Command;
    let tries: [(&str, &[&str]); 3] = [
        ("/sbin/poweroff", &[]),
        ("poweroff", &[]),
        ("systemctl", &["poweroff"]),
    ];
    for (cmd, args) in tries {
        match Command::new(cmd).args(args).status() {
            Ok(st) if st.success() => return,
            Ok(st) => eprintln!("[power] {cmd} 退出码 {:?}", st.code()),
            Err(e) => eprintln!("[power] {cmd} 不可用：{e}"),
        }
    }
    eprintln!("[power] 关机命令都失败了，设备没关");
}

// === C4 begin ===
// 云端事件通道接线（实现见 `src/cloud.rs`）。
//
// 用户主动触发的任务走 `self.job` 那个单槽位，还要弹 "Syncing..." / "Sync failed."
// —— 那是给手指点的。云端推下来的同步是**悄悄**进行的，失败只进日志，
// 所以这里单独一条 mpsc，不抢那个槽位、不弹窗。

/// 最近一轮云端同步的回报口（`None` = 没有在跑的）。
static C4_SYNC_RX: std::sync::Mutex<
    Option<std::sync::mpsc::Receiver<Result<(), String>>>,
> = std::sync::Mutex::new(None);

/// 有一轮云端同步在跑。`rev` 连着来两条时别叠着起线程。
static C4_SYNCING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 见过的最新 rev。同步期间又来了新 rev 时，同步完再补一轮（不然那条会被吞掉）。
static C4_SEEN_REV: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 命令串行执行：整页渲染会另开一个 1404×1872 的内存帧缓冲（~10 MB），
/// 1 GB 内存的设备上不能让几条命令同时抢。
static C4_CMD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 起一轮后台同步（不阻塞主循环；结果从 [`c4_take_sync_done`] 取）。
fn c4_spawn_sync(root: std::path::PathBuf) {
    use std::sync::atomic::Ordering;
    if C4_SYNCING.swap(true, Ordering::SeqCst) {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    *C4_SYNC_RX.lock().unwrap_or_else(|e| e.into_inner()) = Some(rx);
    std::thread::spawn(move || {
        let r = match rmdev::net::Config::load() {
            cfg if cfg.configured() => rmdev::sync::sync_once(&cfg, &root).map(|st| {
                eprintln!(
                    "[cloud] 同步完成 up={} down={} conflicts={} rev={}",
                    st.up, st.down, st.conflicts, st.rev
                );
            }),
            _ => Err("not paired".to_string()),
        };
        C4_SYNCING.store(false, Ordering::SeqCst);
        let _ = tx.send(r);
    });
}

/// 同步线程回来了吗？（非阻塞；拿到结果就清掉接收端）
fn c4_take_sync_done() -> Option<Result<(), String>> {
    let mut slot = C4_SYNC_RX.lock().unwrap_or_else(|e| e.into_inner());
    let rx = slot.as_ref()?;
    match rx.try_recv() {
        Ok(r) => {
            *slot = None;
            Some(r)
        }
        Err(_) => None,
    }
}

impl App {
    /// 每帧一次：取云端事件、派活、收同步结果并重画。只碰内存，不碰网络。
    fn c4_poll_cloud(&mut self) {
        let (rev, cmds) = rmdev::cloud::poll();
        if let Some(rev) = rev {
            // 本地 rev 以 server.json 为准（sync_once 成功会把它写回去）
            let seen = C4_SEEN_REV.fetch_max(rev, std::sync::atomic::Ordering::SeqCst).max(rev);
            let last = rmdev::net::Config::load().last_rev;
            // 正在同步时 `c4_spawn_sync` 自己会跳过；这条 rev 记在 `C4_SEEN_REV` 里，
            // 等这一轮完了在下面补一轮，不会被吞掉。
            if seen > last {
                c4_spawn_sync(self.library.root.clone());
            }
        }
        for cmd in cmds {
            // 渲染慢（整页光栅 + PNG + 上传），绝不能卡主循环
            std::thread::spawn(move || {
                let _one = C4_CMD_LOCK.lock().unwrap_or_else(|e| e.into_inner());
                match rmdev::cloud::run_cmd(&cmd) {
                    Ok(v) => rmdev::cloud::reply(&cmd.id, v),
                    Err(e) => {
                        eprintln!("[cloud] 命令 {} ({}) 失败：{e}", cmd.id, cmd.op);
                        rmdev::cloud::reply_err(&cmd.id, &e);
                    }
                }
            });
        }
        match c4_take_sync_done() {
            Some(Ok(())) => {
                // 拉下来的文件可能改了任何一页：缩略图/光栅缓存全部作废
                self.thumbs.clear();
                self.raster.clear();
                self.c4_reload_open_page();
                self.repaint_current();
                // 同步期间又提交过（rev 更新）：再补一轮，最终追上服务器
                let seen = C4_SEEN_REV.load(std::sync::atomic::Ordering::SeqCst);
                if seen > rmdev::net::Config::load().last_rev {
                    c4_spawn_sync(self.library.root.clone());
                }
            }
            Some(Err(e)) => eprintln!("[cloud] 同步失败：{e}"),
            None => {}
        }
    }

    /// 同步下来的文件可能正是当前这页：没在编辑、没有未保存笔迹时从磁盘重载。
    /// 有未保存改动就只重画 —— 内存里那份是用户的，不能被远端悄悄盖掉。
    fn c4_reload_open_page(&mut self) {
        let Some((uuid, idx)) = (match &self.screen {
            Screen::Notebook(nb) if !nb.dirty && nb.text_edit.is_none() => {
                Some((nb.uuid.clone(), nb.page_index))
            }
            _ => None,
        }) else {
            return;
        };
        let page = self.library.load_page_at(&uuid, idx);
        let texts = self.library.load_scene_texts(&uuid, idx);
        let count = self.library.page_ids(&uuid).len().max(1);
        if let Screen::Notebook(nb) = &mut self.screen {
            nb.page = page;
            nb.texts = texts.iter().map(|t| t.to_item()).collect();
            nb.p11_styles = texts.iter().map(|t| t.style).collect();
            nb.page_count = count;
            // 远端换了内容：撤销栈指的是旧笔迹，作废
            nb.undo.clear();
            nb.redo.clear();
            nb.sel = None;
            nb.lasso = None;
            nb.page_thumbs = None;
        }
    }
}
// === C4 end ===

// === C18 begin ===
// 手掌误触：**笔优先**从"250 ms 时间窗"升级成"状态窗"，再加一道手势闸门。

/// 笔是不是正攥着屏幕的输入权。
///
/// 两个条件任一成立就算：
/// * 笔刚动过 250 ms 内（原来的时间窗，抬笔后的余震）；
/// * **有一笔正在写**（`nb.active` 存在）—— 这一条是新的。
///
/// 为什么必须有第二条：笔按在屏上不动时（思考、找下一笔的起笔点、慢慢描）一个
/// evdev 事件都不发，`pen_ts` 250 ms 就过期了。这时手掌落到屏上，触摸线程解出来
/// 的 2~3 个触点就会被送进双指/三指识别器 —— 于是"写着写着撤销/重做/缩放/翻页"，
/// 而用户什么都没点。时间戳猜不出"笔还在屏上"，状态能。
fn c18_pen_owns_input(app: &App) -> bool {
    if app.pen_ts.elapsed() < Duration::from_millis(250) {
        return true;
    }
    matches!(&app.screen, Screen::Notebook(nb) if nb.active.is_some())
}

/// 手势闸门：笔攥着输入权时，把所有触点从手势识别链路里摘掉。
///
/// 光在 `handle_touch` 里早退是不够的——主循环是**先** `app.handle(ev)`、
/// **再**取手指快照喂 `viewport::Tracker` 和 `touch::classify` 的，那条路上
/// 没有任何笔优先判定。所以这里把快照清空 + 三个识别器全部 `reset()`：
/// 清零之后的 `classify` 既不会起新一轮，也不会给还没结算的那一轮补一个动作。
///
/// 返回 `true` = 这一帧的触点被丢掉了（诊断日志用）。
fn c18_gesture_gate(
    app: &App,
    fingers: &mut Vec<(i64, f32, f32)>,
    tracker: &mut rmdev::viewport::Tracker,
    three: &mut rmdev::input::touch::ThreeFinger,
    two: &mut TwoFinger,
) -> bool {
    if !c18_pen_owns_input(app) {
        return false;
    }
    let n = fingers.len();
    fingers.clear();
    tracker.reset();
    three.reset();
    two.reset();
    if n > 0 && rmdev::input::touch::tlog() {
        eprintln!("[tlog] G 笔攥着输入权 → 丢弃 {n} 个触点，手势作废（防手掌捏合/撤销）");
    }
    true
}

/// 诊断：把这一帧的手势判定结果打到 stderr（只在 `RM_TOUCH_LOG=1` 时）。
///
/// 一行里有：手指数、tracker 给的手势增量、双指/三指整轮结论。和触摸线程的
/// `[tlog] T` 按时间戳对齐，就能看出"手掌进来以后主循环到底判成了什么"。
fn c18_log_gesture(
    fingers: &[(i64, f32, f32)],
    g: rmdev::viewport::Gesture,
    act3: rmdev::input::touch::ThreeFingerAct,
    act2: TwoFingerAct,
) {
    if !rmdev::input::touch::tlog() {
        return;
    }
    // 没有手指、也没有任何判定结果时不必刷屏（笔的空转帧很多）
    if fingers.is_empty()
        && matches!(g, rmdev::viewport::Gesture::None)
        && act3 == rmdev::input::touch::ThreeFingerAct::None
        && act2 == TwoFingerAct::None
    {
        return;
    }
    eprintln!(
        "[tlog] G t={:.1} n={} gesture={:?} two={:?} three={:?}",
        rmdev::fb::now_ms(),
        fingers.len(),
        g,
        act2,
        act3
    );
}
// === C18 end ===

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--bench-ui") {
        bench_ui();
        return;
    }
    // 命令行同步/配对：给部署脚本和联调用，不走 UI。
    if let Some(i) = args.iter().position(|a| a == "--pair") {
        let url = args.get(i + 1).cloned().unwrap_or_default();
        let code = args.get(i + 2).cloned().unwrap_or_default();
        match rmdev::net::pair(&url, &code, "reMarkable 2") {
            Ok(c) => println!("paired device_id={} url={}", c.device_id, c.url),
            Err(e) => {
                eprintln!("pair failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    if args.iter().any(|a| a == "--sync-once") {
        let cfg = rmdev::net::Config::load();
        if !cfg.configured() {
            eprintln!("not paired (no server config)");
            std::process::exit(1);
        }
        let root = std::path::PathBuf::from(
            std::env::var("RMDEV_LIB_ROOT")
                .unwrap_or_else(|_| rmdev::doc::library::DEFAULT_ROOT.to_string()),
        );
        match rmdev::sync::sync_once(&cfg, &root) {
            Ok(st) => println!(
                "sync ok: up={} down={} conflicts={} rev={}",
                st.up, st.down, st.conflicts, st.rev
            ),
            Err(e) => {
                eprintln!("sync failed: {e}");
                std::process::exit(1);
            }
        }
        return;
    }
    // `--clear-ai <uuid>:<page>`：把某一页 `AI` 图层抹掉（页号 0 基）。
    //
    // 为什么要有这个口子：AI 的批注/回答是写进**真实 `.rm` 的独立图层**的，
    // 一旦某种回答把几千条墨迹写进一页（Live Note 早期版本就是这样，一页涨到
    // 1.4 MB），界面上想删干净就得先进图层面板——而那一页本身就慢得点不动。
    // 这条命令绕过 UI，直接从盘上把那层清掉，用户手写的笔迹一条都不动。
    if let Some(i) = args.iter().position(|a| a == "--clear-ai") {
        let spec = args.get(i + 1).cloned().unwrap_or_default();
        let (uuid, page) = match spec.split_once(':') {
            Some((u, p)) => (u.to_string(), p.parse::<usize>().unwrap_or(0)),
            None => (spec, 0),
        };
        let root = std::path::PathBuf::from(
            std::env::var("RMDEV_LIB_ROOT")
                .unwrap_or_else(|_| rmdev::doc::library::DEFAULT_ROOT.to_string()),
        );
        let lib = rmdev::doc::library::Library::with_root(&root);
        match lib.page_id_at(&uuid, page) {
            Some(pid) => {
                let path = root.join(&uuid).join(format!("{pid}.rm"));
                match rmdev::panels::annotate::clear_at(&path) {
                    Ok(n) => {
                        println!("[clear-ai] {uuid} 第 {page} 页：删掉 {n} 条 AI 墨迹");
                        return;
                    }
                    Err(e) => {
                        eprintln!("[clear-ai] 失败：{e}");
                        std::process::exit(1);
                    }
                }
            }
            None => {
                eprintln!("[clear-ai] 没有这一页：{uuid} page={page}");
                std::process::exit(1);
            }
        }
    }
    if let Some(i) = args.iter().position(|a| a == "--render") {
        let what = args.get(i + 1).cloned().unwrap_or_else(|| "library".into());
        let out = args
            .get(i + 2)
            .cloned()
            .unwrap_or_else(|| "/tmp/render.pgm".into());
        render_screen(&what, &out);
        return;
    }
    if args.iter().any(|a| a == "--watch") {
        watch();
    }
    if args.iter().any(|a| a == "--install-entry") {
        install_entry();
        return;
    }
    // === C4 begin ===
    // 云端事件通道：先注册整页渲染回调（命令线程里跑无头 `render_screen`，
    // 用 `Fb::open_mem()`，不碰真实面板），再起 SSE 线程。上面那几个一次性
    // 命令行（`--render` / `--sync-once` …）已经 return，走不到这里。
    rmdev::cloud::set_renderer(render_screen);
    rmdev::cloud::start();
    // === C4 end ===
    let mut fb = match Fb::open() {
        Ok(fb) => fb,
        Err(e) => {
            eprintln!("[dev] 打不开帧缓冲：{e}");
            std::process::exit(1);
        }
    };
    // 启动条暂时不画：真机实测**第一次全屏刷新之后 framegen 队列就不再出帧**
    // （`pan#` 只出 6 条，后面的 70% 和库界面一个都没推下去）。先让库界面的
    // `draw_library` 当"第一次刷新"，这条路实测是通的；`boot_progress` 留着待用。
    // 抓屏：SIGUSR1 → 主循环把当前 UI 平面 dump 成 PGM（`tools/shot.sh` 用）
    unsafe {
        libc::signal(libc::SIGUSR1, on_sigusr1 as libc::sighandler_t);
    }
    // === P6 begin ===
    // SIGTERM/SIGINT：handler 只置位，主循环负责清屏 + wait_idle + 退出。
    // 不用 async 信号处理库：它会在信号上下文里碰锁/分配器，正好会把面板波形打断。
    unsafe {
        libc::signal(libc::SIGTERM, p6_on_quit_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, p6_on_quit_signal as *const () as libc::sighandler_t);
    }
    // === P6 end ===
    // 电池放后台读：sysfs 的 uevent 走 I2C，一次 440 ms。
    spawn_battery_reader();
    // 状态栏的 Wi-Fi 信号 + 云连通性：起进程 / 发 HTTP，同样不能上绘制路径。
    spawn_net_reader();
    let input = match InputThread::spawn(PEN_DEV, TOUCH_DEV, fb.width as f32, fb.height as f32) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[dev] 输入初始化失败：{e}");
            std::process::exit(1);
        }
    };

    // 首帧看门狗：万一 EPDC 的更新队列被卡住，mxcfb ioctl 会永远不返回，
    // 整个程序就死在第一次刷屏上（屏幕停住、输入也没反应）。
    // 与其让用户对着死屏，不如 8 秒没画成第一帧就自己退出，remux 会接回去。
    static FIRST_FRAME: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(8));
        if !FIRST_FRAME.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!("[dev] 首帧超时（EPDC 队列可能卡住），退出让 remux 接回去");
            std::process::exit(3);
        }
    });
    // 图标预热必须赶在**第一帧刷新之前**：会往帧缓冲左上角乱画，但那时屏幕还
    // 没被推过，等于白画。晚一帧就会在用户眼前闪一下。
    let mut fb = fb;
    rmdev::chrome::prewarm(&mut fb);
    if env_flag("RM_BENCH") {
        // 贴图微基准：e-ink 设备的 CPU 调频很激进，跑一次的数字没意义，
        // 连跑 4 次取后 3 次的平均才是这颗芯片的真实速度。
        let mut cv = rmdev::canvas::Canvas::new(fb.width as usize, fb.height as usize);
        cv.clear();
        let mut best = u128::MAX;
        for i in 0..4 {
            let t = std::time::Instant::now();
            cv.paint_all(&mut fb);
            std::hint::black_box(&fb);
            let us = t.elapsed().as_micros();
            eprintln!("[bench] paint_all 第 {} 次 {us}us", i + 1);
            if i > 0 {
                best = best.min(us);
            }
        }
        eprintln!("[bench] paint_all 最好 {best}us（1404x1872）");
        // 对照：纯 memset 同一块区域。如果它也慢，说明瓶颈在帧缓冲的
        // 内存属性（非缓存映射），不在我们的像素循环。
        for label in ["fill_rect", "row_mut_fill", "vec_fill"] {
            let t = std::time::Instant::now();
            match label {
                "fill_rect" => fb.fill_rect(0, 0, fb.width, fb.height, 0),
                "row_mut_fill" => {
                    for y in 0..fb.height {
                        if let Some(r) = fb.row_mut(y, 0, fb.width) {
                            r.fill(0);
                        }
                    }
                }
                _ => {
                    let mut v = vec![0u16; fb.width as usize * fb.height as usize];
                    v.fill(7);
                    std::hint::black_box(&v);
                }
            }
            eprintln!("[bench] {label} {}us", t.elapsed().as_micros());
        }
    }
    // 开屏锁屏：设了密码就直接进锁屏，别先闪一帧库。
    let mut app = if security::is_enabled() {
        App::new_locked(fb)
    } else {
        App::new(fb)
    };
    // `--open <uuid>[:<page>]`：启动直接进某份文档那一页（页号 0 基）。真机验证
    // 要用——每次先在库里找卡片再翻页，翻错一次结论就反了。
    if let Some(spec) = args.iter().position(|a| a == "--open").and_then(|i| args.get(i + 1)) {
        let (uuid, page) = match spec.split_once(':') {
            Some((u, p)) => (u.to_string(), p.parse::<usize>().unwrap_or(0)),
            None => (spec.clone(), 0),
        };
        eprintln!("[dev] --open {uuid} 第 {} 页", page + 1);
        app.open_at(&uuid, page);
    }
    // 多指手势：每批事件之后取一次手指快照，喂给识别器
    let mut tracker = rmdev::viewport::Tracker::new();
    // 双指整轮手势（点按撤销 / 下滑收条）：要时长和整轮位移，自己记
    let mut two = TwoFinger::new();
    // === P13 begin ===
    // 三指点按 = 重做（官方 `threeFingerTapFilter`），和双指识别器互斥
    let mut three = rmdev::input::touch::ThreeFinger::new();
    // === P13 end ===
    FIRST_FRAME.store(true, std::sync::atomic::Ordering::Relaxed);
    let timing = env_flag("RM_TIMING");
    let min_flush = Duration::from_millis(
        std::env::var("RM_MIN_FLUSH_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            // 笔的采样是 ~125Hz（8ms），限速 12ms 会把一半事件攒到下一次，
            // 实测"事件→提交刷新"平均多出 5ms、最坏 10ms。压到 6ms 之后
            // 每个点都单独推一次，链路里只剩面板自己的时间。
            .unwrap_or(6u64),
    );
    let mut t_last = std::time::Instant::now();
    let mut n_ev = 0u32;
    let mut n_flush = 0u32;
    let mut flush_us = 0u128;

    loop {
        // === P6 begin ===
        // SIGTERM/SIGINT：清一帧白，等面板把手上的波形驱完再退出。
        // 直接 exit 会把 EPDC 停在半驱动状态；`wait_idle` 是睡眠前同一套保护。
        if let Some(sig) = p6_take_quit() {
            // 日志按实际信号打；SIGTERM 这条是约定文案（部署脚本会 grep）。
            let name = if sig == libc::SIGINT { "SIGINT" } else { "SIGTERM" };
            eprintln!("[dev] 收到 {name}，清屏后退出");
            let (w, h) = (app.fb.width, app.fb.height);
            app.fb.fill_rect(0, 0, w, h, gray565(255));
            app.fb
                .refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
            let _ = app.fb.wait_idle(Duration::from_millis(3000));
            std::process::exit(0);
        }
        // === P6 end ===
        let mut any = false;
        while let Some(ev) = input.try_recv() {
            any = true;
            n_ev += 1;
            // 超过 200 ms 的"一次事件处理"就是用户感觉到的卡：正常最慢的
            // 整屏刷新也才 ~100 ms（`draw_settings` 实测 96-99 ms），出现
            // 这条说明有东西把主线程按住了，阈值之下的正常路径不会刷屏。
            let t_h = std::time::Instant::now();
            app.handle(ev);
            if t_h.elapsed().as_millis() >= 200 {
                eprintln!("[slow] 事件处理 {}ms", t_h.elapsed().as_millis());
            }
            // 事件处理完再取手指快照：这样一次点击照旧先把按下/抬起走完，
            // 手势只负责它之后的状态。
            let fingers = input.fingers();
            // === C18 begin: 笔攥着输入权时，触点不进手势链路（防手掌捏合/撤销）===
            // 影子绑定一下：原来的快照不动，下面这层可以把它清空。
            let mut fingers = fingers;
            let _c18_dropped =
                c18_gesture_gate(&app, &mut fingers, &mut tracker, &mut three, &mut two);
            // === C18 end ===
            if fingers.len() >= 2 {
                app.multi_touch = true;
            }
            let g = tracker.update(&fingers);
            app.on_gesture(g);
            // 整轮双指：点按 → 撤销；两指向下拖 → 收工具条。
            // 放在 `on_gesture` 之后：捏合/平移先按原样走，手势本身不冲突
            // （点按位移 < 30px，落在 `on_gesture` 的死区里，视图不会动）。
            // === P13 begin ===
            // 三指优先：三指按着时双指那一轮作废，免得一次三指点按同时
            // 判成"撤销 + 重做"（官方两个 filter 也是互斥的）
            let now_ms = rmdev::fb::now_ms();
            let (act3, act2) =
                rmdev::input::touch::classify(&fingers, &mut three, &mut two, now_ms);
            app.on_three_finger(act3);
            app.on_two_finger(act2);
            // === P13 end ===
            // === C18 begin: 诊断 —— 这一帧手势到底判成了什么 ===
            c18_log_gesture(&fingers, g, act3, act2);
            // === C18 end ===
            // 手指全抬起来才算一轮手势结束：那时才整页重绘一次
            if tracker.fingers() == 0 {
                app.on_gesture_end();
            }
        }
        if any {
            // 一批事件只推一次屏：EPDC 的 update 是串行的，逐点推会把队列堆起来，
            // 表现就是"笔跟不上手"。
            let t0 = std::time::Instant::now();
            if app.flush_pending(min_flush) {
                n_flush += 1;
                flush_us += t0.elapsed().as_micros();
            }
            if t0.elapsed().as_millis() >= 200 {
                eprintln!("[slow] flush {}ms", t0.elapsed().as_millis());
            }
        } else {
            let t_tick = std::time::Instant::now();
            app.tick();
            if t_tick.elapsed().as_millis() >= 200 {
                eprintln!("[slow] tick {}ms", t_tick.elapsed().as_millis());
            }
            // 睡着时没必要每 500 µs 醒一次：面板不动，越少唤醒越省电
            std::thread::sleep(if app.sleeping() {
                Duration::from_millis(20)
            } else {
                Duration::from_micros(500)
            });
        }
        if timing && t_last.elapsed().as_millis() >= 1000 {
            eprintln!(
                "[t] ev/s={} flush/s={} avg_flush={}us",
                n_ev,
                n_flush,
                if n_flush > 0 { flush_us / n_flush as u128 } else { 0 }
            );
            n_ev = 0;
            n_flush = 0;
            flush_us = 0;
            t_last = std::time::Instant::now();
        }
    }
}

// === P3 begin ===
/// 状态栏三指示器 + 通知条（官方 `res_06b6382.qml`、`res_06b7a13.qml`）。
impl App {
    /// 打开设置页并直接落在某个 tab 上（不先闪一下 General）。
    fn open_settings_tab(&mut self, tab: usize) {
        self.pending_settings_tab = Some(tab);
        self.open_settings();
        if tab == 1 {
            // Wi-Fi 页要主动扫一次；扫描本身在后台线程，页面先画出来。
            self.settings_refresh_wifi();
        }
    }

    /// 入队一条通知并重画当前界面。
    ///
    /// 库界面画那条带 `1/3` 的通知条；设置页用它自己的底部提示；笔记本/搜索页
    /// 沿用旧的 `toast()` 黑条（`toast()` 保留正是为了这两处）。其它界面（对话框、
    /// 邮件、配对）没有通知条的位置，保持旧 `toast()` 的路径。
    fn notice(&mut self, kind: NoticeKind, msg: &str) {
        eprintln!("[notify] {msg}");
        self.notices.push(kind, msg, std::time::SystemTime::now());
        if matches!(self.screen, Screen::Settings(_)) {
            self.set_notice(msg);
            self.draw_settings();
        } else if matches!(self.screen, Screen::Notebook(_) | Screen::Search(_)) {
            self.toast(msg);
        } else if matches!(self.screen, Screen::Library) {
            self.draw_library();
        } else {
            self.toast(msg);
        }
    }

    /// 清掉过期通知；库界面在条子消失/换内容时重画一帧。
    fn tick_notices(&mut self) {
        let removed = self.notices.tick(std::time::SystemTime::now());
        if removed > 0 && !self.menu_open && matches!(self.screen, Screen::Library) {
            self.draw_library();
        }
    }

    /// 通知条上的点击：左右箭头翻，点文字区/空白处关掉当前条。
    fn notice_action(&mut self, a: u32) {
        let now = std::time::SystemTime::now();
        match a {
            A_P3_NOTICE_PREV => {
                self.notices.prev(now);
            }
            A_P3_NOTICE_NEXT => {
                self.notices.next(now);
            }
            _ => {
                self.notices.dismiss(now);
            }
        }
        self.draw_library();
    }

    /// 通知条：黑底白字一条，压在状态栏（1800..1872）上面。
    ///
    /// 官方通知态就是黑底白字（`res_06b6382.qml` 的 `notificationActive`）。
    /// 多条时右边画 `1/3` + 左右箭头；单条时整条都能点，点了关掉。
    fn draw_notice_bar(&mut self) {
        let now = std::time::SystemTime::now();
        let Some(pos) = self.notices.position(now) else { return };
        let Some(n) = self.notices.current(now) else { return };
        let (text, kind) = (n.text.clone(), n.kind);
        let (w, h) = (self.fb.width, self.fb.height);
        // 官方 navigatorStatusbarHeight = 72（res_06ba1de.qml）
        let y0 = h - 72 - NOTICE_H;
        self.fb.fill_rect(0, y0, w, NOTICE_H, gray565(0));

        let cy = y0 + NOTICE_H / 2;
        let mut tx = NAV_MARGIN;
        if kind == NoticeKind::Error {
            // 官方 StatusIndicatorError 前面那枚 icon_warning_circle：圈 + 感叹号
            widgets::ring(&mut self.fb, tx + 14, cy, 13, 3, 250);
            self.fb.fill_rect(tx + 12, cy - 8, 4, 12, gray565(250));
            self.fb.fill_rect(tx + 12, cy + 7, 4, 4, gray565(250));
            tx += 40;
        }
        // 多条时右边留给 `1/3` 和两个箭头
        let right = if pos.1 > 1 { w - 264 } else { w - NAV_MARGIN };
        if let Some(t) = self.text.as_mut() {
            let shown = t.ellipsize(NOTICE_SIZE, &text, right - tx);
            t.draw(&mut self.fb, tx, y0 + 62, NOTICE_SIZE, &shown, 250);
        }

        if pos.1 > 1 {
            widgets::back_arrow(&mut self.fb, w - 204, cy, 14, 250);
            widgets::chevron_right(&mut self.fb, w - 40, cy, 14, 250);
            if let Some(t) = self.text.as_mut() {
                let label = format!("{}/{}", pos.0, pos.1);
                let tw = t.width(NOTICE_SIZE, &label) as i32;
                t.draw(&mut self.fb, w - 122 - tw / 2, y0 + 62, NOTICE_SIZE, &label, 250);
            }
            self.hits.push(0, y0, w - 264, NOTICE_H, A_P3_NOTICE_CLOSE);
            // 中间的 `1/3` 也算"点空白处关掉"
            self.hits.push(w - 184, y0, 120, NOTICE_H, A_P3_NOTICE_CLOSE);
            self.hits.push(w - 264, y0, 80, NOTICE_H, A_P3_NOTICE_PREV);
            self.hits.push(w - 64, y0, 64, NOTICE_H, A_P3_NOTICE_NEXT);
        } else {
            self.hits.push(0, y0, w, NOTICE_H, A_P3_NOTICE_CLOSE);
        }
    }
}
// === P3 end ===

// === P4 begin ===
/// 磁盘将满 / 只读模式（官方 `res_0686a15.qml` 的 `NotificationItem`、
/// `res_06b7a13.qml` 的 `StatusIndicatorError`、`res_06a2de1.qml:312` 的
/// `onDiskFullClicked: showSettings(Values.SettingsPage.Storage)`）。
impl App {
    /// 重算一次磁盘水位，并把整库的写权限跟着切过去。
    ///
    /// 官方 Storage 页是 1 s 轮询（`res_0686a15.qml` 的 `interval: 1000`），
    /// 我们只在启动和每 60 s 算一次：`df` 是同步子进程，每秒跑没必要，
    /// 而磁盘也不可能在 60 s 里悄悄写满又恢复。
    fn p4_check_disk(&mut self) {
        self.disk_checked = Some(std::time::Instant::now());
        let (used, total) = disk::read();
        let lv = disk::level(used as f64, total as f64);
        // 只读开关每次都同步（库对象可能刚换过），不能只在状态变化时设。
        self.library.set_read_only(!disk::to_savable(lv));
        if lv == self.disk_level {
            return;
        }
        let old = self.disk_level;
        self.disk_level = lv;
        eprintln!(
            "[disk] {old:?} → {lv:?}（已用 {used:.2} / {total:.2} GB，{}）",
            if disk::to_savable(lv) { "可写" } else { "只读" }
        );
        match lv {
            // 只在这几种界面弹通知：P3 的通知条 / 设置页提示 / toast 在这些界面
            // 都有位置。锁屏、对话框、邮件、配对页没有，旧的 `toast()` 会把库
            // 界面糊上来（锁屏尤其不能），所以那里只切只读、不弹提示。
            disk::Level::Full
                if matches!(
                    self.screen,
                    Screen::Library | Screen::Notebook(_) | Screen::Search(_) | Screen::Settings(_)
                ) =>
            {
                self.notice(NoticeKind::Error, P4_DISK_FULL)
            }
            disk::Level::AlmostFull
                if matches!(
                    self.screen,
                    Screen::Library | Screen::Notebook(_) | Screen::Search(_) | Screen::Settings(_)
                ) =>
            {
                self.notice(NoticeKind::Error, P4_DISK_ALMOST)
            }
            // 空间回来了：解除只读，重画一帧把顶栏提示条去掉。
            disk::Level::Ok => {
                if matches!(self.screen, Screen::Library) {
                    self.draw_library();
                }
            }
            _ => {}
        }
    }

    /// 库顶栏的官方提示条：将满/满时占住顶上三个新建按钮的那一排。
    ///
    /// 只读时那三个按钮本来就会被 `library` 的 `guard_write` 拒掉，换成提示条
    /// 正好（Menu 和搜索还留着，能进回收站删文件腾空间）。点它进
    /// Settings > Storage，和官方 `onDiskFullClicked` 一致。
    fn draw_disk_banner(&mut self) {
        // 回收站里同一排是官方的 `Empty trash`：将满时它还能删（删文件正是
        // 腾空间的手段），不能让提示条把它的命中区抢走，所以回收站不画。
        if self.disk_level == disk::Level::Ok || self.view == View::Trash {
            return;
        }
        let (x, y, w, h) = P4_BANNER;
        // 官方 StatusIndicatorError 是黑底白字（`res_06b7a13.qml` 的
        // `color: Values.colorWhite`）。
        si_fill_capsule(&mut self.fb, x, y, w, h, 0);
        let cy = y + h / 2;
        // 前面那枚 `icon_warning_circle`（圈 + 感叹号，和底栏通知条同一枚）
        widgets::ring(&mut self.fb, x + 30, cy, 13, 3, 250);
        self.fb.fill_rect(x + 28, cy - 8, 4, 12, gray565(250));
        self.fb.fill_rect(x + 28, cy + 7, 4, 4, gray565(250));
        let msg = p4_disk_text(self.disk_level);
        if let Some(t) = self.text.as_mut() {
            let shown = t.ellipsize(P4_BANNER_SIZE, msg, w - 100);
            t.draw(&mut self.fb, x + 60, y + 47, P4_BANNER_SIZE, &shown, 250);
        }
        // 命中区上下各放大 12 px，手指够得着；后压的命中区优先，所以正好
        // 盖掉同一排的 +Folder / +Notebook / +Quick sheets。
        self.hits.push(x, y - 12, w, h + 24, A_P4_DISK);
    }
}

/// 顶栏提示条的文案：只有官方原文（`res_06b7a13.qml` 的 `StatusIndicatorError`）。
fn p4_disk_text(level: disk::Level) -> &'static str {
    match level {
        disk::Level::Full => P4_DISK_FULL,
        disk::Level::AlmostFull => P4_DISK_ALMOST,
        // 官方正常水位是没有提示的（`errorText.text = ""`）；调用点也已经挡掉了。
        disk::Level::Ok => "",
    }
}
// === P4 end ===

// === P11 begin ===
//  打字文本落盘切到官方 `.rm` + 写作格式菜单 + 图层面板的"文字层"一行。
//
//  * 落盘：`library.save_scene_texts()`（官方 `.rm` 优先，失败退回 sidecar）；
//  * 读取：`library.load_scene_texts()`（官方优先，没有文字层再看 sidecar）；
//  * 整页写回会冲掉 `RootText`（`doc/rm::RmPage::serialize()` 不写它），
//    所以每次 `save_page_at` 之后都要 `p11_repair_text_layer()` 补一次。
use rmdev::doc::rmtext::{ParagraphStyle, SceneText};

/// 官方格式菜单的 5 项。官方 `formatMenu`（`res_067887b.qml:1619`）其实是 4 个
/// **图标**按钮（`FormatMenu.Style` = Title / Heading / Paragraph / Bulletpoint，
/// 见 `res_06721bb.qml:29-34`），QML 里没有这 5 个字符串；这 5 个英文名是任务书
/// （按官方界面的实测值）定的，落盘映射到 `.rm` 已有的 8 个样式码：
///
/// | 菜单 | `ParagraphStyle` | 磁盘码 |
/// |---|---|---|
/// | Heading | HEADING | 2 |
/// | Subheading | BOLD | 3 |
/// | Bullet | BULLET | 4 |
/// | Checklist | CHECKBOX | 6 |
/// | Body | PLAIN | 1 |
///
/// `.rm` 的文字样式里没有"副标题"这一档，最近的是 BOLD
/// （rmscene 的真机样本 `Bold_Heading_Bullet_Normal.rm` 第一段就是它）。
const P11_FORMATS: [(&str, ParagraphStyle); 5] = [
    ("Heading", ParagraphStyle::HEADING),
    ("Subheading", ParagraphStyle::BOLD),
    ("Bullet", ParagraphStyle::BULLET),
    ("Checklist", ParagraphStyle::CHECKBOX),
    ("Body", ParagraphStyle::PLAIN),
];

/// 格式按钮字号。官方 `Values.fontSizeBody2` = 25 太宽，5 个并排放不下，压到 22。
const P11_FMT_FONT: f32 = 22.0;
/// 工具条里 Cancel 占左边 260、Done 占右边 240（见 `paint_text_editor`），
/// 中间这一段均分给 5 个格式按钮。
const P11_FMT_X: i32 = 300;
const P11_FMT_RIGHT: i32 = 250;
const P11_FMT_Y: i32 = 18;
const P11_FMT_H: i32 = 60;
const P11_FMT_GAP: i32 = 6;

/// 5 个格式按钮的命中区（`by` = 工具条顶边）。纯函数，单测直接量。
fn p11_format_rects(w: i32, by: i32) -> [((i32, i32, i32, i32), ParagraphStyle); P11_FORMATS.len()] {
    let n = P11_FORMATS.len() as i32;
    let total = (w - P11_FMT_X - P11_FMT_RIGHT).max(n);
    let cw = (total - P11_FMT_GAP * (n - 1)) / n;
    std::array::from_fn(|i| {
        let i = i as i32;
        let r = (
            P11_FMT_X + i * (cw + P11_FMT_GAP),
            by + P11_FMT_Y,
            cw,
            P11_FMT_H,
        );
        (r, P11_FORMATS[i as usize].1)
    })
}

/// 图层面板高度：N 个图层 +（有文字时）文字层一行 + Template 行 + 2px 分隔 + Add 行。
fn p11_layers_height(n: i32, has_text: bool) -> i32 {
    n * LAYER_ROW
        + if has_text { LAYER_ROW } else { 0 }
        + LAYER_ROW
        + 2
        + (LAYER_ROW - 2)
}

/// 页面条目 + 样式 → `SceneText`；样式表比条目短时按 PLAIN（不能错位）。
fn p11_to_scene(items: &[TextItem], styles: &[ParagraphStyle]) -> Vec<SceneText> {
    items
        .iter()
        .enumerate()
        .map(|(i, t)| SceneText {
            x: t.x,
            y: t.y,
            size: t.size,
            text: t.text.clone(),
            color: 0,
            style: styles.get(i).copied().unwrap_or(ParagraphStyle::PLAIN),
        })
        .collect()
}

impl App {
    /// P11：工具栏顶边（没有文字编辑器时 None）
    fn p11_toolbar_top(&self) -> Option<i32> {
        match &self.screen {
            Screen::Notebook(nb) => nb
                .text_edit
                .as_ref()
                .map(|e| self.fb.height - e.kb.height() - 96),
            _ => None,
        }
    }

    /// P11：正在编辑的那一段的样式（新建段落用编辑器里记着的那份）
    fn p11_current_style(&self) -> ParagraphStyle {
        match &self.screen {
            Screen::Notebook(nb) => nb
                .text_edit
                .as_ref()
                .map(|e| e.p11_style)
                .unwrap_or(ParagraphStyle::PLAIN),
            _ => ParagraphStyle::PLAIN,
        }
    }

    /// P11：画格式菜单（当前段落的样式反白）
    fn paint_p11_format_menu(&mut self, w: i32, by: i32) {
        let cur = self.p11_current_style();
        for (r, style) in p11_format_rects(w, by) {
            let sel = style == cur;
            if sel {
                self.fb.fill_rect(r.0, r.1, r.2, r.3, gray565(20));
            } else {
                self.fb.fill_rect(r.0, r.1, r.2, r.3, gray565(255));
                self.fb.fill_rect(r.0, r.1, r.2, 2, gray565(20));
                self.fb.fill_rect(r.0, r.1 + r.3 - 2, r.2, 2, gray565(20));
                self.fb.fill_rect(r.0, r.1, 2, r.3, gray565(20));
                self.fb.fill_rect(r.0 + r.2 - 2, r.1, 2, r.3, gray565(20));
            }
            let label = P11_FORMATS
                .iter()
                .find(|(_, s)| *s == style)
                .map(|(l, _)| *l)
                .unwrap_or("Body");
            let tw = self
                .text
                .as_mut()
                .map(|t| t.width(P11_FMT_FONT, label) as i32)
                .unwrap_or(0);
            let (tx, ty) = (r.0 + (r.2 - tw) / 2, r.1 + 39);
            if let Some(t) = self.text.as_mut() {
                t.draw(&mut self.fb, tx, ty, P11_FMT_FONT, label, if sel { 250 } else { 20 });
            }
        }
    }

    /// P11：点到格式按钮就改**当前这一段**的样式（其它段不动）
    fn tap_p11_format(&mut self, x: f32, y: f32) -> bool {
        let Some(by) = self.p11_toolbar_top() else {
            return false;
        };
        let w = self.fb.width;
        let (xi, yi) = (x as i32, y as i32);
        let Some((_, style)) = p11_format_rects(w, by)
            .into_iter()
            .find(|(r, _)| xi >= r.0 && yi >= r.1 && xi < r.0 + r.2 && yi < r.1 + r.3)
        else {
            return false;
        };
        if let Screen::Notebook(nb) = &mut self.screen {
            if let Some(e) = nb.text_edit.as_mut() {
                e.p11_style = style;
                if let Some(i) = e.editing {
                    if let Some(st) = nb.p11_styles.get_mut(i) {
                        *st = style;
                    }
                    if let Some(it) = nb.texts.get_mut(i) {
                        it.size = style.default_size();
                    }
                }
            }
        }
        eprintln!("[text] 段落样式 → {}", style.label());
        self.redraw_page();
        true
    }

    /// P11：把当前页的文字落到官方 `.rm`（失败会自己退回 sidecar，
    /// 见 `Library::save_scene_texts`）
    fn p11_save_page_texts(&mut self, uuid: &str, idx: usize) {
        let (items, styles) = match &self.screen {
            Screen::Notebook(nb) => (nb.texts.clone(), nb.p11_styles.clone()),
            _ => return,
        };
        let scene = p11_to_scene(&items, &styles);
        match self.library.save_scene_texts(uuid, idx, &scene) {
            Ok(kind) => eprintln!(
                "[text] 第 {} 页 {} 段 → {}",
                idx + 1,
                scene.len(),
                kind.label()
            ),
            Err(e) => eprintln!("[text] 文字没存下来：{e}"),
        }
    }

    /// P11：整页写回之后把文字层补回去 —— `RmPage::serialize()` 不写 `RootText`，
    /// 不补的话手停 5 秒自动存盘一次，这一页打的字就没了。
    fn p11_repair_text_layer(&mut self) {
        let (uuid, idx, has) = match &self.screen {
            Screen::Notebook(nb) => (nb.uuid.clone(), nb.page_index, !nb.texts.is_empty()),
            _ => return,
        };
        if has {
            self.p11_save_page_texts(&uuid, idx);
        }
    }
}
// === P11 end ===

// === C15 begin: 自动加页（可绘制文档末尾的虚拟空白页）===
//
// 需求：编辑时永远在最后一页后面挂一张空白页；在这张空白页上落下第一笔，
// 它就变成真实的一页，末尾再挂一张新的。导出 / 分享 / 保存都不带这张页。
//
// 状态只有一条：`App.c15_virt = Some(文档 uuid)` = 这本可写的文档现在挂着虚拟页。
// **虚拟页不进 `.content`**（没有 page id、没有 `.rm`），所以任何按 `.content`
// 取页数的地方——导出、分享、上传、页总览——天然看不到它；纯逻辑在
// `rmdev::doc::autopage`。这里只做三件事：
//
//   1. 打开文档时把"显示页数"改成 真实页数 + 1（`nb.page_count`），
//      这样屏底 `Page N of M`、页滑块、翻页上界都含虚拟页，用户翻得到它；
//   2. 虚拟页上真有改动（落笔/打字/粘贴）时立刻 `add_page` 把它变成真页；
//   3. 页数被别处改过（总览、加页、改字号、同步）之后把显示页数补回来。
//
// 导出 / 存盘这些"要真实页数"的地方调 `c15_export_total` / `c15_commit_virtual`。
use rmdev::doc::autopage;

impl App {
    /// 这本文档现在挂着虚拟页吗。
    fn c15_has_virtual(&self, uuid: &str) -> bool {
        self.c15_virt.as_deref() == Some(uuid)
    }

    /// 真实页数（**不含**虚拟页）。
    ///
    /// 笔记以 `.content` 为准；PDF / 电子书的页数由文档自己决定，`.content` 里
    /// 可能比它少（手工拷进来的 PDF 一页 page id 都没有），所以两边取大的。
    /// 自动加页实体化出来的追加页在 `.content` 里有条目，也就一起算进来了——
    /// 不然重开文档那几页会看不见。
    fn c15_real_pages(&self, uuid: &str) -> usize {
        let from_content = self.library.page_ids(uuid).len();
        let from_doc = match (&self.doc, self.doc_uuid.as_deref()) {
            (Some(d), Some(u)) if u == uuid => d.pages(),
            _ => 0,
        };
        from_content.max(from_doc)
    }

    /// 导出 / 分享用的页数：只算真实页，虚拟页不进附件。
    fn c15_export_total(&self, uuid: &str, fallback: usize) -> usize {
        if !self.c15_has_virtual(uuid) {
            return fallback;
        }
        self.c15_real_pages(uuid)
    }

    /// 笔现在停在虚拟页上吗。
    ///
    /// O(1)：只靠"显示页数 = 真实页数 + 1"这个不变量判断，不读盘
    /// （`tick` 里每圈都会问一次）。页总览里 `page_count` 是真实页数，
    /// 用 `!nb.overview` 把它排除掉。
    fn c15_on_virtual(&self) -> bool {
        let Screen::Notebook(nb) = &self.screen else {
            return false;
        };
        !nb.overview && self.c15_has_virtual(&nb.uuid) && nb.page_index + 1 >= nb.page_count
    }

    /// 打开可绘制文档时在末尾挂虚拟页（只读 / 打不开的文档不挂）。
    ///
    /// 调用点：`open_at` 里 `show_notebook` 之前（`nb` 还没进 `self.screen`）。
    fn c15_attach(&mut self, nb: &mut Notebook) {
        self.c15_virt = None;
        self.c15_fit_virtual(nb);
    }

    /// 按当前文档状态摆好虚拟页：算真实页数、改 `page_count`、该换空白页就换。
    fn c15_fit_virtual(&mut self, nb: &mut Notebook) {
        self.c15_virt = None;
        if self.library.is_read_only() {
            return;
        }
        let Some(b) = self.library.find(&nb.uuid) else {
            return;
        };
        // 只给"可绘制的文档"挂：认识的格式（笔记 / PDF / 电子书 / 漫画…）。
        // 认不出的（纯预览、坏文件）连页面结构都没有，不该多出一页。
        if !b.format.is_known() {
            return;
        }
        if b.format != rmdev::doc::Format::Rm && self.doc_uuid.as_deref() != Some(nb.uuid.as_str()) {
            // 电子书没打开成功（`self.doc` 不是这本）：页数不可信
            return;
        }
        let real = self.c15_real_pages(&nb.uuid);
        self.c15_virt = Some(nb.uuid.clone());
        nb.page_count = autopage::display_total(real);
        if autopage::is_virtual(nb.page_index, real) {
            nb.page_index = autopage::virtual_index(real);
            nb.page = autopage::blank_page(&nb.uuid);
            nb.texts.clear();
            nb.p11_styles.clear();
            nb.dirty = false;
        }
    }

    /// 页数被别处改过之后，把"显示页数"补回 真实页数 + 1。
    ///
    /// 页总览 / 加页 / 改字号 / 云端同步都会重设 `nb.page_count`（它们只认真实页）。
    /// 顺手把从 `load_page_at` 取来的空白虚拟页的作者 UUID 补上——`add_page`
    /// 建真页时作者就是文档 uuid，缺了官方那边的 CRDT 会对不上。
    fn c15_fix_count(&mut self) {
        let Some(uuid) = (match &self.screen {
            Screen::Notebook(nb) if self.c15_has_virtual(&nb.uuid) => Some(nb.uuid.clone()),
            _ => None,
        }) else {
            return;
        };
        let real = self.c15_real_pages(&uuid);
        if let Screen::Notebook(nb) = &mut self.screen {
            if nb.uuid != uuid {
                return;
            }
            nb.page_count = autopage::display_total(real);
            if autopage::is_virtual(nb.page_index, real) && nb.page.total_lines() == 0 {
                nb.page.author = rmdev::doc::library::uuid_bytes(&uuid);
            }
        }
    }

    /// 虚拟页上真有改动了 → **立刻**把它变成真实页，末尾再挂一张新的虚拟页。
    ///
    /// 返回 true = 真的加了一页。只点一下 / 滑一下不算改动，不会多出页。
    fn c15_commit_virtual(&mut self) -> bool {
        if !self.c15_on_virtual() {
            return false;
        }
        let touched = match &self.screen {
            Screen::Notebook(nb) => autopage::touched(&nb.page, nb.texts.len()),
            _ => false,
        };
        if !touched {
            return false;
        }
        let uuid = match &self.screen {
            Screen::Notebook(nb) => nb.uuid.clone(),
            _ => return false,
        };
        let real = self.c15_real_pages(&uuid);
        match autopage::materialize(&self.library, &uuid, real) {
            Ok(at) => {
                let new_real = self.c15_real_pages(&uuid);
                if new_real <= real {
                    // 0 页又没有 `.content`：`add_page` 插不进去，落墨时
                    // `save_page_at` 会把这一页登记回来（它本来就处理空 `.content`）
                    eprintln!("[auto] 加不了页：{uuid} 没有 .content，交给 save_page_at");
                    return false;
                }
                if let Screen::Notebook(nb) = &mut self.screen {
                    if nb.uuid == uuid {
                        nb.page_index = at;
                        nb.page_count = autopage::display_total(new_real);
                    }
                }
                eprintln!(
                    "[auto] 虚拟页实体化：第 {} 页（真实 {new_real} 页 + 末尾空白页）",
                    at + 1
                );
                true
            }
            Err(e) => {
                eprintln!("[auto] 自动加页失败：{e}");
                false
            }
        }
    }

    /// 笔抬起来之后调：虚拟页上落了第一笔就实体化，再单独刷屏底页码那一块。
    ///
    /// 加页之后 M 会变（`Page 3 of 4` → `Page 3 of 5`），笔这一路只做局部刷新，
    /// 不刷这一块的话用户要等下一次整屏重绘才看到新页数。
    fn c15_on_pen_up(&mut self) {
        if !self.c15_commit_virtual() {
            return;
        }
        let (ftx, fty, _, _) = footer_rect();
        let (w, ..) = (self.fb.width, self.fb.height);
        self.repaint_rects(&[(w / 2 - 400, fty, 800, 92)], WAVEFORM_MODE_GC16);
    }

    /// 跳到"没有印刷底图"的页：末尾的虚拟页，或者 PDF/电子书超出自身页数的追加页。
    ///
    /// 这些页不能拿最后一页当底图（否则空白页上会印着上一页的内容）。
    /// 返回 true = 已经处理完，调用方别再走 `goto_page` 的常规路径。
    fn c15_goto_extra(&mut self, idx: usize) -> bool {
        let Some((uuid, cur, dirty)) = (match &self.screen {
            Screen::Notebook(nb) if !nb.overview => Some((nb.uuid.clone(), nb.page_index, nb.dirty)),
            _ => None,
        }) else {
            return false;
        };
        let real = self.c15_real_pages(&uuid);
        let doc_pages = match (&self.doc, self.doc_uuid.as_deref()) {
            (Some(d), Some(u)) if u == uuid => d.pages(),
            _ => 0,
        };
        // 有底图的页走常规路径
        if idx < real && (doc_pages == 0 || idx < doc_pages) {
            return false;
        }
        if idx == cur {
            return true;
        }
        // 和 `goto_page` 一样：先把当前页的墨迹 / 文字存盘
        if dirty {
            if let Screen::Notebook(nb) = &self.screen {
                let _ = self.library.save_page_at(&uuid, cur, &nb.page);
            }
        }
        self.p11_save_page_texts(&uuid, cur);
        let page = if autopage::is_virtual(idx, real) {
            autopage::blank_page(&uuid)
        } else {
            self.library.load_page_at(&uuid, idx)
        };
        let texts = self.library.load_scene_texts(&uuid, idx);
        self.pdf_bg = None;
        self.bg_gen = self.bg_gen.wrapping_add(1);
        if let Screen::Notebook(nb) = &mut self.screen {
            if nb.uuid != uuid {
                return false;
            }
            nb.page = page;
            nb.page_index = idx;
            nb.page_count = autopage::display_total(real);
            nb.dirty = false;
            nb.active = None;
            nb.text_edit = None;
            nb.texts = texts.iter().map(|t| t.to_item()).collect();
            nb.p11_styles = texts.iter().map(|t| t.style).collect();
            nb.clear_sel();
            nb.undo.clear();
            nb.redo.clear();
        }
        eprintln!("[auto] 翻到第 {} 页（虚拟页/文档外：没有底图）", idx + 1);
        self.redraw_page();
        true
    }

    /// OCR / AI 批注 / 实时模式这些"必须有一张真实页"的操作：在虚拟页上不做。
    ///
    /// 返回 true = 现在就在虚拟页上，调用方直接 return（不弹新文案，静默跳过）。
    fn c15_skip_on_virtual(&self, what: &str) -> bool {
        if !self.c15_on_virtual() {
            return false;
        }
        eprintln!("[auto] {what} 不能在虚拟空白页上做（这一页还没有 page id）");
        true
    }
}
// === C15 end ===

#[cfg(test)]
mod tests {
    use super::*;

    /// 自动同步的省电策略：插电 10 min 固定；低电且没插电 30 min 起步；
    /// 一直没变化就翻倍到 60 min 封顶。
    #[test]
    fn auto_sync_interval_backs_off_when_nothing_happens() {
        // 插电：不管多少轮没变化，都还是 10 分钟
        for q in 0..6 {
            assert_eq!(sync_interval_secs(true, false, q), SYNC_BASE);
            assert_eq!(sync_interval_secs(true, true, q), SYNC_BASE);
        }
        // 电池 10 分钟起步：10 / 20 / 40 / 60 / 60
        assert_eq!(sync_interval_secs(false, false, 0), 600);
        assert_eq!(sync_interval_secs(false, false, 1), 1200);
        assert_eq!(sync_interval_secs(false, false, 2), 2400);
        assert_eq!(sync_interval_secs(false, false, 3), SYNC_MAX);
        assert_eq!(sync_interval_secs(false, false, 9), SYNC_MAX);
        // 低电（≤20%）且没插电：30 分钟起步，也封顶 60
        assert_eq!(sync_interval_secs(false, true, 0), 1800);
        assert_eq!(sync_interval_secs(false, true, 1), SYNC_MAX);
    }

    /// 横屏的输入逆变换：逻辑 (x, y) 上屏成物理 (W-1-y, x)，所以物理原点
    /// 必须回到逻辑的**左下角**（x=0、y=逻辑高）。这条错了就是"点左边写右边"。
    #[test]
    fn landscape_input_maps_back_to_the_logical_frame() {
        // `fb::to_logical` 只在横屏时转，而横屏开关是进程级的：默认竖屏下
        // 它必须是恒等（这条钉住"别在竖屏里乱转"）。
        assert_eq!(rmdev::fb::to_logical(123.0, 456.0), (123.0, 456.0));
    }

    /// 长按的两个数是官方的：Qt `mousePressAndHoldInterval` 的 800 ms、
    /// `scrollOverlay` 的 15 px。以前是 550 ms 且"动了也不取消" ——
    /// 慢慢滚列表时手指刚停下就弹菜单，这是用户报的那个手感问题。
    #[test]
    fn long_press_uses_the_official_800ms_and_15px() {
        assert_eq!(LONG_PRESS_MS, 800);
        assert_eq!(SCROLL_LIMIT, 15.0);

        let mut app = App::new(Fb::open_mem());
        app.press = Some((100.0, 100.0, std::time::Instant::now()));
        // 原地按住、手指抖两下：还算按住
        app.cancel_long_press_if_moved(105.0, 108.0);
        assert!(app.press.is_some(), "10 px 以内不该取消");
        // 拖过 15 px：这次长按作废
        app.cancel_long_press_if_moved(116.0, 100.0);
        assert!(app.press.is_none(), "16 px 就该作废");
        // 拖回来也不能复活（官方 isBeingScrolled 一旦为 true 就再也不回 false）
        app.cancel_long_press_if_moved(100.0, 100.0);
        assert!(app.press.is_none(), "回原位不能复活");
    }

    /// 笔尾橡皮的三档映射。默认（Eraser）必须和官方一致：笔尾只是刷白，
    /// 不能因为上一次在菜单里选过"擦整笔"就悄悄删掉整条线。
    #[test]
    fn tail_eraser_mapping() {
        use prefs::TailEraser::*;
        assert!(!tail_stroke_erase(Eraser, false));
        assert!(!tail_stroke_erase(Eraser, true), "固定刷白不该受记忆影响");
        assert!(tail_stroke_erase(Stroke, false));
        assert!(tail_stroke_erase(Stroke, true), "固定擦整笔不该受记忆影响");
        assert!(!tail_stroke_erase(Last, false));
        assert!(tail_stroke_erase(Last, true), "Use last 要跟着记忆走");
    }

    /// 偏好文件：没有 → 网格；写 true 读回 true；再写 false 读回 false。
    /// 用临时路径，不碰真机的 /home/root。
    #[test]
    fn list_mode_prefs_roundtrip() {
        let p = std::env::temp_dir().join(format!("rmdev-prefs-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        assert!(!load_list_mode(&p), "没有偏好文件时应该是网格");
        save_list_mode(&p, true);
        assert!(load_list_mode(&p), "列表模式要能读回来");
        save_list_mode(&p, false);
        assert!(!load_list_mode(&p), "切回网格也要能读回来");
        let _ = std::fs::remove_file(&p);
    }

    /// `settings.json` 里现在有两个键（列表模式、橡皮记忆），互相不能覆盖。
    ///
    /// 以前的 `save_list_mode` 是整份重写 `{"listMode": ...}`，第一次存橡皮
    /// 记忆时就会被它清掉（反过来也一样）。
    #[test]
    fn eraser_memory_and_list_mode_share_one_file() {
        let p = std::env::temp_dir().join(format!("rmdev-state-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        save_list_mode(&p, true);
        state_set(&p, "lastEraserStroke", serde_json::json!(true));
        assert!(load_list_mode(&p));
        assert!(load_last_erase_stroke(&p));
        save_list_mode(&p, false);
        assert!(!load_list_mode(&p));
        assert!(load_last_erase_stroke(&p), "写 listMode 不能冲掉橡皮记忆");
        state_set(&p, "lastEraserStroke", serde_json::json!(false));
        assert!(!load_last_erase_stroke(&p));
        assert!(!load_list_mode(&p), "写橡皮记忆不能冲掉列表模式");
        let _ = std::fs::remove_file(&p);
    }

    /// 列表行选中态：整行反白（黑底白字、白星标），未选中是白底黑字。
    /// 官方 `res_06a4413.qml`：`color: selected ? black : white`。
    #[test]
    fn list_row_selected_is_inverted() {
        let mut app = App::new(Fb::open_mem());
        let b = Book {
            uuid: "test-uuid".into(),
            name: "Quick sheets".into(),
            parent: String::new(),
            pinned: true,
            deleted: false,
            doc_type: "DocumentType".into(),
            last_modified: 0,
            last_opened_page: 0,
            page_count: 3,
            format: rmdev::doc::Format::Rm,
            size: 0,
            last_opened: 0,
        };
        app.draw_list_row(255, &b, true);
        assert_eq!(app.fb.get(900, 300), 0, "选中行的底色应该是黑");
        let white = (260..320)
            .flat_map(|y| (150..460).map(move |x| (x, y)))
            .filter(|&(x, y)| app.fb.get(x, y) > 200)
            .count();
        assert!(white > 100, "选中行标题/星标应该是白字，实际白像素 {white}");
        // 真机上整屏先铺白；这里手动铺，验证未选中时不刷黑底
        let w = app.fb.width;
        app.fb.fill_rect(0, 255, w, LIST_DOC_H, gray565(255));
        app.draw_list_row(255, &b, false);
        assert_eq!(app.fb.get(900, 300), 255, "未选中行的底色应该是白");
    }

    // === P6 begin ===
    /// 造一个只有四件套、没有正文文件的电子书（模拟"没有缩略图"的库）。
    fn p6_ebook_fixture(tag: &str) -> (std::path::PathBuf, String) {
        let dir = std::env::temp_dir().join(format!("rmdev-p6-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let uuid = "p6-ebook-fixture";
        std::fs::write(
            dir.join(format!("{uuid}.metadata")),
            serde_json::json!({
                "type": "DocumentType",
                "visibleName": "P6 Book",
                "parent": "",
                "deleted": false,
                "lastModified": "1",
                "lastOpenedPage": 0,
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            dir.join(format!("{uuid}.content")),
            r#"{"fileType":"epub","pageCount":0}"#,
        )
        .unwrap();
        (dir, uuid.to_string())
    }

    /// 电子书没有 `<uuid>.thumbnails/<page>.png`：第一帧必须是官方空态（白页，
    /// 边框由调用方画），不能 panic，也不能为了渲封面把这一帧卡住。
    #[test]
    fn ebook_thumb_without_png_falls_back_to_empty_state() {
        let (dir, uuid) = p6_ebook_fixture("thumb");
        let mut app = App::new(Fb::open_mem());
        app.library = Library::with_root(&dir);
        let th = app.thumb(&uuid);
        assert_eq!(th.len(), P6_COVER_W * P6_COVER_H);
        assert!(
            th.iter().all(|&p| p == 255),
            "没渲好之前应该是白页（官方空态），不能是随机像素"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 打不开/没有页的文档：降级路径返回 None，绝不 panic。
    #[test]
    fn cover_render_failure_is_none() {
        assert!(p6_render_cover("/nonexistent/rmdev-p6-nope.epub").is_none());
    }

    /// SIGTERM/SIGINT 置位；别的信号不置位。handler 只碰原子量，
    /// 清屏和 `wait_idle` 留给主循环。
    #[test]
    fn quit_signals_are_recognised() {
        assert!(p6_is_quit_signal(libc::SIGTERM));
        assert!(p6_is_quit_signal(libc::SIGINT));
        assert!(!p6_is_quit_signal(libc::SIGUSR1));
        P6_QUIT_SIG.store(0, std::sync::atomic::Ordering::Relaxed);
        p6_on_quit_signal(libc::SIGTERM);
        assert_eq!(p6_take_quit(), Some(libc::SIGTERM));
        assert!(p6_take_quit().is_none(), "取走以后不能重复退出");
        p6_on_quit_signal(libc::SIGINT);
        assert_eq!(p6_take_quit(), Some(libc::SIGINT));
        p6_on_quit_signal(libc::SIGUSR1);
        assert!(p6_take_quit().is_none(), "抓屏信号不该触发退出");
    }
    // === P6 end ===

    // === P4 begin ===
    /// 顶栏提示条的文案必须是官方原文（`res_06b7a13.qml:107/109`），
    /// 不能自创；将满和满了是两句不同的话。
    #[test]
    fn disk_banner_uses_official_strings() {
        assert_eq!(p4_disk_text(disk::Level::Full), "Disk is full. Not enough disk space.");
        assert_eq!(p4_disk_text(disk::Level::AlmostFull), "Disk is almost full.");
        assert_eq!(p4_disk_text(disk::Level::Ok), "", "正常水位没有提示文案");
        assert_ne!(P4_DISK_FULL, P4_DISK_ALMOST);
    }

    /// 只读时库顶栏画一条黑底白字的提示条，命中 `A_P4_DISK`（点了进 Storage）。
    /// 正常水位下不画，新建按钮的位置不能平白多出个命中区。
    #[test]
    fn disk_banner_only_when_not_ok() {
        let mut app = App::new(Fb::open_mem());
        let (x, y, w, h) = P4_BANNER;
        let mid = (x + w / 2, y + h / 2);
        let before = app.fb.get(mid.0, mid.1);
        app.disk_level = disk::Level::Full;
        app.library.set_read_only(true);
        app.draw_library();
        assert_eq!(app.fb.get(mid.0, mid.1), 0, "提示条应该是黑底");
        assert_eq!(app.hits.take(x as f32 + 10.0, y as f32 + 10.0), Some(A_P4_DISK));
        // 文字区有白像素（白字压在黑底上）
        let white = (y + 20..y + 60)
            .flat_map(|yy| (x + 60..x + w - 40).map(move |xx| (xx, yy)))
            .filter(|&(xx, yy)| app.fb.get(xx, yy) > 200)
            .count();
        assert!(white > 100, "提示条上应该是白字，实际白像素 {white}");
        // 回收站同一排是 `Empty trash`（将满时还能靠它删文件），提示条要让位
        app.view = View::Trash;
        app.draw_library();
        assert_ne!(app.fb.get(mid.0, mid.1), 0, "回收站不画提示条");
        assert_ne!(
            app.hits.take(x as f32 + 10.0, y as f32 + 10.0),
            Some(A_P4_DISK)
        );
        app.view = View::MyFiles;
        // 回到正常水位：提示条没了
        app.disk_level = disk::Level::Ok;
        app.library.set_read_only(false);
        app.draw_library();
        assert_eq!(app.fb.get(mid.0, mid.1), before, "正常水位不该画提示条");
    }

    /// 点提示条 = 官方 `onDiskFullClicked: showSettings(Values.SettingsPage.Storage)`
    /// （`res_06a2de1.qml:312`）：直接落 Storage tab（下标 3），不是 General。
    #[test]
    fn disk_banner_opens_storage_settings() {
        let mut app = App::new(Fb::open_mem());
        app.disk_level = disk::Level::Full;
        app.library.set_read_only(true);
        app.draw_library();
        let (x, y, _, _) = P4_BANNER;
        app.tap_library(x as f32 + 10.0, y as f32 + 10.0);
        match &app.screen {
            Screen::Settings(s) => assert_eq!(s.tab, 3, "应该落在 Storage 页"),
            _ => panic!("点顶栏提示条应该进设置页"),
        }
        assert!(app.library.is_read_only(), "进设置页不能把只读状态丢了");
    }
    // === P4 end ===

    // === P11 begin ===
    /// 写作格式菜单的 5 项（任务书 §14）：Heading / Subheading / Bullet /
    /// Checklist / Body，各落到哪个 `ParagraphStyle`、渲染字号多少。
    #[test]
    fn p11_format_menu_maps_to_paragraph_styles() {
        let got: Vec<(&str, u8, f32)> = P11_FORMATS
            .iter()
            .map(|(l, s)| (*l, s.0, s.default_size()))
            .collect();
        assert_eq!(
            got,
            vec![
                ("Heading", 2, 35.0),
                ("Subheading", 3, 25.0),
                ("Bullet", 4, 25.0),
                ("Checklist", 6, 25.0),
                ("Body", 1, 25.0),
            ]
        );
        assert_eq!(P11_FORMATS.len(), 5, "官方 formatMenu 一屏就这 5 项");
    }

    /// 5 个按钮都要落在 Cancel 区（260）右边、Done 区（w-240）左边、互不重叠，
    /// 并且都在 96 高的工具条里。
    #[test]
    fn p11_format_menu_fits_toolbar() {
        let (w, by) = (1404, 900);
        let rects = p11_format_rects(w, by);
        assert_eq!(rects.len(), P11_FORMATS.len());
        for (r, _) in rects.iter() {
            assert!(r.0 >= 260, "压到 Cancel 区了：{r:?}");
            assert!(r.0 + r.2 <= w - 240, "压到 Done 按钮了：{r:?}");
            assert!(r.1 >= by && r.1 + r.3 <= by + 96, "没在工具条里：{r:?}");
            assert!(r.2 >= 120, "太窄了，`Subheading` 放不下：{r:?}");
        }
        for pair in rects.windows(2) {
            assert!(
                pair[0].0 .0 + pair[0].0 .2 <= pair[1].0 .0,
                "按钮重叠了：{:?}",
                pair
            );
        }
    }

    /// 图层面板：有文字时多出"文字层"一行（官方 `rootDocumentLength > 0`）
    #[test]
    fn p11_layers_panel_grows_for_text_layer() {
        let no_text = p11_layers_height(1, false);
        assert_eq!(no_text, LAYER_ROW + LAYER_ROW + 2 + LAYER_ROW - 2);
        assert_eq!(p11_layers_height(1, true), no_text + LAYER_ROW);
        // 5 层时也只多一行
        assert_eq!(
            p11_layers_height(5, true) - p11_layers_height(5, false),
            LAYER_ROW
        );
    }

    /// 段落样式落盘：`p11_to_scene` 把样式带上；样式表短了按 PLAIN，不能错位
    #[test]
    fn p11_scene_items_carry_styles() {
        let items = vec![
            TextItem {
                x: 1.0,
                y: 2.0,
                size: 35.0,
                text: "a".into(),
            },
            TextItem {
                x: 3.0,
                y: 4.0,
                size: 25.0,
                text: "b".into(),
            },
        ];
        let scene = p11_to_scene(&items, &[ParagraphStyle::HEADING]);
        assert_eq!(scene.len(), 2);
        assert_eq!(scene[0].style, ParagraphStyle::HEADING);
        assert_eq!(scene[0].text, "a");
        assert_eq!((scene[0].x, scene[0].y), (1.0, 2.0));
        assert_eq!(scene[1].style, ParagraphStyle::PLAIN, "样式表短了要按 PLAIN");
    }
    // === P11 end ===
    // === P10 begin ===
    // 手写转文字整页流程：只测纯逻辑（切分、插入位置、进度、失败、几何）。

    /// 真库上的插入位置：处理 [5, 2] 两页、每页两段，结果必须是
    /// "原页顺序不变，每个原页后面紧跟它自己的文字页"，`Undo` 能全删回去。
    #[test]
    fn conv_insert_into_a_real_library_keeps_page_order() {
        let dir = std::env::temp_dir().join(format!("rmdev-p10-ocr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let lib = Library::with_root(&dir);
        let uuid = lib.create_book("P10").unwrap();
        while lib.page_ids(&uuid).len() < 6 {
            let n = lib.page_ids(&uuid).len();
            lib.add_page(&uuid, n - 1).unwrap();
        }
        let ids = lib.page_ids(&uuid);
        assert_eq!(ids.len(), 6);
        let rm2 = lib.root.join(&uuid).join(format!("{}.rm", ids[2]));
        let before = std::fs::read(&rm2).unwrap();

        let mut inserted = Vec::new();
        // 转换顺序是降序；每页 OCR 出两段（每段一页）
        for orig in [5usize, 2usize] {
            let segs = vec![vec!["a".to_string()], vec!["b".to_string()]];
            for (seg, lines) in segs.iter().enumerate() {
                let at = lib
                    .insert_text_page(&uuid, orig + seg, &conv_items(lines))
                    .unwrap();
                conv_shift_inserted(&mut inserted, at);
                inserted.push(at);
            }
        }
        let now = lib.page_ids(&uuid);
        assert_eq!(now.len(), 10, "两页 × 两段 = 多出 4 页");
        // 原页 2 还在原位，它的两段文字紧跟在后面
        assert_eq!(now[2], ids[2]);
        assert_eq!(lib.load_texts(&uuid, 3)[0].text, "a");
        assert_eq!(lib.load_texts(&uuid, 4)[0].text, "b");
        // 原页 3..5 原样顺延
        assert_eq!(&now[5..8], &ids[3..6]);
        // 原页 5 的两段文字排在最后
        assert_eq!(now[7], ids[5]);
        assert_eq!(lib.load_texts(&uuid, 8)[0].text, "a");
        assert_eq!(lib.load_texts(&uuid, 9)[0].text, "b");
        // 原页的 `.rm` 一个字节都没动（新页是另写的一个空页）
        assert_eq!(std::fs::read(&rm2).unwrap(), before, "原页 .rm 不许被改写");

        assert_eq!(conv_undo_order(&inserted), vec![9, 8, 4, 3]);
        for idx in conv_undo_order(&inserted) {
            lib.delete_page(&uuid, idx).unwrap();
        }
        assert_eq!(lib.page_ids(&uuid), ids, "Undo 之后回到原来的 6 页");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一页 1872 高、上下各留 120，行高 34 → 48 行；长文字按这个切页。
    #[test]
    fn conv_paginate_splits_by_page_height() {
        assert_eq!(conv_lines_per_page(CONV_PAGE_H), 48);
        assert_eq!(conv_lines_per_page(200.0), 1, "再矮也得放一行，不能除出 0");

        let text: String = (0..100).map(|i| format!("line {i}\n")).collect();
        let pages = conv_paginate(&text, conv_lines_per_page(CONV_PAGE_H));
        assert_eq!(pages.len(), 3, "100 行 = 48 + 48 + 4");
        assert_eq!((pages[0].len(), pages[1].len(), pages[2].len()), (48, 48, 4));
        assert_eq!(pages[0][0], "line 0");
        assert_eq!(pages[2][3], "line 99");

        // 正好一页
        let exact: String = (0..48).map(|i| format!("l{i}\n")).collect();
        assert_eq!(conv_paginate(&exact, 48).len(), 1);
        // 没文字 / 只有空白 → 一页都不该插
        assert!(conv_paginate("", 48).is_empty());
        assert!(conv_paginate("\n \n\n", 48).is_empty());
        // 中间的空行留着（段落间距），尾部的丢掉
        let p = conv_paginate("a\n\nb\n\n", 48);
        assert_eq!(p.len(), 1);
        assert_eq!(p[0], vec!["a".to_string(), String::new(), "b".to_string()]);
    }

    /// 文字块落在新页上：第一行 120，行距 34，左边 40。
    #[test]
    fn conv_items_start_at_the_page_margin() {
        let lines = vec!["a".to_string(), "b".to_string()];
        let items = conv_items(&lines);
        assert_eq!(items.len(), 2);
        assert_eq!((items[0].x, items[0].y), (40.0, 120.0));
        assert_eq!(items[1].y, 154.0);
        assert_eq!(items[0].size, 26.0);
        assert_eq!(items[1].text, "b");
    }

    /// 选中的页按**降序**处理：从后往前插页，还没处理的页码不会被挤歪。
    #[test]
    fn conv_pages_are_processed_descending() {
        assert_eq!(conv_pages_from_selection(&[2, 5, 8], 0), vec![8, 5, 2]);
        // 重复的去掉
        assert_eq!(conv_pages_from_selection(&[3, 3, 1], 0), vec![3, 1]);
        // 一页都没选 → 转当前页（官方 Convert 一直是可点的）
        assert_eq!(conv_pages_from_selection(&[], 4), vec![4]);
    }

    /// 每页都插在原页之后；先插的页会因为后面再插页而整体后移一格。
    #[test]
    fn conv_insert_positions_follow_the_original_pages() {
        // 页码 → 新页序号：原页 p 的第 0 段在 p+1，第 1 段在 p+2
        assert_eq!((8 + 1, 8 + 2), (9, 10));
        // 模拟处理 [8, 5]：先插 9，再插 6 → 9 要变成 10
        let mut inserted = vec![9usize];
        conv_shift_inserted(&mut inserted, 6);
        inserted.push(6);
        assert_eq!(inserted, vec![10, 6], "先插的后移，新页记自己位置");
        // 再插 3：10→11、6→7
        conv_shift_inserted(&mut inserted, 3);
        inserted.push(3);
        assert_eq!(inserted, vec![11, 7, 3]);
        // 同页的第二段：插在 10 后面，9 不动
        let mut one = vec![9usize];
        conv_shift_inserted(&mut one, 10);
        one.push(10);
        assert_eq!(one, vec![9, 10]);
        // Undo 从后往前删
        assert_eq!(conv_undo_order(&inserted), vec![11, 7, 3]);
        assert_eq!(conv_undo_order(&[4, 9, 4, 2]), vec![9, 4, 2]);
    }

    /// 进度：百分比四舍五入到整数、循环边界；文案照官方 `Converting to text (%1)`。
    #[test]
    fn conv_progress_percent_and_label() {
        assert_eq!(conv_percent(0, 3), 0);
        assert_eq!(conv_percent(1, 3), 33);
        assert_eq!(conv_percent(2, 3), 66);
        assert_eq!(conv_percent(3, 3), 100);
        assert_eq!(conv_percent(9, 3), 100, "超了也算 100");
        assert_eq!(conv_percent(0, 0), 0, "没页时不能除零");

        assert_eq!(conv_progress_text(CONV_LANG), "Converting to text (English)");
        // 官方 `lang.slice(0, i)`：只取第一个空格前那一段
        assert_eq!(
            conv_progress_text("English (United States)"),
            "Converting to text (English)"
        );
    }

    /// 失败路径：三种官方错误文案；传输层错误 → Wi-Fi 那句。
    #[test]
    fn conv_error_text_is_the_official_string() {
        assert_eq!(
            conv_error_text("cannot connect to 10.0.0.2:8080"),
            "Unable to convert text. Please check your Wi-Fi connection."
        );
        assert_eq!(
            conv_error_text("read failed: timed out"),
            "Unable to convert text. Please check your Wi-Fi connection."
        );
        assert_eq!(conv_error_text("HTTP 500"), "Unable to convert notes");
        assert_eq!(conv_error_text("no text"), "Unable to convert notes");
        assert_eq!(conv_error_text(""), "Unable to convert notes");
    }

    /// 状态机：一页都没认出来 = 失败（不留空页给 Undo）；有插入才算完成；
    /// 通知带 key、不自动消失。
    #[test]
    fn conv_stage_machine_and_notice() {
        let mut c = Conv::new();
        assert!(!c.active());
        c.begin_select("doc-1");
        assert!(c.selecting());
        assert_eq!(c.uuid, "doc-1");

        c.start(vec![3, 1]);
        assert!(c.running());
        assert_eq!(c.current_page(), Some(3));
        assert_eq!(c.percent(), 0);
        c.advance();
        assert_eq!(c.current_page(), Some(1));
        assert_eq!(c.percent(), 50);
        // 两页都处理完，但一页都没插 → 失败
        c.advance();
        assert!(c.all_done());
        c.finish();
        assert_eq!(c.stage, ConvStage::Failed, "没有文字就是官方 NoText 那档");

        let mut c = Conv::new();
        c.begin_select("doc-2");
        c.start(vec![0]);
        c.inserted.push(1);
        c.advance();
        c.finish();
        assert_eq!(c.stage, ConvStage::Done);

        c.set_notice(NoticeKind::Error, "Unable to convert notes");
        let n = c.notice.as_ref().unwrap();
        assert_eq!(n.key, CONV_NOTICE_KEY, "进度/结果用同一个 key，完成后替换");
        assert_eq!(n.kind, NoticeKind::Error);
        let later = std::time::SystemTime::now() + CONV_NOTICE_TTL - Duration::from_secs(1);
        assert!(!n.expired(later), "进度通知不许自己消失");
        c.reset();
        assert!(!c.active());
        assert!(c.notice.is_none(), "退出时通知要清掉");
    }

    /// 底部状态条几何：贴屏幕底、按钮在条内、进度按百分比算宽度。
    #[test]
    fn conv_bar_geometry() {
        let (w, h) = (1404, 1872);
        let (bx, by, bw, bh) = conv_bar_rect(w, h);
        assert_eq!((bx, by, bw, bh), (0, 1872 - 72, 1404, 72));
        let (ux, uy, uw, uh) = conv_bar_button(w, h);
        assert!(ux > 0 && ux + uw <= w, "按钮不能出屏");
        assert!(uy >= by && uy + uh <= by + bh, "按钮要贴在状态条里");
        let p = conv_bar_progress(w, h);
        assert!(p.0 + p.2 <= ux, "进度条在按钮左边，不重叠");
        assert_eq!(conv_bar_fill(w, h, 0).2, 0);
        assert_eq!(conv_bar_fill(w, h, 50).2, p.2 / 2);
        assert_eq!(conv_bar_fill(w, h, 100).2, p.2);
        assert_eq!(conv_bar_fill(w, h, 200).2, p.2, "上限 100%");
        // 头部三格：Cancel 在左、Convert 在右，互不重叠
        let c = conv_header_cancel(w);
        let g = conv_header_convert(w);
        assert_eq!((c.0, c.1, c.3), (0, 0, pagesel::HDR_H));
        assert!(g.0 >= c.0 + c.2);
        assert_eq!(g.0 + g.2, w);
    }
    // === P10 end ===
    // === P7 begin ===
    /// 页滑块预览框的 2× 放大：每个源像素铺成 2×2，尺寸正好翻倍。
    /// 放大后是 612×784，塞进官方 615×820 的框里还留得下边框。
    #[test]
    fn p7_scale2_doubles_each_pixel() {
        let src = vec![0u8, 10, 20, 30, 40, 50]; // 3×2
        let out = p7_scale2(&src, 3, 2);
        assert_eq!(out.len(), 12 * 2); // 6×4
        let at = |x: usize, y: usize| out[y * 6 + x];
        for (sy, sx, v) in [(0, 0, 0u8), (0, 1, 10), (1, 2, 50)] {
            assert_eq!(at(sx * 2, sy * 2), v);
            assert_eq!(at(sx * 2 + 1, sy * 2), v);
            assert_eq!(at(sx * 2, sy * 2 + 1), v);
            assert_eq!(at(sx * 2 + 1, sy * 2 + 1), v);
        }
        // 页总览那档尺寸放大后要能塞进官方预览框
        let big = p7_scale2(&vec![9u8; 306 * 392], 306, 392);
        assert_eq!(big.len(), 612 * 784);
        assert!(612 <= rmdev::panels::slider::THUMB_W);
        assert!(784 <= rmdev::panels::slider::THUMB_H);
    }
    // === P7 end ===

    // === D1 begin ===
    /// 造一本最小的笔记本（不动磁盘、不读库）。
    fn d1_test_nb() -> Notebook {
        Notebook {
            uuid: "d1-test-doc".into(),
            name: "D1 test".into(),
            page: RmPage::default(),
            page_index: 0,
            page_count: 1,
            overview: false,
            page_thumbs: None,
            swipe: None,
            chrome: Chrome::default(),
            active: None,
            exit_taps: 0,
            dirty: false,
            tap_consumed: false,
            texts: Vec::new(),
            p11_styles: Vec::new(),
            text_edit: None,
            pending_text: None,
            sel: None,
            lasso: None,
            drag_sel: None,
            pending_paste: None,
            undo: Vec::new(),
            redo: Vec::new(),
            tag_edit: None,
            layer_rename: None,
            erase_rect: None,
            view: PageView::identity(),
            last_pen: 0,
            landscape_back: None,
        }
    }

    fn d1_pen_at(x: f32, y: f32) -> InputEvent {
        InputEvent::Pen(PenEvent::Down(rmdev::pen::line::InputPoint {
            x,
            y,
            pressure: 0.6,
            tilt_x: 0.0,
            tilt_y: 0.0,
            rubber: false,
        }))
    }

    fn d1_pen_move(x: f32, y: f32) -> InputEvent {
        InputEvent::Pen(PenEvent::Move(rmdev::pen::line::InputPoint {
            x,
            y,
            pressure: 0.6,
            tilt_x: 0.0,
            tilt_y: 0.0,
            rubber: false,
        }))
    }

    /// E 的头号验收点：**卡片显示期间笔必须能写，写下的笔迹真的进页面模型**。
    ///
    /// 跑的是真实分发路径：`handle(Down/Move/Up)` → `handle_notebook` →
    /// `d1_pen`（卡片开着直接放行）→ 正常落墨。卡片里那一笔也照写
    /// （`selectai::route` 的 Pen 永远 → Ink），只是被卡片盖住。
    #[test]
    fn d1_pen_still_writes_while_answer_card_is_open() {
        let mut app = App::new(Fb::open_mem());
        let mut nb = d1_test_nb();
        // 卡片开着（假数据：不连服务器）
        app.ask_ai.set_inline(Some(rmdev::panels::selectai::card_rect()));
        app.ask_ai.seed(ai::Mode::Select, "q", "回答");
        app.selai.card = Some(rmdev::panels::selectai::card_rect());
        app.selai.doc = nb.uuid.clone();
        app.selai.on = false;
        nb.chrome.ask_ai = false;
        app.screen = Screen::Notebook(Box::new(nb));

        // 卡片外面写一笔（下半部，y=1400）
        app.handle(d1_pen_at(300.0, 1400.0));
        for k in 1..=8 {
            app.handle(d1_pen_move(300.0 + k as f32 * 20.0, 1400.0 + k as f32 * 4.0));
        }
        app.handle(InputEvent::Pen(PenEvent::Up));
        let n1 = match &app.screen {
            Screen::Notebook(nb) => nb.page.total_lines(),
            _ => 0,
        };
        assert_eq!(n1, 1, "卡片开着时，卡片外的笔迹必须落进墨迹层");

        // 卡片**里面**也写：笔不被卡片吃掉（selectai::route: Pen → Ink）
        app.handle(d1_pen_at(600.0, 400.0));
        for k in 1..=6 {
            app.handle(d1_pen_move(600.0 + k as f32 * 15.0, 400.0 + k as f32 * 6.0));
        }
        app.handle(InputEvent::Pen(PenEvent::Up));
        let n2 = match &app.screen {
            Screen::Notebook(nb) => nb.page.total_lines(),
            _ => 0,
        };
        assert_eq!(n2, 2, "笔落在卡片里也要照写（只是被盖住）");
        // 卡片还在（笔没有把它关掉/吃掉）
        assert!(app.ask_ai.open, "笔不能把卡片关掉");
        // 标脏 → tick 的自动存盘（手停 5 s）会把它写回磁盘
        assert!(
            matches!(&app.screen, Screen::Notebook(nb) if nb.dirty),
            "写完要标脏，自动存盘才会落盘"
        );
        // 笔迹真的进了 canvas（会被整页图/存盘带走）
        let mut ink = 0;
        for y in 1390..1450 {
            for x in 290..470 {
                if app.canvas.dark_at(x, y) > 0 {
                    ink += 1;
                }
            }
        }
        assert!(ink > 20, "墨迹要画进画布（{ink} px）");
    }

    /// 关掉卡片：纸面补回来，刚写的字一笔不丢。
    #[test]
    fn d1_close_card_keeps_the_ink() {
        let mut app = App::new(Fb::open_mem());
        let nb = d1_test_nb();
        app.screen = Screen::Notebook(Box::new(nb));
        app.ask_ai.set_inline(Some(rmdev::panels::selectai::card_rect()));
        app.ask_ai.seed(ai::Mode::Select, "q", "回答");
        app.selai.card = Some(rmdev::panels::selectai::card_rect());
        app.handle(d1_pen_at(400.0, 300.0)); // 卡片里面写
        app.handle(d1_pen_move(460.0, 330.0));
        app.handle(InputEvent::Pen(PenEvent::Up));
        let before = match &app.screen {
            Screen::Notebook(nb) => nb.page.total_lines(),
            _ => 0,
        };
        app.d1_close_card();
        assert!(!app.ask_ai.open);
        let after = match &app.screen {
            Screen::Notebook(nb) => nb.page.total_lines(),
            _ => 0,
        };
        assert_eq!(before, after, "关卡片不能丢笔迹");
        assert_eq!(after, 1);
        // 关掉之后纸面恢复成"没有卡片"：卡片顶边那一块又变白了
        assert!(app.fb.get(700, 200) > 200, "卡片区域要恢复成纸面");
    }

    /// 整页图 = 印刷底图 + 墨迹 + 文字层（发出去的就是屏幕上这一页）。
    #[test]
    fn d1_page_gray_has_bg_ink_and_text() {
        let mut app = App::new(Fb::open_mem());
        let nb = d1_test_nb();
        app.screen = Screen::Notebook(Box::new(nb));
        // 底图：整屏浅灰（模拟印刷页）
        app.pdf_bg = Some(vec![200u8; (app.fb.width * app.fb.height) as usize]);
        // 墨迹：画布上一条黑线
        app.canvas
            .ink_segment(300.0, 300.0, 3.0, 500.0, 300.0, 3.0);
        // 文字层
        if let Screen::Notebook(nb) = &mut app.screen {
            nb.texts.push(TextItem {
                x: 200.0,
                y: 1200.0,
                size: 30.0,
                text: "Hello".into(),
            });
        }
        let gray = app.d1_page_gray();
        let (w, h) = (app.fb.width as usize, app.fb.height as usize);
        assert_eq!(gray.len(), w * h, "整页图就是 1404×1872，不许 2x");
        // 底图（印刷页）在：不是纯白
        assert!(gray[1500 * w + 1300] < 210, "底图要带上");
        // 墨迹在
        assert!(gray[300 * w + 400] < 60, "墨迹要带上");
        // 文字层在（Hello 那行有黑像素；文字坐标是页面坐标，画的时候 + PAGE_DX）
        let dark = (1180..1260)
            .flat_map(|y| (880..1060).map(move |x| (x, y)))
            .filter(|&(x, y)| gray[y * w + x] < 100)
            .count();
        assert!(dark > 10, "文字层要带上（{dark} px）");
        // 裁图 = 整页图里那块
        let (cw, ch, cut) = rmdev::panels::selectai::crop(
            &gray,
            w,
            h,
            rmdev::panels::selectai::Rect::new(300, 280, 520, 320),
        );
        assert_eq!((cw, ch), (220, 40));
        assert_eq!(cut[20 * cw + 100], gray[300 * w + 400], "裁图取的是同一批像素");
    }

    /// 工具条第 13 格：点一下进工具 + 弹浮层，再点一下退出（回普通笔）。
    #[test]
    fn d1_cell_toggles_the_tool() {
        let mut app = App::new(Fb::open_mem());
        let nb = d1_test_nb();
        app.screen = Screen::Notebook(Box::new(nb));
        assert!(!app.selai.on && !app.selai.popover);
        app.on_chrome(Msg::AiSelect);
        assert!(app.selai.on, "第一次点 = 进工具");
        assert!(app.selai.popover, "进工具就弹两项浮层");
        assert!(
            matches!(&app.screen, Screen::Notebook(nb) if nb.chrome.ask_ai),
            "图标要反白"
        );
        // 再点一次 = 退出
        app.on_chrome(Msg::AiSelect);
        assert!(!app.selai.on, "再点一次退出");
        assert!(!app.selai.popover);
        assert!(
            matches!(&app.screen, Screen::Notebook(nb) if !nb.chrome.ask_ai),
            "反白要撤掉"
        );
        // 退出后笔就是普通笔：落笔即落墨
        app.handle(d1_pen_at(400.0, 400.0));
        app.handle(d1_pen_move(480.0, 440.0));
        app.handle(InputEvent::Pen(PenEvent::Up));
        assert_eq!(
            match &app.screen {
                Screen::Notebook(nb) => nb.page.total_lines(),
                _ => 0,
            },
            1,
            "退出工具后笔回到普通笔"
        );
    }

    /// 工具开着时：笔拖一个框 → 抬笔进"问问题"（不落墨）。
    #[test]
    fn d1_drag_box_opens_the_question_bar() {
        let mut app = App::new(Fb::open_mem());
        let nb = d1_test_nb();
        app.screen = Screen::Notebook(Box::new(nb));
        app.selai.on = true;
        app.selai.popover = false;
        app.selai.doc = "d1-test-doc".into();
        if let Screen::Notebook(nb) = &mut app.screen {
            nb.chrome.ask_ai = true;
        }
        app.handle(d1_pen_at(300.0, 400.0));
        app.handle(d1_pen_move(700.0, 800.0));
        app.handle(d1_pen_move(900.0, 1200.0));
        app.handle(InputEvent::Pen(PenEvent::Up));
        // 一个字都没落进墨迹层
        assert_eq!(
            match &app.screen {
                Screen::Notebook(nb) => nb.page.total_lines(),
                _ => 0,
            },
            0,
            "框选不依赖笔迹，也不许落墨"
        );
        assert_eq!(app.selai.stage, rmdev::panels::selectai::Stage::Ask);
        assert!(app.selai.kb.is_some(), "抬笔就该出提问键盘");
        assert_eq!(
            app.selai.rect.map(|r| r.arr()),
            Some([300, 400, 900, 1200]),
            "矩形要按拖的方向归一化"
        );
    }

    /// 把帧缓冲整块读成灰度（比较用）。
    fn d1_dump(fb: &Fb) -> Vec<u8> {
        let mut v = Vec::with_capacity((fb.width * fb.height) as usize);
        for y in 0..fb.height {
            for x in 0..fb.width {
                v.push(fb.get(x, y));
            }
        }
        v
    }

    /// D6（Live Note）那两个浮层占的地方。
    ///
    /// 它们的显隐由 **进程级全局状态**（`d6::live` 的 static）决定，而 bin 里的
    /// 测试是并行跑的：D6 自己的测试会把 `is_on`/`btn_shown` 改来改去，于是同
    /// 一个 App 在两次 `redraw_page` 之间那块像素会变（按钮"开着反白/关着白底"）。
    /// 这跟"卡片不许动页面"是两码事（按钮 952..1300×10..90 和卡片不重叠），
    /// 所以像素不变量测试把这两块排除掉。
    fn d1_is_d6_zone(x: i32, y: i32) -> bool {
        let (w, h) = (1404, 1872);
        let (bx, by, bw, bh) = d6::live::btn_rect(w);
        if x >= bx && x < bx + bw && y >= by && y < by + bh {
            return true;
        }
        let (_, ty, _, th) = d6::live::band_rect(w, h);
        y >= ty && y < ty + th
    }

    /// 造一页"印刷页"：整屏浅灰底 + 几道黑条（模拟 PDF 正文），再加一笔墨、一段文字。
    fn d1_print_page(app: &mut App) {
        let (w, h) = (app.fb.width as usize, app.fb.height as usize);
        let mut bg = vec![255u8; w * h];
        for y in 0..h {
            if y % 40 < 12 {
                for x in 200..1200 {
                    bg[y * w + x] = 60;
                }
            }
        }
        app.pdf_bg = Some(bg);
        app.canvas.ink_segment(300.0, 300.0, 3.0, 500.0, 300.0, 3.0);
        if let Screen::Notebook(nb) = &mut app.screen {
            nb.texts.push(TextItem {
                x: 200.0,
                y: 1200.0,
                size: 30.0,
                text: "Hello".into(),
            });
        }
        app.redraw_page();
    }

    /// BUG 1 的底线（用户原话）：**卡片打开当帧，卡片矩形以外的像素逐点不变**。
    ///
    /// 真机上"整页看着白了"其实是卡片盖住了 PDF 的正文区：像素上卡片外一个点
    /// 都没变（`dev/shots/d1-before-send.png` 与 `d1-card-white.png` 逐点比对
    /// 为证：卡片下方 / 上方 / 右侧全是 0 差异）。这条测试把这个不变量钉死。
    #[test]
    fn d1_card_open_keeps_every_pixel_outside_it() {
        // `live::is_on()` 是进程级全局：**必须拿 `live_guard()`**，否则会和
        // `d6_band_*` 那些测试并行互相踩（它们正拿着闸门画状态条，这里一句
        // `set_on(false)` 就把带子关了 → 那边断言"分隔线断了"蹦出来）。
        let _g = live_guard();
        rmdev::panels::live::set_on(false);
        let mut app = App::new(Fb::open_mem());
        let nb = d1_test_nb();
        app.screen = Screen::Notebook(Box::new(nb));
        d1_print_page(&mut app);
        let before = d1_dump(&app.fb);

        // 真机 d1_ask 的开卡片动作：show_pending + 自适应矩形 + 整页重绘
        app.ask_ai.show_pending();
        let keep = rmdev::panels::selectai::card_rect_for(app.ask_ai.inline_needed_height());
        app.selai.card = Some(keep);
        app.ask_ai.set_inline(Some(keep));
        app.redraw_page();
        let after = d1_dump(&app.fb);

        let (cx, cy, cw, ch) = keep;
        let (w, h) = (app.fb.width as usize, app.fb.height as usize);
        let mut diff = 0;
        let mut first = None;
        for y in 0..h {
            for x in 0..w {
                let inside = (x as i32) >= cx
                    && (x as i32) < cx + cw
                    && (y as i32) >= cy
                    && (y as i32) < cy + ch;
                if !inside
                    && !d1_is_d6_zone(x as i32, y as i32)
                    && before[y * w + x] != after[y * w + x]
                {
                    diff += 1;
                    if first.is_none() {
                        first = Some((x, y, before[y * w + x], after[y * w + x]));
                    }
                }
            }
        }
        assert_eq!(diff, 0, "卡片外变了 {diff} 个像素，第一个 {first:?}");
        // 卡片里是"浮层"：黑表头
        assert!(app.fb.get(cx + 5, cy + 5) < 40, "卡片要有黑表头");
        // 卡片高度是自适应的（打开时最矮），页面下半部照旧看得见
        assert!(ch <= rmdev::panels::selectai::CARD_MIN_H + 60, "打开时卡片要矮");
    }

    /// ⑤ 公式回流之后，「卡片外逐像素不变」仍然成立：
    /// 行内公式（含 60px 高的复杂公式）都会被裁在卡片里，页面一个点都不许动。
    #[test]
    fn d1_merged_formula_card_keeps_the_page() {
        let mut app = App::new(Fb::open_mem());
        let nb = d1_test_nb();
        app.screen = Screen::Notebook(Box::new(nb));
        d1_print_page(&mut app);
        let before = d1_dump(&app.fb);

        // 真机那种"每个公式单独换行"的回答
        let answer = "验证：被积函数\n$xe^{-x}$\n在\n$[0,1]$\n上从\n$0$\n增到\n$e^{-1} \\approx 0.368$";
        app.ask_ai.set_inline(Some(rmdev::panels::selectai::card_rect()));
        app.ask_ai.seed(ai::Mode::Select, "q", answer);
        for (k, w, h) in [
            ("xe^{-x}", 130usize, 60usize),
            ("[0,1]", 90, 50),
            ("0", 40, 80),
            ("e^{-1} \\approx 0.368", 220, 55),
        ] {
            let g = c9_formula_gray(w, h);
            app.ask_ai
                .seed_png(k, &rmdev::share::png_gray(&g, w, h));
        }
        let keep = rmdev::panels::selectai::card_rect_for(app.ask_ai.inline_needed_height());
        app.selai.card = Some(keep);
        app.ask_ai.set_inline(Some(keep));
        app.redraw_page();
        let after = d1_dump(&app.fb);

        let (cx, cy, cw, ch) = keep;
        let (w, h) = (app.fb.width as usize, app.fb.height as usize);
        let mut diff = 0;
        let mut first = None;
        for y in 0..h {
            for x in 0..w {
                let inside = (x as i32) >= cx
                    && (x as i32) < cx + cw
                    && (y as i32) >= cy
                    && (y as i32) < cy + ch;
                if !inside
                    && !d1_is_d6_zone(x as i32, y as i32)
                    && before[y * w + x] != after[y * w + x]
                {
                    diff += 1;
                    if first.is_none() {
                        first = Some((x, y, before[y * w + x], after[y * w + x]));
                    }
                }
            }
        }
        assert_eq!(diff, 0, "回流后卡片外变了 {diff} 个像素，第一个 {first:?}");
        // 卡片本身要矮（回流成 2~3 行），别回到"占满上半屏"
        assert!(ch <= 420, "回流后卡片不该这么高（{ch}）");
    }

    /// BUG 4 的底线：关掉卡片后，卡片矩形（含外扩一圈）和"从来没开过"逐点相同。
    #[test]
    fn d1_close_card_restores_the_page_exactly() {
        let _g = live_guard();
        rmdev::panels::live::set_on(false);
        let mut app = App::new(Fb::open_mem());
        let nb = d1_test_nb();
        app.screen = Screen::Notebook(Box::new(nb));
        d1_print_page(&mut app);
        let never = d1_dump(&app.fb);

        // 开卡片（矮）→ 内容长高到上限 → 关掉
        app.ask_ai.show_pending();
        let keep = rmdev::panels::selectai::card_rect_for(app.ask_ai.inline_needed_height());
        app.selai.card = Some(keep);
        app.ask_ai.set_inline(Some(keep));
        app.redraw_page();
        let mut ans = String::new();
        for _ in 0..200 {
            ans.push_str("很长的一段回答，会把卡片撑到上限。\n");
        }
        app.ask_ai.seed(ai::Mode::Select, "q", &ans);
        app.d1_paint_card();
        assert_eq!(
            app.selai.card.map(|r| r.3),
            Some(rmdev::panels::selectai::CARD_H),
            "长回答要把卡片撑到上限"
        );
        app.d1_close_card();
        assert!(!app.ask_ai.open);
        assert!(app.selai.card.is_none());

        let after = d1_dump(&app.fb);
        let (w, _h) = (app.fb.width as usize, app.fb.height as usize);
        // 卡片最大矩形 + 外扩 2 px 都要和"从未开过"一致
        let (cx, cy, cw, ch) = rmdev::panels::selectai::card_rect();
        let mut diff = 0;
        let mut first = None;
        for y in (cy - 2).max(0)..(cy + ch + 2).min(app.fb.height) {
            for x in (cx - 2).max(0)..(cx + cw + 2).min(app.fb.width) {
                let i = y as usize * w + x as usize;
                if !d1_is_d6_zone(x, y) && never[i] != after[i] {
                    diff += 1;
                    if first.is_none() {
                        first = Some((x, y, never[i], after[i]));
                    }
                }
            }
        }
        assert_eq!(diff, 0, "关卡片后卡片区域没复原（{diff} px，第一个 {first:?}）");
    }

    /// BUG 3：**笔点 Close = 关卡片**，而且这一下不落墨。
    #[test]
    fn d1_pen_closes_the_card() {
        let mut app = App::new(Fb::open_mem());
        let nb = d1_test_nb();
        app.screen = Screen::Notebook(Box::new(nb));
        app.ask_ai.show_pending();
        let keep = rmdev::panels::selectai::card_rect_for(app.ask_ai.inline_needed_height());
        app.selai.card = Some(keep);
        app.ask_ai.set_inline(Some(keep));
        app.redraw_page();
        let (cx, cy, cw, ch) = rmdev::panels::selectai::card_close_rect();
        app.handle(d1_pen_at((cx + cw / 2) as f32, (cy + ch / 2) as f32));
        app.handle(InputEvent::Pen(PenEvent::Up));
        assert!(!app.ask_ai.open, "笔点 Close 要关卡片");
        assert_eq!(
            match &app.screen {
                Screen::Notebook(nb) => nb.page.total_lines(),
                _ => 0,
            },
            0,
            "点 Close 不能顺手写一笔"
        );
    }

    /// 笔点滚动条热区 → 拖 = 滚卡片；卡片中部 → 照写。
    #[test]
    fn d1_pen_scrolls_from_the_scrollbar_hot_zone() {
        let mut app = App::new(Fb::open_mem());
        let nb = d1_test_nb();
        app.screen = Screen::Notebook(Box::new(nb));
        let mut ans = String::new();
        for _ in 0..120 {
            ans.push_str("够长的一行回答，用来把卡片撑到能滚。\n");
        }
        app.ask_ai.set_inline(Some(rmdev::panels::selectai::card_rect()));
        app.ask_ai.seed(ai::Mode::Select, "q", &ans);
        app.selai.card = Some(rmdev::panels::selectai::card_rect());
        assert!(app.ask_ai.max_scroll_pub() > 0);

        let (sx, sy, sw, sh) = rmdev::panels::selectai::card_scroll_rect();
        let before = app.ask_ai.scroll;
        app.handle(d1_pen_at((sx + sw / 2) as f32, (sy + sh / 2) as f32));
        app.handle(d1_pen_move(
            (sx + sw / 2) as f32,
            (sy + sh / 2) as f32 - 120.0,
        ));
        app.handle(InputEvent::Pen(PenEvent::Up));
        assert!(app.ask_ai.scroll > before, "笔拖滚动条要把内容滚下去");
        assert_eq!(
            match &app.screen {
                Screen::Notebook(nb) => nb.page.total_lines(),
                _ => 0,
            },
            0,
            "拖滚动条不能落墨"
        );
    }

    /// 零笔迹的印刷页（PDF 底图、一个字都没写）也必须拿得到图 ——
    /// 用户点名的重点：原版选区在这种页面上什么都选不到，我们这个不依赖笔画。
    #[test]
    fn d1_printed_page_without_ink_still_sends_something() {
        let mut app = App::new(Fb::open_mem());
        let nb = d1_test_nb();
        app.screen = Screen::Notebook(Box::new(nb));
        assert_eq!(
            match &app.screen {
                Screen::Notebook(nb) => nb.page.total_lines(),
                _ => 0,
            },
            0,
            "零笔迹"
        );
        // 印刷底图：一整页浅灰 + 几道"字行"
        let (w, h) = (app.fb.width as usize, app.fb.height as usize);
        let mut bg = vec![255u8; w * h];
        for y in 0..h {
            if y % 40 < 12 {
                for x in 200..1200 {
                    bg[y * w + x] = 60;
                }
            }
        }
        app.pdf_bg = Some(bg);
        let gray = app.d1_page_gray();
        let dark = (0..h).step_by(97).filter(|&y| gray[y * w + 700] < 200).count();
        assert!(dark > 0, "整页图里要有印刷内容（不依赖任何笔迹）");
        // 框一块 → 裁图非空、尺寸对
        let r = rmdev::panels::selectai::Rect::new(260, 300, 1150, 1080);
        let (cw, ch, cut) = rmdev::panels::selectai::crop(&gray, w, h, r);
        assert_eq!((cw, ch), (890, 780));
        assert_eq!(cut.len(), cw * ch);
        assert!(cut.iter().any(|v| *v < 200), "裁图里要有内容");
    }

    /// 笔点浮层第一行（Rectangle）= 选中工具 1，且这一下不落墨。
    #[test]
    fn d1_popover_rect_item_is_clickable_with_the_pen() {
        let mut app = App::new(Fb::open_mem());
        let nb = d1_test_nb();
        app.screen = Screen::Notebook(Box::new(nb));
        app.on_chrome(Msg::AiSelect);
        assert!(app.selai.popover);
        let (px, py, _, _) = rmdev::panels::selectai::popover_rect();
        app.handle(d1_pen_at(px as f32 + 40.0, py as f32 + 40.0));
        assert!(!app.selai.popover, "点第一行 = 收起浮层");
        assert!(app.selai.on, "工具还开着，等着拖框");
        assert_eq!(
            app.selai.stage,
            rmdev::panels::selectai::Stage::Idle
        );
        assert_eq!(
            match &app.screen {
                Screen::Notebook(nb) => nb.page.total_lines(),
                _ => 0,
            },
            0,
            "点浮层不落墨"
        );
    }

    /// 手指在卡片里 = 滚动（卡片吃）；卡片外的触摸照常给页面。
    /// **笔**不走这条（`d1_pen` 直接放行到落墨）。
    #[test]
    fn d1_finger_scrolls_the_card_only_inside_it() {
        let mut app = App::new(Fb::open_mem());
        let nb = d1_test_nb();
        app.screen = Screen::Notebook(Box::new(nb));
        app.ask_ai.set_inline(Some(rmdev::panels::selectai::card_rect()));
        // 够长的回答 → 卡片里能滚
        let mut ans = String::new();
        for _ in 0..80 {
            ans.push_str("这是一行够长的回答，用来把卡片撑到可滚动。\n");
        }
        app.ask_ai.seed(ai::Mode::Select, "q", &ans);
        app.selai.card = Some(rmdev::panels::selectai::card_rect());
        assert!(app.ask_ai.max_scroll_pub() > 0, "假数据要够长");

        let (cx, cy, cw, ch) = rmdev::panels::selectai::card_rect();
        // 卡片里按下 + 往上拖 → 内容滚动
        assert!(app.d1_touch(&TouchEvent::Down {
            x: (cx + cw / 2) as f32,
            y: (cy + ch / 2) as f32
        }));
        let before = app.ask_ai.scroll;
        assert!(app.d1_touch(&TouchEvent::Move {
            x: (cx + cw / 2) as f32,
            y: (cy + ch / 2 - 120) as f32
        }));
        assert!(app.ask_ai.scroll > before, "手指上滑要往下滚");
        assert!(app.d1_touch(&TouchEvent::Up));
        // 卡片外：不吃
        assert!(!app.d1_touch(&TouchEvent::Down { x: 700.0, y: 1600.0 }));
        // Close：卡片右上角
        assert!(app.d1_touch(&TouchEvent::Down {
            x: (cx + cw - 40) as f32,
            y: (cy + 20) as f32
        }));
        assert!(!app.ask_ai.open, "点 Close 要关卡片");
    }

    /// 反方向拖（右下 → 左上）也要能用：同一个矩形、同一个结果。
    #[test]
    fn d1_reverse_drag_works_too() {
        let mut app = App::new(Fb::open_mem());
        let nb = d1_test_nb();
        app.screen = Screen::Notebook(Box::new(nb));
        app.selai.on = true;
        app.selai.popover = false;
        app.selai.doc = "d1-test-doc".into();
        app.handle(d1_pen_at(900.0, 1200.0));
        app.handle(d1_pen_move(600.0, 800.0));
        app.handle(d1_pen_move(300.0, 400.0));
        app.handle(InputEvent::Pen(PenEvent::Up));
        assert_eq!(
            app.selai.rect.map(|r| r.arr()),
            Some([300, 400, 900, 1200]),
            "右下 → 左上要归一化成有序矩形"
        );
        assert_eq!(app.selai.stage, rmdev::panels::selectai::Stage::Ask);
    }

    /// 拖得太小 = 误触：回工具态，不出键盘，不落墨。
    #[test]
    fn d1_tiny_drag_is_ignored() {
        let mut app = App::new(Fb::open_mem());
        let nb = d1_test_nb();
        app.screen = Screen::Notebook(Box::new(nb));
        app.selai.on = true;
        app.selai.popover = false;
        app.selai.doc = "d1-test-doc".into();
        app.handle(d1_pen_at(300.0, 400.0));
        app.handle(d1_pen_move(320.0, 420.0));
        app.handle(InputEvent::Pen(PenEvent::Up));
        assert_eq!(app.selai.stage, rmdev::panels::selectai::Stage::Idle);
        assert!(app.selai.kb.is_none());
        assert!(app.selai.rect.is_none());
        assert_eq!(
            match &app.screen {
                Screen::Notebook(nb) => nb.page.total_lines(),
                _ => 0,
            },
            0
        );
    }
    // === D1 end ===

    // === C18 begin ===
    // 手掌误触：笔优先的状态闸门（单测直接喂状态机，不碰设备）

    /// 一本空白笔记本（字段和 `d6_test_nb` 同一套，这里只要一个能放笔迹的画布）。
    fn c18_nb() -> Notebook {
        Notebook {
            uuid: "c18-test-doc".into(),
            name: "C18 test".into(),
            page: RmPage::default(),
            page_index: 0,
            page_count: 1,
            overview: false,
            page_thumbs: None,
            swipe: None,
            chrome: Chrome::default(),
            active: None,
            exit_taps: 0,
            dirty: false,
            tap_consumed: false,
            texts: Vec::new(),
            p11_styles: Vec::new(),
            text_edit: None,
            pending_text: None,
            sel: None,
            lasso: None,
            drag_sel: None,
            pending_paste: None,
            undo: Vec::new(),
            redo: Vec::new(),
            tag_edit: None,
            layer_rename: None,
            erase_rect: None,
            view: PageView::identity(),
            last_pen: 0,
            landscape_back: None,
        }
    }

    /// 一笔正在写（笔按在屏上、还没抬）。
    fn c18_active_stroke() -> Active {
        let tool = Tool::Ballpoint;
        let mut builder =
            StrokeBuilder::new(tool, tool_color(tool, rmdev::pen::line::Color::Black), 2.0, 1.0);
        builder.begin(rmdev::pen::InputPoint {
            x: 300.0,
            y: 400.0,
            pressure: 0.5,
            tilt_x: 0.0,
            tilt_y: 0.0,
            rubber: false,
        });
        Active {
            builder,
            prev: (300.0, 400.0, 1.0),
            moved: false,
            tap_x: 300.0,
            tap_y: 400.0,
            stroke_erase: false,
            erase: Vec::new(),
        }
    }

    fn c18_app(with_stroke: bool) -> App {
        let mut app = App::new(Fb::open_mem());
        let mut nb = c18_nb();
        if with_stroke {
            nb.active = Some(c18_active_stroke());
        }
        app.screen = Screen::Notebook(Box::new(nb));
        app
    }

    /// 笔**停着不动**按在屏上时，`pen_ts` 早过期了，但笔仍然攥着输入权。
    ///
    /// 这就是手掌能溜进来的那个洞：只看"笔刚动过 250 ms"的话，描线/思考
    /// 时的时间窗一过，手掌就被当成手指了。
    #[test]
    fn c18_stroke_in_progress_owns_the_input_even_after_250ms_of_silence() {
        let mut app = c18_app(true);
        app.pen_ts = std::time::Instant::now() - Duration::from_secs(5);
        assert!(c18_pen_owns_input(&app), "笔正按在屏上：静默多久都只认笔");
        // 抬笔之后（active 没了）才算放开
        if let Screen::Notebook(nb) = &mut app.screen {
            nb.active = None;
        }
        assert!(!c18_pen_owns_input(&app), "抬笔 5 s 之后触摸该恢复");

        // 原来的时间窗照旧生效（抬笔 250 ms 内的余震也算笔的）
        let mut app = c18_app(false);
        app.pen_ts = std::time::Instant::now();
        assert!(c18_pen_owns_input(&app), "刚动过的笔仍然优先");
        app.pen_ts = std::time::Instant::now() - Duration::from_secs(5);
        assert!(!c18_pen_owns_input(&app));
    }

    /// 闸门：笔在写时触点被丢干净，三个识别器都被 `reset()`。
    #[test]
    fn c18_gate_drops_contacts_and_resets_the_recognizers() {
        let mut app = c18_app(true);
        app.pen_ts = std::time::Instant::now() - Duration::from_secs(5);
        let mut tracker = rmdev::viewport::Tracker::new();
        let mut three = rmdev::input::touch::ThreeFinger::new();
        let mut two = TwoFinger::new();
        let mut fingers = vec![(10i64, 900.0f32, 1200.0f32), (11, 940.0, 1260.0)];
        assert!(c18_gesture_gate(
            &app,
            &mut fingers,
            &mut tracker,
            &mut three,
            &mut two
        ));
        assert!(fingers.is_empty(), "触点必须被丢干净");
        assert_eq!(tracker.fingers(), 0);

        // 笔没在写：闸门不碰任何东西
        let mut app = c18_app(false);
        app.pen_ts = std::time::Instant::now() - Duration::from_secs(5);
        let mut fingers = vec![(10i64, 900.0f32, 1200.0f32), (11, 940.0, 1260.0)];
        assert!(!c18_gesture_gate(
            &app,
            &mut fingers,
            &mut tracker,
            &mut three,
            &mut two
        ));
        assert_eq!(fingers.len(), 2, "笔闲着时手势照旧");
    }

    /// **根因回归**：手掌先起了一轮"双指点按"，笔在这中间落下来，
    /// 手掌一抬——旧代码会补一次撤销（把用户刚写的那一笔删掉）。
    #[test]
    fn c18_pen_landing_kills_a_half_finished_palm_round() {
        let mut three = rmdev::input::touch::ThreeFinger::new();
        let mut two = TwoFinger::new();
        // 0 ms：手掌的 2 个触点按上，双指那一轮开始记
        let palm = [(70i64, 1000.0f32, 1500.0f32), (71, 1040.0, 1560.0)];
        assert_eq!(
            rmdev::input::touch::classify(&palm, &mut three, &mut two, 0.0),
            (rmdev::input::touch::ThreeFingerAct::None, TwoFingerAct::None)
        );
        // 120 ms：笔落下来写字（闸门触发，这一轮作废）
        let mut app = c18_app(true);
        app.pen_ts = std::time::Instant::now();
        let mut tracker = rmdev::viewport::Tracker::new();
        let mut fingers = palm.to_vec();
        assert!(c18_gesture_gate(
            &app,
            &mut fingers,
            &mut tracker,
            &mut three,
            &mut two
        ));
        // 闸门把这一轮的手掌触点全丢了（`fingers.clear()`）——这是"作废"的具体做法，
        // 所以下面用原始的 `palm` 只抬走一根去问识别器：识别器已被 `reset()`，
        // 单指按 130 ms 不该再结算成撤销。
        assert!(
            fingers.len() < 2,
            "闸门应该把这一轮的手掌触点丢掉，实际还剩 {}",
            fingers.len()
        );
        assert_eq!(
            rmdev::input::touch::classify(&palm[..1], &mut three, &mut two, 130.0),
            (rmdev::input::touch::ThreeFingerAct::None, TwoFingerAct::None),
            "笔落下以后，手掌那一轮不许再结算成撤销"
        );
        // 对照：没有闸门时这一轮确实会给 Undo —— 证明上面那个 None 是闸门的功劳
        let mut three2 = rmdev::input::touch::ThreeFinger::new();
        let mut two2 = TwoFinger::new();
        rmdev::input::touch::classify(&palm, &mut three2, &mut two2, 0.0);
        assert_eq!(
            rmdev::input::touch::classify(&palm[..1], &mut three2, &mut two2, 130.0),
            (rmdev::input::touch::ThreeFingerAct::None, TwoFingerAct::Undo)
        );
    }
    // === C18 end ===
}
