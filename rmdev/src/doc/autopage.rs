//! 自动加页：可绘制文档末尾的**虚拟空白页**。
//!
//! 需求（用户原话）：任何 quick sheet / PDF / 电子书等可绘制的文档，编辑时在
//! 最后一个有改动的页后面**自动多挂一张空白页**；这张空白页上落了第一笔，
//! 它就变成真实的一页，末尾再挂一张新的。导出 / 分享 / 保存的文件里**没有**
//! 这张空白页——它只存在于编辑态。
//!
//! # 落法：虚拟页不是"页数差一"，是文档里根本没有这一页
//!
//! 它没有 page id、不写进 `.content`、不写 `.rm`。所以任何按 `.content` 取
//! 页数的地方（[`Library::page_ids`]、`share`、`sync`）天然看不到它，不需要在
//! 每个导出点各写一遍过滤。需要它"存在"的两个地方由 `main.rs` 的
//! `App::c15_*` 系列补出来：屏底 `Page N of M` 的 M（[`display_total`]）和
//! 翻页上界；以及在这页上落墨时把它变成真页（[`materialize`]）。
//!
//! # 为什么先补页条目再 `add_page`
//!
//! PDF / 电子书的 `.content` 里页条目可能比文档页数少（手工拷进来的 PDF 甚至
//! 一页都没有）。`Library::add_page` 只在**现有条目**后面插入，直接调它会把
//! 新页插到中间去，真实页序就错位了。所以先 `ensure_pages(real)` 补齐，再插。

use crate::doc::library::{uuid_bytes, Library};
use crate::doc::rm::RmPage;

/// 屏底 `Page N of M` 里的 M：真实页数 + 末尾那张虚拟页。
pub fn display_total(real: usize) -> usize {
    real + 1
}

/// 虚拟页的序号（0 基）：等于真实页数，也就是"最后一个真实页的下一页"。
pub fn virtual_index(real: usize) -> usize {
    real
}

/// 这个 0 基页号是不是虚拟页（`real` = 真实页数）。
pub fn is_virtual(index: usize, real: usize) -> bool {
    index >= real
}

/// 一页算不算"被改过"：有墨迹（橡皮线也算，它是文档内容）或有文字。
///
/// 虚拟页上只点一下、滑一下都不算——不能因为手指碰过就多出一页。
pub fn touched(page: &RmPage, texts: usize) -> bool {
    texts > 0 || page.total_lines() > 0
}

/// 一张干净的虚拟页。作者 UUID 跟文档一致：`add_page` 建真页时就是这么写的。
pub fn blank_page(uuid: &str) -> RmPage {
    let mut page = RmPage::default();
    page.author = uuid_bytes(uuid);
    page
}

