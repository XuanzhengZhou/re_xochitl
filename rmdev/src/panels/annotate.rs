//! AI 批注写回（C12）：把服务器产的 `annotate` ops 画进页面里**新的 `AI` 图层**，
//! 用户自己的笔迹一个字节都不碰。
//!
//! # 协议
//!
//! ```text
//! {"kind":"annotate","page":3,"ops":[
//!   {"op":"text","x":120,"y":300,"text":"这里漏了负号","size":30},
//!   {"op":"box","x0":100,"y0":260,"x1":420,"y1":340},
//!   {"op":"stroke","pts":[[100,260],[420,260]],"w":2}
//! ]}
//! ```
//!
//! * 坐标系 = **设备像素**：1404×1872，原点左上，y 向下。`text` 的 `(x,y)` 是
//!   基线左端；`size` 是字号（缺省 30，夹在 8..200）；`stroke.w` 是笔宽（缺省 2）。
//! * 容错：`box` 也认 `rect:[x0,y0,x1,y1]`，`stroke` 也认 `points` 和 `width`
//!   （`docs/CLOUD.md` 写的是这套字段名，服务器两边都可能发）；
//!   不认识的字段一律忽略。
//! * 坏 op 不连坐：未知 `op`、缺字段、空 text、NaN/Inf 都只丢**那一条**，
//!   记进 [`Stats::dropped`]；整份 JSON 坏掉（不是对象 / ops 不是数组 /
//!   kind 不是 annotate）才返回 `Err`。
//! * 越界坐标不是丢，是 **clamp 进 1404×1872** 并记进 [`Stats::clamped`] ——
//!   模型写 `x:1500` 时画在右边框上，比整条不见有用。
//!
//! # 落盘：只写 `AI` 层
//!
//! [`apply`] 是**纯内存**的：先把 ops 全转成 `Line`（`doc::rm::Layer` 只装笔迹），
//! 再整层替换 `AI` 层的 `lines`。落盘只有 [`apply_at`]：读原 `.rm` → parse →
//! [`apply`] → `RmPage::serialize()` → 同目录临时文件 + `rename` 原子替换。
//! 原有图层的 `lines` 不经过任何改写路径，所以"原层不动"。
//!
//! 幂等的做法是**整层重写**：同一批 ops 再 apply 一次是覆盖，不是叠第二份；
//! `AI` 层按名字找，已有就复用（位置/顺序不动），没有才新加一层。
//! 一页最多 5 层，已经满了且没有 `AI` 层时 `apply` 返回 `Err`，页面保持不变 ——
//! 宁可这次批注失败，也不能拿用户某一层去写（那会毁掉手写）。
//!
//! [`clear`] 删掉 `AI` 层；`AI` 是唯一一层时只能清空它的笔迹（不能把页面删成 0 层）。
//!
//! # 16 级灰里怎么认出 AI 的
//!
//! e-ink 没有颜色，靠三条一起看：
//!
//! 1. **图层名 `AI`**：图层面板（`main.rs::paint_layers_panel`，纯按 `layers` 画）
//!    自动多一行 `AI`，眼睛图标能隐藏、垃圾桶能删；
//! 2. **浅灰**：AI 的每一笔都是 `Color::Gray`（`Canvas::ink_tone` = 110/255，
//!    用户黑笔是 255）。面板对中间灰走网点（`Canvas::ink_capsule` 的 Bayer），
//!    所以屏上看到的是浅灰网点、手写是实心黑，密度差一倍多；
//! 3. **虚线框 + 定宽细线**：`box` 画的是一圈 12 px 实 / 8 px 空的虚线
//!    （用户不会画出正好 12/8 的直角虚线框），`text`/框的线宽固定 1.5 px，
//!    只有 `stroke` 用协议里的 `w`。
//!
//! # 已知限制
//!
//! * `text` 要字体：把字形**光栅化成扫描线**写成笔迹（`Layer` 里没有文字项）。
//!   字体找不到（`RMDEV_ANNOT_FONT` / 设备 NotoSansSC / 本机 `~/re_xochitl/fonts`）
//!   时文字 op 丢弃并计数，框和线照画 —— 就是"批注有记号没文字"。
//! * **不自动折行**：一行画到页面右边界就停（要分行就发多条 `text` op）。
//! * 汉字是 1 px 扫描线拼的实心字，被面板抖成浅灰网点；实测 34 px 一行
//!   17 个字 = 399 根线、`.rm` 涨 45.6 KB，大段批注会明显撑大页面文件。
//! * `RmPage::serialize()` 不写 `RootText`（打字文字层）。[`apply_at`] 落盘前
//!   把原文件的文字层读出来再补回去，和 `main.rs` 的 P11 修复同一个办法。
//! * 用户自己把某层改名成 `AI` 时按名字复用会被当成 AI 层（协议就是这么约定的）；
//!   `AI` 层被用户改名后，下一次 apply 会再建一个新的 `AI` 层。

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ab_glyph::{Font, FontVec, ScaleFont};
use serde_json::Value;

use crate::doc::rm::{Layer, RmPage};
use crate::doc::rmtext;
use crate::pen::{Color, Line, Point, Tool};
use crate::viewport::{PAGE_H, PAGE_W};

/// AI 图层的名字（图层面板和落盘都按它找）。
pub const AI_LAYER: &str = "AI";

/// 协议坐标 → `Line.points` 场景坐标：页面 x 居中于 0（见 `viewport.rs` 头注释），
/// 所以 x 要减去半个页宽，y 不变。
const PAGE_CX: f32 = PAGE_W * 0.5;

/// AI 笔迹统一用浅灰（`Canvas::ink_tone(Color::Gray)` = 110，用户黑笔是 255）。
pub const AI_COLOR: Color = Color::Gray;
/// 框/文字的线宽（设备像素）。`stroke` 用协议里的 `w`。
const AI_W: f32 = 1.5;
/// 文字 op 最多这么多字符，多的截断。
pub const MAX_TEXT_CHARS: usize = 200;
/// 一笔 stroke 最多这么多点，多的截断（防一份坏 JSON 把一页撑成几十 MB）。
pub const MAX_STROKE_PTS: usize = 4096;
/// 虚线：实 12 px、空 8 px。
const DASH_ON: f32 = 12.0;
const DASH_OFF: f32 = 8.0;
const DASH_PERIOD: f32 = DASH_ON + DASH_OFF;
/// 字号 / 笔宽的允许区间，超了 clamp。
const SIZE_MIN: f32 = 8.0;
const SIZE_MAX: f32 = 200.0;
const W_MIN: f32 = 0.5;
const W_MAX: f32 = 20.0;
/// `text.size` 缺省值。
const DEFAULT_SIZE: f32 = 30.0;
/// `stroke.w` 缺省值。
const DEFAULT_W: f32 = 2.0;
/// 一页最多 5 层（`RmPage::add_layer` 的硬限制）。
const MAX_LAYERS: usize = 5;

// ---------------------------------------------------------------- 协议

/// 一条批注 op。
///
/// [`Op::Bad`] 是"解析时就不合法"的占位：不连坐整批，`apply` 里记一次
/// [`Stats::dropped`] 就跳过。
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Text {
        x: f32,
        y: f32,
        size: f32,
        text: String,
    },
    Box {
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
    },
    Stroke {
        pts: Vec<(f32, f32)>,
        w: f32,
    },
    Bad(String),
}

/// 一整条 `annotate` 事件。`page` 只做记录（翻到哪一页由调用方决定）。
#[derive(Clone, Debug, PartialEq)]
pub struct Batch {
    pub page: Option<usize>,
    pub ops: Vec<Op>,
}

impl Batch {
    /// 合法 op 的条数（`Bad` 不算）。
    pub fn valid_len(&self) -> usize {
        self.ops.iter().filter(|o| !matches!(o, Op::Bad(_))).count()
    }
}

/// 解析一条 `annotate` 事件。整份 JSON 坏掉才 `Err`，坏 op 留在 [`Op::Bad`] 里。
pub fn parse_event(json: &str) -> Result<Batch, String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("JSON 解析失败：{e}"))?;
    let obj = v.as_object().ok_or("事件必须是 JSON 对象")?;
    if let Some(k) = obj.get("kind") {
        let k = k.as_str().ok_or("kind 必须是字符串")?;
        if k != "annotate" {
            return Err(format!("kind 不是 annotate（是 {k:?}）"));
        }
    }
    let page = match obj.get("page") {
        Some(p) => Some(p.as_u64().ok_or("page 必须是非负整数")? as usize),
        None => None,
    };
    let ops = obj
        .get("ops")
        .ok_or("缺 ops 字段")?
        .as_array()
        .ok_or("ops 必须是数组")?;
    Ok(Batch {
        page,
        ops: ops.iter().map(parse_op).collect(),
    })
}

