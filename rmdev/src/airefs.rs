//! AI 引用框：`<源文档>.airefs.json` → 纸面上的框 / `AI` 角标（`docs/AI-REFS.md`）。
//!
//! 服务器发布一张回答 PDF 时，会往源文档的索引里追加一条：
//!
//! ```json
//! {"ai":"<回答文档 uuid>","page":3,"rect":[100,200,600,700],"kind":"rect","n":1,"pdf_page":0}
//! ```
//!
//! * `kind=rect`：框选提问。纸面上把当时选的那块画成虚线框，框里右上角标 `AI 1`。
//! * `kind=page`：整页提问。不框整页（太吵），在页面左上角依次排 `AI 1`、`AI 2`…
//! * 点法：**手指长按** → 弹 `View`；点 `View` 才跳转。笔尖点击、短按都不算
//!   （用户 2026-11-09 定的，怕写字时误触）。
//!
//! 这个文件**只读**：设备不改它（改由服务器发布时整份重写）。读不到 / JSON 坏了
//! 就当没有框——绝不让一个坏索引挡住正常阅读。
//!
//! 坐标是**设备像素**（1404×1872，原点 = 页面左上），和 `selectai::Rect` 同一套。

use std::path::Path;

/// 页面左上角 `AI` 角标的尺寸。
pub const MARK_W: i32 = 132;
pub const MARK_H: i32 = 84;
/// 第一个角标的位置（避开左上角工具条，工具条宽 120）。
pub const MARK_X: i32 = 150;
pub const MARK_Y: i32 = 140;
/// 多个整页提问之间的行距（角标高 + 20）。
pub const MARK_GAP: i32 = 104;

/// 一个引用（服务器写的）。
#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// 回答 PDF 的库 uuid。
    pub ai: String,
    /// 框画在哪一页（0 基）。
    pub page: u32,
    /// 框选区域（设备像素）；整页提问是 `None`。
    pub rect: Option<[f64; 4]>,
    /// `rect` / `page`。
    pub kind: String,
    /// 第几轮（显示用）。
    pub n: u32,
    /// 回答的第一行（菜单里显示；服务器发布时写进来）。
    pub title: String,
    /// 跳到 PDF 的第几页（0 基）。
    pub pdf_page: u32,
}

/// 画在纸上的一个框/角标（已算好屏幕坐标）。
#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    pub ai: String,
    pub n: u32,
    /// 回答的第一行（菜单行显示的内容）。
    pub title: String,
    pub pdf_page: u32,
    /// 屏幕坐标 `(x, y, w, h)`（还没过视图变换）。
    pub rect: (i32, i32, i32, i32),
    /// 是不是虚线框（`false` = 左上角 `AI` 角标）。
    pub boxed: bool,
}

/// 一份文档的引用索引。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Refs {
    pub items: Vec<Item>,
}

impl Refs {
    /// 读 `<root>/<doc>.airefs.json`。没有/坏了都返回空。
    pub fn load(root: &Path, doc: &str) -> Refs {
        let p = root.join(format!("{doc}.airefs.json"));
        match std::fs::read(p) {
            Ok(b) => Refs::parse(&b),
            Err(_) => Refs::default(),
        }
    }

