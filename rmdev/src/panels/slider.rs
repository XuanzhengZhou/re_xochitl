//! 文档页滑块（官方 `res_067887b.qml` 的 `pageSlider` / `hidePageSliderTimer` /
//! `fastScroll`，组件体在 `res_06519a7.qml`）。
//!
//! 官方几何（`res_06519a7.qml`，逐条对着抄）：
//! - 组件是 `MouseArea`，`height: 100`，在文档页里 `anchors { left: parent.left;
//!   right: parent.right; bottom: parent.bottom; bottomMargin: 50 }` —— 也就是
//!   **贴底一条 100 px 的横条**，底边距屏底 50 px；
//! - 条本体是 `Rectangle { height: 8; radius: 4; color: colorBlack }`，横向铺满，
//!   居中于那条 100 px（`navbar`）；
//! - 左右各有 `leftVisibleMargin` / `rightVisibleMargin`：工具条收起时 128/128，
//!   展开时 138/138（`interactiveMargin = toolbar.width * 0.55` 会把整体往右推，
//!   但算下来屏幕坐标仍是 138/138）。所以横向可拖范围是 `[128, w-128)`；
//! - `onPressed`：`pageThumbnailVisible = mouse.y >= 0 && mouse.y <= height`，
//!   `value = pageFromPos(mouse.x)`；`pageFromPos` 就是
//!   `round((x - margin) / visibleWidth * (pageCount - 1))` 再夹到 `[from, to]`；
//! - `onMouseYChanged`：`pageThumbnailVisible = mouse.y > -cancelThreshold`，
//!   `cancelThreshold: 200` —— **往上拖过 200 px 就算取消**：页号不再跟着走、
//!   缩略图和页码消失；`onReleased` 里 `if (pageThumbnailVisible) accepted()`，
//!   没按着就不跳页；
//! - 拖动时头顶浮一个 `PageThumbnail`（竖屏 615×820，`bottomMargin: 100`，外面套
//!   `thumbnailCover`：白底 + 2 px 黑框），上方一行 `Page %1`
//!   （`qsTr("Page %1").arg(slider.value + 1)`）；底部浮一条 `Page %1 of %2`；
//! - 松手后 `hidePageSliderTimer` 起表，`Values.popupTimeout`（4000 ms，见
//!   `res_06ba1de.qml:100`）到点自动收；按着的时候 `onTriggered` 里
//!   `if (!pageSlider.pressed)` 不收。
//!
//! 约定同其它面板：几何全是纯函数（[`bar`] / [`page_at_x`] / [`x_of`] /
//! [`next_state`] / [`thumb_rect`] / [`band_rect`] / [`label`] / [`page_label`]，
//! 都能单测），[`paint`] 只照着算好的几何画，不刷新屏幕 —— 刷哪块由调用方
//! （`main.rs` 的 `repaint_rects`）决定。

use crate::fb::{gray565, Fb};
use crate::ui::text::Text;
use crate::ui::widgets;

/// 横条高度（官方 `height: 100`）。
pub const H: i32 = 100;
/// 往上拖多少像素算取消（官方 `cancelThreshold: 200`）。
pub const CANCEL: i32 = 200;
/// 在条上按下之后往上滑多少才真的把滑块拉出来。官方是 `fastScroll` 的
/// `distance: 1.0 * touchArea.pixelsPerCm`（1 cm ≈ 60~90 px），而且
/// `releaseRequired: true`（滑完抬手才弹）。取 60 px、滑到就弹：少一次抬手，
/// 又不会把横向翻页（手抖往上飘十几像素）误判成拉滑块。
pub const OPEN_UP: i32 = 60;
/// 静置多久自动收。官方 `Values.popupTimeout` 是 4000 ms
/// （`res_06ba1de.qml:100`），1:1 照抄。
pub const HIDE_MS: u64 = 4000;

