//! 文档层：`.rm` 编解码 + 库扫描 + 各种电子书格式的统一渲染接口。
//!
//! # 为什么不直接用现成的阅读器引擎
//!
//! 评估过的：KOReader（Lua + crengine + mupdf，三个运行时都塞不进来）、
//! crengine（C++，不用 Qt 也要拖 ICU + libre2，交叉编译到 armv7 得先起 qemu）、
//! MuPDF（AGPL，静态链接 12 MB 起，根分区只剩 14 MB）。设备是 Cortex-A7 +
//! 994 MB 内存 + 纯 framebuffer，**每多一个 C 依赖就多一份交叉编译和崩溃面**。
//!
//! 所以这里的做法是：**格式解析全自己写**，只借纯 Rust 的叶子 crate
//! （`miniz_oxide` 解 deflate、`zune-jpeg`/`png` 解图片）。
//! PDF 是唯一例外——设备自带 `libpdfium.so`，`dlopen` 用现成的，不打包。
//!
//! # 分两层
//!
//! * **重排层**（[`reflow`]）：EPUB / TXT / Markdown / HTML / FB2 / MOBI / DOCX
//!   都先解析成「章节 → 文本块」，再共用同一套断行/分页/光栅化。
//! * **页面层**（[`pdf`] / `comic`）：每页是一张已经画好的图，直接缩放输出。
//!
//! 两者都套进 [`Doc`]，UI 只认这一个接口。

pub mod autopage;
pub mod comic;
pub mod docx;
pub mod epub;
pub mod gbk;
pub mod fb2;
pub mod library;
pub mod mobi;
pub mod pagecache;
pub mod pdf;
pub mod reflow;
pub mod rm;
pub mod rmtext;
pub mod text;

pub use library::{Book, Library};
pub use pagecache::{CacheStats, PageCache};
pub use rm::RmPage;

use std::path::Path;

// ---------------------------------------------------------------------------
// 格式
// ---------------------------------------------------------------------------

/// 认得的文档格式。`Rm` 是我们自己的笔记，不走 [`Doc`]。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Rm,
    Pdf,
    Epub,
    /// `.txt` 纯文本
    Txt,
    /// `.md` / `.markdown`
    Markdown,
    /// `.html` / `.htm` / `.xhtml`
    Html,
    /// FictionBook 2
    Fb2,
    /// Mobipocket / Kindle 旧格式
    Mobi,
    /// KF8（Kindle Format 8）
    Azw3,
    /// 漫画：zip 包图片
    Cbz,
    /// Word
    Docx,
    /// RTF
    Rtf,
    Unknown,
}

impl Format {
    /// 按扩展名判（大小写不敏感，可以带点）。
    pub fn of_ext(ext: &str) -> Format {
        let ext = ext.trim_start_matches('.').to_ascii_lowercase();
        match ext.as_str() {
            "pdf" => Format::Pdf,
            "epub" => Format::Epub,
            "txt" | "text" | "log" => Format::Txt,
            "md" | "markdown" | "mdown" => Format::Markdown,
            "html" | "htm" | "xhtml" | "xht" => Format::Html,
            "fb2" => Format::Fb2,
            "mobi" | "prc" | "palmbook" => Format::Mobi,
            "azw3" | "azw" | "kf8" => Format::Azw3,
            "cbz" | "cbr" | "cb7" => Format::Cbz,
            "docx" => Format::Docx,
            "rtf" => Format::Rtf,
            _ => Format::Unknown,
        }
    }

    /// 按路径的扩展名判。
    pub fn of_path(path: &str) -> Format {
        let ext = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("");
        Format::of_ext(ext)
    }

    /// 按文件头判，扩展名不认识或没有时兜底。
    pub fn of_magic(head: &[u8]) -> Format {
        if head.starts_with(b"%PDF") {
            return Format::Pdf;
        }
        if head.starts_with(b"{\\rtf") {
            return Format::Rtf;
        }
        if head.starts_with(b"<?xml") || head.starts_with(b"\xef\xbb\xbf<?xml") {
            // 可能是 FB2；再往里找 <FictionBook
            let s = String::from_utf8_lossy(&head[..head.len().min(2048)]);
            if s.contains("FictionBook") {
                return Format::Fb2;
            }
            if s.contains("html") {
                return Format::Html;
            }
        }
        if head.len() > 68 && &head[60..68] == b"BOOKMOBI" {
            return Format::Mobi;
        }
        if head.len() > 64 && &head[60..64] == b"TEXtREAd" {
            return Format::Mobi;
        }
        Format::Unknown
    }