fn num(obj: &serde_json::Map<String, Value>, key: &str) -> Option<f32> {
    obj.get(key).and_then(as_f32)
}

fn as_f32(v: &Value) -> Option<f32> {
    v.as_f64().map(|x| x as f32)
}

/// 有限数才算数：NaN/Inf 一律当坏 op（JSON 里写不出 NaN，但程序里能造）。
fn finite(v: f32) -> bool {
    v.is_finite()
}

fn parse_op(v: &Value) -> Op {
    let Some(obj) = v.as_object() else {
        return Op::Bad("op 不是 JSON 对象".into());
    };
    let Some(name) = obj.get("op").and_then(Value::as_str) else {
        return Op::Bad("缺 op 字段".into());
    };
    match name {
        "text" => {
            let (Some(x), Some(y)) = (num(obj, "x"), num(obj, "y")) else {
                return Op::Bad("text 缺 x/y（或不是数字）".into());
            };
            if !finite(x) || !finite(y) {
                return Op::Bad("text 坐标不是有限数".into());
            }
            let size = match obj.get("size") {
                None => DEFAULT_SIZE,
                Some(_) => match num(obj, "size") {
                    Some(s) if finite(s) => s,
                    _ => return Op::Bad("text.size 不是有限数".into()),
                },
            };
            let Some(text) = obj.get("text").and_then(Value::as_str) else {
                return Op::Bad("text 缺 text 字段（或不是字符串）".into());
            };
            if text.trim().is_empty() {
                return Op::Bad("text 是空串".into());
            }
            Op::Text {
                x,
                y,
                size,
                text: text.to_string(),
            }
        }
        "box" => {
            // 两种写法：四个字段，或者 rect:[x0,y0,x1,y1]
            let (x0, y0, x1, y1) = match obj.get("rect") {
                Some(r) => {
                    let Some([a, b, c, d]) = r.as_array().map(|a| a.as_slice()) else {
                        return Op::Bad("box.rect 不是 4 个数的数组".into());
                    };
                    let (Some(x0), Some(y0), Some(x1), Some(y1)) =
                        (as_f32(a), as_f32(b), as_f32(c), as_f32(d))
                    else {
                        return Op::Bad("box.rect 里有非数字".into());
                    };
                    (x0, y0, x1, y1)
                }
                None => {
                    let (Some(x0), Some(y0), Some(x1), Some(y1)) =
                        (num(obj, "x0"), num(obj, "y0"), num(obj, "x1"), num(obj, "y1"))
                    else {
                        return Op::Bad("box 缺 x0..y1（或不是数字）".into());
                    };
                    (x0, y0, x1, y1)
                }
            };
            if !finite(x0) || !finite(y0) || !finite(x1) || !finite(y1) {
                return Op::Bad("box 坐标不是有限数".into());
            }
            Op::Box { x0, y0, x1, y1 }
        }
        "stroke" => {
            let raw = match obj.get("pts").or_else(|| obj.get("points")) {
                Some(p) => p,
                None => return Op::Bad("stroke 缺 pts 字段".into()),
            };
            let Some(arr) = raw.as_array() else {
                return Op::Bad("stroke.pts 不是数组".into());
            };
            let mut pts = Vec::with_capacity(arr.len());
            for p in arr {
                let Some(pair) = p.as_array().filter(|a| a.len() == 2) else {
                    return Op::Bad("stroke.pts 里有一项不是 [x,y]".into());
                };
                let (Some(x), Some(y)) = (as_f32(&pair[0]), as_f32(&pair[1])) else {
                    return Op::Bad("stroke.pts 里有非数字".into());
                };
                if !finite(x) || !finite(y) {
                    return Op::Bad("stroke.pts 里有非有限数".into());
                }
                pts.push((x, y));
            }
            if pts.is_empty() {
                return Op::Bad("stroke.pts 是空的".into());
            }
            let w = match obj.get("w").or_else(|| obj.get("width")) {
                None => DEFAULT_W,
                Some(_) => match num(obj, "w").or_else(|| num(obj, "width")) {
                    Some(w) if finite(w) => w,
                    _ => return Op::Bad("stroke.w 不是有限数".into()),
                },
            };
            Op::Stroke { pts, w }
        }
        other => Op::Bad(format!("未知 op `{other}`")),
    }
}

// ---------------------------------------------------------------- 结果统计

/// 一批 ops 的处理结果（日志/服务器回执用）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// 写进 AI 层的 text op 数
    pub text: usize,
    /// 写进 AI 层的 box op 数
    pub boxes: usize,
    /// 写进 AI 层的 stroke op 数
    pub strokes: usize,
    /// 追加到 AI 层的 `Line` 总数
    pub lines: usize,
    /// 坐标/字号/笔宽越界被 clamp 的 op 数
    pub clamped: usize,
    /// 被截断的 op 数（超长 text / 超多点数的 stroke）
    pub truncated: usize,
    /// 丢弃的 op 数（坏 op、NaN、空 text、退化框、没字体…）
    pub dropped: usize,
}

impl Stats {
    /// 真正画出来的 op 数。
    pub fn applied(&self) -> usize {
        self.text + self.boxes + self.strokes
    }
}

// ---------------------------------------------------------------- 写进 AI 层

/// 这一页的 `AI` 层（只读）。
pub fn ai_layer(page: &RmPage) -> Option<&Layer> {
    page.layers.iter().find(|l| l.name == AI_LAYER)
}

pub fn ai_index(page: &RmPage) -> Option<usize> {
    page.layers.iter().position(|l| l.name == AI_LAYER)
}

fn clamp_px(v: f32, lo: f32, hi: f32) -> (f32, bool) {
    if v < lo {
        (lo, true)
    } else if v > hi {
        (hi, true)
    } else {
        (v, false)
    }
}

/// 设备像素 → 场景坐标的一个点。
fn pt(x: f32, y: f32, w: f32) -> Point {
    Point {
        x: x - PAGE_CX,
        y,
        speed: 0.0,
        dir: 0.0,
        width: w,
        pressure: 0.5,
    }
}

fn polyline(pts: &[(f32, f32)], w: f32) -> Line {
    let mut line = Line::new(Tool::Fineliner, AI_COLOR, 1.0);
    for &(x, y) in pts {
        line.push(pt(x, y, w));
    }
    line
}

/// 一圈虚线框（12 实 / 8 空，顺时针从左上角开始）。每条虚线段是一根 `Line`。
fn dashed_box(x0: f32, y0: f32, x1: f32, y1: f32) -> Vec<Line> {
    let (ax, ay) = (x0.min(x1), y0.min(y1));
    let (bx, by) = (x0.max(x1), y0.max(y1));
    let corners = [(ax, ay), (bx, ay), (bx, by), (ax, by), (ax, ay)];
    let mut out = Vec::new();
    let mut phase = 0.0f32; // 0..DASH_PERIOD
    for side in corners.windows(2) {
        let (p0, p1) = (side[0], side[1]);
        let (dx, dy) = (p1.0 - p0.0, p1.1 - p0.1);
        let len = (dx * dx + dy * dy).sqrt();
        if len <= 0.0 {
            continue;
        }
        let (ux, uy) = (dx / len, dy / len);
        let mut t = 0.0;
        while t < len - 1e-4 {
            let on = phase < DASH_ON;
            let remain = if on {
                DASH_ON - phase
            } else {
                DASH_PERIOD - phase
            };
            let seg = remain.min(len - t);
            if on {
                let a = (p0.0 + ux * t, p0.1 + uy * t);
                let b = (p0.0 + ux * (t + seg), p0.1 + uy * (t + seg));
                out.push(polyline(&[a, b], AI_W));
            }
            t += seg;
            phase = (phase + seg) % DASH_PERIOD;
        }
    }
    out
}

