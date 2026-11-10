//! 用设备上的 `/usr/lib/libpdfium.so` 把 PDF 页渲染成灰度位图。
//!
//! 用 `dlopen`/`dlsym` 动态取符号，不链接任何库——设备上有就渲染，
//! 没有就返回错误，调用方按"打不开"处理。
//!
//! # 缓存（主循环不用改也能吃到）
//!
//! 主循环每翻一页都会 `Pdf::open(同一个路径)` 再 `render(...)`。最贵的两步
//! 其实是 `dlopen + FPDF_LoadDocument`（把整份 PDF 的 xref/对象树解析一遍）
//! 和 pdfium 的整页光栅化。所以这里分两层：
//!
//! * **文档复用**：同一路径的 `Pdf` 共享同一个 `FPDF_DOCUMENT`，只有换书才重开；
//! * **页缓存**：最近 [`DEFAULT_PAGES`] 页的灰度，键 = (页号, 渲染宽度)，
//!   同一页重复请求（翻回去、重绘）直接返回缓存副本。
//!
//! 要"往前翻也不卡"，调用方在合适的时候再调 [`Pdf::prefetch`]：
//! **同步、不开线程**，什么时候调由 UI 决定。
//!
//! # 文字层（搜索 + 大纲）
//!
//! 同一份 `libpdfium.so`：`FPDFText_*` 取页文本，`FPDF*Bookmark*` 走大纲。
//! 每个符号都用 `dlsym` 单独取、各自存成 `Option`——裁剪过符号的库照样能
//! 渲染，只是 [`Pdf::page_text`] / [`Pdf::find`] / [`Pdf::outline`] 退化成
//! `None` / 空，**绝不 panic**。页文本懒加载后缓存在 `TextLayer` 里。

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_ulong, c_void};
use std::rc::Rc;

use super::pagecache::{CacheStats, CachedDoc, PageRenderer, SlotCache, DEFAULT_PAGES};

type GetPageCount = unsafe extern "C" fn(*mut c_void) -> c_int;
type GetPageSizeF = unsafe extern "C" fn(*mut c_void) -> f32;
type LoadPage = unsafe extern "C" fn(*mut c_void, c_int) -> *mut c_void;
type CreateEx = unsafe extern "C" fn(c_int, c_int, c_int, *mut c_void, c_int) -> *mut c_void;
type FillRect = unsafe extern "C" fn(*mut c_void, c_int, c_int, c_int, c_int, u32);
type Render = unsafe extern "C" fn(
    *mut c_void,
    *mut c_void,
    c_int,
    c_int,
    c_int,
    c_int,
    c_int,
    c_int,
);
type Destroy = unsafe extern "C" fn(*mut c_void);
type ClosePage = unsafe extern "C" fn(*mut c_void);
type CloseDoc = unsafe extern "C" fn(*mut c_void);

/// 从 libpdfium 取出来的函数指针。整份文档共用一份。
#[derive(Clone, Copy)]
struct Fns {
    page_w: GetPageSizeF,
    page_h: GetPageSizeF,
    load_page: LoadPage,
    create_ex: CreateEx,
    fill_rect: FillRect,
    render: Render,
    destroy: Destroy,
    close_page: ClosePage,
    close_doc: CloseDoc,
}

// ---- 文字层 / 大纲的符号 ---------------------------------------------------
//
// 签名照 pdfium 公开头文件：`FPDFText_LoadPage` 收的是 **FPDF_PAGE**（不是
// 文档 + 页号），所以要先用 `FPDF_LoadPage` 拿到页句柄。`FPDFBookmark_GetTitle`
// 的长度参数是 C 的 `unsigned long`（armv7 是 32 位，x86_64 是 64 位），
// 所以用 `c_ulong` 而不是写死 u32。

type TextLoadPage = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type TextCountChars = unsafe extern "C" fn(*mut c_void) -> c_int;
type TextGetText = unsafe extern "C" fn(*mut c_void, c_int, c_int, *mut u16) -> c_int;
type BookmarkChild = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
type BookmarkTitleFn = unsafe extern "C" fn(*mut c_void, *mut c_void, c_ulong) -> c_ulong;
type BookmarkDestFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> *mut c_void;
type DestPageIndexFn = unsafe extern "C" fn(*mut c_void, *mut c_void) -> c_int;

/// 文字层/大纲的符号表。**每个字段都是 `Option`**：只要有一个取不到，
/// 用到它的那条路径就整条降级（`page_text` → `None`，大纲 → 空），
/// 不 panic、也不影响渲染。
#[derive(Clone, Copy, Default)]
struct TextFns {
    load_page: Option<LoadPage>,
    close_page: Option<ClosePage>,
    load_text: Option<TextLoadPage>,
    count_chars: Option<TextCountChars>,
    get_text: Option<TextGetText>,
    close_text: Option<ClosePage>,
    first_child: Option<BookmarkChild>,
    next_sibling: Option<BookmarkChild>,
    bookmark_title: Option<BookmarkTitleFn>,
    bookmark_dest: Option<BookmarkDestFn>,
    dest_page: Option<DestPageIndexFn>,
}

