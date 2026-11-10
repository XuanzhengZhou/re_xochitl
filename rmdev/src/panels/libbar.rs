//! 库文档多选的动作条（官方 `res_06a18e1.qml` 的 `NavigatorModalAction`）。
//!
//! 长按进选择态；顶栏 `Cancel` + 选中数 + `Select all` / `Deselect all`，
//! 底部动作条按选中内容显隐。显隐规则照官方 `currentActions()`：
//!
//! * 回收站里只有 `Restore` 与 `Delete`（官方 `res_06953193.qml:41-46`）；
//! * `Upload` 只在**没选文件夹**时出现（官方 `!includesFolders`）；
//! * `Rename` / `Duplicate` / `Send` 只在**恰好一个**条目时出现（官方
//!   `size === 1`；`Duplicate` / `Send` 还要求不是文件夹）；
//! * `Tag` / `Favorite` / `Move` / `Move to trash` 只要选中有东西就在。
//!
//! 文案：`Duplicate` / `Move` / `Tag` / `Send` / `Upload` / `Restore` /
//! `Favorite` / `Rename` / `Move to trash` / `Delete` 全部照官方
//! `name: qsTr(...)` 原样抄。顶栏的 `Select all` / `Deselect all` / `Cancel`
//! 由任务契约给定（官方的 `NavigatorModalActionBase.qml` 不在 carve 里；
//! 能查到的只有 `res_06a644c.qml` 页多选的 `Select all` / `Select none`，
//! 库多选这版用 `Deselect all`）。
//!
//! 布局差异（已知）：官方那排是**顶栏**的图标按钮（`ButtonHeader`，`icon` +
//! `title`），我们做成**底部**纯文字条；八项在 1404 宽里每项 175px，
//! `paint_bar` 会逐档降字号把长文案塞进格子。
//!
//! 约定同 `pagesel`：`items()` / `hits()` 是纯布局（可单测），`paint()` 只照着画。

use crate::fb::{gray565, Fb};
use crate::ui::text::Text;

/// 顶栏高（和库的 `HEADER_H` 一致）
pub const HDR_H: i32 = 120;
/// 选择态盖住的那条标题带（库的 `GRID_TOP`：顶栏 120 + 标题行 135）
pub const BAND_H: i32 = 255;
/// 底部动作条高（官方 `NavigatorModalActionBase` 是 120）
pub const BAR_H: i32 = 120;
/// 顶栏左边的 `Cancel` 命中区宽
const CANCEL_W: i32 = 220;
/// 顶栏右边的全选按钮命中区宽
const ALL_W: i32 = 320;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    Duplicate,
    Move,
    Tag,
    Send,
    Upload,
    Favorite,
    Rename,
    /// 移到回收站（官方 `Move to trash`）
    Trash,
    /// 回收站里的"永久删除"（官方 `Delete`，只出现在回收站视图）
    Delete,
    Restore,
    SelectAll,
    DeselectAll,
    Cancel,
}

/// 底部动作条：按选中内容显隐，顺序照官方 `currentActions()`
/// （`res_06953193.qml:74-97`）。
///
/// `n_docs` / `n_folders` 是当前选中集合里文档和文件夹的个数；`in_trash`
/// 表示回收站视图。选中 0 项 = 没进选择态，动作条不画。
pub fn items(n_docs: usize, n_folders: usize, in_trash: bool) -> Vec<(Act, &'static str)> {
    let n = n_docs + n_folders;
    if n == 0 {
        return Vec::new();
    }
    // 官方回收站分支（`res_06953193.qml:41-46`）：`Restore` 与 `Delete` 两项
    if in_trash {
        return vec![(Act::Restore, "Restore"), (Act::Delete, "Delete")];
    }
    let mut v = Vec::with_capacity(8);
    // Upload：有文件夹就不能进（官方 `!includesFolders`）
    if n_folders == 0 {
        v.push((Act::Upload, "Upload"));
    }
    v.push((Act::Tag, "Tag"));
    // 官方 name 恒为 "Favorite"，只有图标随"是否全收藏"变（`favoriteIcon()`）
    v.push((Act::Favorite, "Favorite"));
    if n == 1 {
        v.push((Act::Rename, "Rename"));
        // Duplicate / Send：单个**文档**才有，文件夹不行
        if n_folders == 0 {
            v.push((Act::Duplicate, "Duplicate"));
            v.push((Act::Send, "Send"));
        }
    }
    v.push((Act::Move, "Move"));
    v.push((Act::Trash, "Move to trash"));
    v
}

/// 顶栏那个下划线按钮该发哪个动作。
pub fn all_action(all_selected: bool) -> Act {
    if all_selected {
        Act::DeselectAll
    } else {
        Act::SelectAll
    }
}

/// 顶栏那个下划线按钮的文案。
pub fn all_label(all_selected: bool) -> &'static str {
    if all_selected {
        "Deselect all"
    } else {
        "Select all"
    }
}

/// 顶栏标题：选中数。
pub fn title(n: usize) -> String {
    format!("{n} selected")
}