/// 把一批 ops 写进页面的 `AI` 层，返回统计。
///
/// * `AI` 层已存在 → **整层重写**（幂等）；位置/顺序不动，可见性置为 `true`
///   （新批注要看得见，用户想藏再点眼睛）；
/// * 不存在 → 追加到最上面一层；一页已满 5 层则 `Err`，**页面一个字节都不改**；
/// * `page.current` 不动：用户下一笔还落在原来那层，不会写进 AI 层；
/// * `ops` 为空且没有 `AI` 层 → 什么都不做（不白占一层）。
/// 把一批 op 转成墨迹线（**纯函数**，不碰 page）。[`apply`] / [`append`] 共用。
fn build(ops: &[Op]) -> Result<(Vec<Line>, Stats), String> {
    let mut lines: Vec<Line> = Vec::new();
    let mut st = Stats::default();
    for op in ops {
        match op {
            Op::Bad(_) => st.dropped += 1,
            Op::Text { x, y, size, text } => {
                // NaN/Inf：`clamp_px` 对 NaN 两头都不命中，必须在这里先拦。
                // 坏 op 只丢这一条（Op 可能不经过 parse_event，直接从代码里构造）。
                if !(*x).is_finite() || !(*y).is_finite() || !(*size).is_finite() {
                    st.dropped += 1;
                    continue;
                }
                let (x, cx) = clamp_px(*x, 0.0, PAGE_W);
                let (y, cy) = clamp_px(*y, 0.0, PAGE_H);
                let (size, cs) = clamp_px(*size, SIZE_MIN, SIZE_MAX);
                if cx || cy || cs {
                    st.clamped += 1;
                }
                // 原点被 clamp 到右/下边界时一个字都放不下（基线已经在线外）
                if x >= PAGE_W || y >= PAGE_H {
                    st.dropped += 1;
                    continue;
                }
                let chars = text.chars().count();
                let body: String = text.chars().take(MAX_TEXT_CHARS).collect();
                if chars > MAX_TEXT_CHARS {
                    st.truncated += 1;
                }
                match text_lines(&body, x, y, size) {
                    Some((v, clipped)) => {
                        st.text += 1;
                        if clipped {
                            st.truncated += 1;
                        }
                        st.lines += v.len();
                        lines.extend(v);
                    }
                    // 没有字体：记号（框/线）照画，文字只能丢
                    None => st.dropped += 1,
                }
            }
            Op::Box { x0, y0, x1, y1 } => {
                if ![*x0, *y0, *x1, *y1].iter().all(|v| v.is_finite()) {
                    st.dropped += 1;
                    continue;
                }
                let mut any = false;
                let (ax, c0) = clamp_px(*x0, 0.0, PAGE_W);
                let (ay, c1) = clamp_px(*y0, 0.0, PAGE_H);
                let (bx, c2) = clamp_px(*x1, 0.0, PAGE_W);
                let (by, c3) = clamp_px(*y1, 0.0, PAGE_H);
                any |= c0 || c1 || c2 || c3;
                if any {
                    st.clamped += 1;
                }
                if (bx - ax).abs() < 0.5 && (by - ay).abs() < 0.5 {
                    st.dropped += 1;
                    continue;
                }
                let v = dashed_box(ax, ay, bx, by);
                if v.is_empty() {
                    st.dropped += 1;
                } else {
                    st.boxes += 1;
                    st.lines += v.len();
                    lines.extend(v);
                }
            }
            Op::Stroke { pts, w } => {
                if !finite(*w) {
                    st.dropped += 1;
                    continue;
                }
                let mut clamped = false;
                let mut norm: Vec<(f32, f32)> = Vec::with_capacity(pts.len().min(MAX_STROKE_PTS));
                let mut bad = false;
                for &(x, y) in pts.iter().take(MAX_STROKE_PTS) {
                    if !finite(x) || !finite(y) {
                        bad = true;
                        break;
                    }
                    let (x, cx) = clamp_px(x, 0.0, PAGE_W);
                    let (y, cy) = clamp_px(y, 0.0, PAGE_H);
                    clamped |= cx || cy;
                    norm.push((x, y));
                }
                if bad {
                    st.dropped += 1;
                    continue;
                }
                if pts.len() > MAX_STROKE_PTS {
                    st.truncated += 1;
                }
                let (w, cw) = clamp_px(*w, W_MIN, W_MAX);
                if clamped || cw {
                    st.clamped += 1;
                }
                st.strokes += 1;
                st.lines += 1;
                lines.push(polyline(&norm, w));
            }
        }
    }
    Ok((lines, st))
}

pub fn apply(page: &mut RmPage, ops: &[Op]) -> Result<Stats, String> {
    // 1. 先把 op 全转成线（这步不改 page）
    let (lines, st) = build(ops)?;

    // 2. 找/建 AI 层。没有要写的东西又没 AI 层：连层都不建。
    let idx = match ai_index(page) {
        Some(i) => i,
        None => {
            if lines.is_empty() {
                return Ok(st);
            }
            if page.layers.len() >= MAX_LAYERS {
                return Err(format!(
                    "一页最多 {MAX_LAYERS} 层，已经是 {} 层且没有 `{AI_LAYER}` 层；\
                     这次批注没有落盘（不能拿用户图层顶替）",
                    page.layers.len()
                ));
            }
            let keep = page.current;
            page.layers.push(Layer {
                name: AI_LAYER.to_string(),
                visible: true,
                lines: Vec::new(),
            });
            page.current = keep;
            page.layers.len() - 1
        }
    };
    // 3. 整层重写（幂等的关键）
    page.layers[idx].lines = lines;
    page.layers[idx].visible = true;
    Ok(st)
}

/// 往 `AI` 层**追加**一批 ops（不清空原有内容）。
///
/// [`apply`] 是"整层重写"——那是给"批改这一页"用的：每次批改的结果都是这一页
/// 的完整答案。但 Live Note 是**一问接一问**，第 2 次回答不该把第 1 次的抹掉，
/// 所以另开一个追加口子。
///
/// 层数上限、坏 op、clamp 规则全和 [`apply`] 一样（共用同一段转换代码）。
pub fn append(page: &mut RmPage, ops: &[Op]) -> Result<Stats, String> {
    let (lines, st) = build(ops)?;
    if lines.is_empty() {
        return Ok(st);
    }
    let idx = match ai_index(page) {
        Some(i) => i,
        None => {
            if page.layers.len() >= MAX_LAYERS {
                return Err(format!(
                    "一页最多 {MAX_LAYERS} 层，已经是 {} 层且没有 `{AI_LAYER}` 层；\
                     这次的回答没有落盘（不能拿用户图层顶替）",
                    page.layers.len()
                ));
            }
            let keep = page.current;
            page.layers.push(Layer {
                name: AI_LAYER.to_string(),
                visible: true,
                lines: Vec::new(),
            });
            page.current = keep;
            page.layers.len() - 1
        }
    };
    page.layers[idx].lines.extend(lines);
    page.layers[idx].visible = true;
    Ok(st)
}

/// 从磁盘上抹掉某一页的 `AI` 层，返回删掉的墨迹条数。
///
/// 和 [`apply_at`] 同一套落盘规矩（读 → parse → 改 → `serialize` → 补回
/// `RootText` → 原子替换），只是这次是把 `AI` 层整个拿掉、**用户笔迹一条不动**。
/// 给 `rmdev --clear-ai <uuid>:<page>` 用：那种"一页被 AI 写了几千条、界面上
/// 点不动"的页，只能从盘上救。
pub fn clear_at(path: &Path) -> Result<usize, String> {
    let raw = fs::read(path).map_err(|e| format!("读 {} 失败：{e}", path.display()))?;
    let mut page =
        RmPage::parse(&raw).map_err(|e| format!("解析 {} 失败：{e}", path.display()))?;
    let texts = rmtext::parse_texts(&raw).unwrap_or_default();
    let n = ai_layer(&page).map(|l| l.lines.len()).unwrap_or(0);
    if n == 0 {
        return Ok(0);
    }
    if !clear(&mut page) {
        return Ok(0);
    }
    let mut out = page.serialize();
    if !texts.is_empty() {
        out = rmtext::serialize_texts(&out, &texts)
            .map_err(|e| format!("{} 补回文字层失败：{e}", path.display()))?;
    }
    write_atomic(path, &out).map_err(|e| format!("写 {} 失败：{e}", path.display()))?;
    Ok(n)
}

/// 抹掉 `AI` 层：删层，`AI` 是唯一一层时只清空笔迹（页面不能 0 层）。
/// 本来就没有 `AI` 层 → `false`，不改任何东西。
pub fn clear(page: &mut RmPage) -> bool {
    let Some(i) = ai_index(page) else {
        return false;
    };
    if page.layers.len() > 1 {
        page.delete_layer(i)
    } else {
        page.layers[i].lines.clear();
        page.layers[i].visible = true;
        true
    }
}

// ---------------------------------------------------------------- 落盘