impl TextFns {
    /// 逐个 `dlsym`；缺的就是 `None`（`sym` 那边已经打过日志）。
    fn resolve(sym: &dyn Fn(&str) -> *mut c_void) -> TextFns {
        TextFns {
            load_page: sym_fn(sym, "FPDF_LoadPage"),
            close_page: sym_fn(sym, "FPDF_ClosePage"),
            load_text: sym_fn(sym, "FPDFText_LoadPage"),
            count_chars: sym_fn(sym, "FPDFText_CountChars"),
            get_text: sym_fn(sym, "FPDFText_GetText"),
            close_text: sym_fn(sym, "FPDFText_ClosePage"),
            first_child: sym_fn(sym, "FPDF_GetFirstChildBookmark"),
            next_sibling: sym_fn(sym, "FPDF_GetNextSiblingBookmark"),
            bookmark_title: sym_fn(sym, "FPDFBookmark_GetTitle"),
            bookmark_dest: sym_fn(sym, "FPDFBookmark_GetDest"),
            dest_page: sym_fn(sym, "FPDFDest_GetDestPageIndex"),
        }
    }

    /// 五个书签符号都在，才拿得到可用的大纲表。
    fn bookmarks(&self) -> Option<BookmarkFns> {
        Some(BookmarkFns {
            first_child: self.first_child?,
            next_sibling: self.next_sibling?,
            title: self.bookmark_title?,
            dest: self.bookmark_dest?,
            dest_page: self.dest_page?,
        })
    }
}

/// 走大纲要的那 5 个函数：齐了才有意义，缺一个就整块放弃。
#[derive(Clone, Copy)]
struct BookmarkFns {
    first_child: BookmarkChild,
    next_sibling: BookmarkChild,
    title: BookmarkTitleFn,
    dest: BookmarkDestFn,
    dest_page: DestPageIndexFn,
}

/// 取一个符号并转成函数指针；取不到 → `None`（这就是"缺符号不 panic"的入口）。
fn sym_fn<T: Copy>(sym: &dyn Fn(&str) -> *mut c_void, name: &str) -> Option<T> {
    let p = sym(name);
    if p.is_null() {
        return None;
    }
    // SAFETY: dlsym 给的就是这个 C 函数的地址，指针和函数指针同宽（armv7 4 字节）。
    Some(unsafe { std::mem::transmute_copy(&p) })
}

/// 一份已打开的 PDF（句柄 + 符号 + 页数）。贵的是 `load`，所以由
/// [`SlotCache`] 按路径留最近一份。
struct Pdfium {
    lib: *mut c_void,
    doc: *mut c_void,
    page_count: usize,
    fns: Fns,
    /// 文字层/大纲的符号。缺就全 `None`，渲染一点不受影响。
    text_fns: TextFns,
}

impl Pdfium {
    fn load(path: &str) -> Result<Pdfium, String> {
        unsafe {
            let p = CString::new("/usr/lib/libpdfium.so").unwrap();
            let lib = libc::dlopen(p.as_ptr(), libc::RTLD_NOW);
            if lib.is_null() {
                return Err("dlopen libpdfium.so 失败".into());
            }
            let sym = |name: &str| -> *mut c_void {
                let n = CString::new(name).unwrap();
                let f = libc::dlsym(lib, n.as_ptr());
                if f.is_null() {
                    eprintln!("[pdf] 缺少符号 {name}");
                }
                f
            };
            let init: unsafe extern "C" fn() = std::mem::transmute(sym("FPDF_InitLibrary"));
            let load_doc: unsafe extern "C" fn(*const c_char, *const c_char) -> *mut c_void =
                std::mem::transmute(sym("FPDF_LoadDocument"));
            init();

            let cpath = CString::new(path).map_err(|_| "路径含 NUL".to_string())?;
            let doc = load_doc(cpath.as_ptr(), std::ptr::null());
            if doc.is_null() {
                return Err(format!("打不开 {path}"));
            }
            let get_count: GetPageCount = std::mem::transmute(sym("FPDF_GetPageCount"));
            let text_fns = TextFns::resolve(&sym);
            Ok(Pdfium {
                lib,
                doc,
                page_count: get_count(doc).max(0) as usize,
                text_fns,
                fns: Fns {
                    page_w: std::mem::transmute(sym("FPDF_GetPageWidthF")),
                    page_h: std::mem::transmute(sym("FPDF_GetPageHeightF")),
                    load_page: std::mem::transmute(sym("FPDF_LoadPage")),
                    create_ex: std::mem::transmute(sym("FPDFBitmap_CreateEx")),
                    fill_rect: std::mem::transmute(sym("FPDFBitmap_FillRect")),
                    render: std::mem::transmute(sym("FPDF_RenderPageBitmap")),
                    destroy: std::mem::transmute(sym("FPDFBitmap_Destroy")),
                    close_page: std::mem::transmute(sym("FPDF_ClosePage")),
                    close_doc: std::mem::transmute(sym("FPDF_CloseDocument")),
                },
            })
        }
    }
}

impl PageRenderer for Pdfium {
    fn page_count(&self) -> usize {
        self.page_count
    }

    /// 第 `idx` 页的高宽比（h/w）。取不到就当 A4。
    fn aspect(&self, idx: u32) -> f32 {
        unsafe {
            let page = (self.fns.load_page)(self.doc, idx as c_int);
            if page.is_null() {
                return 1.414;
            }
            let w = (self.fns.page_w)(page);
            let h = (self.fns.page_h)(page);
            (self.fns.close_page)(page);
            if w > 1.0 && h > 1.0 {
                h / w
            } else {
                1.414
            }
        }
    }

