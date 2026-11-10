//! 关机确认框（官方 `res_06b4fb3.qml` PowerOffDialog）。
//!
//! 结构（官方原样，`Values` 常量见 `Values.qml`）：
//!
//! ```text
//! FontIcon icon_power   size = iconSizeMedium(40)   top + navigatorSettingsItemSecondMargin(200)
//! TextHeaderLarge       宽 = 屏宽/2，居中            top = 图标底 + navigatorHeaderTitleMargin(25)
//!   "Do you want to turn off your reMarkable?"
//! 两个方按钮            宽 = 屏宽/4(351)、高 = 宽/4(87.75)
//!   左：黑底白字 "Turn off"   right = 屏宽/2 - navigatorSidebarItemMargin(22)
//!   右：白底黑框 "Cancel"     left  = 屏宽/2 + 22
//!   两者的 bottom = 屏高/2 - navigatorSettingsItemSecondMargin(200)
//! ```
//!
//! 长按电源键 3 s 弹它（`poweroffConfirmationTimer`），点 `Turn off` 才真关机。
//! `reboot` 那一档只有零售演示模式会用到，没做。

use crate::fb::{gray565, Fb};
use crate::ui::text::Text;

/// icomoon 里的电源符号（`Icons.icon_power`）。
///
/// 码位是从设备上的 `icomoon.ttf` 里逐字形渲染出来认的：`e963` 才是
/// "缺口的圆 + 中间一竖"那个电源符号（`e964` 是下载托盘）。
pub const ICON_POWER: char = '\u{e963}';
/// 官方 `Values.iconSizeMedium`
pub const ICON_SIZE: i32 = 40;
/// 官方 `Values.navigatorSettingsItemSecondMargin`
pub const SECOND_MARGIN: i32 = 200;
/// 官方 `Values.navigatorHeaderTitleMargin`
pub const TITLE_MARGIN: i32 = 25;
/// 官方 `Values.navigatorSidebarItemMargin`
pub const SIDE_MARGIN: i32 = 22;

/// 标题（官方一句话，按 702 宽折成两行；这里写死，保证版面和官方一致）
pub const TITLE_LINES: [&str; 2] = ["Do you want to turn off your", "reMarkable?"];
pub const TURN_OFF: &str = "Turn off";
pub const CANCEL: &str = "Cancel";

/// 字号：标题是 `TextHeaderLarge`（= 设置页大标题的 38），按钮是普通粗体。
const FONT_TITLE: f32 = 38.0;
const FONT_BTN: f32 = 25.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    TurnOff,
    Cancel,
}

/// 两个按钮的矩形 `(x, y, w, h)`：左 = Turn off，右 = Cancel。
///
/// 纯布局，可单测——这些数字全是从官方 QML 的 anchor 关系算出来的，
/// 不是照着截图量的，所以换屏（M1/M2）也仍然成立。
pub fn buttons(w: i32, h: i32) -> ((i32, i32, i32, i32), (i32, i32, i32, i32)) {
    let bw = w / 4;
    let bh = bw / 4;
    let bottom = h / 2 - SECOND_MARGIN;
    let y = bottom - bh;
    let off = (w / 2 - SIDE_MARGIN - bw, y, bw, bh);
    let cancel = (w / 2 + SIDE_MARGIN, y, bw, bh);
    (off, cancel)
}

/// 命中测试。
pub fn hit(x: f32, y: f32, w: i32, h: i32) -> Option<Act> {
    let (off, cancel) = buttons(w, h);
    for (r, act) in [(off, Act::TurnOff), (cancel, Act::Cancel)] {
        if x >= r.0 as f32 && x < (r.0 + r.2) as f32 && y >= r.1 as f32 && y < (r.1 + r.3) as f32 {
            return Some(act);
        }
    }
    None
}