/// 同目录临时文件 + `rename`：写到一半断电也不会留下半份 `.rm`。
fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "page.rm".to_string());
    let tmp: PathBuf = dir.join(format!(".{name}.d2.tmp"));
    let r = (|| -> io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

/// 把一批 ops 写回磁盘上的一个 `.rm`（**唯一**允许的落盘方式）。
///
/// 读原字节 → `RmPage::parse` → [`apply`] → `serialize()` → 原子替换。
/// 原文件里的 `RootText`（打字文字层）会被原样补回去：`serialize()` 不写它，
/// 不补的话"批改一次，这一页打的字就没了"。
///
/// 一条 op 都没画出来（全是坏 op / 没字体）时**直接返回**，连整页重写都省了。
/// 换掉文件之后不用手动清缓存：`Library::read_page` 的解析缓存按
/// `(文件长度, mtime)` 校验（`doc/rm.rs::ParsedPages::get`），下次读会自动重解析。
pub fn apply_at(path: &Path, ops: &[Op]) -> Result<Stats, String> {
    let raw = fs::read(path).map_err(|e| format!("读 {} 失败：{e}", path.display()))?;
    let mut page =
        RmPage::parse(&raw).map_err(|e| format!("解析 {} 失败：{e}", path.display()))?;
    // 文字层先收下来；读失败只影响"打字文字要不要补"，不影响墨迹
    let texts = rmtext::parse_texts(&raw).unwrap_or_default();
    let had_ai = ai_index(&page).is_some();
    let st = apply(&mut page, ops)?;
    if !had_ai && ai_index(&page).is_none() {
        // 一条都没画出来（全是坏 op / 没字体）：页面一个字节都不动，
        // 连整页重写都省了 —— serialize 是照我们的块序重排的，能不写就不写。
        return Ok(st);
    }
    let mut out = page.serialize();
    if !texts.is_empty() {
        out = rmtext::serialize_texts(&out, &texts)
            .map_err(|e| format!("{} 补回文字层失败：{e}", path.display()))?;
    }
    write_atomic(path, &out).map_err(|e| format!("写 {} 失败：{e}", path.display()))?;
    Ok(st)
}

// ---------------------------------------------------------------- 文字 → 折线

/// 能不能画出 `text` op（字体找得到）。只影响显示，不影响协议解析。
pub fn text_renderable() -> bool {
    font().is_some()
}

fn font() -> Option<&'static FontVec> {
    static FONT: OnceLock<Option<FontVec>> = OnceLock::new();
    FONT.get_or_init(load_font).as_ref()
}

/// 字体候选和设备上的 `ui::text::Text` 一致（`RMDEV_FONT` 那套）。
/// 汉字要 NotoSansSC；Maison Neue / Noto Sans 只有拉丁。
fn load_font() -> Option<FontVec> {
    let mut cands: Vec<String> = Vec::new();
    if let Ok(p) = std::env::var("RMDEV_ANNOT_FONT") {
        cands.push(p);
    }
    if let Ok(home) = std::env::var("HOME") {
        cands.push(format!("{home}/rm2_re/fonts/NotoSansSC.otf"));
        cands.push(format!("{home}/rm2_re/fonts/MaisonNeue-Book.ttf"));
    }
    for p in [
        "/home/root/.fonts/NotoSansSC.otf",
        "/home/root/.fonts/MaisonNeue-Book.ttf",
        "/usr/share/remarkable/webui/fonts/MaisonNeue-Book.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
    ] {
        cands.push(p.to_string());
    }
    for p in cands {
        let Ok(data) = fs::read(&p) else { continue };
        let Ok(f) = FontVec::try_from_vec(data) else {
            continue;
        };
        return Some(f);
    }
    None
}

/// 一行字 → 笔迹。没字体时返回 `None`。
///
/// `(x,y)` 是基线左端的设备像素坐标。`Layer` 里没有文字项，所以文字只能"写"成线：
/// 用 `ab_glyph` 把字形光栅化成覆盖率，再**逐行把有墨的连续像素连成一根 1 px 横线**。
/// 这样字是实心的（`Color::Gray` 落在纸上再被面板抖成浅灰网点），
/// 而不是空心轮廓 —— 30 px 的汉字钩边在灰度屏上根本认不出。代价是线多：
/// 一个 34 px 汉字约 20~25 根，一行 17 个字 ~ 400 根（每根 ~110 字节）。
/// 行距必须是 1 px：试过 2 px 行 + 2 px 笔宽（线数减半），1 px 的圆头会把
/// 汉字 1~2 px 的笔画间空隙糊死，字直接变成墨块。
///
/// 返回值第二个是"被右边界截断了吗"：**不自动折行**（要分行就发多条 `text` op），
/// 一行画到 `PAGE_W` 就停，免得批注正文跑到纸外面。
fn text_lines(text: &str, x: f32, y: f32, size: f32) -> Option<(Vec<Line>, bool)> {
    let f = font()?;
    let scaled = f.as_scaled(size);
    let mut pen = x;
    let mut out = Vec::new();
    let mut clipped = false;
    for ch in text.chars() {
        let gid = f.glyph_id(ch);
        let adv = scaled.h_advance(gid);
        if pen + adv > PAGE_W {
            clipped = true;
            break;
        }
        let glyph = ab_glyph::Glyph {
            id: gid,
            scale: ab_glyph::PxScale::from(size),
            position: ab_glyph::point(pen, y),
        };
        if let Some(og) = scaled.outline_glyph(glyph) {
            let b = og.px_bounds();
            let (w, h) = (b.width() as usize, b.height() as usize);
            if w > 0 && h > 0 {
                let mut cov = vec![0f32; w * h];
                og.draw(|gx, gy, c| {
                    let i = gy as usize * w + gx as usize;
                    if i < cov.len() {
                        cov[i] = c;
                    }
                });
                for row in 0..h {
                    let base = row * w;
                    let mut col = 0;
                    while col < w {
                        if cov[base + col] <= 0.5 {
                            col += 1;
                            continue;
                        }
                        let start = col;
                        while col < w && cov[base + col] > 0.5 {
                            col += 1;
                        }
                        // 线心压在像素行中心、两端各缩进半格：1 px 笔宽正好只盖这一行，
                        // 左右也不越界（否则每根线两端各糊 1 px，汉字空隙会闭合）
                        let cy = b.min.y + row as f32 + 0.5;
                        out.push(polyline(
                            &[
                                (b.min.x + start as f32 + 0.5, cy),
                                (b.min.x + col as f32 - 0.5, cy),
                            ],
                            1.0,
                        ));
                    }
                }
            }
        }
        pen += adv;
    }
    Some((out, clipped))
}

// ---------------------------------------------------------------- 出图（--render）

/// 假 ops：一条虚线框（框住用户写错的 "2"）、一行中文批注、一条下划线。
const DEMO_OPS: &str = r#"{"kind":"annotate","page":1,"ops":[
  {"op":"box","x0":346,"y0":396,"x1":416,"y1":486},
  {"op":"text","x":200,"y":566,"text":"这里漏了负号：应为 x = ±2","size":34},
  {"op":"stroke","pts":[[196,498],[420,498]],"w":3}
]}"#;

/// 一条手写笔迹：每段插几个带一点点起伏的点，看起来像手画的而不是矢量图。
fn hand_line(dst: &mut Vec<Line>, pts: &[(f32, f32)]) {
    let mut line = Line::new(Tool::Ballpoint, Color::Black, 3.0);
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let n = 8;
        for i in 0..n {
            let t = i as f32 / n as f32;
            let wobble = ((t * 9.0).sin()) * 0.8;
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let len = (dx * dx + dy * dy).sqrt().max(0.001);
            line.push(pt(
                a.0 + dx * t - dy / len * wobble,
                a.1 + dy * t + dx / len * wobble,
                3.0,
            ));
        }
    }
    if let Some(last) = pts.last() {
        line.push(pt(last.0, last.1, 3.0));
    }
    dst.push(line);
}

/// "原手写"：`x² = 4` 下面写 `x = 2`（漏了负根，正是 AI 要挑的错）。
fn demo_page() -> RmPage {
    let mut page = RmPage::default();
    page.layers.clear();
    let mut lines = Vec::new();
    // 第一行：x² = 4
    hand_line(&mut lines, &[(200.0, 300.0), (248.0, 340.0)]);
    hand_line(&mut lines, &[(248.0, 300.0), (200.0, 340.0)]);
    hand_line(&mut lines, &[(258.0, 268.0), (286.0, 268.0)]);
    hand_line(&mut lines, &[(286.0, 268.0), (258.0, 294.0)]);
    hand_line(&mut lines, &[(258.0, 294.0), (286.0, 294.0)]);
    hand_line(&mut lines, &[(312.0, 312.0), (358.0, 312.0)]);
    hand_line(&mut lines, &[(312.0, 326.0), (358.0, 326.0)]);
    hand_line(&mut lines, &[(402.0, 300.0), (378.0, 340.0)]);
    hand_line(&mut lines, &[(378.0, 340.0), (416.0, 340.0)]);
    hand_line(&mut lines, &[(400.0, 300.0), (400.0, 352.0)]);
    // 第二行：x = 2
    hand_line(&mut lines, &[(200.0, 430.0), (248.0, 470.0)]);
    hand_line(&mut lines, &[(248.0, 430.0), (200.0, 470.0)]);
    hand_line(&mut lines, &[(280.0, 443.0), (326.0, 443.0)]);
    hand_line(&mut lines, &[(280.0, 457.0), (326.0, 457.0)]);
    hand_line(&mut lines, &[(360.0, 412.0), (404.0, 412.0)]);
    hand_line(&mut lines, &[(404.0, 412.0), (360.0, 470.0)]);
    hand_line(&mut lines, &[(360.0, 470.0), (404.0, 470.0)]);
    page.layers.push(Layer {
        name: "Layer 1".to_string(),
        visible: true,
        lines,
    });
    page.current = 0;
    page
}

