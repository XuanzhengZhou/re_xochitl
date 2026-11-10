//! 电子书 "Text settings" 整页 sheet（文档设置 → `Text size` 那一行）。
//!
//! 规格来源：官方 QML 树里的 `FormatTextSize` / `FormatFont` /
//! `FormatAlignment` / `FormatMargins` / `FormatLineSpacing` 五个格子 + 顶部
//! `Header`：左 `Reset`、标题 `Text settings`、右 `Done`；改动前若文档里有墨迹，
//! 先弹 `ConfirmChangeDialog`，标题 `Misalignment may occur`，按钮
//! `Yes, change settings` / `No, keep current settings`）。
//!
//! 范围（T1）：`Text size` / `Page margins` / `Line spacing` 三段可改；
//! `Font` / `Justification` 两项**画成灰色不可点**，subtext `Not available`
//! （字体只有一种；两端对齐要改 `reflow::wrap` 的分配逻辑，留给后面的任务）。
//!
//! 点 `<档位>` 只改面板里的草稿配置（不重排文档）；`Done` 才把配置交给
//! `main.rs` 的 `App::set_reading_layout`。排版参数按文档持久化到
//! `~/.config/rmdev/readcfg.json`：`{"<uuid>": {"font_size":25.0, ...}}`。
//!
//! 约定同 `nbsettings`：布局是纯函数（`hits()` 可单测），`draw()` 只照着画。

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::doc::reflow::{FONT_SIZES, LINE_SPACINGS, MARGINS};
use crate::doc::LayoutCfg;
use crate::fb::{gray565, Fb};
use crate::ui::text::Text;
use crate::ui::widgets;

/// 黑头高（与库里其它整页 sheet 一致）。
pub const HDR_H: i32 = 118;

/// 五段等分：118 + 5 × 350 = 1868，正好铺满 1872。
const SEC_H: i32 = 350;
/// 段标题基线（段顶 + 80）。
const TITLE_DY: i32 = 80;
/// 档位圆心（段顶 + 170）。
const STOP_DY: i32 = 170;
/// 档位圆半径 / 环宽 / 选中内圆。
const STOP_R: i32 = 26;
const STOP_T: i32 = 4;
const STOP_IN: i32 = 12;
/// 档位步进（5 档 = 880 宽，3 档 = 440 宽，都按屏宽居中）。
const STOP_STEP: i32 = 220;
/// 命中区边长（官方触控目标 ≥ 80×80，这里给 100）。
const HIT: i32 = 100;

const FONT_HDR: f32 = 33.0;
const FONT_TITLE: f32 = 36.0;
const FONT_SUB: f32 = 28.0;
const FONT_DIALOG_TITLE: f32 = 34.0;
const FONT_DIALOG_BODY: f32 = 26.0;
const FONT_BUTTON: f32 = 25.0;

const BG: u8 = 255;
const INK: u8 = 20;
/// 不可点 / `Not available` 的灰。
const MUTED: u8 = 150;
/// 段之间的细分隔线。
const SEP: u8 = 220;

/// 确认框几何：卡片 1240×440 居中，两个按钮各 520×90。
const CARD_W: i32 = 1240;
const CARD_H: i32 = 440;
const BTN_W: i32 = 520;
const BTN_H: i32 = 90;

/// 官方 QML 标题/文案（一个字都不要改）。
const TITLE: &str = "Text settings";
const RESET: &str = "Reset";
const DONE: &str = "Done";
const NOT_AVAILABLE: &str = "Not available";
const CONFIRM_TITLE: &str = "Misalignment may occur";
const CONFIRM_BODY: &str = "We see that you’ve made some annotations, highlights or doodles. If you decide to change your settings, they may become misaligned. This could be irreversible.";
const CONFIRM_YES: &str = "Yes, change settings";
const CONFIRM_NO: &str = "No, keep current settings";