    /// 把第 `idx` 页渲染成 `w×h` 的灰度图（255 = 白）。返回长度 `w*h`。
    fn render(&self, idx: u32, w: u32, h: u32) -> Result<Vec<u8>, String> {
        let (w, h) = (w as usize, h as usize);
        unsafe {
            let page = (self.fns.load_page)(self.doc, idx as c_int);
            if page.is_null() {
                return Err(format!("没有第 {idx} 页"));
            }
            // format 1 = FPDFBitmap_Gray（8bpp 灰度）：面板本来就只有 16 级灰，
            // 让 pdfium 直接出灰度就省掉一次 9.9MB 的 BGRA 缓冲 + 248 万次
            // 逐像素"越界检查 + 三通道加权"，那一段实测占 render 的很大一块。
            let mut gray = vec![255u8; w * h];
            let bmp = (self.fns.create_ex)(
                w as c_int,
                h as c_int,
                1,
                gray.as_mut_ptr() as *mut c_void,
                w as c_int,
            );
            if !bmp.is_null() {
                (self.fns.fill_rect)(bmp, 0, 0, w as c_int, h as c_int, 0xFFFF_FFFF);
                // flags: FPDF_ANNOT = 1
                (self.fns.render)(bmp, page, 0, 0, w as c_int, h as c_int, 0, 1);
                (self.fns.destroy)(bmp);
                (self.fns.close_page)(page);
                return Ok(gray);
            }
            // 兜底：这个 pdfium 不认灰度位图 → 老路（BGRA + 自己转）
            let mut buf = vec![0u8; w * h * 4];
            let bmp = (self.fns.create_ex)(
                w as c_int,
                h as c_int,
                4,
                buf.as_mut_ptr() as *mut c_void,
                (w * 4) as c_int,
            );
            if bmp.is_null() {
                (self.fns.close_page)(page);
                return Err("建位图失败".into());
            }
            (self.fns.fill_rect)(bmp, 0, 0, w as c_int, h as c_int, 0xFFFF_FFFF);
            (self.fns.render)(bmp, page, 0, 0, w as c_int, h as c_int, 0, 1);
            (self.fns.destroy)(bmp);
            (self.fns.close_page)(page);

            // 切片 zip：逐像素 `buf[i*4+k]` 每次都要边界检查，向量化不了
            for (dst, px) in gray.iter_mut().zip(buf.chunks_exact(4)) {
                let b = px[0] as u32;
                let g = px[1] as u32;
                let r = px[2] as u32;
                *dst = ((r * 299 + g * 587 + b * 114) / 1000) as u8;
            }
            Ok(gray)
        }
    }
}