/// `--render annot:<spec>`：本机假数据出图（不读库、不连网）。
///
/// 画的是"用户手写 + AI 批注"的合成页：同一张画布上先画用户黑笔，再画 AI 的
/// 灰虚线框 / 中文批注 / 下划线 —— 和真机上 `redraw_page` 走的是同一条
/// `Canvas::ink_line`，所以出图能看见的，真机就画得出来。
pub fn render_demo(spec: &str, out: &str) {
    let mut page = demo_page();
    let batch = match parse_event(DEMO_OPS) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[render] annot 假 ops 解析失败：{e}");
            return;
        }
    };
    let st = match apply(&mut page, &batch.ops) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[render] annot apply 失败：{e}");
            return;
        }
    };
    let mut canvas = crate::canvas::Canvas::new(PAGE_W as usize, PAGE_H as usize);
    canvas.clear();
    for l in page.visible_lines() {
        canvas.ink_line(l, 1.0, (PAGE_CX, 0.0), crate::pen::pencil::DEFAULT);
    }
    let mut fb = crate::fb::Fb::open_mem();
    canvas.paint_all(&mut fb);
    match fb.dump_pgm(out) {
        Ok(()) => eprintln!(
            "[render] annot:{} → {out}（{} 层，AI 层 {} 条线：文字 {} / 框 {} / 线 {}，\
             无字体丢弃 {}）",
            if spec.is_empty() { "page" } else { spec },
            page.layers.len(),
            st.lines,
            st.text,
            st.boxes,
            st.strokes,
            st.dropped
        ),
        Err(e) => eprintln!("[render] 写 {out} 失败：{e}"),
    }
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    use super::*;

    fn line(pts: &[(f32, f32)]) -> Line {
        let mut l = Line::new(Tool::Ballpoint, Color::Black, 2.0);
        for &(x, y) in pts {
            l.push(pt(x, y, 2.0));
        }
        l
    }

    /// 两层用户笔迹的页面（`current` 指向第 0 层）
    fn user_page() -> RmPage {
        let mut page = RmPage::default();
        page.layers.clear();
        page.layers.push(Layer {
            name: "Layer 1".into(),
            visible: true,
            lines: vec![
                line(&[(300.0, 300.0), (420.0, 360.0)]),
                line(&[(300.0, 360.0), (420.0, 300.0)]),
            ],
        });
        page.layers.push(Layer {
            name: "备注".into(),
            visible: false,
            lines: vec![line(&[(100.0, 800.0), (200.0, 810.0), (300.0, 800.0)])],
        });
        page.current = 0;
        page
    }

    fn ops_text() -> Vec<Op> {
        vec![Op::Text {
            x: 120.0,
            y: 300.0,
            size: 30.0,
            text: "这里漏了负号".into(),
        }]
    }

    fn ops_batch() -> Vec<Op> {
        vec![
            Op::Box {
                x0: 100.0,
                y0: 260.0,
                x1: 420.0,
                y1: 340.0,
            },
            Op::Stroke {
                pts: vec![(100.0, 350.0), (420.0, 360.0)],
                w: 2.0,
            },
        ]
    }

    // ------------------------------------------------------------ 解析

    #[test]
    fn parse_all_three_kinds() {
        let b = parse_event(
            r#"{"kind":"annotate","page":3,"ops":[
                {"op":"text","x":120,"y":300,"text":"这里漏了负号","size":30},
                {"op":"box","x0":100,"y0":260,"x1":420,"y1":340},
                {"op":"stroke","pts":[[100,260],[420,260]],"w":2}
            ]}"#,
        )
        .unwrap();
        assert_eq!(b.page, Some(3));
        assert_eq!(b.ops.len(), 3);
        assert_eq!(b.valid_len(), 3);
        assert!(matches!(&b.ops[0], Op::Text { x, y, size, text }
            if *x == 120.0 && *y == 300.0 && *size == 30.0 && text == "这里漏了负号"));
        assert!(matches!(&b.ops[1], Op::Box { x0, y0, x1, y1 }
            if *x0 == 100.0 && *y0 == 260.0 && *x1 == 420.0 && *y1 == 340.0));
        assert!(matches!(&b.ops[2], Op::Stroke { pts, w }
            if pts == &[(100.0, 260.0), (420.0, 260.0)] && *w == 2.0));
    }

    /// `docs/CLOUD.md` 里的另一种字段名（服务器两边都可能发）：rect / points / width。
    #[test]
    fn parse_alias_spellings() {
        let b = parse_event(
            r#"{"ops":[
                {"op":"box","rect":[10,20,30,40]},
                {"op":"stroke","points":[[1,2],[3,4]],"width":7},
                {"op":"text","x":1,"y":2,"text":"ok"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(b.page, None);
        assert!(matches!(&b.ops[0], Op::Box { x0: 10.0, y0: 20.0, x1: 30.0, y1: 40.0 }));
        assert!(matches!(&b.ops[1], Op::Stroke { w, .. } if *w == 7.0));
        // size 缺省 30
        assert!(matches!(&b.ops[2], Op::Text { size: 30.0, .. }));
    }

    #[test]
    fn parse_unknown_op_is_bad_not_fatal() {
        let b = parse_event(
            r#"{"kind":"annotate","ops":[
                {"op":"arrow","x0":1,"y0":2,"x1":3,"y1":4},
                {"op":"box","x0":1,"y0":2,"x1":3,"y1":4}
            ]}"#,
        )
        .unwrap();
        assert_eq!(b.ops.len(), 2);
        assert_eq!(b.valid_len(), 1);
        assert!(matches!(&b.ops[0], Op::Bad(m) if m.contains("arrow")));
        assert!(matches!(&b.ops[1], Op::Box { .. }));
    }

    #[test]
    fn parse_missing_fields_are_bad() {
        let b = parse_event(
            r#"{"ops":[
                {"op":"text","x":1,"text":"没有 y"},
                {"op":"text","x":1,"y":2},
                {"op":"box","x0":1,"y0":2,"x1":3},
                {"op":"stroke","w":2},
                {"op":"stroke","pts":[[1,2],[3]]},
                {"op":"text","x":1,"y":2,"text":"   "}
            ]}"#,
        )
        .unwrap();
        assert_eq!(b.valid_len(), 0, "全都是坏 op");
        for o in &b.ops {
            assert!(matches!(o, Op::Bad(_)), "{o:?}");
        }
    }

    #[test]
    fn parse_whole_batch_errors() {
        assert!(parse_event("{").is_err(), "半截 JSON");
        assert!(parse_event("[]").is_err(), "不是对象");
        assert!(parse_event(r#"{"ops":{}}"#).is_err(), "ops 不是数组");
        assert!(parse_event(r#"{"kind":"annotate"}"#).is_err(), "缺 ops");
        assert!(
            parse_event(r#"{"kind":"render","ops":[]}"#).is_err(),
            "kind 不对"
        );
        assert!(parse_event(r#"{"page":-1,"ops":[]}"#).is_err(), "page 负数");
    }

    /// JSON 里的越界坐标（`1e400`）serde_json 自己就拒绝整份 —— 记在文档里，
    /// 这里锁住行为，免得以后换成宽松解析悄悄变成 inf。
    #[test]
    fn parse_out_of_range_number() {
        let r = parse_event(r#"{"ops":[{"op":"text","x":1e400,"y":2,"text":"x"}]}"#);
        assert!(r.is_err(), "1e400 直接被 serde_json 拒掉：{r:?}");
    }

    // ------------------------------------------------------------ 原层不动

    /// 逐线比两层用户笔迹：apply 前后 serialize→parse 出来的 lines 必须一模一样。
    #[test]
    fn apply_leaves_user_layers_untouched() {
        let mut page = user_page();
        let before = RmPage::parse(&page.serialize()).unwrap();
        let st = apply(&mut page, &ops_batch()).unwrap();
        assert!(st.boxes + st.strokes >= 2);
        let after = RmPage::parse(&page.serialize()).unwrap();
        assert_eq!(after.layers.len(), before.layers.len() + 1, "只多 AI 层");
        for (b, a) in before.layers.iter().zip(after.layers.iter()) {
            assert_eq!(a.name, b.name, "层名不许变");
            assert_eq!(a.visible, b.visible, "可见性不许变");
            assert_eq!(a.lines.len(), b.lines.len(), "{} 的笔数变了", b.name);
            for (lb, la) in b.lines.iter().zip(a.lines.iter()) {
                assert_eq!(la.tool, lb.tool);
                assert_eq!(la.color, lb.color);
                assert_eq!(la.thickness, lb.thickness);
                assert_eq!(la.points.len(), lb.points.len());
                for (pb, pa) in lb.points.iter().zip(la.points.iter()) {
                    assert_eq!((pa.x, pa.y), (pb.x, pb.y), "{} 的坐标变了", b.name);
                    assert_eq!((pa.width, pa.pressure), (pb.width, pb.pressure));
                }
            }
        }
        assert_eq!(after.layers.last().unwrap().name, AI_LAYER, "AI 层在最上面");
    }

    // ------------------------------------------------------------ 幂等 / 复用

    #[test]
    fn apply_twice_overwrites_the_same_layer() {
        let mut page = user_page();
        let ops = ops_batch();
        let st1 = apply(&mut page, &ops).unwrap();
        let n1 = ai_layer(&page).unwrap().lines.len();
        assert_eq!(n1, st1.lines);
        let bytes1 = page.serialize();
        let st2 = apply(&mut page, &ops).unwrap();
        assert_eq!(st2, st1, "统计也要一样");
        assert_eq!(page.layers.len(), 3, "不许叠出第二个 AI 层");
        assert_eq!(ai_layer(&page).unwrap().lines.len(), n1);
        let bytes2 = page.serialize();
        assert_eq!(bytes1, bytes2, "同一批 ops 重复 apply，落盘字节必须相同");
    }

    #[test]
    fn apply_reuses_an_existing_ai_layer_in_place() {
        let mut page = user_page();
        apply(&mut page, &ops_batch()).unwrap();
        // 用户把 AI 层挪到最下面、并藏起来
        let ai = page.layers.len() - 1;
        page.layers.swap(0, ai);
        let ai = page.layers.iter().position(|l| l.name == AI_LAYER).unwrap();
        page.layers[ai].visible = false;
        // 先记下"旧批注长什么样"（框的段数），再拿另一批 ops 覆盖
        let old = page.layers[ai].lines.len();
        apply(&mut page, &ops_text()).unwrap();
        assert_eq!(page.layers.len(), 3, "复用，不新增层");
        assert_eq!(page.layers[ai].name, AI_LAYER, "位置不动");
        assert!(page.layers[ai].visible, "新批注要看得见");
        let n2 = page.layers[ai].lines.len();
        if text_renderable() {
            assert!(n2 > 0);
            assert_ne!(n2, old, "整层被新 ops 覆盖，不是叠两份");
        } else {
            assert_eq!(n2, 0, "没有字体时 text 丢弃，整层被重写成空");
        }
    }

    #[test]
    fn apply_keeps_current_layer() {
        let mut page = user_page();
        page.current = 1;
        apply(&mut page, &ops_batch()).unwrap();
        assert_eq!(page.current, 1, "用户下一笔还落在原层，不能写进 AI 层");
    }

    // ------------------------------------------------------------ clear

    #[test]
    fn clear_removes_ai_layer_and_keeps_user_ink() {
        let mut page = user_page();
        let before = page.serialize();
        apply(&mut page, &ops_batch()).unwrap();
        assert!(clear(&mut page));
        // 加一层再删掉，序列化出来的字节要和原来**逐字节相同**：
        // 说明 AI 层没在用户那两层里留下任何痕迹（id 序号也是确定的）。
        assert_eq!(page.serialize(), before, "用户部分逐字节回到原样");
        assert_eq!(page.layers.len(), 2);
        assert!(ai_layer(&page).is_none());
        assert!(page.layers.iter().all(|l| !l.name.is_empty()));
        let after = RmPage::parse(&page.serialize()).unwrap();
        assert!(after.layers.iter().all(|l| l.name != AI_LAYER));
        assert_eq!(after.total_lines(), 3, "用户三笔还在");
    }

    #[test]
    fn clear_without_ai_layer_is_false_and_harmless() {
        let mut page = user_page();
        let before = page.serialize();
        assert!(!clear(&mut page));
        assert_eq!(page.serialize(), before, "没 AI 层就一个字节都不动");
        assert!(!clear(&mut RmPage::default()));
    }

    #[test]
    fn clear_keeps_the_only_layer() {
        let mut page = RmPage::default();
        page.layers.clear();
        page.layers.push(Layer {
            name: AI_LAYER.into(),
            visible: true,
            lines: vec![line(&[(10.0, 10.0), (20.0, 20.0)])],
        });
        page.current = 0;
        assert!(clear(&mut page));
        assert_eq!(page.layers.len(), 1, "页面不能变成 0 层");
        assert!(page.layers[0].lines.is_empty());
    }

    // ------------------------------------------------------------ 追加（Live Note）

    #[test]
    fn append_keeps_earlier_answers() {
        let mut page = user_page();
        let user_before = page.layers[0].lines.len();
        let s1 = append(&mut page, &ops_batch()).unwrap();
        assert!(s1.applied() > 0);
        let after_first = ai_layer(&page).unwrap().lines.len();
        let s2 = append(&mut page, &ops_batch()).unwrap();
        assert!(s2.applied() > 0);
        let after_second = ai_layer(&page).unwrap().lines.len();
        assert_eq!(
            after_second,
            after_first * 2,
            "第 2 次回答不能把第 1 次的抹掉"
        );
        assert_eq!(page.layers[0].lines.len(), user_before, "用户笔迹一个不能少");
        assert_eq!(page.current, 0, "当前层不能变成 AI 层");
    }

    #[test]
    fn apply_is_idempotent_but_append_is_not() {
        let mut page = user_page();
        apply(&mut page, &ops_batch()).unwrap();
        let once = ai_layer(&page).unwrap().lines.len();
        apply(&mut page, &ops_batch()).unwrap();
        assert_eq!(ai_layer(&page).unwrap().lines.len(), once, "apply 整层重写");
        append(&mut page, &ops_batch()).unwrap();
        assert_eq!(ai_layer(&page).unwrap().lines.len(), once * 2);
    }

    #[test]
    fn append_with_no_drawable_op_does_not_create_a_layer() {
        let mut page = user_page();
        let before = page.serialize();
        let st = append(&mut page, &[]).unwrap();
        assert_eq!(st.applied(), 0);
        assert!(ai_layer(&page).is_none(), "没有可画的东西就别占一层");
        assert_eq!(page.serialize(), before);
    }

    #[test]
    fn append_respects_the_layer_limit() {
        let mut page = RmPage::default();
        page.layers.clear();
        for i in 0..MAX_LAYERS {
            page.layers.push(Layer {
                name: format!("Layer {}", i + 1),
                visible: true,
                lines: vec![line(&[(10.0 * i as f32, 0.0), (20.0, 20.0)])],
            });
        }
        let before = page.serialize();
        let e = append(&mut page, &ops_batch()).unwrap_err();
        assert!(e.contains("最多 5 层"), "{e}");
        assert_eq!(page.serialize(), before, "页面一个字节都不许动");
    }

    #[test]
    fn appended_ink_is_still_erasable_gray_ink() {
        // 追加进来的线必须和 apply 的同一套样式：灰、AI 工具、1.0 粗细
        let mut page = RmPage::default();
        append(&mut page, &ops_batch()).unwrap();
        let l = &ai_layer(&page).unwrap().lines[0];
        assert_eq!(l.color, AI_COLOR);
        assert_eq!(l.tool, Tool::Fineliner);
    }

    // ------------------------------------------------------------ 层数上限

    #[test]
    fn layer_limit_errors_without_touching_the_page() {
        let mut page = RmPage::default();
        page.layers.clear();
        for i in 0..MAX_LAYERS {
            page.layers.push(Layer {
                name: format!("Layer {}", i + 1),
                visible: true,
                lines: vec![line(&[(10.0 * i as f32, 0.0), (20.0, 20.0)])],
            });
        }
        page.current = 2;
        let before = page.serialize();
        let e = apply(&mut page, &ops_batch()).unwrap_err();
        assert!(e.contains("最多 5 层"), "{e}");
        assert_eq!(page.serialize(), before, "页面一个字节都不许动");
        assert_eq!(page.layers.len(), 5);
        assert_eq!(page.current, 2);
    }

    #[test]
    fn layer_limit_is_fine_when_ai_layer_already_exists() {
        let mut page = RmPage::default();
        page.layers.clear();
        for i in 0..MAX_LAYERS {
            page.layers.push(Layer {
                name: if i == 3 { AI_LAYER.into() } else { format!("L{i}") },
                visible: true,
                lines: Vec::new(),
            });
        }
        page.current = 0;
        let st = apply(&mut page, &ops_batch()).unwrap();
        assert!(st.lines > 0);
        assert_eq!(page.layers.len(), 5);
        assert_eq!(page.layers[3].name, AI_LAYER);
        assert!(!page.layers[3].lines.is_empty());
    }

    // ------------------------------------------------------------ clamp / 丢弃

    #[test]
    fn out_of_bounds_ops_are_clamped_and_counted() {
        let mut page = user_page();
        let ops = vec![
            Op::Stroke {
                pts: vec![(-50.0, -80.0), (5000.0, 5000.0)],
                w: 2.0,
            },
            Op::Box {
                x0: -10.0,
                y0: -10.0,
                x1: 9000.0,
                y1: 9000.0,
            },
            Op::Text {
                x: -100.0,
                y: 1800.0,
                size: 300.0,
                text: "越界".into(),
            },
        ];
        let st = apply(&mut page, &ops).unwrap();
        assert_eq!(st.clamped, 3, "三条都越界：{st:?}");
        // 所有落到 AI 层的点都在页面里（场景坐标：x ∈ [-702,702]、y ∈ [0,1872]）
        for l in &ai_layer(&page).unwrap().lines {
            for p in &l.points {
                assert!(p.x >= -PAGE_CX && p.x <= PAGE_CX, "x 越界：{}", p.x);
                assert!(p.y >= 0.0 && p.y <= PAGE_H, "y 越界：{}", p.y);
                assert!(p.is_valid(), "点不在官方合法域：{p:?}");
            }
        }
    }

    /// 原点被 clamp 到页面右/下边界时一个字都放不下 → 丢弃（但 clamp 仍计数）。
    #[test]
    fn text_at_the_far_edge_is_dropped() {
        let mut page = user_page();
        let st = apply(
            &mut page,
            &[Op::Text {
                x: 2000.0,
                y: 2000.0,
                size: 30.0,
                text: "放不下".into(),
            }],
        )
        .unwrap();
        assert_eq!(st.clamped, 1);
        assert_eq!(st.dropped, 1);
        assert_eq!(st.applied(), 0);
        assert_eq!(page.layers.len(), 2, "没画东西就不建 AI 层");
    }

    #[test]
    fn nan_inf_ops_are_dropped() {
        let mut page = user_page();
        let ops = vec![
            Op::Text {
                x: f32::NAN,
                y: 10.0,
                size: 30.0,
                text: "NaN".into(),
            },
            Op::Box {
                x0: 0.0,
                y0: 0.0,
                x1: f32::INFINITY,
                y1: 10.0,
            },
            Op::Stroke {
                pts: vec![(1.0, 1.0), (f32::NAN, 2.0)],
                w: 2.0,
            },
            Op::Stroke {
                pts: vec![(1.0, 1.0), (2.0, 2.0)],
                w: f32::NAN,
            },
        ];
        let st = apply(&mut page, &ops).unwrap();
        assert_eq!(st.applied(), 0);
        assert_eq!(st.dropped, 4);
        assert_eq!(st.lines, 0);
        assert_eq!(page.layers.len(), 2, "什么都没画出来就不建 AI 层");
    }

    #[test]
    fn empty_ops_do_nothing() {
        let mut page = user_page();
        let before = page.serialize();
        let st = apply(&mut page, &[]).unwrap();
        assert_eq!(st, Stats::default());
        assert_eq!(page.serialize(), before);
        // 已经有 AI 层时，空 ops 会把 AI 层清空（整层重写）
        apply(&mut page, &ops_batch()).unwrap();
        apply(&mut page, &[]).unwrap();
        assert_eq!(ai_layer(&page).unwrap().lines.len(), 0);
    }

    #[test]
    fn counters_add_up() {
        let ops = vec![
            Op::Bad("未知 op".into()),
            Op::Text {
                x: 10.0,
                y: 20.0,
                size: 30.0,
                text: "好".into(),
            },
            Op::Box {
                x0: 0.0,
                y0: 0.0,
                x1: 50.0,
                y1: 50.0,
            },
            Op::Stroke {
                pts: vec![(0.0, 0.0), (5.0, 5.0)],
                w: 2.0,
            },
            Op::Box {
                x0: 5.0,
                y0: 5.0,
                x1: 5.0,
                y1: 5.0,
            },
        ];
        let mut page = user_page();
        let st = apply(&mut page, &ops).unwrap();
        assert_eq!(st.dropped, 2, "Bad + 退化框");
        assert_eq!(st.applied(), 3);
        assert_eq!(st.applied() + st.dropped, ops.len());
    }

    #[test]
    fn overlong_text_is_truncated() {
        let mut page = user_page();
        let long: String = "字".repeat(MAX_TEXT_CHARS + 50);
        let ops = vec![Op::Text {
            x: 10.0,
            y: 500.0,
            size: 30.0,
            text: long,
        }];
        let st = apply(&mut page, &ops).unwrap();
        assert_eq!(st.text + st.dropped, 1);
        if !text_renderable() {
            return;
        }
        assert_eq!(st.truncated, 2, "先按 200 字截断，再按页面宽度截断");
        // 一行画到右边界就停：所有点都不许跑到纸外面（字形轮廓允许 2px 溢出）
        let max_x = ai_layer(&page)
            .unwrap()
            .lines
            .iter()
            .flat_map(|l| l.points.iter())
            .fold(f32::MIN, |a, p| a.max(p.x));
        assert!(max_x <= PAGE_CX + 2.0, "右边界越界：{max_x}");
    }

    #[test]
    fn normal_text_is_not_truncated() {
        let mut page = user_page();
        let st = apply(&mut page, &ops_text()).unwrap();
        assert_eq!(st.truncated, 0);
    }

    #[test]
    fn too_many_stroke_points_are_truncated() {
        let mut page = user_page();
        let pts: Vec<(f32, f32)> = (0..MAX_STROKE_PTS + 100)
            .map(|i| ((i % 1000) as f32, (i % 800) as f32))
            .collect();
        let st = apply(&mut page, &[Op::Stroke { pts, w: 2.0 }]).unwrap();
        assert_eq!(st.truncated, 1);
        assert_eq!(st.strokes, 1);
        assert_eq!(ai_layer(&page).unwrap().lines[0].points.len(), MAX_STROKE_PTS);
    }

    // ------------------------------------------------------------ 样式（看得出是 AI）

    #[test]
    fn ai_ink_is_gray_and_boxes_are_dashed() {
        let mut page = user_page();
        apply(&mut page, &ops_batch()).unwrap();
        let ai = ai_layer(&page).unwrap();
        assert!(ai.lines.iter().all(|l| l.color == AI_COLOR));
        assert!(ai.lines.iter().all(|l| l.tool == Tool::Fineliner));
        // 框 320×80 周长 800：虚线 12/8 → 几十段，且总和远小于周长
        let dashes: Vec<&Line> = ai.lines.iter().filter(|l| l.points.len() == 2).collect();
        assert!(dashes.len() > 20, "虚线段数：{}", dashes.len());
        let total: f32 = dashes
            .iter()
            .map(|l| {
                let (a, b) = (&l.points[0], &l.points[1]);
                ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
            })
            .sum();
        assert!(total < 4.0 * 400.0 * 0.8, "虚线总长 {total} 该明显小于周长");
    }

    #[test]
    fn single_point_stroke_is_a_dot() {
        let mut page = user_page();
        let st = apply(
            &mut page,
            &[Op::Stroke {
                pts: vec![(700.0, 900.0)],
                w: 3.0,
            }],
        )
        .unwrap();
        assert_eq!(st.strokes, 1);
        assert_eq!(ai_layer(&page).unwrap().lines[0].points.len(), 1);
    }

    // ------------------------------------------------------------ 落盘

    #[test]
    fn apply_at_is_atomic_and_keeps_user_ink() {
        let dir = std::env::temp_dir().join(format!(
            "rmdev-annot-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("abc.rm");
        let page = user_page();
        fs::write(&path, page.serialize()).unwrap();
        let st = apply_at(&path, &ops_batch()).unwrap();
        assert!(st.lines > 0);
        let raw = fs::read(&path).unwrap();
        let parsed = RmPage::parse(&raw).unwrap();
        assert_eq!(parsed.layers.len(), 3);
        assert_eq!(parsed.layers[2].name, AI_LAYER);
        // 用户两层：层名/可见性/笔数/坐标逐点一致
        assert_eq!(parsed.layers[0].name, "Layer 1");
        assert_eq!(parsed.layers[1].name, "备注");
        assert!(!parsed.layers[1].visible);
        assert_eq!(parsed.layers[0].lines.len(), 2);
        assert_eq!(parsed.layers[0].lines[0].points.len(), 2);
        assert_eq!(parsed.layers[1].lines[0].points.len(), 3);
        // 临时文件不许留下
        let left: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(left, vec!["abc.rm".to_string()], "目录里只剩页面文件");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_at_preserves_typed_text_layer() {
        use crate::doc::rmtext::{ParagraphStyle, SceneText};
        let dir = std::env::temp_dir().join(format!(
            "rmdev-annot-text-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("text.rm");
        let page = user_page();
        let texts = vec![SceneText {
            text: "手打的一行字".to_string(),
            x: 100.0,
            y: 200.0,
            size: 32.0,
            color: 0,
            style: ParagraphStyle::PLAIN,
        }];
        let bytes = rmtext::serialize_texts(&page.serialize(), &texts).unwrap();
        fs::write(&path, &bytes).unwrap();
        apply_at(&path, &ops_batch()).unwrap();
        let raw = fs::read(&path).unwrap();
        let got = rmtext::parse_texts(&raw).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "手打的一行字");
        let parsed = RmPage::parse(&raw).unwrap();
        assert_eq!(parsed.layers.last().unwrap().name, AI_LAYER, "笔迹层也在");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_at_writes_nothing_when_all_ops_are_bad() {
        let dir = std::env::temp_dir().join(format!(
            "rmdev-annot-noop-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("page.rm");
        let bytes = user_page().serialize();
        fs::write(&path, &bytes).unwrap();
        let st = apply_at(&path, &[Op::Bad("未知 op".into())]).unwrap();
        assert_eq!(st.dropped, 1);
        assert_eq!(fs::read(&path).unwrap(), bytes, "没画东西就别碰文件");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn apply_at_missing_file_is_an_error() {
        let p = std::env::temp_dir().join("rmdev-annot-nope/nope.rm");
        assert!(apply_at(&p, &ops_batch()).is_err());
        assert!(!p.exists(), "失败时不许凭空造文件");
    }

    // ------------------------------------------------------------ 文字 / 出图

    #[test]
    fn text_op_becomes_strokes_when_a_font_exists() {
        let mut page = user_page();
        let st = apply(&mut page, &ops_text()).unwrap();
        assert_eq!(st.text + st.dropped, 1);
        if text_renderable() {
            assert_eq!(st.text, 1);
            assert!(st.lines > 0, "汉字要拆成折线");
            // 文字在 (120,300) 基线附近、浅灰、定宽
            let ai = ai_layer(&page).unwrap();
            let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
            for p in ai.lines.iter().flat_map(|l| l.points.iter()) {
                let (px, py) = (p.x + PAGE_CX, p.y);
                x0 = x0.min(px);
                x1 = x1.max(px);
                y0 = y0.min(py);
                y1 = y1.max(py);
            }
            assert!((x0 - 120.0).abs() < 8.0, "左边界 {x0}");
            assert!(x1 > 200.0, "一行中文字得铺开：{x1}");
            // 汉字坐在基线上、偶有 1~2 px 下垂（这/漏 的收笔）
            assert!(y1 <= 302.0 && y0 > 300.0 - 40.0, "字在基线以上：{y0}..{y1}");
        } else {
            assert_eq!(st.dropped, 1);
        }
    }


    /// `--render annot:page` 的那张图真能画出来（离屏，不碰设备）。
    ///
    /// 注意 `Canvas` 对中间灰走**有序抖动**（e-ink 二值波形显示不了真灰度），
    /// 所以 AI 的 `Color::Gray` 在 PGM 里是 ~43% 的黑点，不是 110 的灰像素；
    /// 这里按覆盖率验证"网点密度 < 实心"，正好对应屏上的浅灰观感。
    #[test]
    fn demo_render_writes_a_pgm() {
        let out = std::env::temp_dir().join(format!("rmdev-annot-demo-{}.pgm", std::process::id()));
        let s = out.to_string_lossy().to_string();
        render_demo("page", &s);
        let bytes = fs::read(&out).unwrap();
        let hdr = b"P5\n1404 1872\n255\n";
        assert!(bytes.starts_with(hdr), "PGM 头不对");
        assert_eq!(bytes.len(), hdr.len() + 1404 * 1872);
        let px = &bytes[hdr.len()..];
        let dark = |x0: i32, y0: i32, x1: i32, y1: i32| -> (usize, usize) {
            let (mut n, mut all) = (0usize, 0usize);
            for y in y0..y1 {
                for x in x0..x1 {
                    let v = px[(y as usize) * 1404 + x as usize];
                    if v < 40 {
                        n += 1;
                    }
                    all += 1;
                }
            }
            (n, all)
        };
        // 手写第一行落在 y≈290..360，实心黑
        let (hw, _) = dark(190, 285, 430, 365);
        assert!(hw > 200, "手写没画出来（{hw} 个黑点）");
        // AI 文字那一行：有网点（画出来了）但不是实心一片
        let (tw, tall) = dark(190, 530, 1000, 580);
        assert!(tw > 30, "AI 文字没画出来（{tw} 个黑点）");
        assert!(
            (tw as f32) < 0.4 * tall as f32,
            "AI 文字该是网点浅灰而不是实心：{tw}/{tall}"
        );
        // 虚线框：上边一行有墨，但**不连续**（12 实 / 8 空）
        let (edge, _) = dark(344, 394, 418, 400);
        assert!(edge > 20, "虚线框没画出来（{edge} 个黑点）");
        let mut cols = 0;
        for x in 346..416 {
            let mut on = false;
            for y in 484..492 {
                if px[(y as usize) * 1404 + x as usize] < 40 {
                    on = true;
                }
            }
            if on {
                cols += 1;
            }
        }
        assert!(cols > 5, "框下边没墨（{cols} 列）");
        assert!(cols < 62, "框下边该是虚线，不能连成一条：{cols}/70 列有墨");
        let _ = fs::remove_file(&out);
    }
}

#[cfg(test)]
mod model_output_tests {
    use super::*;

    /// **真模型输出**（2026-11-09，服务器 `/api/v1/agent/ask` 的 `grade` 模式，
    /// 一页「印刷积分题 + 手写错解」的合成图）。这条测试的意义：模型真的会这么回，
    /// 设备端解析器必须吃得下，而且坐标都落在页内。
    const REAL: &str = r#"{"kind":"annotate","ops":[
      {"op":"box","x0":315,"y0":448,"x1":980,"y1":492},
      {"op":"text","x":325,"y":522,"text":"第 2 题不对，这是第 1 题的原函数","size":26},
      {"op":"box","x0":560,"y0":532,"x1":900,"y1":578},
      {"op":"text","x":910,"y":572,"text":"第 3 题应为 ½，不是 1","size":26},
      {"op":"box","x0":565,"y0":615,"x1":785,"y1":672},
      {"op":"text","x":230,"y":712,"text":"第 4 题要写分部积分，答案应为 1","size":26}
    ]}"#;

    #[test]
    fn real_grade_output_parses_and_lands_on_the_page() {
        let batch = parse_event(REAL).expect("真模型输出必须能解析");
        assert_eq!(batch.ops.len(), 6);
        assert_eq!(batch.valid_len(), 6, "一条都不该被丢");
        let mut page = RmPage::default();
        let st = apply(&mut page, &batch.ops).expect("apply 不该失败");
        assert_eq!(st.dropped, 0, "{st:?}");
        assert_eq!(st.boxes, 3, "{st:?}");
        assert_eq!(st.text, 3, "{st:?}");
        assert_eq!(st.clamped, 0, "模型的坐标本来就在页内：{st:?}");
        // AI 层真的建起来了，而且是唯一新增的一层
        assert_eq!(page.layers.len(), 2, "原来 1 层 + AI 层");
        assert_eq!(page.layers[1].name, "AI");
        assert!(!page.layers[1].lines.is_empty());
        // 用户那一层一个字节没动
        assert!(page.layers[0].lines.is_empty());
    }

    /// 批语里出现 `$...$` 是**上一版 prompt 的毛病**（会把美元符号画到纸上）。
    /// 记录一下现在的行为：解析器照收，但会被原样画成笔迹 —— 所以 prompt 里禁了。
    #[test]
    fn latex_in_note_would_be_drawn_as_ink() {
        let batch = parse_event(
            r#"{"kind":"annotate","ops":[{"op":"text","x":10,"y":100,"text":"$1/2$","size":30}]}"#,
        )
        .unwrap();
        let mut page = RmPage::default();
        let st = apply(&mut page, &batch.ops).unwrap();
        assert_eq!(st.text, 1);
        assert!(page.layers[1].lines.len() > 10, "美元符号和斜杠都会变成笔画");
    }
}