    /// 认不认得。
    pub fn is_known(self) -> bool {
        !matches!(self, Format::Unknown)
    }

    /// 库里存文件用的扩展名（重新打开时按这个拼路径）。
    pub fn ext(self) -> &'static str {
        match self {
            Format::Rm => "",
            Format::Pdf => "pdf",
            Format::Epub => "epub",
            Format::Txt => "txt",
            Format::Markdown => "md",
            Format::Html => "html",
            Format::Fb2 => "fb2",
            Format::Mobi => "mobi",
            Format::Azw3 => "azw3",
            Format::Cbz => "cbz",
            Format::Docx => "docx",
            Format::Rtf => "rtf",
            Format::Unknown => "",
        }
    }

    /// 界面上显示的名字（官方 UI 用英文）。
    pub fn label(self) -> &'static str {
        match self {
            Format::Rm => "Notebook",
            Format::Pdf => "PDF",
            Format::Epub => "EPUB",
            Format::Txt => "Text",
            Format::Markdown => "Markdown",
            Format::Html => "HTML",
            Format::Fb2 => "FB2",
            Format::Mobi => "MOBI",
            Format::Azw3 => "AZW3",
            Format::Cbz => "Comic",
            Format::Docx => "Word",
            Format::Rtf => "RTF",
            Format::Unknown => "File",
        }
    }

    /// 是不是电子书（`.rm` 笔记和未知类型不算；PDF 单列，不算在 Ebooks 里）。
    pub fn is_ebook(self) -> bool {
        self.is_known() && !matches!(self, Format::Rm | Format::Pdf)
    }

    /// 是不是重排格式（能改字号/行距/边距）。
    pub fn reflowable(self) -> bool {
        matches!(
            self,
            Format::Epub
                | Format::Txt
                | Format::Markdown
                | Format::Html
                | Format::Fb2
                | Format::Mobi
                | Format::Azw3
                | Format::Docx
                | Format::Rtf
        )
    }
}

// ---------------------------------------------------------------------------
// 统一接口
// ---------------------------------------------------------------------------

/// 目录里的一项。`page` 是页号（0 基）。
#[derive(Clone, Debug)]
pub struct Toc {
    /// 1 = 一级标题
    pub depth: u8,
    pub title: String,
    pub page: usize,
}

/// 阅读排版参数。只对重排格式有意义。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct LayoutCfg {
    pub font_size: f32,
    pub line_spacing: f32,
    pub margin: i32,
}

impl Default for LayoutCfg {
    fn default() -> LayoutCfg {
        LayoutCfg {
            font_size: reflow::DEFAULT_FONT_SIZE,
            line_spacing: reflow::DEFAULT_LINE_SPACING,
            margin: reflow::DEFAULT_MARGIN,
        }
    }
}

impl LayoutCfg {
    pub fn from_reflow(c: reflow::Cfg) -> LayoutCfg {
        LayoutCfg {
            font_size: c.font_size,
            line_spacing: c.line_spacing,
            margin: c.margin,
        }
    }

    pub fn to_reflow(self) -> reflow::Cfg {
        reflow::Cfg {
            font_size: self.font_size,
            line_spacing: self.line_spacing,
            margin: self.margin,
        }
    }
}

/// 一本能翻页的文档。UI 只认这个接口。
pub trait Doc {
    fn format(&self) -> Format;

    /// 总页数。重排格式在改排版后会变。
    fn pages(&self) -> usize;

    /// 第 `idx` 页的宽高比 **w/h**。
    fn aspect(&self, idx: usize) -> f32;

    /// 渲染成 `w*h` 灰度（255 = 白），长度必须正好 `w*h`。
    fn render(&self, idx: usize, w: usize, h: usize) -> Result<Vec<u8>, String>;

    fn toc(&self) -> Vec<Toc> {
        Vec::new()
    }

    /// 全文搜索：返回 (页号, 该页里的片段)。
    fn search(&self, _needle: &str, _limit: usize) -> Vec<(usize, String)> {
        Vec::new()
    }

    /// 这一页属于哪一章（页眉显示）。
    fn heading(&self, _idx: usize) -> Option<String> {
        None
    }