/// 五段：文案 + 档位数（0 = 灰色不可点的占位行）。
const SECTIONS: [(&str, usize); 5] = [
    ("Text size", FONT_SIZES.len()),
    ("Font", 0),
    ("Justification", 0),
    ("Page margins", MARGINS.len()),
    ("Line spacing", LINE_SPACINGS.len()),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    /// 字号档位（下标进 [`FONT_SIZES`]）
    Size(usize),
    /// 页边距档位（下标进 [`MARGINS`]）
    Margin(usize),
    /// 行距档位（下标进 [`LINE_SPACINGS`]）
    Spacing(usize),
    /// 顶部左侧 `Reset`：草稿配置回默认值，点 `Done` 才生效。
    Reset,
    /// 顶部右侧 `Done`：有墨迹且改过 → 先弹确认框。
    Done,
    /// 确认框 `Yes, change settings`
    Confirm,
    /// 确认框 `No, keep current settings`：丢弃本次修改。
    Cancel,
}

pub struct TextCfg {
    open: bool,
    /// 待确认的新配置（用户点了改但还没确认）
    pending: Option<LayoutCfg>,
    /// 文档里有没有墨迹（有才弹确认框）
    has_ink: bool,
    confirm: bool,
    cfg: LayoutCfg,
    /// 打开那一刻的配置：`Cancel` 要能原样还回去。
    initial: LayoutCfg,
}