/// 条底边离屏幕底边（官方 `bottomMargin: 50`）。
const BOTTOM_GAP: i32 = 50;
/// 横向可拖范围左右各留多少（官方 128；工具条展开时是 138，差 10 px 看不出来，
/// 统一按收起的 128 算，正好让开 120 px 的工具条）。
const MARGIN: i32 = 128;
/// 工具条宽：白底从它右边开始铺，否则会把工具条最下面那格（文档设置）刷掉。
/// 官方 `pageSliderBackground` 是垫在 ToolBox **下面**的，我们这里画在它上面，
/// 所以得自己让开。
const TOOLBAR_W: i32 = 120;
/// 条本体厚度（官方 `navbar` 的 8 px，现在不用转 90° 了）。
const BAR_T: i32 = 8;
/// 游标半径：官方当前页指示点 `width: 35`（白底 + 2 px 黑边），半径 17。
const KNOB_R: i32 = 17;
/// 缩略图预览框（官方竖屏 `width: 615; height: 820`）。
pub const THUMB_W: i32 = 615;
pub const THUMB_H: i32 = 820;
/// 预览框离条顶边（官方 `bottomMargin: 100`）。
const THUMB_GAP: i32 = 100;
/// 条背景从条顶边往上一共多高（官方 `pageSliderBackground` 的 `topMargin: -100`）。
const BAND_UP: i32 = 100;

/// 底下一行 `Page N of M` 的字号：和页脚（`paint_footer`，34）保持一致，
/// 这样条浮出来的时候页脚不会"跳一下"。
const FONT_LBL: f32 = 34.0;
/// 预览框里那个大号 `Page N`。
const FONT_BIG: f32 = 64.0;

/// 纸色（官方 `pageSliderBackground` 是一块默认白 Rectangle）
const PAPER: u8 = 255;
/// 墨色（官方 `Values.colorBlack`）
const INK: u8 = 0;

/// 滑块状态。官方是 `visible` + `pressed` + `pageThumbnailVisible` 三个布尔，
/// 这里收成三个互斥态：
/// - `Idle`：条浮着、没按（等 [`HIDE_MS`] 自动收）；
/// - `Pressed`：手指按着，页号跟着 `x` 走、缩略图预览露出来；
/// - `Canceled`：往上拖过 [`CANCEL`]，预览收掉、页号冻住；松手不跳页。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Pressed,
    Canceled,
}

/// 那条 100 px 横条的 y 范围 `[top, bottom)`：贴底、底边留 50 px。
pub fn bar(h: i32) -> (i32, i32) {
    let bot = h - BOTTOM_GAP;
    (bot - H, bot)
}

/// 条后面那块白底的屏幕矩形：官方 `pageSliderBackground` 从条顶往上再 100 px
/// 一直铺到屏幕底。拖动 / 收起都只刷这一块（+ 预览框），不整屏。
pub fn band_rect(w: i32, h: i32) -> (i32, i32, i32, i32) {
    let (top, _) = bar(h);
    let y = top - BAND_UP;
    (0, y, w, h - y)
}

/// 缩略图预览框（官方 `PageThumbnail`：竖屏 615×820，水平居中，
/// 底边离条顶 100 px）。
pub fn thumb_rect(w: i32, h: i32) -> (i32, i32, i32, i32) {
    let (top, _) = bar(h);
    ((w - THUMB_W) / 2, top - THUMB_GAP - THUMB_H, THUMB_W, THUMB_H)
}

/// 在条上按下之后有没有往上滑够 [`OPEN_UP`]（条上按下 → 拉出滑块）。
pub fn opens_from(y0: f32, y: f32) -> bool {
    (y0 - y) >= OPEN_UP as f32
}

/// 手指在 `y` 时算不算"还在条上"。官方
/// `pageThumbnailVisible = mouse.y > -cancelThreshold`（`mouse.y` 相对条顶），
/// 换算到屏幕坐标就是 `y > bar_top - 200`。
pub fn next_state(y: i32, bar_top: i32) -> State {
    if y > bar_top - CANCEL {
        State::Pressed
    } else {
        State::Canceled
    }
}

/// 横向位置 `x` 对应第几页（1-based，夹在 `1..=total`）。
///
/// 官方 `pageFromPos`：`round((x - margin) / visibleWidth * (pageCount - 1))`，
/// 再 `Math.max(from, Math.min(to, ...))`。
pub fn page_at_x(x: i32, w: i32, total: usize) -> usize {
    if total <= 1 {
        return 1;
    }
    let span = (w - 2 * MARGIN).max(1);
    let f = ((x - MARGIN) as f64 / span as f64).clamp(0.0, 1.0);
    1 + (f * (total - 1) as f64).round() as usize
}

/// 第 `page` 页（1-based）的游标中心 x。是 [`page_at_x`] 的逆。
pub fn x_of(page: usize, w: i32, total: usize) -> i32 {
    if total <= 1 {
        return MARGIN;
    }
    let span = (w - 2 * MARGIN).max(1);
    let p = page.clamp(1, total) - 1;
    MARGIN + (span as f64 * p as f64 / (total - 1) as f64).round() as i32
}