    /// 换排版参数。返回 `true` = 页数可能变了，调用方要重新取。
    fn set_layout(&mut self, _cfg: LayoutCfg) -> bool {
        false
    }

    fn layout_cfg(&self) -> LayoutCfg {
        LayoutCfg::default()
    }
}

// ---------------------------------------------------------------------------
// 打开
// ---------------------------------------------------------------------------

/// 按扩展名（不认识再看文件头）打开一本电子书。
///
/// 不处理 `.rm` 笔记——那是 [`Library`] 的活，走另一条路。
pub fn open(path: &str) -> Result<Box<dyn Doc>, String> {
    let mut fmt = Format::of_path(path);
    if !fmt.is_known() {
        // 扩展名不认识：读前 2 KB 猜
        use std::io::Read;
        let mut head = [0u8; 2048];
        let n = std::fs::File::open(path)
            .and_then(|mut f| f.read(&mut head))
            .map_err(|e| format!("cannot read {path}: {e}"))?;
        fmt = Format::of_magic(&head[..n]);
    }
    match fmt {
        Format::Pdf => Ok(Box::new(pdf::Pdf::open(path)?)),
        Format::Epub => Ok(Box::new(epub::Epub::open(path)?)),
        Format::Docx => Ok(Box::new(docx::DocxDoc::open(path)?)),
        Format::Fb2 => Ok(Box::new(fb2::Fb2Doc::open(path)?)),
        Format::Cbz => Ok(Box::new(comic::Comic::open(path)?)),
        Format::Mobi | Format::Azw3 => Ok(Box::new(mobi::MobiDoc::open(path)?)),
        Format::Txt | Format::Markdown | Format::Html | Format::Rtf => {
            Ok(Box::new(text::TextDoc::open(path, fmt)?))
        }
        other => Err(format!(
            "{} is not supported yet ({})",
            other.label(),
            path
        )),
    }
}

// ---------------------------------------------------------------------------
// 重排后端的胶水
// ---------------------------------------------------------------------------

/// 给重排后端套上 [`Doc`]：它们都实现同一组固有方法
/// （`page_count` / `aspect` / `render` / `outline` / `find` / `chapter_title` /
/// `apply_layout` / `current_layout`），只有类型和 `Format` 不同。
#[macro_export]
macro_rules! impl_reflow_doc {
    ($ty:ty, $fmt:expr) => {
        impl $crate::doc::Doc for $ty {
            fn format(&self) -> $crate::doc::Format {
                $fmt
            }
            fn pages(&self) -> usize {
                self.page_count()
            }
            fn aspect(&self, idx: usize) -> f32 {
                self.aspect(idx)
            }
            fn render(&self, idx: usize, w: usize, h: usize) -> Result<Vec<u8>, String> {
                self.render(idx, w, h)
            }
            fn toc(&self) -> Vec<$crate::doc::Toc> {
                self.outline()
            }
            fn search(&self, needle: &str, limit: usize) -> Vec<(usize, String)> {
                self.find(needle, limit)
            }
            fn heading(&self, idx: usize) -> Option<String> {
                self.chapter_title(idx)
            }
            fn set_layout(&mut self, cfg: $crate::doc::LayoutCfg) -> bool {
                self.apply_layout(cfg)
            }
            fn layout_cfg(&self) -> $crate::doc::LayoutCfg {
                self.current_layout()
            }
        }
    };
}

impl Doc for comic::Comic {
    fn format(&self) -> Format {
        Format::Cbz
    }
    fn pages(&self) -> usize {
        self.page_count()
    }
    fn aspect(&self, idx: usize) -> f32 {
        self.aspect(idx)
    }
    fn render(&self, idx: usize, w: usize, h: usize) -> Result<Vec<u8>, String> {
        self.render(idx, w, h)
    }
}

impl Doc for pdf::Pdf {
    fn format(&self) -> Format {
        Format::Pdf
    }
    fn pages(&self) -> usize {
        self.page_count()
    }
    /// `Pdf::aspect` 是**高宽比**（h/w，pdfium 的历史约定），这里转成 w/h。
    fn aspect(&self, idx: usize) -> f32 {
        let a = self.aspect(idx);
        if a > 0.01 {
            1.0 / a
        } else {
            1.0
        }
    }
    fn render(&self, idx: usize, w: usize, h: usize) -> Result<Vec<u8>, String> {
        self.render(idx, w, h)
    }
    fn toc(&self) -> Vec<Toc> {
        self.outline()
    }
    fn search(&self, needle: &str, limit: usize) -> Vec<(usize, String)> {
        self.find(needle, limit)
    }
}