/// 本期可点区域，顺序 = 绘制顺序（后压的优先）。
pub fn hits(
    n_docs: usize,
    n_folders: usize,
    in_trash: bool,
    all_selected: bool,
    w: i32,
    h: i32,
) -> Vec<((i32, i32, i32, i32), Act)> {
    if n_docs + n_folders == 0 {
        return Vec::new();
    }
    let mut v = Vec::with_capacity(10);
    // 顶栏：左 Cancel、右全选/取消全选
    v.push(((0, 0, CANCEL_W, HDR_H), Act::Cancel));
    v.push((
        (w - 33 - ALL_W, 0, ALL_W, HDR_H),
        all_action(all_selected),
    ));
    // 底部动作条：等分
    let list = items(n_docs, n_folders, in_trash);
    let by = h - BAR_H;
    let n = list.len() as i32;
    for (i, (act, _)) in list.iter().enumerate() {
        let x0 = w * i as i32 / n;
        let x1 = w * (i as i32 + 1) / n;
        v.push(((x0, by, x1 - x0, BAR_H), *act));
    }
    v
}

/// 命中测试（后画的优先，和别的面板一致）。
pub fn hit(
    n_docs: usize,
    n_folders: usize,
    in_trash: bool,
    all_selected: bool,
    x: f32,
    y: f32,
    w: i32,
    h: i32,
) -> Option<Act> {
    hits(n_docs, n_folders, in_trash, all_selected, w, h)
        .into_iter()
        .rev()
        .find(|(r, _)| {
            x >= r.0 as f32
                && y >= r.1 as f32
                && x < (r.0 + r.2) as f32
                && y < (r.1 + r.3) as f32
        })
        .map(|(_, a)| a)
}

/// 选择态的整块 UI：顶栏 + 底部动作条。
pub fn paint(
    fb: &mut Fb,
    text: &mut Option<Text>,
    n_docs: usize,
    n_folders: usize,
    in_trash: bool,
    all_selected: bool,
    w: i32,
    h: i32,
) {
    paint_header(
        fb,
        text,
        n_docs + n_folders,
        all_selected,
        w,
    );
    paint_bar(fb, text, n_docs, n_folders, in_trash, w, h);
}

/// 顶栏：黑条 `Cancel` + 标题（选中数）+ `Select all` / `Deselect all`。
///
/// 黑条只占 120，下面 120..255 铺白：进选择态后库的标题行不再画
/// （`draw_library_top` 直接 return），这块得有人盖住。
pub fn paint_header(
    fb: &mut Fb,
    text: &mut Option<Text>,
    n: usize,
    all_selected: bool,
    w: i32,
) {
    fb.fill_rect(0, 0, w, BAND_H, gray565(255));
    fb.fill_rect(0, 0, w, HDR_H, gray565(0));
    if let Some(t) = text.as_mut() {
        t.draw_bold(fb, 33, 73, 32.0, "Cancel", 250);
        let title = title(n);
        let tw = t.width_bold(34.0, &title) as i32;
        t.draw_bold(fb, (w - tw) / 2, 73, 34.0, &title, 250);
        let label = all_label(all_selected);
        let lw = t.width_bold(30.0, label) as i32;
        let x = w - 33 - lw;
        t.draw_bold(fb, x, 74, 30.0, label, 250);
        // 官方那行是下划线文字按钮（同页多选的 `TextHeaderSmall`）
        fb.fill_rect(x, 82, lw, 2, gray565(250));
    }
}