    /// 解析（容错：字段缺/类型不对的条目整条丢掉，不影响别的）。
    pub fn parse(b: &[u8]) -> Refs {
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(b) else {
            return Refs::default();
        };
        let mut out = Vec::new();
        for it in v.get("items").and_then(|x| x.as_array()).into_iter().flatten() {
            let Some(ai) = it.get("ai").and_then(|x| x.as_str()) else {
                continue;
            };
            if ai.is_empty() {
                continue;
            }
            let page = it.get("page").and_then(|x| x.as_u64()).unwrap_or(0) as u32;
            let kind = it
                .get("kind")
                .and_then(|x| x.as_str())
                .unwrap_or("rect")
                .to_string();
            let rect = it.get("rect").and_then(|x| x.as_array()).and_then(|a| {
                let g = |i: usize| a.get(i).and_then(|v| v.as_f64());
                Some([g(0)?, g(1)?, g(2)?, g(3)?])
            });
            out.push(Item {
                ai: ai.to_string(),
                page,
                rect,
                kind,
                n: it.get("n").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
                title: it
                    .get("title")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                pdf_page: it.get("pdf_page").and_then(|x| x.as_u64()).unwrap_or(0) as u32,
            });
        }
        // 服务器是追加写的，顺序就是提问顺序；这里再按 n 稳定排一下，
        // 万一以后并发发布顺序乱了，角标编号也不跳。
        out.sort_by_key(|i| (i.n, i.page));
        Refs { items: out }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 这一页上要画的框/角标。
    ///
    /// 整页提问按**本页内的先后**排角标（第 0 个在 `MARK_Y`，往下叠）。
    /// 框选提问直接用存下来的矩形；越界/面积为 0 的条目跳过。
    pub fn marks(&self, page: u32) -> Vec<Mark> {
        let mut out = Vec::new();
        let mut slot = 0i32;
        for it in self.items.iter().filter(|i| i.page == page) {
            if it.kind == "page" || it.rect.is_none() {
                out.push(Mark {
                    ai: it.ai.clone(),
                    n: it.n,
                    title: it.title.clone(),
                    pdf_page: it.pdf_page,
                    rect: (MARK_X, MARK_Y + slot * MARK_GAP, MARK_W, MARK_H),
                    boxed: false,
                });
                slot += 1;
                continue;
            }
            let r = it.rect.unwrap();
            let (x0, y0) = (r[0].min(r[2]) as i32, r[1].min(r[3]) as i32);
            let (x1, y1) = (r[0].max(r[2]) as i32, r[1].max(r[3]) as i32);
            if x1 - x0 < 8 || y1 - y0 < 8 {
                continue;
            }
            out.push(Mark {
                ai: it.ai.clone(),
                n: it.n,
                title: it.title.clone(),
                pdf_page: it.pdf_page,
                rect: (x0, y0, x1 - x0, y1 - y0),
                boxed: true,
            });
        }
        out
    }

    /// 落在哪个引用上（长按用）。后画的在上层，所以从后往前找。
    pub fn hit(&self, page: u32, x: i32, y: i32) -> Option<Mark> {
        self.marks(page).into_iter().rev().find(|m| {
            let (mx, my, mw, mh) = m.rect;
            x >= mx && x < mx + mw && y >= my && y < my + mh
        })
    }
}

// ---------------------------------------------------------------------------
// 引用菜单（工具栏第 14 格）：这一页上所有引用，一行一条。
//
// 官方没有这个东西（官方把批注摊在纸面上）。用户 2026-11-09 定的形式：
// 点工具栏的 AI 按钮 → 弹出列表，每行是**回答的第一行内容**；方框引用在纸面
// 上画一个箭头指向框的左上角顶点，整页引用没有指向（本来就没有锚点）。
// 这样一个页面上堆了好几个引用也处理得过来。

/// 菜单左边距：贴着工具条右边 8 px。
pub const MENU_X: i32 = 128;
pub const MENU_W: i32 = 820;
/// 行高：和 `Menu::Pens` 那些 foldout 一致，手指好点。
pub const MENU_ROW: i32 = 104;
/// 第一行贴着工具栏 AI 按钮（`AI_Y + BTN` = 960）顶部。
pub const MENU_Y: i32 = 960;

/// `n` 条引用的列表矩形（超出屏幕就往上缩）。左利手时贴工具条左边。
pub fn menu_rect(n: usize, screen_h: i32) -> (i32, i32, i32, i32) {
    let h = (n as i32).clamp(1, 6) * MENU_ROW + 8;
    let mut y = MENU_Y;
    if y + h > screen_h - 8 {
        y = (screen_h - h - 8).max(8);
    }
    (crate::fb::flip_x(MENU_X, MENU_W), y, MENU_W, h)
}

/// 点在菜单第几行上（`None` = 没点中）。
pub fn menu_row_at(n: usize, screen_h: i32, x: i32, y: i32) -> Option<usize> {
    let (mx, my, mw, mh) = menu_rect(n, screen_h);
    if x < mx || x >= mx + mw || y < my || y >= my + mh {
        return None;
    }
    let row = ((y - my) / MENU_ROW) as usize;
    (row < n.min(6)).then_some(row)
}

/// `View` 弹层：贴在框/角标右边或下面，别跑出屏幕。
///
/// 190×96 是照库里 `Move to trash` 那条菜单行的大小定的，手指好点。
pub const VIEW_W: i32 = 190;
pub const VIEW_H: i32 = 96;

/// 给定框/角标，算 `View` 弹层的位置（屏幕坐标）。
pub fn view_rect(mark: &Mark, screen_w: i32, screen_h: i32) -> (i32, i32, i32, i32) {
    let (mx, my, mw, mh) = mark.rect;
    // 优先贴在框右边；右边放不下就贴左边；再不行就放框下面。
    let mut x = mx + mw + 12;
    if x + VIEW_W > screen_w - 8 {
        x = mx - VIEW_W - 12;
    }
    if x < 8 {
        x = (mx + mw / 2 - VIEW_W / 2).clamp(8, (screen_w - VIEW_W - 8).max(8));
    }
    let mut y = my + mh / 2 - VIEW_H / 2;
    if y + VIEW_H > screen_h - 8 {
        y = screen_h - VIEW_H - 8;
    }
    (x, y.max(8), VIEW_W, VIEW_H)
}

/// 点是否落在 `View` 弹层里。
pub fn view_hit(r: (i32, i32, i32, i32), x: i32, y: i32) -> bool {
    let (rx, ry, rw, rh) = r;
    x >= rx && x < rx + rw && y >= ry && y < ry + rh
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs(json: &str) -> Refs {
        Refs::parse(json.as_bytes())
    }

    #[test]
    fn parses_and_sorts_by_n() {
        let r = refs(
            r#"{"items":[
                 {"ai":"b","page":0,"kind":"page","n":2},
                 {"ai":"a","page":0,"kind":"page","n":1}
               ]}"#,
        );
        assert_eq!(r.items.len(), 2);
        assert_eq!(r.items[0].ai, "a");
        assert_eq!(r.items[1].ai, "b");
    }

    #[test]
    fn bad_items_only_drop_themselves() {
        let r = refs(
            r#"{"items":[
                 {"page":1,"n":1},
                 {"ai":"ok","page":0,"rect":[10,20,110,220],"n":2},
                 {"ai":"","page":0,"n":3},
                 "junk"
               ]}"#,
        );
        assert_eq!(r.items.len(), 1);
        assert_eq!(r.items[0].ai, "ok");
    }

    #[test]
    fn broken_json_is_empty() {
        assert!(refs("not json").is_empty());
        assert!(refs("{}").is_empty());
    }

    #[test]
    fn page_marks_stack_in_order() {
        let r = refs(
            r#"{"items":[
                 {"ai":"a","page":0,"kind":"page","n":1},
                 {"ai":"b","page":0,"kind":"page","n":2},
                 {"ai":"c","page":1,"kind":"page","n":3}
               ]}"#,
        );
        let m = r.marks(0);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].rect, (MARK_X, MARK_Y, MARK_W, MARK_H));
        assert_eq!(m[1].rect, (MARK_X, MARK_Y + MARK_GAP, MARK_W, MARK_H));
        assert!(m.iter().all(|x| !x.boxed));
        assert_eq!(r.marks(1).len(), 1);
        assert_eq!(r.marks(2).len(), 0);
    }

    #[test]
    fn rect_marks_normalize_and_skip_degenerate() {
        let r = refs(
            r#"{"items":[
                 {"ai":"a","page":2,"rect":[600,700,100,200],"n":1},
                 {"ai":"b","page":2,"rect":[300,300,301,301],"n":2}
               ]}"#,
        );
        let m = r.marks(2);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].rect, (100, 200, 500, 500));
        assert!(m[0].boxed);
    }

    #[test]
    fn hit_prefers_topmost() {
        let r = refs(
            r#"{"items":[
                 {"ai":"a","page":0,"kind":"page","n":1},
                 {"ai":"b","page":0,"kind":"page","n":2}
               ]}"#,
        );
        let a = r.hit(0, MARK_X + 5, MARK_Y + 5).unwrap();
        assert_eq!(a.ai, "a");
        let b = r.hit(0, MARK_X + 5, MARK_Y + MARK_GAP + 5).unwrap();
        assert_eq!(b.ai, "b");
        assert!(r.hit(0, MARK_X - 50, MARK_Y).is_none());
    }

    #[test]
    fn view_popover_stays_on_screen() {
        let m = Mark {
            ai: "a".into(),
            n: 1,
            title: "答案第一行".into(),
            pdf_page: 0,
            rect: (1200, 300, 150, 80),
            boxed: true,
        };
        let (x, y, w, h) = view_rect(&m, 1404, 1872);
        assert!(x >= 8 && x + w <= 1404 - 8);
        assert!(y >= 8 && y + h <= 1872 - 8);
        // 左边放不下才翻到左边；这里翻不过去就居中
        assert!(x < 1200);

        let m2 = Mark {
            rect: (10, 1700, 100, 100),
            ..m
        };
        let (_, y2, ..) = view_rect(&m2, 1404, 1872);
        assert!(y2 + VIEW_H <= 1872 - 8);
    }

    #[test]
    fn title_lands_in_mark() {
        let r = refs(
            r#"{"items":[{"ai":"a","page":0,"rect":[10,20,110,220],"n":1,"title":"先把这个式子展开"}]}"#,
        );
        let m = r.marks(0);
        assert_eq!(m[0].title, "先把这个式子展开");
        // 没有 title 的老条目：空串，界面自己退回 `AI n`
        let old = refs(r#"{"items":[{"ai":"b","page":0,"kind":"page","n":1}]}"#);
        assert_eq!(old.marks(0)[0].title, "");
    }

    #[test]
    fn menu_rows_hit_the_right_entry() {
        // 行高 104，从 MENU_Y 起；第 0 行顶边、最后一行底边前一像素都算中
        let n = 3;
        assert_eq!(menu_row_at(n, 1872, MENU_X + 10, MENU_Y + 5), Some(0));
        assert_eq!(menu_row_at(n, 1872, MENU_X + 10, MENU_Y + 150), Some(1));
        assert_eq!(menu_row_at(n, 1872, MENU_X + 10, MENU_Y + 2 * 104 + 5), Some(2));
        // 第 4 行不存在
        assert_eq!(menu_row_at(n, 1872, MENU_X + 10, MENU_Y + 3 * 104 + 5), None);
        // 菜单外面
        assert_eq!(menu_row_at(n, 1872, MENU_X - 1, MENU_Y + 5), None);
        assert_eq!(menu_row_at(n, 1872, MENU_X + MENU_W, MENU_Y + 5), None);
    }

    #[test]
    fn menu_rect_stays_on_screen() {
        // 引用很多时列表不能长到屏幕外
        let (_, y, _, h) = menu_rect(20, 1872);
        assert!(y + h <= 1872 - 8);
        assert!(h <= 6 * MENU_ROW + 8);
        // 一条也要有个能点的框
        let (_, _, _, h1) = menu_rect(1, 1872);
        assert_eq!(h1, MENU_ROW + 8);
    }

    #[test]
    fn view_hit_is_exact() {
        let r = (100, 200, 190, 96);
        assert!(view_hit(r, 100, 200));
        assert!(view_hit(r, 289, 295));
        assert!(!view_hit(r, 290, 295));
        assert!(!view_hit(r, 100, 296));
    }
}