/// 画整页（白底）。`fb` 的尺寸就是屏尺寸。
pub fn paint(fb: &mut Fb, text: &mut Option<Text>, w: i32, h: i32) {
    fb.fill_rect(0, 0, w, h, gray565(255));

    let cx = w / 2;
    if let Some(t) = text.as_mut() {
        // 电源图标：官方 `top: parent.top + 200`，40×40，水平居中
        t.icon(fb, cx, SECOND_MARGIN + ICON_SIZE / 2, ICON_SIZE, ICON_POWER, 0);

        // 标题两行
        for (i, line) in TITLE_LINES.iter().enumerate() {
            let tw = t.width_bold(FONT_TITLE, line);
            let base = SECOND_MARGIN
                + ICON_SIZE
                + TITLE_MARGIN
                + (FONT_TITLE * 0.78) as i32
                + i as i32 * (FONT_TITLE * 1.25) as i32;
            t.draw_bold(
                fb,
                (w as f32 / 2.0 - tw / 2.0) as i32,
                base,
                FONT_TITLE,
                line,
                0,
            );
        }

        let (off, cancel) = buttons(w, h);
        // Turn off：黑底白字
        fb.fill_rect(off.0, off.1, off.2, off.3, gray565(0));
        let tw = t.width_bold(FONT_BTN, TURN_OFF);
        t.draw_bold(
            fb,
            off.0 + ((off.2 as f32 - tw) / 2.0) as i32,
            off.1 + off.3 / 2 + (FONT_BTN * 0.36) as i32,
            FONT_BTN,
            TURN_OFF,
            255,
        );
        // Cancel：白底黑框
        crate::ui::widgets::outline_rect(fb, cancel.0, cancel.1, cancel.2, cancel.3, 2, 0);
        let tw = t.width_bold(FONT_BTN, CANCEL);
        t.draw_bold(
            fb,
            cancel.0 + ((cancel.2 as f32 - tw) / 2.0) as i32,
            cancel.1 + cancel.3 / 2 + (FONT_BTN * 0.36) as i32,
            FONT_BTN,
            CANCEL,
            0,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 官方 anchor 关系算出来的几何，和截图量的结果一致：
    /// 351×87 的方按钮，分居中线两侧 22 px，底边在屏高一半往上 200 px。
    #[test]
    fn buttons_match_the_official_anchors() {
        let (off, cancel) = buttons(1404, 1872);
        // 官方高是 87.75（`width / 4` 是实数），我们取整成 87，
        // 所以顶边比实数版低 1 px——底边精确对齐（那才是 anchor 定死的边）。
        assert_eq!(off, (329, 649, 351, 87));
        assert_eq!(cancel, (724, 649, 351, 87));
        // 两个按钮之间正好留出 2 × 22 px 的缝
        assert_eq!(cancel.0 - (off.0 + off.2), 44);
        // 左右对称
        assert_eq!(off.0, 1404 - (cancel.0 + cancel.2));
        // 底边 = 屏高/2 - 200
        assert_eq!(off.1 + off.3, 1872 / 2 - SECOND_MARGIN);
    }

    #[test]
    fn hit_test_covers_both_buttons_and_nothing_else() {
        let (w, h) = (1404, 1872);
        let (off, cancel) = buttons(w, h);
        let mid = |r: (i32, i32, i32, i32)| {
            (r.0 as f32 + r.2 as f32 / 2.0, r.1 as f32 + r.3 as f32 / 2.0)
        };
        assert_eq!(hit(mid(off).0, mid(off).1, w, h), Some(Act::TurnOff));
        assert_eq!(hit(mid(cancel).0, mid(cancel).1, w, h), Some(Act::Cancel));
        // 边角外、标题区、按钮之间都不算
        assert_eq!(hit(1.0, 1.0, w, h), None);
        assert_eq!(hit(off.0 as f32 - 1.0, mid(off).1, w, h), None);
        assert_eq!(hit(off.0 as f32 + off.2 as f32, mid(off).1, w, h), None);
        assert_eq!(hit(702.0, 300.0, w, h), None);
    }

    /// 画出来：整页白、左边按钮黑块、右边按钮只有 2 px 黑框。
    #[test]
    fn paint_draws_a_black_button_and_an_outlined_one() {
        let mut fb = Fb::open_mem();
        let mut text = Text::load();
        let (w, h) = (fb.width, fb.height);
        paint(&mut fb, &mut text, w, h);
        let (off, cancel) = buttons(w, h);
        assert_eq!(fb.get(off.0 + 5, off.1 + 5), 0, "Turn off 是黑底");
        assert_eq!(fb.get(cancel.0, cancel.1), 0, "Cancel 的 2px 边框");
        assert_eq!(fb.get(cancel.0 + 1, cancel.1 + 1), 0, "边框厚度是 2 px");
        assert_eq!(fb.get(cancel.0 + 3, cancel.1 + 3), 255, "边框里侧是白的");
        assert_eq!(
            fb.get(cancel.0 + cancel.2 / 2, cancel.1 + cancel.3 - 6),
            255,
            "Cancel 里面是白的"
        );
        assert_eq!(fb.get(5, 5), 255, "整页白底");
        if text.is_some() {
            // 标题那一带要有墨
            let dark = (280..360)
                .flat_map(|y| (300..1100).map(move |x| (x, y)))
                .filter(|&(x, y)| fb.get(x, y) < 128)
                .count();
            assert!(dark > 800, "标题没画出来（暗像素 {dark}）");
        }
    }
}