/// 把虚拟页变成真的：先补齐 `.content` 的页条目，再 [`Library::add_page`]。
///
/// 返回新页序号（正常等于调用前的 `real`）。0 页、连 `.content` 都没有的空文档
/// 加不了页（`add_page` 报 `NotFound`），这时原样返回 `real`，让落墨时的
/// [`Library::save_page_at`] 把这一页登记回去——它本来就处理空 `.content`。
pub fn materialize(lib: &Library, uuid: &str, real: usize) -> std::io::Result<usize> {
    if real > 0 && lib.page_ids(uuid).len() < real {
        // 只补不删：多出来的页可能已经写满批注（PDF/电子书的页数由文档决定）
        lib.ensure_pages(uuid, real)?;
    }
    match lib.add_page(uuid, real.saturating_sub(1)) {
        Ok(at) => Ok(at),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(real),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 临时库目录（每个用例一个，别的测试看不到）。
    fn tmp_lib(tag: &str) -> Library {
        let dir = std::env::temp_dir().join(format!("rmdev-autopage-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Library::with_root(&dir)
    }

    /// 造一份"0 页"的 `.content`（官方删光所有页之后就是这个样子）。
    fn empty_content(lib: &Library, uuid: &str) {
        std::fs::write(
            lib.root.join(format!("{uuid}.content")),
            json!({ "cPages": { "pages": [] }, "pageCount": 0 }).to_string(),
        )
        .unwrap();
    }

    fn line() -> crate::pen::Line {
        let mut l = crate::pen::Line::new(
            crate::pen::Tool::BallpointV2,
            crate::pen::Color::Black,
            2.0,
        );
        l.push(crate::pen::Point {
            x: 120.0,
            y: 160.0,
            speed: 0.0,
            dir: 0.0,
            width: 3.2,
            pressure: 0.9,
        });
        l
    }

    #[test]
    fn display_total_is_one_more_than_real() {
        assert_eq!(display_total(0), 1, "0 页的空文档也要有一张虚拟页");
        assert_eq!(display_total(3), 4);
        assert_eq!(virtual_index(0), 0);
        assert_eq!(virtual_index(3), 3);
        assert!(is_virtual(0, 0));
        assert!(is_virtual(3, 3));
        assert!(!is_virtual(2, 3));
    }

    #[test]
    fn touched_ignores_a_blank_page_and_counts_ink_and_text() {
        let page = RmPage::default();
        assert!(!touched(&page, 0), "空白页不算改动");
        assert!(touched(&page, 1), "有文字就算改动");
        let mut page = RmPage::default();
        page.lines_mut().push(line());
        assert!(touched(&page, 0), "有墨迹就算改动");
    }

    #[test]
    fn blank_page_carries_the_document_author() {
        let uuid = "1554f45c-b223-40d2-adf7-d436232fa56a";
        let page = blank_page(uuid);
        assert_eq!(page.author, uuid_bytes(uuid));
        assert_eq!(page.total_lines(), 0);
    }

    #[test]
    fn materialize_appends_one_real_page_at_the_end() {
        let lib = tmp_lib("append");
        let id = lib.create_book("笔记本").unwrap();
        let real = lib.page_ids(&id).len();
        assert_eq!(real, 1);

        let at = materialize(&lib, &id, real).unwrap();
        assert_eq!(at, 1, "新页就该接在最后一页后面");
        assert_eq!(lib.page_ids(&id).len(), 2);
        // 新页的 `.rm` 真的写出来了，而且能解析回一张空页
        let ids = lib.page_ids(&id);
        let p = lib.root.join(&id).join(format!("{}.rm", ids[1]));
        assert!(p.exists(), "新页的 .rm 没写出来");
        assert_eq!(lib.load_page_at(&id, 1).total_lines(), 0);

        // 再挂一张就再往后加一页（每次落笔都这样）
        assert_eq!(materialize(&lib, &id, 2).unwrap(), 2);
        assert_eq!(lib.page_ids(&id).len(), 3);
    }

    #[test]
    fn materialize_registers_the_first_page_of_an_empty_document() {
        let lib = tmp_lib("empty");
        let id = lib.create_book("空文档").unwrap();
        empty_content(&lib, &id);
        assert_eq!(lib.page_ids(&id).len(), 0, "前提：这是 0 页文档");

        let at = materialize(&lib, &id, 0).unwrap();
        assert_eq!(at, 0);
        assert_eq!(lib.page_ids(&id).len(), 1, "第一页要登记回 .content");
    }

    /// 手工拷进来的 PDF 可能一页 page id 都没有：不能把新页插到第 0 页去。
    #[test]
    fn materialize_fills_missing_page_entries_first() {
        let lib = tmp_lib("short");
        let id = lib.create_book("书").unwrap();
        empty_content(&lib, &id);
        assert_eq!(lib.page_ids(&id).len(), 0);

        let at = materialize(&lib, &id, 3).unwrap();
        assert_eq!(at, 3, "虚拟页在 3 号位，新页就必须是 3 号");
        assert_eq!(lib.page_ids(&id).len(), 4);
    }

    /// 0 页又没有 `.content`：加不了页，返回 `real` 让 `save_page_at` 兜底。
    #[test]
    fn materialize_without_content_defers_to_save_page_at() {
        let lib = tmp_lib("nocontent");
        std::fs::create_dir_all(lib.root.join("nope")).unwrap();
        assert_eq!(materialize(&lib, "nope", 0).unwrap(), 0);
        assert!(lib.page_ids("nope").is_empty());

        // `save_page_at` 会把这一页登记回来（0 页文档落墨的那条路）
        let mut page = RmPage::default();
        page.lines_mut().push(line());
        lib.save_page_at("nope", 0, &page).unwrap();
        assert_eq!(lib.page_ids("nope").len(), 1);
    }

    /// 只读（磁盘满）时不能加页：返回错误，页还是虚的。
    #[test]
    fn materialize_refuses_in_read_only_mode() {
        let lib = tmp_lib("ro");
        let id = lib.create_book("只读").unwrap();
        lib.set_read_only(true);
        let e = materialize(&lib, &id, 1).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::Other);
        assert_eq!(lib.page_ids(&id).len(), 1, "只读时一个字节都不能动");
    }
}