impl Drop for Pdfium {
    fn drop(&mut self) {
        unsafe {
            if !self.doc.is_null() {
                (self.fns.close_doc)(self.doc);
            }
            if !self.lib.is_null() {
                libc::dlclose(self.lib);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 文字层（搜索 / 大纲）
// ---------------------------------------------------------------------------

/// 书签标题的上限。目录项就一行字，超过这个长度的只可能是畸形文件——
/// 直接放弃，别为一个标题去分配几 MB。
const MAX_TITLE_BYTES: c_ulong = 8 * 1024;

/// 单页最多取这么多 UTF-16 码元（约 4 MB）。见 [`TextLayer::extract`]。
const MAX_PAGE_CHARS: usize = 1 << 21;

/// 文字层 + 每页文本缓存。
///
/// 和渲染共用同一个 `FPDF_DOCUMENT`（这里只存指针副本），但符号表是独立
/// 一份：文字/大纲的符号缺了不影响渲染。文本**懒加载**——只有真取过的页
/// 才进缓存，一次搜索不会把一本 500 页的 PDF 全解析一遍。
struct TextLayer {
    fns: TextFns,
    doc: *mut c_void,
    /// 页号 → 纯文本。
    cache: RefCell<HashMap<usize, String>>,
}

impl TextLayer {
    fn new(fns: TextFns, doc: *mut c_void) -> TextLayer {
        TextLayer {
            fns,
            doc,
            cache: RefCell::new(HashMap::new()),
        }
    }

    /// 第 `idx` 页的纯文本（pdfium 的字符顺序，行尾是 `\r\n`）。
    /// 缺符号 / 页不存在 → `None`。取到过就缓存，第二次不再走 pdfium。
    fn page_text(&self, idx: usize) -> Option<String> {
        if let Some(t) = self.cache.borrow().get(&idx) {
            return Some(t.clone());
        }
        let text = self.extract(idx)?;
        self.cache.borrow_mut().insert(idx, text.clone());
        Some(text)
    }

    /// 真正取文本：`LoadPage` → `FPDFText_LoadPage` → `GetText` → 关句柄。
    fn extract(&self, idx: usize) -> Option<String> {
        let f = self.fns;
        let load_page = f.load_page?;
        let close_page = f.close_page?;
        let load_text = f.load_text?;
        let count = f.count_chars?;
        let get_text = f.get_text?;
        let close_text = f.close_text?;
        unsafe {
            let page = load_page(self.doc, idx as c_int);
            if page.is_null() {
                return None;
            }
            let tp = load_text(page);
            let mut out = None;
            if !tp.is_null() {
                let raw = count(tp);
                let mut ok = raw >= 0;
                let mut s = String::new();
                // 一页几千个字符是常态；上限只防畸形文件报个 2^31 的字符数，
                // 让我们去分配 GB 内存（分配失败是 abort，比 panic 还糟）。
                let n = (raw.max(0) as usize).min(MAX_PAGE_CHARS);
                if ok && n > 0 {
                    // FPDFText_GetText 的返回值**含结尾 NUL**，所以多要一个码元。
                    let mut buf = vec![0u16; n + 1];
                    let got = get_text(tp, 0, n as c_int, buf.as_mut_ptr());
                    ok = got > 0;
                    if ok {
                        let units = (got as usize - 1).min(buf.len());
                        s = utf16le_to_string(&buf[..units]);
                    }
                }
                close_text(tp);
                if ok {
                    // 真机验证用：`grep '[pdf] text'` 看这页到底取到多少字。
                    eprintln!("[pdf] text 第 {} 页 {} 字", idx + 1, s.chars().count());
                    out = Some(s);
                }
            }
            close_page(page);
            out
        }
    }

    /// 逐页找 `needle`（大小写不敏感），**每页最多一条**命中，
    /// 凑够 `limit` 就停（懒：后面的页根本不打开）。
    fn find(&self, pages: usize, needle: &str, limit: usize) -> Vec<(usize, String)> {
        let mut out = Vec::new();
        if limit == 0 {
            return out;
        }
        let needle: Vec<char> = needle.to_lowercase().chars().collect();
        if needle.is_empty() {
            return out;
        }
        for idx in 0..pages {
            if out.len() >= limit {
                break;
            }
            let Some(text) = self.page_text(idx) else {
                continue;
            };
            if let Some((start, end)) = find_span(&text, &needle) {
                out.push((idx, snippet(&text, start, end)));
            }
        }
        out
    }

    /// 大纲：从虚拟根（`parent = NULL`）开始深度优先。
    fn outline(&self) -> Vec<crate::doc::Toc> {
        let mut out = Vec::new();
        let Some(bm) = self.fns.bookmarks() else {
            return out;
        };
        unsafe { walk_bookmarks(self.doc, std::ptr::null_mut(), bm, 1, &mut out) };
        out
    }
}

/// 走一层书签：先自己、再孩子、再兄弟。`depth` 从 1 开始。
///
/// 深度封顶 8 层：畸形文件里的环状书签不该把栈打爆（栈溢出 = 进程直接死）。
unsafe fn walk_bookmarks(
    doc: *mut c_void,
    parent: *mut c_void,
    bm: BookmarkFns,
    depth: u8,
    out: &mut Vec<crate::doc::Toc>,
) {
    const MAX_DEPTH: u8 = 8;
    let mut cur = (bm.first_child)(doc, parent);
    while !cur.is_null() {
        if let Some(title) = bookmark_title(bm.title, cur) {
            let dest = (bm.dest)(doc, cur);
            let mut page = 0usize;
            if !dest.is_null() {
                let i = (bm.dest_page)(doc, dest);
                if i >= 0 {
                    page = i as usize;
                }
            }
            out.push(crate::doc::Toc { depth, title, page });
        }
        if depth < MAX_DEPTH {
            walk_bookmarks(doc, cur, bm, depth + 1, out);
        }
        cur = (bm.next_sibling)(doc, cur);
    }
}

/// 取书签标题（UTF-16LE）。空标题 / 畸形 / 超长 → `None`。
unsafe fn bookmark_title(get: BookmarkTitleFn, bm: *mut c_void) -> Option<String> {
    // 第一次传空缓冲问字节数（含结尾 NUL），第二次真取。
    let bytes = get(bm, std::ptr::null_mut(), 0);
    if bytes < 4 || bytes > MAX_TITLE_BYTES {
        return None;
    }
    let mut buf = vec![0u8; bytes as usize];
    let got = get(bm, buf.as_mut_ptr() as *mut c_void, bytes).min(bytes);
    // 小端两个字节一个码元；奇数长度时最后那个孤字节丢掉。
    let units: Vec<u16> = buf[..got as usize]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let title = utf16le_to_string(&units);
    if title.is_empty() {
        None
    } else {
        Some(title)
    }
}

/// UTF-16 码元 → `String`。
///
/// pdfium 给的都是 UTF-16LE，调用方按小端拆成码元再进来（标题还带结尾 NUL）。
/// 两条约定：
///
/// * `0` 当结束符丢掉——pdfium 的缓冲永远带一个；
/// * 孤立/错位的代理项换成 `U+FFFD`，**不 panic**（`char::decode_utf16` 的语义）。
fn utf16le_to_string(units: &[u16]) -> String {
    let mut out = String::with_capacity(units.len());
    for c in char::decode_utf16(units.iter().copied()) {
        match c {
            Ok('\0') => {}
            Ok(c) => out.push(c),
            Err(_) => out.push(char::REPLACEMENT_CHARACTER),
        }
    }
    out
}

/// 大小写不敏感地在 `hay` 里找 `needle`（已小写），返回**原文字节**区间。
///
/// 不能拿 `to_lowercase()` 的下标当原串下标：'İ' 之类小写后会变成一个以上
/// 字符，偏移就对不上了，切片段会在非 char 边界上 panic。所以按字符建表，
/// 记住每个小写单元对应的原文字节偏移。
fn find_span(hay: &str, needle: &[char]) -> Option<(usize, usize)> {
    if needle.is_empty() {
        return None;
    }
    let mut units: Vec<char> = Vec::with_capacity(hay.len());
    let mut offs: Vec<usize> = Vec::with_capacity(hay.len());
    for (i, c) in hay.char_indices() {
        for lc in c.to_lowercase() {
            units.push(lc);
            offs.push(i);
        }
    }
    if needle.len() > units.len() {
        return None;
    }
    let pos = (0..=units.len() - needle.len()).find(|&i| units[i..i + needle.len()] == *needle)?;
    let start = offs[pos];
    // 末字符的**原文**字节终点：小写单元可能比原文多（'İ' → "i̇"），
    // 所以取最后一个单元对应的原字符再往后一个 char。
    let last = offs[pos + needle.len() - 1];
    let end = last + hay[last..].chars().next().map_or(0, char::len_utf8);
    Some((start, end))
}

/// 命中处前后各留 30 个字符；换行/制表压成空格（搜索结果只有一行）。
fn snippet(text: &str, start: usize, end: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    let a = text[..start.min(text.len())]
        .chars()
        .count()
        .saturating_sub(30);
    let b = (text[..end.min(text.len())].chars().count() + 30).min(chars.len());
    let mut body = String::with_capacity(b.saturating_sub(a) + 2);
    let mut space = false;
    for c in &chars[a..b] {
        let c = if c.is_whitespace() { ' ' } else { *c };
        if c == ' ' && space {
            continue;
        }
        space = c == ' ';
        body.push(c);
    }
    let body = body.trim();
    let mut out = String::with_capacity(body.len() + 4);
    if a > 0 {
        out.push('…');
    }
    out.push_str(body);
    if b < chars.len() {
        out.push('…');
    }
    out
}

thread_local! {
    /// 最近打开的一份 PDF。切页命中，换书才重开。
    static DOCS: RefCell<SlotCache<PdfDoc>> = const { RefCell::new(SlotCache::new()) };
}

/// 一份已打开的 PDF：渲染后端（页缓存里还带着 `FPDF_DOCUMENT`）+ 文字层。
///
/// 文字层要 `FPDF_DOCUMENT` 指针，而那个指针藏在 [`CachedDoc`] 里拿不出来，
/// 所以在这里和 `CachedDoc` 一起挂到同一个 `Rc` 上：同路径的句柄共享渲染
/// 缓存，也共享文字缓存。
struct PdfDoc {
    cached: CachedDoc<Pdfium>,
    text: TextLayer,
}

/// 一份 PDF 的对外句柄。
///
/// 同一个路径多次 `open` 拿到的是**同一个底层文档 + 同一个页缓存**；
/// 句柄各自 drop 也没关系（`Rc` 管生命周期）。
pub struct Pdf {
    doc: Rc<PdfDoc>,
}

/// 文档缓存键：路径 + 文件长度 + mtime。
///
/// 加元数据是为了不变语义：文件被替换过（同步、重新导入）就重开一份，
/// 和"每次 open 都重新 LoadDocument"的旧行为一致。
fn doc_key(path: &str) -> String {
    use std::time::UNIX_EPOCH;
    match std::fs::metadata(path) {
        Ok(m) => {
            let mt = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("{path}|{}|{mt}", m.len())
        }
        Err(_) => path.to_string(),
    }
}

impl Pdf {
    pub fn open(path: &str) -> Result<Pdf, String> {
        let key = doc_key(path);
        let doc = DOCS.with(|slot| {
            let mut slot = slot.borrow_mut();
            slot.get_or_try_insert(&key, || {
                let pdfium = Pdfium::load(path)?;
                let text = TextLayer::new(pdfium.text_fns, pdfium.doc);
                Ok::<_, String>(Rc::new(PdfDoc {
                    cached: CachedDoc::new(pdfium, DEFAULT_PAGES),
                    text,
                }))
            })
        })?;
        Ok(Pdf { doc })
    }

    pub fn page_count(&self) -> usize {
        self.doc.cached.page_count()
    }

    /// 第 `idx` 页的高宽比（h/w）。同一页问多次只向后端要一次。
    pub fn aspect(&self, idx: usize) -> f32 {
        self.doc.cached.aspect(idx as u32)
    }

    /// 把第 `idx` 页渲染成 `w×h` 的灰度图（255 = 白）。返回长度 `w*h`。
    ///
    /// 同样的 (页号, w, h) 第二次请求直接返回缓存副本，不再走 pdfium。
    pub fn render(&self, idx: usize, w: usize, h: usize) -> Result<Vec<u8>, String> {
        self.doc.cached.render(idx as u32, w as u32, h as u32)
    }

    /// 第 `idx` 页的纯文本。缺符号 / 页不存在 → `None`（**不 panic**）。
    ///
    /// 每页只向 pdfium 要一次，之后走缓存。
    pub fn page_text(&self, idx: usize) -> Option<String> {
        self.doc.text.page_text(idx)
    }

    /// 全书搜索，大小写不敏感；每页最多一条，凑够 `limit` 就停。
    /// 逐页懒取（命中前的页才解析），搜不到 → 空 Vec。
    pub fn find(&self, needle: &str, limit: usize) -> Vec<(usize, String)> {
        self.doc.text.find(self.page_count(), needle, limit)
    }

    /// 大纲（pdfium 书签树）。没有大纲 / 缺符号 → 空 Vec。
    pub fn outline(&self) -> Vec<crate::doc::Toc> {
        self.doc.text.outline()
    }

    /// 预热第 `page` 页：按 `screen_w × screen_h` 做 contain 适配
    /// （公式和调用方算 `ww/hh` 的那几行一致），渲染一次塞进缓存。
    ///
    /// **同步、不开线程**；返回 `true` = 真的渲染了，`false` = 已在缓存 /
    /// 页号越界 / 渲染失败（预热不该影响前台，所以静默）。
    ///
    /// 调用时机由调用方定：比如当前页已经刷新到屏幕之后，再预热下一页。
    pub fn prefetch(&self, page: u32, screen_w: u32, screen_h: u32) -> bool {
        self.doc.cached.prefetch(page, screen_w, screen_h)
    }

    pub fn cache_stats(&self) -> CacheStats {
        self.doc.cached.stats()
    }

    /// 真正落到 pdfium 的渲染次数（含预热）。日志/诊断用。
    pub fn render_count(&self) -> u64 {
        self.doc.cached.stats().renders
    }

    pub fn clear_cache(&self) {
        self.doc.cached.clear_cache()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 主机上没有 libpdfium，设备上也没有这个假文件：两条路径都该是 Err
    /// 而不是 panic，并且**失败不进单槽缓存**。
    #[test]
    fn open_failure_is_clean() {
        assert!(Pdf::open("/nonexistent/rmdev-pagecache-test.pdf").is_err());
        assert_eq!(
            DOCS.with(|s| s.borrow().current_key().map(str::to_string)),
            None
        );
    }

    // ---------------- UTF-16LE 解码 ----------------

    /// pdfium 的中文文本就是普通 BMP 码元，别被"UTF-16LE"这个名字骗了。
    #[test]
    fn utf16_chinese_and_ascii() {
        // "中文 abc"
        let units = [0x4E2D, 0x6587, 0x0020, 0x0061, 0x0062, 0x0063];
        assert_eq!(utf16le_to_string(&units), "中文 abc");
        assert_eq!(utf16le_to_string(&[]), "");
    }

    /// 代理对（emoji）要合成一个 char；孤立的半个代理项换成 U+FFFD，不能炸。
    #[test]
    fn utf16_surrogate_pair_and_lone_surrogate() {
        // U+1F600 = D83D DE00
        assert_eq!(utf16le_to_string(&[0xD83D, 0xDE00]), "\u{1F600}");
        assert_eq!(
            utf16le_to_string(&[0x0041, 0xD83D]),
            "A\u{FFFD}",
            "缺低位的孤立代理项 → 替换字符"
        );
        assert_eq!(
            utf16le_to_string(&[0xDE00, 0x0042]),
            "\u{FFFD}B",
            "反向孤立的低代理项也要吃掉"
        );
        // 奇数个码元（3 个）也照常走，不做 len & 1 这种假设
        assert_eq!(utf16le_to_string(&[0x0041, 0x0042, 0xD83D]), "AB\u{FFFD}");
    }

    /// 结尾 NUL（pdfium 的缓冲永远带一个）连同中间误入的 NUL 一起丢掉。
    #[test]
    fn utf16_nul_is_dropped() {
        assert_eq!(utf16le_to_string(&[0x41, 0x00]), "A");
        assert_eq!(utf16le_to_string(&[0x41, 0x00, 0x42]), "AB");
        assert_eq!(utf16le_to_string(&[0x4E2D, 0x0000, 0x6587]), "中文");
    }

    // ---------------- 假的 pdfium 文字层 ----------------
    //
    // 本机没有 libpdfium，所以 `page_text`/`find`/`outline` 的算法只能靠一张
    // **假的 `TextFns` 表**来验证：下面这几个 `extern "C"` 假函数就是 pdfium，
    // 句柄值 = 下标 + 1（不 deref，只当不透明指针用）。
    mod fake {
        use super::*;
        use std::cell::RefCell;

        thread_local! {
            static PAGES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
            /// `get_text` 被调了几次 —— 用来验证懒加载和"取过就缓存"。
            static CALLS: RefCell<usize> = const { RefCell::new(0) };
            /// 扁平书签树：`(标题, 页号, 父节点下标)`。
            static TREE: RefCell<Vec<(String, c_int, Option<usize>)>> =
                const { RefCell::new(Vec::new()) };
        }

        pub fn install_pages(pages: &[&str]) {
            PAGES.with(|p| *p.borrow_mut() = pages.iter().map(|s| (*s).to_string()).collect());
            CALLS.with(|c| *c.borrow_mut() = 0);
        }

        pub fn install_bookmarks(nodes: &[(&str, c_int, Option<usize>)]) {
            TREE.with(|t| {
                *t.borrow_mut() = nodes
                    .iter()
                    .map(|(s, p, parent)| ((*s).to_string(), *p, *parent))
                    .collect()
            });
        }

        pub fn calls() -> usize {
            CALLS.with(|c| *c.borrow())
        }

        fn id_of(h: *mut c_void) -> Option<usize> {
            if h.is_null() {
                None
            } else {
                Some(h as usize - 1)
            }
        }

        fn page_units(h: *mut c_void) -> Vec<u16> {
            PAGES.with(|p| {
                p.borrow()
                    .get(id_of(h).unwrap_or(usize::MAX))
                    .map(|s| s.encode_utf16().collect())
                    .unwrap_or_default()
            })
        }

        unsafe extern "C" fn load_page(_doc: *mut c_void, idx: c_int) -> *mut c_void {
            (idx as usize + 1) as *mut c_void
        }

        unsafe extern "C" fn close_page(_page: *mut c_void) {}

        unsafe extern "C" fn load_text(page: *mut c_void) -> *mut c_void {
            page
        }

        unsafe extern "C" fn close_text(_tp: *mut c_void) {}

        unsafe extern "C" fn count_chars(tp: *mut c_void) -> c_int {
            page_units(tp).len() as c_int
        }

        /// 按 pdfium 的语义：写 `count` 个码元 + 结尾 NUL，返回写入数（含 NUL）。
        unsafe extern "C" fn get_text(
            tp: *mut c_void,
            start: c_int,
            count: c_int,
            buf: *mut u16,
        ) -> c_int {
            CALLS.with(|c| *c.borrow_mut() += 1);
            let units = page_units(tp);
            let start = (start.max(0) as usize).min(units.len());
            let end = (start + count.max(0) as usize).min(units.len());
            let slice = &units[start..end];
            if !buf.is_null() {
                for (i, u) in slice.iter().enumerate() {
                    *buf.add(i) = *u;
                }
                *buf.add(slice.len()) = 0;
            }
            (slice.len() + 1) as c_int
        }

        unsafe extern "C" fn bm_first(_doc: *mut c_void, parent: *mut c_void) -> *mut c_void {
            let want = id_of(parent);
            TREE.with(|t| {
                t.borrow()
                    .iter()
                    .position(|(_, _, p)| *p == want)
                    .map_or(std::ptr::null_mut(), |i| (i + 1) as *mut c_void)
            })
        }

        unsafe extern "C" fn bm_next(_doc: *mut c_void, bm: *mut c_void) -> *mut c_void {
            let Some(cur) = id_of(bm) else {
                return std::ptr::null_mut();
            };
            TREE.with(|t| {
                let t = t.borrow();
                let want = t[cur].2;
                (cur + 1..t.len())
                    .find(|&i| t[i].2 == want)
                    .map_or(std::ptr::null_mut(), |i| (i + 1) as *mut c_void)
            })
        }

        /// 和 pdfium 一样：缓冲不够大就不拷，只返回需要的字节数（含 NUL）。
        unsafe extern "C" fn bm_title(
            bm: *mut c_void,
            buf: *mut c_void,
            buflen: c_ulong,
        ) -> c_ulong {
            let Some(i) = id_of(bm) else {
                return 0;
            };
            let bytes: Vec<u8> = TREE.with(|t| {
                let mut v: Vec<u8> = t.borrow()[i]
                    .0
                    .encode_utf16()
                    .flat_map(u16::to_le_bytes)
                    .collect();
                v.extend_from_slice(&[0, 0]);
                v
            });
            if !buf.is_null() && buflen as usize >= bytes.len() {
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, bytes.len());
            }
            bytes.len() as c_ulong
        }

        unsafe extern "C" fn bm_dest(_doc: *mut c_void, bm: *mut c_void) -> *mut c_void {
            bm
        }

        unsafe extern "C" fn bm_dest_page(_doc: *mut c_void, dest: *mut c_void) -> c_int {
            match id_of(dest) {
                Some(i) => TREE.with(|t| t.borrow()[i].1),
                None => -1,
            }
        }

        /// 只有文字层符号（没有书签）。
        pub fn text_fns() -> TextFns {
            TextFns {
                load_page: Some(load_page),
                close_page: Some(close_page),
                load_text: Some(load_text),
                count_chars: Some(count_chars),
                get_text: Some(get_text),
                close_text: Some(close_text),
                ..TextFns::default()
            }
        }

        /// 只有书签符号（没有文字层）。
        pub fn bookmark_fns() -> TextFns {
            TextFns {
                first_child: Some(bm_first),
                next_sibling: Some(bm_next),
                bookmark_title: Some(bm_title),
                bookmark_dest: Some(bm_dest),
                dest_page: Some(bm_dest_page),
                ..TextFns::default()
            }
        }
    }

    #[test]
    fn find_ignores_case_and_honours_limit() {
        fake::install_pages(&["Alpha beta", "第二页 Rust 真好", "gamma rust"]);
        let t = TextLayer::new(fake::text_fns(), std::ptr::null_mut());

        assert_eq!(
            t.find(3, "RUST", 10),
            vec![
                (1, "第二页 Rust 真好".to_string()),
                (2, "gamma rust".to_string())
            ],
            "大小写不敏感，每页一条，保持页序"
        );
        assert_eq!(t.find(3, "rust", 1).len(), 1, "limit 要生效");
        assert_eq!(t.find(3, "真好", 10).len(), 1, "中文照常能搜");
        assert!(t.find(3, "zzz", 10).is_empty());
        assert!(t.find(3, "", 10).is_empty(), "空 needle 直接空");
        assert!(t.find(3, "rust", 0).is_empty(), "limit = 0 直接空");
    }

    /// 懒加载 + 缓存：命中就停，第二次搜同一个词不再碰 pdfium。
    #[test]
    fn find_is_lazy_and_caches_pages() {
        let mut pages: Vec<String> = (0..100).map(|i| format!("page {i} nothing at all")).collect();
        pages[5] = "page 5 has the needle".to_string();
        let refs: Vec<&str> = pages.iter().map(String::as_str).collect();
        fake::install_pages(&refs);
        let t = TextLayer::new(fake::text_fns(), std::ptr::null_mut());

        let hits = t.find(pages.len(), "needle", 1);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, 5);
        assert_eq!(fake::calls(), 6, "命中后立刻停：只该打开 0..=5 这 6 页");

        assert_eq!(t.find(pages.len(), "needle", 1), hits);
        assert_eq!(fake::calls(), 6, "第二次全走缓存，一页都不该重取");
    }

    /// 片段 = 命中前后各 30 个字符 + 两端省略号。
    #[test]
    fn snippet_keeps_thirty_chars_of_context() {
        let long = "a".repeat(100) + "TARGET" + &"b".repeat(100);
        fake::install_pages(&[long.as_str()]);
        let t = TextLayer::new(fake::text_fns(), std::ptr::null_mut());
        let hits = t.find(1, "target", 5);
        assert_eq!(hits.len(), 1);
        let s = &hits[0].1;
        assert!(s.starts_with('…'), "前面被截断要有省略号：{s}");
        assert!(s.ends_with('…'));
        assert!(s.contains("TARGET"));
        assert_eq!(s.chars().count(), 1 + 30 + 6 + 30 + 1);
    }

    /// 大纲：深度优先、depth 从 1 起、页号走 `FPDFDest_GetDestPageIndex`。
    #[test]
    fn outline_walks_bookmark_tree() {
        fake::install_bookmarks(&[
            ("Chapter 1", 0, None),
            ("Section 1.1", 2, Some(0)),
            ("Chapter 2", 5, None),
        ]);
        let toc = TextLayer::new(fake::bookmark_fns(), std::ptr::null_mut()).outline();
        assert_eq!(toc.len(), 3);
        let got: Vec<(u8, &str, usize)> = toc
            .iter()
            .map(|t| (t.depth, t.title.as_str(), t.page))
            .collect();
        assert_eq!(
            got,
            vec![
                (1, "Chapter 1", 0),
                (2, "Section 1.1", 2),
                (1, "Chapter 2", 5),
            ]
        );
    }

    // ---------------- 缺符号降级 ----------------

    // 一组空实现，只为拼出一个"没有 pdfium"的 `Pdfium`：文字层的方法根本
    // 走不到它们（符号全 `None` 时提前返回）。
    unsafe extern "C" fn no_page_size(_page: *mut c_void) -> f32 {
        1.0
    }
    unsafe extern "C" fn no_load_page(_doc: *mut c_void, _idx: c_int) -> *mut c_void {
        std::ptr::null_mut()
    }
    unsafe extern "C" fn no_create_ex(
        _w: c_int,
        _h: c_int,
        _format: c_int,
        _buf: *mut c_void,
        _stride: c_int,
    ) -> *mut c_void {
        std::ptr::null_mut()
    }
    unsafe extern "C" fn no_fill_rect(
        _bmp: *mut c_void,
        _x: c_int,
        _y: c_int,
        _w: c_int,
        _h: c_int,
        _color: u32,
    ) {
    }
    unsafe extern "C" fn no_render(
        _bmp: *mut c_void,
        _page: *mut c_void,
        _x: c_int,
        _y: c_int,
        _w: c_int,
        _h: c_int,
        _rotate: c_int,
        _flags: c_int,
    ) {
    }
    unsafe extern "C" fn no_destroy(_bmp: *mut c_void) {}
    unsafe extern "C" fn no_close_page(_page: *mut c_void) {}
    unsafe extern "C" fn no_close_doc(_doc: *mut c_void) {}

    impl Pdf {
        /// 符号全缺的 `Pdf`（= 本机跑单测时的真实情况）：空句柄 + 空符号表。
        fn without_pdfium() -> Pdf {
            let pdfium = Pdfium {
                lib: std::ptr::null_mut(),
                doc: std::ptr::null_mut(),
                page_count: 0,
                fns: Fns {
                    page_w: no_page_size,
                    page_h: no_page_size,
                    load_page: no_load_page,
                    create_ex: no_create_ex,
                    fill_rect: no_fill_rect,
                    render: no_render,
                    destroy: no_destroy,
                    close_page: no_close_page,
                    close_doc: no_close_doc,
                },
                text_fns: TextFns::default(),
            };
            Pdf {
                doc: Rc::new(PdfDoc {
                    cached: CachedDoc::new(pdfium, 0),
                    text: TextLayer::new(TextFns::default(), std::ptr::null_mut()),
                }),
            }
        }
    }

    /// 任何一个符号取不到：`page_text` → `None`，`find`/`outline` → 空，
    /// `Doc` 的转发路径也一样，**绝不 panic**。
    #[test]
    fn pdf_without_text_symbols_degrades() {
        let pdf = Pdf::without_pdfium();
        assert!(pdf.page_text(0).is_none());
        assert!(pdf.page_text(9999).is_none());
        assert!(pdf.find("hello", 10).is_empty());
        assert!(pdf.outline().is_empty());

        use crate::doc::Doc;
        assert!(Doc::search(&pdf, "hello", 10).is_empty());
        assert!(Doc::toc(&pdf).is_empty());
    }
}