impl TextCfg {
    pub fn new() -> TextCfg {
        TextCfg {
            open: false,
            pending: None,
            has_ink: false,
            confirm: false,
            cfg: LayoutCfg::default(),
            initial: LayoutCfg::default(),
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    /// 确认框是否正在显示（`Done` 之后的二次确认）。
    pub fn is_confirming(&self) -> bool {
        self.open && self.confirm
    }

    /// 打开面板。`has_ink` = 当前文档有没有手写（决定是否弹确认）。
    pub fn show(&mut self, cfg: LayoutCfg, has_ink: bool) {
        self.open = true;
        self.cfg = cfg;
        self.initial = cfg;
        self.has_ink = has_ink;
        self.confirm = false;
        self.pending = None;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.confirm = false;
        self.pending = None;
    }

    /// 当前面板上"用户已选但未生效"的配置。确认后由 main.rs 拿去 apply。
    pub fn cfg(&self) -> LayoutCfg {
        self.cfg
    }

    /// 点一个命中区。返回 `Some(cfg)` = 用户已确认（`Done` 且无需确认，
    /// 或确认框点了 `Yes`），调用方拿去 `set_reading_layout` + 落盘。
    ///
    /// 只改面板状态（点档位 / `Reset` / 弹确认框）时返回 `None`。
    pub fn click(&mut self, act: Act) -> Option<LayoutCfg> {
        if !self.open {
            return None;
        }
        // 确认框盖住整页：只认它自己的两个按钮
        if self.confirm {
            return match act {
                Act::Confirm => {
                    self.confirm = false;
                    self.open = false;
                    let cfg = self.pending.take().unwrap_or(self.cfg);
                    self.cfg = cfg;
                    Some(cfg)
                }
                // 官方 `onRejected: root.close()`：整页关掉，什么都不改
                Act::Cancel => {
                    self.confirm = false;
                    self.open = false;
                    self.pending = None;
                    self.cfg = self.initial;
                    None
                }
                _ => None,
            };
        }
        match act {
            Act::Size(i) if i < FONT_SIZES.len() => self.cfg.font_size = FONT_SIZES[i],
            Act::Margin(i) if i < MARGINS.len() => self.cfg.margin = MARGINS[i],
            Act::Spacing(i) if i < LINE_SPACINGS.len() => self.cfg.line_spacing = LINE_SPACINGS[i],
            Act::Reset => self.cfg = LayoutCfg::default(),
            Act::Done => {
                // 官方 `confirmLayoutChange`：有批注且真的改了才警告
                if self.has_ink && self.cfg != self.initial {
                    self.pending = Some(self.cfg);
                    self.confirm = true;
                } else {
                    self.open = false;
                    return Some(self.cfg);
                }
            }
            _ => {}
        }
        None
    }

    /// 命中区。顺序 = 绘制顺序（表头左/右 → 三段档位；确认态只有两个按钮）。
    pub fn hits(&self, w: i32, h: i32) -> Vec<((i32, i32, i32, i32), Act)> {
        if !self.open {
            return Vec::new();
        }
        if self.confirm {
            let (no, yes) = dialog_buttons(w, h);
            return vec![(no, Act::Cancel), (yes, Act::Confirm)];
        }
        let mut v = Vec::with_capacity(2 + FONT_SIZES.len() + MARGINS.len() + LINE_SPACINGS.len());
        v.push(((0, 0, 240, HDR_H), Act::Reset));
        v.push(((w - 240, 0, 240, HDR_H), Act::Done));
        for (i, (_, n)) in SECTIONS.iter().enumerate() {
            if *n == 0 {
                continue;
            }
            for (k, cx) in stop_centers(*n, w).into_iter().enumerate() {
                let cy = sec_top(i) + STOP_DY;
                let r = (cx - HIT / 2, cy - HIT / 2, HIT, HIT);
                let act = match i {
                    0 => Act::Size(k),
                    3 => Act::Margin(k),
                    _ => Act::Spacing(k),
                };
                v.push((r, act));
            }
        }
        // 屏高不够时（理论上不会）裁掉屏外的命中区，别让手指点到看不见的东西
        v.retain(|(r, _)| r.1 >= 0 && r.1 + r.3 <= h);
        v
    }

    /// 命中测试。
    pub fn hit(&self, x: f32, y: f32, w: i32, h: i32) -> Option<Act> {
        self.hits(w, h)
            .into_iter()
            .find(|(r, _)| {
                x >= r.0 as f32
                    && y >= r.1 as f32
                    && x < (r.0 + r.2) as f32
                    && y < (r.1 + r.3) as f32
            })
            .map(|(_, a)| a)
    }

    /// 画。`fb` 是帧缓冲，`text` 是字体（没加载字体时只画形状）。
    pub fn draw(&mut self, fb: &mut Fb, text: &mut Option<Text>) {
        let (w, h) = (fb.width, fb.height);
        fb.fill_rect(0, 0, w, h, gray565(BG));
        for i in 0..SECTIONS.len() {
            self.draw_section(fb, text, i, w);
        }
        self.draw_header(fb, text, w);
        if self.confirm {
            self.draw_confirm(fb, text, w, h);
        }
    }

    fn draw_header(&self, fb: &mut Fb, text: &mut Option<Text>, w: i32) {
        fb.fill_rect(0, 0, w, HDR_H, gray565(0));
        if let Some(t) = text.as_mut() {
            t.draw_bold(fb, 32, 74, FONT_HDR, RESET, 255);
            let tw = t.width_bold(FONT_HDR, TITLE);
            t.draw_bold(fb, ((w as f32 - tw) / 2.0) as i32, 74, FONT_HDR, TITLE, 255);
            let dw = t.width_bold(FONT_HDR, DONE);
            t.draw_bold(fb, w - 32 - dw as i32, 74, FONT_HDR, DONE, 255);
        }
    }

    fn draw_section(&self, fb: &mut Fb, text: &mut Option<Text>, i: usize, w: i32) {
        let (label, n) = SECTIONS[i];
        let top = sec_top(i);
        if let Some(t) = text.as_mut() {
            let ink = if n == 0 { MUTED } else { INK };
            t.draw_bold(fb, 33, top + TITLE_DY, FONT_TITLE, label, ink);
            if n == 0 {
                t.draw(fb, 33, top + TITLE_DY + 52, FONT_SUB, NOT_AVAILABLE, MUTED);
            }
        }
        if n > 0 {
            let sel = match i {
                0 => pos_f32(&FONT_SIZES, self.cfg.font_size),
                3 => MARGINS.iter().position(|m| *m == self.cfg.margin),
                _ => pos_f32(&LINE_SPACINGS, self.cfg.line_spacing),
            };
            let cy = top + STOP_DY;
            for (k, cx) in stop_centers(n, w).into_iter().enumerate() {
                widgets::ring(fb, cx, cy, STOP_R, STOP_T, INK);
                if sel == Some(k) {
                    widgets::disc(fb, cx, cy, STOP_IN, INK);
                }
            }
        }
        if i + 1 < SECTIONS.len() {
            let y = top + SEC_H - 2;
            fb.fill_rect(33, y, w - 66, 2, gray565(SEP));
        }
    }

    fn draw_confirm(&self, fb: &mut Fb, text: &mut Option<Text>, w: i32, h: i32) {
        fb.dim(0, 0, w, h, gray565(140), 1, 2);
        let (cx, cy) = ((w - CARD_W) / 2, (h - CARD_H) / 2);
        fb.fill_rect(cx, cy, CARD_W, CARD_H, gray565(255));
        widgets::outline_rect(fb, cx, cy, CARD_W, CARD_H, 2, INK);
        let (no, yes) = dialog_buttons(w, h);
        widgets::outline_rect(fb, no.0, no.1, no.2, no.3, 2, INK);
        widgets::outline_rect(fb, yes.0, yes.1, yes.2, yes.3, 2, INK);
        if let Some(t) = text.as_mut() {
            t.draw_bold(fb, cx + 48, cy + 84, FONT_DIALOG_TITLE, CONFIRM_TITLE, INK);
            let lines = wrap(t, FONT_DIALOG_BODY, CONFIRM_BODY, CARD_W - 96);
            for (i, line) in lines.iter().enumerate().take(4) {
                t.draw(
                    fb,
                    cx + 48,
                    cy + 140 + i as i32 * 40,
                    FONT_DIALOG_BODY,
                    line,
                    60,
                );
            }
            t.draw(fb, no.0 + 24, no.1 + 58, FONT_BUTTON, CONFIRM_NO, INK);
            t.draw(fb, yes.0 + 24, yes.1 + 58, FONT_BUTTON, CONFIRM_YES, INK);
        }
    }
}

impl Default for TextCfg {
    fn default() -> Self {
        TextCfg::new()
    }
}

// ---------------------------------------------------------------------------
// 布局（纯函数，能单测）
// ---------------------------------------------------------------------------

fn sec_top(i: usize) -> i32 {
    HDR_H + SEC_H * i as i32
}

/// 档位圆心。`n` 个档位整体按屏宽居中，步进固定 [`STOP_STEP`]。
fn stop_centers(n: usize, w: i32) -> Vec<i32> {
    if n == 0 {
        return Vec::new();
    }
    let total = (n as i32 - 1) * STOP_STEP;
    let x0 = (w - total) / 2;
    (0..n as i32).map(|k| x0 + k * STOP_STEP).collect()
}

/// 确认框两个按钮：(左 = `No, keep current settings`, 右 = `Yes, change settings`)。
fn dialog_buttons(w: i32, h: i32) -> ((i32, i32, i32, i32), (i32, i32, i32, i32)) {
    let (cx, cy) = ((w - CARD_W) / 2, (h - CARD_H) / 2);
    let y = cy + CARD_H - 130;
    let no = (cx + 48, y, BTN_W, BTN_H);
    let yes = (cx + CARD_W - 48 - BTN_W, y, BTN_W, BTN_H);
    (no, yes)
}

fn pos_f32(arr: &[f32], v: f32) -> Option<usize> {
    arr.iter().position(|x| (x - v).abs() < 0.005)
}

/// 按词贪心折行（只用于确认框正文）。
fn wrap(t: &mut Text, size: f32, s: &str, max_w: i32) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let cand = if cur.is_empty() {
            word.to_string()
        } else {
            format!("{cur} {word}")
        };
        if t.width(size, &cand) <= max_w as f32 {
            cur = cand;
        } else {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            cur = word.to_string();
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

// ---------------------------------------------------------------------------
// 持久化：~/.config/rmdev/readcfg.json
// ---------------------------------------------------------------------------

/// 配置文件路径（`RMDEV_CONFIG_DIR` 可覆盖，和设置页共用同一个目录）。
pub fn readcfg_path() -> PathBuf {
    crate::settings::config_dir().join("readcfg.json")
}

/// 读这本文档的排版。没有 / 文件坏了 / 没这一项都返回 `None`（不 panic）。
pub fn load_cfg(uuid: &str) -> Option<LayoutCfg> {
    load_cfg_at(&readcfg_path(), uuid)
}

/// 写这本文档的排版。失败只打日志（设备可能只读），不影响阅读。
pub fn save_cfg(uuid: &str, cfg: LayoutCfg) {
    save_cfg_at(&readcfg_path(), uuid, cfg);
}

/// [`load_cfg`] 的纯函数版：路径自己给，单测用临时目录。
pub fn load_cfg_at(path: &Path, uuid: &str) -> Option<LayoutCfg> {
    let s = std::fs::read_to_string(path).ok()?;
    let v: Value = serde_json::from_str(&s).ok()?;
    let o = v.get(uuid)?.as_object()?;
    let cfg = LayoutCfg {
        font_size: o.get("font_size")?.as_f64()? as f32,
        line_spacing: o.get("line_spacing")?.as_f64()? as f32,
        margin: o.get("margin")?.as_i64()? as i32,
    };
    // 手改坏的数值也夹到合法范围（字号 8..72 / 行距 1..3 / 边距 0..400）
    Some(LayoutCfg::from_reflow(cfg.to_reflow().sanitized()))
}

/// [`save_cfg`] 的纯函数版。保留别的文档的条目（读改写）。
pub fn save_cfg_at(path: &Path, uuid: &str, cfg: LayoutCfg) {
    let mut root: Map<String, Value> = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    root.insert(
        uuid.to_string(),
        json!({
            "font_size": round3(cfg.font_size),
            "line_spacing": round3(cfg.line_spacing),
            "margin": cfg.margin,
        }),
    );
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    match serde_json::to_string_pretty(&root) {
        Ok(s) => {
            if let Err(e) = std::fs::write(path, s) {
                eprintln!("[readcfg] 写 {} 失败：{e}", path.display());
            }
        }
        Err(e) => eprintln!("[readcfg] 序列化失败：{e}"),
    }
}

/// f32 → 保留 3 位小数的 f64：1.55 直接写 `1.55`，别写成 `1.5499999523…`。
fn round3(v: f32) -> f64 {
    ((v as f64) * 1000.0).round() / 1000.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    const W: i32 = 1404;
    const H: i32 = 1872;

    fn opened(cfg: LayoutCfg, has_ink: bool) -> TextCfg {
        let mut p = TextCfg::new();
        p.show(cfg, has_ink);
        p
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d =
            std::env::temp_dir().join(format!("rmdev-readcfg-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// 命中区全在屏内、两两不重叠（顶栏 + 5 + 3 + 3 个档位）。
    #[test]
    fn hits_fit_screen_and_do_not_overlap() {
        let p = opened(LayoutCfg::default(), false);
        let hs = p.hits(W, H);
        assert_eq!(
            hs.len(),
            2 + FONT_SIZES.len() + MARGINS.len() + LINE_SPACINGS.len()
        );
        for (r, act) in &hs {
            assert!(
                r.0 >= 0 && r.1 >= 0 && r.0 + r.2 <= W && r.1 + r.3 <= H,
                "{act:?} 命中区 {r:?} 出屏"
            );
            assert!(r.2 >= 80 && r.3 >= 80, "{act:?} 命中区 {r:?} 太小");
        }
        for i in 0..hs.len() {
            for j in i + 1..hs.len() {
                let (a, b) = (hs[i].0, hs[j].0);
                let overlap =
                    a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3;
                assert!(!overlap, "{:?} 和 {:?} 重叠", hs[i].1, hs[j].1);
            }
        }
    }

    /// 点档位只改草稿：`Margin(2)` 只动边距，字号/行距不动。
    #[test]
    fn picking_stops_updates_cfg_only() {
        let cfg = LayoutCfg {
            font_size: 21.0,
            line_spacing: 1.55,
            margin: 80,
        };
        let mut p = opened(cfg, false);
        assert_eq!(p.cfg(), cfg, "show() 后草稿 = 传入值");
        p.click(Act::Margin(2));
        assert_eq!(p.cfg().margin, MARGINS[2]);
        assert_eq!(p.cfg().font_size, 21.0);
        p.click(Act::Size(4));
        assert_eq!(p.cfg().font_size, FONT_SIZES[4]);
        p.click(Act::Spacing(0));
        assert_eq!(p.cfg().line_spacing, LINE_SPACINGS[0]);
        assert!(!p.is_confirming(), "还没点 Done，不该弹确认框");
        // 越界档位不动状态，也不崩
        let before = p.cfg();
        p.click(Act::Size(99));
        p.click(Act::Margin(99));
        p.click(Act::Spacing(99));
        assert_eq!(p.cfg(), before);
    }

    /// 有墨迹 + 改过 → `Done` 先进确认态；`Cancel` 把配置还原成打开时的值。
    #[test]
    fn ink_warns_before_done_and_cancel_restores() {
        let base = LayoutCfg::default();
        let mut p = opened(base, true);
        p.click(Act::Spacing(2));
        assert_eq!(p.click(Act::Done), None, "有墨迹要先确认");
        assert!(p.is_open() && p.is_confirming());
        // 确认态只剩两个按钮
        let hs = p.hits(W, H);
        assert_eq!(hs.len(), 2);
        assert!(hs.iter().any(|(_, a)| *a == Act::Confirm));
        assert!(hs.iter().any(|(_, a)| *a == Act::Cancel));
        assert_eq!(p.click(Act::Cancel), None);
        assert!(!p.is_open());
        assert_eq!(p.cfg(), base, "No, keep current settings 要还原");
    }

    /// 确认框点 `Yes` → 返回新配置；无墨迹时 `Done` 直接生效并关闭。
    #[test]
    fn confirm_applies_and_plain_done_is_direct() {
        let base = LayoutCfg::default();
        let mut p = opened(base, true);
        p.click(Act::Size(4));
        p.click(Act::Done);
        let out = p.click(Act::Confirm).expect("确认后应返回配置");
        assert_eq!(out.font_size, FONT_SIZES[4]);
        assert!(!p.is_open() && !p.is_confirming());

        let mut q = opened(base, false);
        q.click(Act::Size(0));
        assert_eq!(q.click(Act::Done), Some(q.cfg()));
        assert!(!q.is_open());
        // 有墨迹但根本没改 → 不打扰，直接关
        let mut r = opened(base, true);
        assert_eq!(r.click(Act::Done), Some(base));
        assert!(!r.is_confirming());
    }

    /// `Reset` 回默认值，`Done` 后才交给调用方。
    #[test]
    fn reset_returns_defaults() {
        let mut p = opened(
            LayoutCfg {
                font_size: 36.0,
                line_spacing: 1.85,
                margin: 120,
            },
            false,
        );
        p.click(Act::Reset);
        assert_eq!(p.cfg(), LayoutCfg::default());
        assert_eq!(p.click(Act::Done), Some(LayoutCfg::default()));
        assert!(!p.is_open());
    }

    /// 落盘往返：同一 uuid 读回同一个配置；别的 uuid 不受影响。
    #[test]
    fn save_and_load_roundtrip() {
        let dir = temp_dir("roundtrip");
        let path = dir.join("readcfg.json");
        let cfg = LayoutCfg {
            font_size: 30.0,
            line_spacing: 1.30,
            margin: 120,
        };
        assert_eq!(load_cfg_at(&path, "u1"), None, "文件还不存在");
        save_cfg_at(&path, "u1", cfg);
        assert_eq!(load_cfg_at(&path, "u1"), Some(cfg));
        // 落盘写法别带 f32 的二进制尾巴（1.30 就写 1.3）
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"line_spacing\": 1.3"), "{raw}");
        assert!(!raw.contains("1.29999"), "{raw}");
        assert_eq!(load_cfg_at(&path, "u2"), None);
        // 换一个文档写：u1 的条目要保留
        let other = LayoutCfg {
            font_size: 18.0,
            line_spacing: 1.85,
            margin: 50,
        };
        save_cfg_at(&path, "u2", other);
        assert_eq!(load_cfg_at(&path, "u1"), Some(cfg));
        assert_eq!(load_cfg_at(&path, "u2"), Some(other));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 边界：坏 JSON / 缺字段 / 非对象都当没有；坏文件还能被正常覆盖。
    #[test]
    fn broken_json_is_ignored_not_panicked() {
        let dir = temp_dir("broken");
        let path = dir.join("readcfg.json");
        std::fs::write(&path, "not json at all").unwrap();
        assert_eq!(load_cfg_at(&path, "u"), None);
        std::fs::write(&path, r#"{"u": {"font_size": 30.0}}"#).unwrap();
        assert_eq!(load_cfg_at(&path, "u"), None, "缺字段就不认");
        std::fs::write(&path, r#"{"u": 3}"#).unwrap();
        assert_eq!(load_cfg_at(&path, "u"), None);
        // 手改成离谱数值：读出来夹回合法范围，不 panic
        std::fs::write(
            &path,
            r#"{"u": {"font_size": 9999, "line_spacing": 0.1, "margin": -5}}"#,
        )
        .unwrap();
        let c = load_cfg_at(&path, "u").expect("能解析");
        assert!((8.0..=72.0).contains(&c.font_size));
        assert!((1.0..=3.0).contains(&c.line_spacing));
        assert!(c.margin >= 0);
        // 坏文件被 save 覆盖后恢复正常
        save_cfg_at(&path, "u", LayoutCfg::default());
        assert_eq!(load_cfg_at(&path, "u"), Some(LayoutCfg::default()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