/// `TextDoc` 的格式是构造时传进来的（Txt/Markdown/Html/Rtf 共用一个类型），
/// 所以不能套宏里的固定 `Format`，手写一份。
impl Doc for text::TextDoc {
    fn format(&self) -> Format {
        text::TextDoc::format(self)
    }
    fn pages(&self) -> usize {
        self.page_count()
    }
    fn aspect(&self, idx: usize) -> f32 {
        self.aspect(idx)
    }
    fn render(&self, idx: usize, w: usize, h: usize) -> Result<Vec<u8>, String> {
        self.render(idx, w, h)
    }
    fn toc(&self) -> Vec<Toc> {
        self.outline()
    }
    fn search(&self, needle: &str, limit: usize) -> Vec<(usize, String)> {
        self.find(needle, limit)
    }
    fn heading(&self, idx: usize) -> Option<String> {
        self.chapter_title(idx)
    }
    fn set_layout(&mut self, cfg: LayoutCfg) -> bool {
        self.apply_layout(cfg)
    }
    fn layout_cfg(&self) -> LayoutCfg {
        self.current_layout()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_from_extension() {
        assert_eq!(Format::of_path("/a/B.PDF"), Format::Pdf);
        assert_eq!(Format::of_path("x.epub"), Format::Epub);
        assert_eq!(Format::of_path("x.md"), Format::Markdown);
        assert_eq!(Format::of_path("x.azw3"), Format::Azw3);
        assert_eq!(Format::of_path("x.weird"), Format::Unknown);
        assert_eq!(Format::of_path("noext"), Format::Unknown);
    }

    #[test]
    fn format_from_magic() {
        assert_eq!(Format::of_magic(b"%PDF-1.7\n..."), Format::Pdf);
        assert_eq!(Format::of_magic(b"{\\rtf1\\ansi"), Format::Rtf);
        assert_eq!(
            Format::of_magic(b"<?xml version=\"1.0\"?><FictionBook>"),
            Format::Fb2
        );
        let mut mobi = vec![0u8; 80];
        mobi[60..68].copy_from_slice(b"BOOKMOBI");
        assert_eq!(Format::of_magic(&mobi), Format::Mobi);
        assert_eq!(Format::of_magic(b"hello"), Format::Unknown);
    }

    #[test]
    fn reflowable_flags() {
        assert!(Format::Epub.reflowable());
        assert!(Format::Mobi.reflowable());
        assert!(!Format::Pdf.reflowable());
        assert!(!Format::Cbz.reflowable());
    }

    #[test]
    fn layout_cfg_roundtrip() {
        let c = LayoutCfg {
            font_size: 30.0,
            line_spacing: 1.85,
            margin: 120,
        };
        assert_eq!(LayoutCfg::from_reflow(c.to_reflow()), c);
    }

    #[test]
    fn open_unknown_type_is_err_not_panic() {
        assert!(open("/nonexistent/x.qqq").is_err());
    }
}

impl_reflow_doc!(epub::Epub, Format::Epub);
impl_reflow_doc!(docx::DocxDoc, Format::Docx);
impl_reflow_doc!(fb2::Fb2Doc, Format::Fb2);

/// MOBI 和 AZW3 共用一个类型，格式由文件头决定。
impl Doc for mobi::MobiDoc {
    fn format(&self) -> Format {
        mobi::MobiDoc::format(self)
    }
    fn pages(&self) -> usize {
        self.page_count()
    }
    fn aspect(&self, idx: usize) -> f32 {
        self.aspect(idx)
    }
    fn render(&self, idx: usize, w: usize, h: usize) -> Result<Vec<u8>, String> {
        self.render(idx, w, h)
    }
    fn toc(&self) -> Vec<Toc> {
        self.outline()
    }
    fn search(&self, needle: &str, limit: usize) -> Vec<(usize, String)> {
        self.find(needle, limit)
    }
    fn heading(&self, idx: usize) -> Option<String> {
        self.chapter_title(idx)
    }
    fn set_layout(&mut self, cfg: LayoutCfg) -> bool {
        self.apply_layout(cfg)
    }
    fn layout_cfg(&self) -> LayoutCfg {
        self.current_layout()
    }
}