/// 底部动作条：黑底、各项等分、白字居中；文案/项数全来自 [`items`]。
pub fn paint_bar(
    fb: &mut Fb,
    text: &mut Option<Text>,
    n_docs: usize,
    n_folders: usize,
    in_trash: bool,
    w: i32,
    h: i32,
) {
    let list = items(n_docs, n_folders, in_trash);
    if list.is_empty() {
        return;
    }
    let by = h - BAR_H;
    fb.fill_rect(0, by, w, BAR_H, gray565(0));
    let n = list.len() as i32;
    for (i, (_, label)) in list.iter().enumerate() {
        let x0 = w * i as i32 / n;
        let x1 = w * (i as i32 + 1) / n;
        if let Some(t) = text.as_mut() {
            // 八项时分到的格子只有 175 宽，`Move to trash` 在 32px 下放不下：
            // 逐档降字号直到塞得进格子（官方那排是图标按钮，我们只有文字）。
            let cell = x1 - x0;
            let mut size = 32.0f32;
            let mut tw = t.width_bold(size, label) as i32;
            while tw > cell - 16 && size > 16.0 {
                size -= 2.0;
                tw = t.width_bold(size, label) as i32;
            }
            t.draw_bold(fb, x0 + (cell - tw).max(0) / 2, by + 76, size, label, 250);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actions(v: &[(Act, &'static str)]) -> Vec<Act> {
        v.iter().map(|(a, _)| *a).collect()
    }

    /// 选中 0 项 = 没进选择态：动作条和命中区都不该有。
    #[test]
    fn empty_selection_has_no_bar() {
        assert!(items(0, 0, false).is_empty());
        assert!(items(0, 0, true).is_empty());
        assert!(hits(0, 0, false, false, 1404, 1872).is_empty());
        assert_eq!(hit(0, 0, false, false, 700.0, 1800.0, 1404, 1872), None);
    }

    /// 只有一个文档：官方八项全在，顺序照 `currentActions()`。
    #[test]
    fn single_document_shows_all_eight_actions() {
        let v = items(1, 0, false);
        assert_eq!(
            actions(&v),
            vec![
                Act::Upload,
                Act::Tag,
                Act::Favorite,
                Act::Rename,
                Act::Duplicate,
                Act::Send,
                Act::Move,
                Act::Trash,
            ]
        );
        assert_eq!(
            v.iter().map(|(_, l)| *l).collect::<Vec<_>>(),
            vec![
                "Upload",
                "Tag",
                "Favorite",
                "Rename",
                "Duplicate",
                "Send",
                "Move",
                "Move to trash",
            ]
        );
        assert!(!actions(&v).contains(&Act::Restore));
        assert!(!actions(&v).contains(&Act::Delete));
    }

    /// 多选文档：Rename / Duplicate / Send 消失（官方要求 `size === 1`）。
    #[test]
    fn multiple_documents_hide_single_only_actions() {
        let v = items(3, 0, false);
        assert_eq!(
            actions(&v),
            vec![
                Act::Upload,
                Act::Tag,
                Act::Favorite,
                Act::Move,
                Act::Trash,
            ]
        );
    }

    /// 文件夹：Upload 不能进（官方 `includesFolders`），单文件夹也没有
    /// Duplicate / Send；Tag / Favorite / Rename / Move / Move to trash 都在。
    #[test]
    fn folders_block_upload_and_single_only_actions() {
        let one = actions(&items(0, 1, false));
        assert_eq!(
            one,
            vec![
                Act::Tag,
                Act::Favorite,
                Act::Rename,
                Act::Move,
                Act::Trash,
            ]
        );

        let mixed = actions(&items(1, 1, false));
        assert_eq!(mixed, vec![Act::Tag, Act::Favorite, Act::Move, Act::Trash]);
    }

    /// 回收站里两项：`Restore` 与 `Delete`（官方 41-46 行）。
    #[test]
    fn trash_offers_restore_and_delete() {
        for (d, f) in [(1, 0), (0, 1), (2, 3)] {
            assert_eq!(
                items(d, f, true),
                vec![(Act::Restore, "Restore"), (Act::Delete, "Delete")]
            );
        }
    }

    /// 顶栏：没全选是 `Select all`，全选后变 `Deselect all`；标题带选中数。
    #[test]
    fn header_labels_follow_state() {
        assert_eq!(all_action(false), Act::SelectAll);
        assert_eq!(all_action(true), Act::DeselectAll);
        assert_eq!(all_label(false), "Select all");
        assert_eq!(all_label(true), "Deselect all");
        assert_eq!(title(1), "1 selected");
        assert_eq!(title(12), "12 selected");
    }

    /// 命中区：顶栏两个按钮 + 底条等分；都在屏内、互不重叠、≥80×80；
    /// 中点能命中对应动作，正文区域不算面板命中。
    #[test]
    fn hits_layout_and_dispatch() {
        let (w, h) = (1404, 1872);
        for (d, f, trash, all) in [
            (1, 0, false, false),
            (2, 0, false, true),
            (0, 1, false, false),
            (1, 1, true, false),
        ] {
            let hs = hits(d, f, trash, all, w, h);
            let expect = 2 + items(d, f, trash).len();
            assert_eq!(hs.len(), expect, "{d}/{f}/{trash} 命中区个数");
            for (r, act) in &hs {
                assert!(r.2 >= 80 && r.3 >= 80, "{act:?} 命中区 {r:?} 太小");
                assert!(r.0 >= 0 && r.0 + r.2 <= w, "{act:?} 出屏");
                assert!(r.1 >= 0 && r.1 + r.3 <= h, "{act:?} 出屏");
                let cx = (r.0 + r.2 / 2) as f32;
                let cy = (r.1 + r.3 / 2) as f32;
                assert_eq!(
                    hit(d, f, trash, all, cx, cy, w, h),
                    Some(*act),
                    "{act:?} 中点没命中"
                );
            }
            for i in 0..hs.len() {
                for j in i + 1..hs.len() {
                    let (a, b) = (hs[i].0, hs[j].0);
                    assert!(
                        !(a.0 < b.0 + b.2
                            && b.0 < a.0 + a.2
                            && a.1 < b.1 + b.3
                            && b.1 < a.1 + a.3),
                        "命中区 {i}/{j} 叠了（{d}/{f}/{trash}）"
                    );
                }
            }
        }
        // 正文中部不是面板
        assert_eq!(hit(1, 0, false, false, 700.0, 700.0, w, h), None);
        // 顶栏左边 = Cancel，右边 = 全选按钮
        assert_eq!(hit(1, 0, false, false, 60.0, 60.0, w, h), Some(Act::Cancel));
        assert_eq!(
            hit(1, 0, false, true, (w - 60) as f32, 60.0, w, h),
            Some(Act::DeselectAll)
        );
    }
}