/// 条上那行页码。官方 `res_067887b.qml:1808`：`qsTr("Page %1 of %2")`，
/// 参数是 `currentPage + 1` 和 `pageCount`。
pub fn label(page: usize, total: usize) -> String {
    format!("Page {page} of {total}")
}

/// 预览框里那行页码。官方 `res_06519a7.qml` 的 `labelColumn`：
/// `qsTr("Page %1").arg(slider.value + 1)`。
pub fn page_label(page: usize) -> String {
    format!("Page {page}")
}

/// 照着算好的几何画：白底横条 + 8 px 黑条 + 圆游标，底下一行 `Page N of M`；
/// `dragging`（按着、且没被往上拖取消）时上面再浮一个预览框：有缩略图就贴图，
/// 没有就画一个大号 `Page N`（官方那边是 `Loading` 圈 + `Page %1`）。
///
/// 只写帧缓冲，**不刷新**：调用方拿 [`band_rect`] / [`thumb_rect`] 去
/// `repaint_rects`，所以拖得多快都只刷底部那一条，正文一笔都不重画。
pub fn paint(
    fb: &mut Fb,
    text: &mut Option<Text>,
    thumb: Option<(&[u8], i32, i32)>,
    w: i32,
    h: i32,
    page: usize,
    total: usize,
    dragging: bool,
) {
    let total = total.max(1);
    let page = page.clamp(1, total);
    let (top, bot) = bar(h);
    if w <= 2 * MARGIN || bot <= top {
        return;
    }
    let (_, by, bw, bh) = band_rect(w, h);
    // 白底：官方 `pageSliderBackground`。从工具条右边铺起（它是垫在 ToolBox
    // 下面的，我们画在它上面，得自己让开）。
    if bw > TOOLBAR_W {
        fb.fill_rect(TOOLBAR_W, by, bw - TOOLBAR_W, bh, gray565(PAPER));
    }
    // 预览框：官方 `PageThumbnail` + `thumbnailCover`（白底 + 2 px 黑框）
    if dragging {
        let (tx, ty, tw, th) = thumb_rect(w, h);
        fb.fill_rect(tx, ty, tw, th, gray565(PAPER));
        match thumb {
            Some((px, iw, ih)) if iw > 0 && ih > 0 && px.len() >= (iw * ih) as usize => {
                fb.blit_gray(tx + (tw - iw) / 2, ty + (th - ih) / 2, iw, ih, px);
            }
            _ => {
                if let Some(t) = text.as_mut() {
                    let s = page_label(page);
                    let sw = t.width(FONT_BIG, &s) as i32;
                    t.draw(fb, tx + (tw - sw) / 2, ty + th / 2 + 22, FONT_BIG, &s, INK);
                }
            }
        }
        // 边框最后画：贴图哪怕大一两像素也被压在框下面
        widgets::outline_rect(fb, tx, ty, tw, th, 2, INK);
    }
    // 条本体：官方 `navbar`（height 8 / radius 4 / colorBlack），居中于条带
    let cy = (top + bot) / 2;
    fb.fill_rect(
        MARGIN,
        cy - BAR_T / 2,
        (w - 2 * MARGIN).max(1),
        BAR_T,
        gray565(INK),
    );
    // 游标：官方当前页指示点（白底 + 2 px 黑边）。官方拖动时这个点会隐掉、
    // 只留缩略图；我们没有实时缩略图，游标跟着手指走才看得清停在哪一页。
    let cx = x_of(page, w, total);
    widgets::disc(fb, cx, cy, KNOB_R, PAPER);
    widgets::ring(fb, cx, cy, KNOB_R, 2, INK);
    // 页码：和页脚同一条基线（`paint_footer` 的 1846），条浮出来时不跳
    if let Some(t) = text.as_mut() {
        let s = label(page, total);
        let sw = t.width(FONT_LBL, &s) as i32;
        t.draw(fb, (w - sw) / 2, bot + 24, FONT_LBL, &s, INK);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机尺寸：1404×1872。
    const TW: i32 = 1404;
    const TH: i32 = 1872;

    #[test]
    fn label_is_the_official_wording() {
        // res_067887b.qml:1808 `qsTr("Page %1 of %2")`
        assert_eq!(label(3, 41), "Page 3 of 41");
        assert_eq!(label(1, 1), "Page 1 of 1");
        assert_eq!(label(100, 200), "Page 100 of 200");
        // res_06519a7.qml labelColumn `qsTr("Page %1").arg(slider.value + 1)`
        assert_eq!(page_label(21), "Page 21");
        assert_eq!(page_label(1), "Page 1");
    }

    #[test]
    fn bar_is_a_100px_strip_floating_50px_above_the_bottom() {
        let (top, bot) = bar(TH);
        assert_eq!(H, 100);
        assert_eq!(bot - top, H);
        assert_eq!(TH - bot, 50, "官方 bottomMargin: 50");
        assert_eq!((top, bot), (1722, 1822));
        // 页脚 `Page N of M` 基线 1846 落在条下面，不打架
        assert!(bot < 1846);
    }

    #[test]
    fn band_covers_the_bar_plus_100px_above_it() {
        let (x, y, w, hh) = band_rect(TW, TH);
        let (top, bot) = bar(TH);
        assert_eq!((x, y, w), (0, top - 100, TW));
        assert_eq!(y + hh, TH, "白底一直铺到屏幕底");
        assert!(y < top && top < bot && bot < y + hh);
    }

    #[test]
    fn thumb_rect_is_the_official_615x820_above_the_bar() {
        let (x, y, w, hh) = thumb_rect(TW, TH);
        let (top, _) = bar(TH);
        assert_eq!((w, hh), (THUMB_W, THUMB_H));
        assert_eq!(y + hh, top - 100, "官方 bottomMargin: 100");
        assert_eq!(x, (TW - THUMB_W) / 2, "水平居中");
        // 整框在屏内，且正好贴在白底顶边（一个像素都不叠）
        assert!(y >= 0 && x >= 0 && x + w <= TW);
        assert_eq!(y + hh, band_rect(TW, TH).1);
    }

    #[test]
    fn page_at_x_clamps_at_both_ends() {
        // 官方 `Math.max(from, Math.min(to, ...))`：滑出条外也只给 1..=total
        assert_eq!(page_at_x(-9999, TW, 41), 1);
        assert_eq!(page_at_x(0, TW, 41), 1);
        assert_eq!(page_at_x(MARGIN, TW, 41), 1);
        assert_eq!(page_at_x(TW / 2, TW, 41), 21);
        assert_eq!(page_at_x(TW - MARGIN, TW, 41), 41);
        assert_eq!(page_at_x(TW, TW, 41), 41);
        assert_eq!(page_at_x(9999, TW, 41), 41);
    }

    #[test]
    fn page_at_x_round_trips_through_x_of() {
        for total in [2usize, 3, 41, 300] {
            for p in 1..=total {
                let x = x_of(p, TW, total);
                assert_eq!(page_at_x(x, TW, total), p, "total={total} page={p}");
            }
        }
    }

    #[test]
    fn one_page_documents_never_divide_by_zero() {
        assert_eq!(page_at_x(TW / 2, TW, 1), 1);
        assert_eq!(page_at_x(0, TW, 0), 1);
        assert_eq!(x_of(1, TW, 1), MARGIN);
        assert_eq!(x_of(5, TW, 0), MARGIN);
    }

    #[test]
    fn opening_needs_a_real_upward_swipe() {
        // 横向翻页时手往上飘十几像素不算"从底部上滑"
        assert!(!opens_from(1800.0, 1790.0));
        assert!(!opens_from(1800.0, 1741.0));
        assert!(opens_from(1800.0, 1740.0));
        assert!(opens_from(1800.0, 1600.0));
        // 往下滑永远不算
        assert!(!opens_from(1800.0, 1860.0));
    }

    #[test]
    fn dragging_up_past_200px_cancels_and_coming_back_resumes() {
        let (top, _) = bar(TH);
        // 官方 `pageThumbnailVisible = mouse.y > -cancelThreshold`（严格大于）
        assert_eq!(next_state(top, top), State::Pressed);
        assert_eq!(next_state(top + 50, top), State::Pressed);
        assert_eq!(next_state(top - 199, top), State::Pressed);
        assert_eq!(next_state(top - 200, top), State::Canceled);
        assert_eq!(next_state(top - 400, top), State::Canceled);
        assert_eq!(next_state(0, top), State::Canceled);
        // 拖回来又算按着（官方那两个属性也是这么互相翻的）
        assert_eq!(next_state(top - 100, top), State::Pressed);
    }

    #[test]
    fn x_of_keeps_the_knob_inside_the_drag_range() {
        for total in [2usize, 41, 300] {
            for p in 1..=total {
                let x = x_of(p, TW, total);
                assert!(
                    (MARGIN..=TW - MARGIN).contains(&x),
                    "total={total} page={p} x={x} 跑出可拖范围"
                );
            }
        }
        assert_eq!(x_of(1, TW, 41), MARGIN);
        assert_eq!(x_of(41, TW, 41), TW - MARGIN);
    }

    /// `paint` 冒烟：**只动底部那条**（`band_rect`），上面（含工具条、纸面）
    /// 一个像素都不许改；`dragging` 时另外只多动预览框那一块。
    #[test]
    fn paint_only_touches_the_bottom_band_and_the_preview() {
        let mut fb = Fb::open_mem();
        let (w, h) = (fb.width, fb.height);
        let mut none: Option<Text> = None;
        fb.fill_rect(0, 0, w, h, gray565(255));
        paint(&mut fb, &mut none, None, w, h, 21, 41, true);

        let (_, band_y, _, _) = band_rect(w, h);
        let (tx, ty, tw, th) = thumb_rect(w, h);
        let inked = |fb: &Fb, x0: i32, y0: i32, ww: i32, hh: i32| {
            (y0..y0 + hh).any(|y| (x0..x0 + ww).any(|x| fb.get(x, y) < 128))
        };
        // 条里必须有墨（黑条 + 游标框）
        assert!(inked(&fb, MARGIN, band_y, w - 2 * MARGIN, h - band_y));
        // 白底之上、预览框之外，全屏一个像素都不许动
        for y in (0..band_y).step_by(37) {
            for x in (0..w).step_by(97) {
                if x >= tx && x < tx + tw && y >= ty && y < ty + th {
                    continue;
                }
                assert_eq!(fb.get(x, y), 255, "({x},{y}) 在底部那条上面，被画脏了");
            }
        }
        // 工具条那一列（白底从 120 才开始铺）也不能动
        assert_eq!(fb.get(60, band_y + 10), 255, "把工具条最下面那格刷掉了");
        // 预览框本身有墨（边框 / 中间那个大号页码）
        assert!(inked(&fb, tx, ty, tw, th), "预览框里一个黑像素都没有");
    }

    /// 没按着（`dragging = false`）只画条，预览框区域一像素不动 —— 松手后那块
    /// 由调用方拿 `thumb_rect` 去 `repaint_rects`，从纸面补回来。
    #[test]
    fn idle_state_draws_no_preview() {
        let mut fb = Fb::open_mem();
        let (w, h) = (fb.width, fb.height);
        let mut none: Option<Text> = None;
        fb.fill_rect(0, 0, w, h, gray565(255));
        paint(&mut fb, &mut none, None, w, h, 21, 41, false);
        let (tx, ty, tw, th) = thumb_rect(w, h);
        assert!(
            (ty..ty + th).all(|y| (tx..tx + tw).all(|x| fb.get(x, y) == 255)),
            "没按着不该画预览框"
        );
        // 但条还在（3~4 s 内看得见停在哪一页）
        let (_, band_y, _, _) = band_rect(w, h);
        assert!((band_y..h).any(|y| (0..w).any(|x| fb.get(x, y) < 128)));
    }

    /// 有缩略图时贴进预览框：整块贴图应该出现在框里（居中）。
    #[test]
    fn paint_blits_a_thumbnail_into_the_preview_frame() {
        let mut fb = Fb::open_mem();
        let (w, h) = (fb.width, fb.height);
        let mut none: Option<Text> = None;
        fb.fill_rect(0, 0, w, h, gray565(255));
        let (tw, th) = (40i32, 50i32);
        let px = vec![0u8; (tw * th) as usize]; // 全黑
        paint(&mut fb, &mut none, Some((&px, tw, th)), w, h, 7, 41, true);
        let (tx, ty, fw, fh) = thumb_rect(w, h);
        let ox = tx + (fw - tw) / 2;
        let oy = ty + (fh - th) / 2;
        assert_eq!(fb.get(ox + tw / 2, oy + th / 2), 0, "缩略图没贴到框中间");
        // 贴图之外还是白的
        assert_eq!(fb.get(ox - 3, oy + th / 2), 255);
        assert_eq!(fb.get(ox + tw / 2, oy - 3), 255);
    }
}
