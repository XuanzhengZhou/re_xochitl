//! EPUB 阅读：ZIP + OPF + XHTML → 段落文本 → 分页 → 灰度位图。
//!
//! # 为什么手写
//!
//! 设备是 armv7，依赖越少越好：这里只用到 `miniz_oxide`（纯 Rust、无 unsafe）
//! 解 zip 的 deflate 流，ZIP 目录和 XML 都是自己扫的。XML 只当**标签流**处理，
//! 不做树/命名空间/CSS——阅读器要的是"把字排出来"，不是"还原版式"。
//!
//! # 坐标系
//!
//! 排版在**排版单位**里做：页面固定 `1000 × 1400`（[`PAGE_W`]/[`PAGE_H`]），
//! 字号、页边距都是这个单位。`page_count`/`aspect` 与 UI 传进来的像素尺寸无关，
//! 只有 `render` 才乘以缩放系数（`w / PAGE_W`，必要时再被 `h / PAGE_H` 夹住）。
//! 好处：同一份分页结果可以渲染成缩略图和整页，翻页时不用重排。
//!
//! # 缓存
//!
//! * 文档（解析好的 zip + 分段文本）：按路径 + 长度 + mtime 留最近两份，
//!   UI 若每帧都 `Epub::open(同一路径)` 也只是 `Rc` 克隆，不重新解压；
//! * 分页结果：跟在文档上，键 = (字号, 行距, 页边距)，改设置才重排；
//! * 最近一次渲染：同一页同尺寸重复要（重绘/翻回来）直接返回缓存副本。
//!
//! `render` 是纯函数：同样的 (页号, w, h, 设置) 一定逐字节相同。

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use super::reflow::{
    self, fonts, paginate, Block, Book, Cfg, Chapter, Img, Kind, Layout, Page, TocEntry, PAGE_H,
    PAGE_W,
};
pub use super::reflow::{
    DEFAULT_FONT_SIZE, DEFAULT_LINE_SPACING, DEFAULT_MARGIN, FONT_SIZES, LINE_SPACINGS, MARGINS,
};

/// 单个 zip 条目解压上限：防压缩炸弹把设备内存吃光。
const MAX_ENTRY: usize = 32 << 20;
/// 整个 epub 文件上限（正常书远小于这个数）。
const MAX_FILE: u64 = 128 << 20;
/// 抽出来的正文总字符上限：再长的书也只读到这儿，保证 open 有界。
const MAX_CHARS: usize = 8 << 20;
/// zip 中央目录里的条目数上限。
const MAX_ENTRIES: usize = 20_000;
/// 图片解码缓存条数（解码结果 `Arc<Img>`，8 张够一屏的装饰图复用）。
const IMG_CACHE: usize = 8;

// ---------------------------------------------------------------------------
// 对外接口
// ---------------------------------------------------------------------------

/// 一份已打开、已分好页的 EPUB。
///
/// `open` 之后 `page_count`/`render` 都是 O(1) 取缓存；改阅读设置只标记失效，
/// 下一次 `page_count`/`render` 时重排。
pub struct Epub {
    shared: Rc<Shared>,
    font_size: f32,
    line_spacing: f32,
    margin: i32,
}

impl Epub {
    /// 打开一个 `.epub`。打不开一律 `Err`（英文短句，UI 直接当"打不开"处理）。
    ///
    /// 这里会把分页也做掉（[`Self::open`] 贵一次，之后翻页只要渲染）。
    /// 空 spine 不报错：`page_count()` 会返回 0，`render` 返回越界错误。
    pub fn open(path: &str) -> Result<Epub, String> {
        let shared = shared_for(path)?;
        let epub = Epub {
            shared,
            font_size: DEFAULT_FONT_SIZE,
            line_spacing: DEFAULT_LINE_SPACING,
            margin: DEFAULT_MARGIN,
        };
        epub.ensure_layout();
        Ok(epub)
    }

    /// 按当前字号/行距排版后有几页。空书为 0。
    pub fn page_count(&self) -> usize {
        self.ensure_layout();
        self.shared
            .pag
            .borrow()
            .as_ref()
            .map_or(0, |l| l.pages.len())
    }

    /// 页宽高比（**w/h**）。
    ///
    /// 注意：`doc::pdf::Pdf::aspect` 返的是 **h/w**（那边直接 `w = fh / asp`）。
    /// EPUB 这边按接口约定给 w/h，UI 用 `w = min(fw, fh * aspect)` 这类公式。
    pub fn aspect(&self, _idx: usize) -> f32 {
        PAGE_W / PAGE_H
    }

    /// 把第 `idx` 页渲染成 `w×h` 灰度图（行优先，0=黑 255=白，白底黑字）。
    ///
    /// 页号越界、尺寸为 0 返回 `Err`；其余情况不会 panic。
    pub fn render(&self, idx: usize, w: usize, h: usize) -> Result<Vec<u8>, String> {
        if w == 0 || h == 0 {
            return Err("render size must be positive".into());
        }
        self.ensure_layout();
        let cfg = self.cfg();
        let page_count = {
            let lay = self.shared.pag.borrow();
            lay.as_ref().map_or(0, |l| l.pages.len())
        };
        if idx >= page_count {
            return Err(format!("page {idx} out of range (0..{page_count})"));
        }
        // 重绘/翻回来直接吃缓存：UI 常常一帧里问两次同一页
        if let Some(slot) = self.shared.last.borrow().as_ref() {
            if slot.idx == idx && slot.w == w && slot.h == h && slot.cfg == cfg {
                return Ok(slot.gray.clone());
            }
        }
        let page: Page = {
            let lay = self.shared.pag.borrow();
            lay.as_ref()
                .map_or_else(Page::default, |l| l.pages[idx].clone())
        };
        // 光栅化交给共享排版引擎：宽度优先填满，页高不够时按高夹住，
        // 传进来的 (w,h) 比例和 aspect 不一致时空白居中。
        let gray = reflow::draw_page(&page, w, h);
        let _unused_scale_marker = 0;
        *self.shared.last.borrow_mut() = Some(RenderSlot {
            idx,
            w,
            h,
            cfg,
            gray: gray.clone(),
        });
        Ok(gray)
    }

    /// 字号（排版单位）。超出 [8, 72] 会被夹住。
    pub fn set_font_size(&mut self, size: f32) {
        if size.is_finite() {
            self.font_size = size.clamp(8.0, 72.0);
        }
    }

    /// 行距倍数（× 字号）。超出 [1.0, 3.0] 会被夹住。
    pub fn set_line_spacing(&mut self, k: f32) {
        if k.is_finite() {
            self.line_spacing = k.clamp(1.0, 3.0);
        }
    }

    /// 页边距（排版单位）。夹在 [0, 400]，保证正文至少还有 200 单位宽。
    pub fn set_margin(&mut self, px: i32) {
        self.margin = px.clamp(0, (PAGE_W as i32 - 200) / 2);
    }

    /// 目录：(层级 1..=3, 标题, 起始页)。解析不出来就是空 `Vec`。
    pub fn toc(&self) -> Vec<(u8, String, usize)> {
        self.ensure_layout();
        let lay = self.shared.pag.borrow();
        let Some(lay) = lay.as_ref() else {
            return Vec::new();
        };
        let doc = &self.shared.doc;
        doc.toc
            .iter()
            .filter_map(|e| {
                let ci = *doc.path_index.get(&e.path)?;
                let start = lay.chapter_page.get(ci).copied().unwrap_or(0);
                // 锚点指向标题时用标题所在页，否则退到章首页
                let page = if e.anchor.is_empty() {
                    start
                } else {
                    lay.anchors
                        .get(&(ci, e.anchor.clone()))
                        .copied()
                        .unwrap_or(start)
                };
                Some((e.level.clamp(1, 3), e.title.clone(), page))
            })
            .collect()
    }

    /// [`crate::doc::Doc`] 要的形状：`toc()` 的别名。
    pub fn outline(&self) -> Vec<crate::doc::Toc> {
        self.toc()
            .into_iter()
            .map(|(depth, title, page)| crate::doc::Toc {
                depth,
                title,
                page,
            })
            .collect()
    }

    /// 全文搜索（按页）。没排过版就先排。
    pub fn find(&self, needle: &str, limit: usize) -> Vec<(usize, String)> {
        self.ensure_layout();
        let lay = self.shared.pag.borrow();
        lay.as_ref()
            .map_or_else(Vec::new, |l| reflow::search(l, needle, limit))
    }

    /// 第 `idx` 页属于哪一章：取「起始页 <= idx」的最后一个一级目录项。
    pub fn chapter_title(&self, idx: usize) -> Option<String> {
        let mut best: Option<(usize, String)> = None;
        for (depth, title, page) in self.toc() {
            if depth == 1 && page <= idx && best.as_ref().map_or(true, |(p, _)| page >= *p) {
                best = Some((page, title));
            }
        }
        best.map(|(_, t)| t)
    }

    pub fn current_layout(&self) -> crate::doc::LayoutCfg {
        crate::doc::LayoutCfg::from_reflow(self.cfg())
    }

    /// 换排版参数；参数没变就不重排（返回 false）。
    pub fn apply_layout(&mut self, cfg: crate::doc::LayoutCfg) -> bool {
        if crate::doc::LayoutCfg::from_reflow(self.cfg()) == cfg {
            return false;
        }
        self.font_size = cfg.font_size;
        self.line_spacing = cfg.line_spacing;
        self.set_margin(cfg.margin);
        true
    }

    fn cfg(&self) -> Cfg {
        Cfg {
            font_size: self.font_size,
            line_spacing: self.line_spacing,
            margin: self.margin,
        }
    }

    /// 分页结果不在缓存里（或设置变了）就重排一次。
    fn ensure_layout(&self) {
        let cfg = self.cfg();
        if self
            .shared
            .pag
            .borrow()
            .as_ref()
            .map_or(false, |l| l.cfg == cfg)
        {
            return;
        }
        let fonts = fonts();
        let mut guard = fonts.borrow_mut();
        let layout = paginate(&self.shared.doc, guard.as_mut(), cfg);
        drop(guard);
        *self.shared.pag.borrow_mut() = Some(layout);
    }
}

// ---------------------------------------------------------------------------
// 共享状态 / 文档缓存
// ---------------------------------------------------------------------------

struct Shared {
    doc: Book,
    pag: RefCell<Option<Layout>>,
    last: RefCell<Option<RenderSlot>>,
}

struct RenderSlot {
    idx: usize,
    w: usize,
    h: usize,
    cfg: Cfg,
    gray: Vec<u8>,
}

thread_local! {
    /// 最近解析过的两份书。UI 若照 `Pdf::open` 的老习惯每帧都 open，
    /// 这里保证不会每帧重新解压/扫 XML（解析一次 1 MB 的书是几十毫秒）。
    static DOCS: RefCell<Vec<(String, Rc<Shared>)>> = const { RefCell::new(Vec::new()) };
}

const DOC_SLOTS: usize = 2;

/// 文档缓存键：路径 + 文件长度 + mtime。
///
/// 加元数据是为了不变语义：书被同步/重新导入覆盖过就重解一份。
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

fn shared_for(path: &str) -> Result<Rc<Shared>, String> {
    let key = doc_key(path);
    DOCS.with(|cell| {
        let mut v = cell.borrow_mut();
        if let Some(pos) = v.iter().position(|(k, _)| *k == key) {
            let (k, s) = v.remove(pos);
            v.insert(0, (k, s.clone()));
            return Ok(s);
        }
        // 失败的解析不进缓存：文件可能正在被写完
        let parsed = Rc::new(parse_file(path)?);
        v.insert(0, (key, parsed.clone()));
        v.truncate(DOC_SLOTS);
        Ok(parsed)
    })
}

// ---------------------------------------------------------------------------
// ZIP
// ---------------------------------------------------------------------------

const SIG_LOC: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];
const SIG_CEN: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
const SIG_EOCD: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];

struct ZipEntry {
    name: String,
    method: u16,
    comp_size: usize,
    local_off: usize,
}

struct Zip {
    data: Vec<u8>,
    entries: Vec<ZipEntry>,
    /// 文件名 → entries 下标。另存一份小写表，有些书里 OPF 的 href 大小写和
    /// zip 条目不一致（Windows 上打包的常见），严格匹配会让整本书读不出来。
    by_name: HashMap<String, usize>,
    by_lower: HashMap<String, usize>,
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

impl Zip {
    fn parse(data: Vec<u8>) -> Result<Zip, String> {
        if data.len() < 22 {
            return Err("not a zip archive".into());
        }
        // EOCD 在尾部，注释最长 65535 字节
        let first = data.len().saturating_sub(22 + 0xffff);
        let mut eocd = None;
        for i in (first..=data.len() - 22).rev() {
            if data[i..i + 4] == SIG_EOCD {
                eocd = Some(i);
                break;
            }
        }
        let eocd = eocd.ok_or_else(|| "not a zip archive".to_string())?;
        let count = u16_at(&data, eocd + 10).unwrap_or(0) as usize;
        let cd_off = u32_at(&data, eocd + 16).unwrap_or(0) as usize;
        if count == 0 || cd_off == 0 || cd_off >= data.len() {
            return Err("zip archive has no entries".into());
        }
        if count > MAX_ENTRIES {
            return Err("zip archive has too many entries".into());
        }
        let mut entries = Vec::with_capacity(count);
        let mut p = cd_off;
        for _ in 0..count {
            if data.get(p..p + 4) != Some(&SIG_CEN[..]) {
                // 中央目录被截断/伪造：能读到几条算几条，但一条都没有就报错
                break;
            }
            if u16_at(&data, p + 8).unwrap_or(0) & 1 != 0 {
                return Err("encrypted ebook is not supported".into());
            }
            let method = u16_at(&data, p + 10).unwrap_or(0);
            let comp_size = u32_at(&data, p + 20).unwrap_or(0) as usize;
            let local_off = u32_at(&data, p + 42).unwrap_or(0) as usize;
            let nlen = u16_at(&data, p + 28).unwrap_or(0) as usize;
            let elen = u16_at(&data, p + 30).unwrap_or(0) as usize;
            let clen = u16_at(&data, p + 32).unwrap_or(0) as usize;
            let name = data
                .get(p + 46..p + 46 + nlen)
                .map(|b| String::from_utf8_lossy(b).into_owned())
                .unwrap_or_default();
            p += 46 + nlen + elen + clen;
            if comp_size == 0xffff_ffff || local_off == 0xffff_ffff {
                return Err("zip64 archives are not supported".into());
            }
            if name.is_empty() {
                continue;
            }
            entries.push(ZipEntry {
                name: name.trim_start_matches("./").to_string(),
                method,
                comp_size,
                local_off,
            });
        }
        if entries.is_empty() {
            return Err("zip archive has no entries".into());
        }
        let mut by_name = HashMap::with_capacity(entries.len());
        let mut by_lower = HashMap::with_capacity(entries.len());
        for (i, e) in entries.iter().enumerate() {
            by_name.insert(e.name.clone(), i);
            by_lower.entry(e.name.to_ascii_lowercase()).or_insert(i);
        }
        Ok(Zip {
            data,
            entries,
            by_name,
            by_lower,
        })
    }

    fn find(&self, name: &str) -> Option<usize> {
        self.by_name
            .get(name)
            .copied()
            .or_else(|| self.by_lower.get(&name.to_ascii_lowercase()).copied())
    }

    /// 读一个条目。不存在返回 `Ok(None)`，数据坏了返回 `Err`。
    fn read(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        let Some(i) = self.find(name) else {
            return Ok(None);
        };
        let e = &self.entries[i];
        let off = e.local_off;
        if self.data.get(off..off + 4) != Some(&SIG_LOC[..]) {
            return Err("corrupt zip local header".into());
        }
        let nlen = u16_at(&self.data, off + 26).unwrap_or(0) as usize;
        let elen = u16_at(&self.data, off + 28).unwrap_or(0) as usize;
        let start = off + 30 + nlen + elen;
        let end = start
            .checked_add(e.comp_size)
            .ok_or_else(|| "corrupt zip entry size".to_string())?;
        let raw = self
            .data
            .get(start..end)
            .ok_or_else(|| "truncated zip entry".to_string())?;
        match e.method {
            0 => {
                if raw.len() > MAX_ENTRY {
                    return Err("zip entry is too large".into());
                }
                Ok(Some(raw.to_vec()))
            }
            8 => miniz_oxide::inflate::decompress_to_vec_with_limit(raw, MAX_ENTRY)
                .map(Some)
                .map_err(|_| "cannot inflate zip entry".to_string()),
            _ => Err("unsupported zip compression method".into()),
        }
    }
}

// ---------------------------------------------------------------------------
// 极简 XML 扫描
// ---------------------------------------------------------------------------

/// 标签流。文本节点只给切片，由调用方决定是否解码实体。
enum Tok<'a> {
    Open { name: &'a str, attrs: &'a str },
    Close { name: &'a str },
    Text(&'a str),
}

/// 扫 `>` 的位置，跳过引号里的内容（属性值里合法地出现 `>`）。
fn tag_end(b: &[u8], mut i: usize) -> Option<usize> {
    let mut quote = 0u8;
    let limit = (i + 100_000).min(b.len());
    while i < limit {
        let c = b[i];
        if quote != 0 {
            if c == quote {
                quote = 0;
            }
        } else if c == b'"' || c == b'\'' {
            quote = c;
        } else if c == b'>' {
            return Some(i);
        }
        i += 1;
    }
    None
}

/// 把 XML/XHTML 拍成标签 + 文本事件。注释、CDATA、DOCTYPE、PI 都处理了
/// （注释整段丢掉；CDATA 当文本）。
fn scan<F: FnMut(Tok<'_>)>(s: &str, mut f: F) {
    let b = s.as_bytes();
    let mut i = 0usize;
    let mut text_start = 0usize;
    while i < b.len() {
        if b[i] != b'<' {
            i += 1;
            continue;
        }
        if i > text_start {
            f(Tok::Text(&s[text_start..i]));
        }
        let rest = &s[i..];
        if rest.starts_with("<!--") {
            i = match rest.find("-->") {
                Some(k) => i + k + 3,
                None => b.len(),
            };
            text_start = i;
            continue;
        }
        if rest.starts_with("<![CDATA[") {
            let start = i + 9;
            match s[start..].find("]]>") {
                Some(k) => {
                    f(Tok::Text(&s[start..start + k]));
                    i = start + k + 3;
                }
                None => {
                    f(Tok::Text(&s[start..]));
                    i = b.len();
                }
            }
            text_start = i;
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            i = match tag_end(b, i + 2) {
                Some(e) => e + 1,
                None => b.len(),
            };
            text_start = i;
            continue;
        }
        let Some(end) = tag_end(b, i + 1) else { break };
        let inner = &s[i + 1..end];
        let close = inner.starts_with('/');
        let body = inner.trim_start_matches('/').trim_end();
        let body = body.trim_end_matches('/').trim_end();
        let name_end = body.find(|c: char| c.is_whitespace()).unwrap_or(body.len());
        let name = &body[..name_end];
        let attrs = &body[name_end..];
        // 只认本地名（`enc:EncryptionMethod` → `EncryptionMethod`）：
        // 各家的命名空间前缀不统一，带前缀比较会漏标签。
        let name = match name.rfind(':') {
            Some(k) => &name[k + 1..],
            None => name,
        };
        if !name.is_empty() {
            if close {
                f(Tok::Close { name });
            } else {
                f(Tok::Open { name, attrs });
            }
        }
        i = end + 1;
        text_start = i;
    }
    if text_start < b.len() {
        f(Tok::Text(&s[text_start..]));
    }
}

/// 取属性值（大小写不敏感，实体已解码）。没有这个属性返回 `None`。
///
/// 自己扫而不是用正则/XML 库：属性可能单双引号、无引号、带换行，
/// 扫一遍几十行就够了。
fn attr(attrs: &str, want: &str) -> Option<String> {
    let b = attrs.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        let start = i;
        while i < b.len() && b[i] != b'=' && !(b[i] as char).is_whitespace() {
            i += 1;
        }
        let name = &attrs[start..i];
        while i < b.len() && (b[i] as char).is_whitespace() {
            i += 1;
        }
        if i < b.len() && b[i] == b'=' {
            i += 1;
            while i < b.len() && (b[i] as char).is_whitespace() {
                i += 1;
            }
            let (val, next) = if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
                let q = b[i];
                let vstart = i + 1;
                let mut j = vstart;
                while j < b.len() && b[j] != q {
                    j += 1;
                }
                (&attrs[vstart..j], (j + 1).min(b.len()))
            } else {
                let vstart = i;
                let mut j = i;
                while j < b.len() && !(b[j] as char).is_whitespace() {
                    j += 1;
                }
                (&attrs[vstart..j], j)
            };
            if name.eq_ignore_ascii_case(want) {
                return Some(decode_entities(val));
            }
            i = next;
        } else if name.eq_ignore_ascii_case(want) {
            return Some(String::new());
        }
        if i == start {
            i += 1;
        }
    }
    None
}

/// 常见实体解码。`&nbsp;` 解成普通空格——断行时要能吃掉它。
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < s.len() {
        let ch = s[i..].chars().next().unwrap();
        if ch != '&' {
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let tail = &s[i + 1..];
        let matched = tail
            .find(';')
            .filter(|&k| k <= 10)
            .and_then(|k| entity_char(&tail[..k]).map(|c| (c, k + 2)));
        match matched {
            Some((c, skip)) => {
                out.push(c);
                i += skip;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

fn entity_char(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        // &nbsp; 解成普通空格：断行时要能吃掉它
        "nbsp" => Some(' '),
        "hellip" => Some('…'),
        "mdash" => Some('—'),
        "ndash" => Some('–'),
        "ldquo" => Some('“'),
        "rdquo" => Some('”'),
        "lsquo" => Some('‘'),
        "rsquo" => Some('’'),
        _ => {
            let hex = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X"));
            let code = match hex {
                Some(h) => u32::from_str_radix(h, 16).ok()?,
                None => name.strip_prefix('#')?.parse().ok()?,
            };
            char::from_u32(code).or(Some('\u{fffd}'))
        }
    }
}

/// 字节 → 字符串。EPUB 规范要求 UTF-8，但老书常见 windows-1252；
/// 非法 UTF-8 时按 Latin-1 逐字节映射，至少不会满屏替换符。
fn decode_text(bytes: &[u8]) -> String {
    let s = match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    };
    s.trim_start_matches('\u{feff}').to_string()
}

// ---------------------------------------------------------------------------
// 路径
// ---------------------------------------------------------------------------

fn pct_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hi = (b[i + 1] as char).to_digit(16);
            let lo = (b[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn dirname(path: &str) -> &str {
    match path.rfind('/') {
        Some(i) => &path[..i],
        None => "",
    }
}

/// 把 OPF 里的相对 href 拼成 zip 内路径，顺手处理 `./`、`../`。
fn join_path(base: &str, rel: &str) -> String {
    let rel = pct_decode(rel.trim());
    let rel = rel.trim_start_matches('/');
    let joined = if base.is_empty() {
        rel.to_string()
    } else {
        format!("{base}/{rel}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn split_href(href: &str) -> (String, String) {
    match href.split_once('#') {
        Some((p, a)) => (pct_decode(p.trim()), pct_decode(a.trim())),
        None => (pct_decode(href.trim()), String::new()),
    }
}

// ---------------------------------------------------------------------------
// 文档解析：container.xml → OPF → spine → XHTML → 文本
// ---------------------------------------------------------------------------


const BLOCK_TAGS: [&str; 26] = [
    "p",
    "div",
    "section",
    "article",
    "blockquote",
    "li",
    "tr",
    "td",
    "th",
    "pre",
    "table",
    "ul",
    "ol",
    "dl",
    "dd",
    "dt",
    "figure",
    "figcaption",
    "header",
    "footer",
    "aside",
    "nav",
    "main",
    "hgroup",
    "address",
    "hr",
];

fn is_block_tag(name: &str) -> bool {
    BLOCK_TAGS.iter().any(|t| name.eq_ignore_ascii_case(t))
}

fn is_html_path(path: &str) -> bool {
    let p = path.to_ascii_lowercase();
    p.ends_with(".xhtml") || p.ends_with(".html") || p.ends_with(".htm") || p.ends_with(".xml")
}

/// 段落结束：把累积文本推成一个 Block。空段落丢掉。
fn flush(out: &mut Vec<Block>, cur: &mut String, kind: &mut Kind, id: &mut String) {
    let text = cur.trim();
    if !text.is_empty() {
        out.push(Block {
            kind: *kind,
            text: text.to_string(),
            id: std::mem::take(id),
            img: None,
        });
    } else {
        id.clear();
    }
    cur.clear();
    *kind = Kind::Body;
}

/// 文本节点追加：实体解码 + 空白折叠（HTML 里换行/缩进都不算内容）。
fn push_text(cur: &mut String, raw: &str) {
    let decoded = decode_entities(raw);
    let mut prev_space = cur.is_empty() || cur.ends_with(' ');
    for ch in decoded.chars() {
        if ch.is_whitespace() {
            if !prev_space {
                cur.push(' ');
            }
            prev_space = true;
        } else {
            cur.push(ch);
            prev_space = false;
        }
    }
}

/// XHTML → 段落块。保留 `<p>/<br>/<h1..h3>/<li>` 的边界，其余标签只决定
/// 文本进不进（`script`/`style`/`head`/`svg` 不要）。
///
/// `<img src>` 和 SVG 里的 `<image xlink:href>` 产出一个 `Kind::Image` 块，
/// `text` 先放**原始 href**：这里只扫标签流，不知道这本 epub 的 zip 怎么办，
/// 由 [`resolve_images`] 在解析时按 xhtml 自己的目录解析 + 解码。
fn blocks_from_html(html: &str) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut cur = String::new();
    let mut kind = Kind::Body;
    let mut id = String::new();
    // 正在跳过的元素（script/style/head/svg）：只认同名的结束标签
    let mut skip: Option<String> = None;
    scan(html, |tok| match tok {
        Tok::Text(t) => {
            if skip.is_none() {
                push_text(&mut cur, t);
            }
        }
        Tok::Open { name, attrs } => {
            // 图片要在 `skip` 判断**之前**处理：封面图常写成
            // `<svg><image xlink:href="cover.jpg"/></svg>`，而 svg 整棵是跳过的。
            // `<img>` 和 `<image>` 都是空元素，不会打乱 skip 状态。
            if name.eq_ignore_ascii_case("img") || name.eq_ignore_ascii_case("image") {
                let src = attr(attrs, "src")
                    .or_else(|| attr(attrs, "xlink:href"))
                    .or_else(|| attr(attrs, "href"));
                if let Some(src) = src.filter(|s| !s.trim().is_empty()) {
                    flush(&mut out, &mut cur, &mut kind, &mut id);
                    out.push(Block {
                        kind: Kind::Image,
                        text: src,
                        id: String::new(),
                        img: None,
                    });
                }
                return;
            }
            if skip.is_some() {
                return;
            }
            if ["script", "style", "head", "title", "svg"]
                .iter()
                .any(|t| name.eq_ignore_ascii_case(t))
            {
                skip = Some(name.to_string());
                return;
            }
            if name.eq_ignore_ascii_case("br") {
                cur.push('\n');
                return;
            }
            let heading = if name.eq_ignore_ascii_case("h1") {
                Some(Kind::H1)
            } else if name.eq_ignore_ascii_case("h2") {
                Some(Kind::H2)
            } else if name.eq_ignore_ascii_case("h3")
                || name.eq_ignore_ascii_case("h4")
                || name.eq_ignore_ascii_case("h5")
                || name.eq_ignore_ascii_case("h6")
            {
                Some(Kind::H3)
            } else {
                None
            };
            if let Some(h) = heading {
                flush(&mut out, &mut cur, &mut kind, &mut id);
                kind = h;
                id = attr(attrs, "id").unwrap_or_default();
                return;
            }
            if is_block_tag(name) {
                flush(&mut out, &mut cur, &mut kind, &mut id);
            }
        }
        Tok::Close { name } => {
            if let Some(s) = skip.as_deref() {
                if name.eq_ignore_ascii_case(s) {
                    skip = None;
                }
                return;
            }
            if name.eq_ignore_ascii_case("h1")
                || name.eq_ignore_ascii_case("h2")
                || name.eq_ignore_ascii_case("h3")
                || name.eq_ignore_ascii_case("h4")
                || name.eq_ignore_ascii_case("h5")
                || name.eq_ignore_ascii_case("h6")
                || is_block_tag(name)
            {
                flush(&mut out, &mut cur, &mut kind, &mut id);
            }
        }
    });
    flush(&mut out, &mut cur, &mut kind, &mut id);
    out
}

/// 把图片块里的 href 换成真像素。
///
/// `base` 是当前 xhtml 的目录（`src` 相对它解析，`../` 由 [`join_path`] 吃掉）。
/// 找不到文件、格式不支持（GIF/WebP/SVG）、解码失败、或者解码后超过
/// [`reflow::MAX_IMG_PIXELS`]（8 MB）的，或者整本书的图片额度
/// [`reflow::MAX_IMG_TOTAL`] 已经用完的，**整块丢掉**：页面上就少一张图，
/// 不 panic，也不会把一行路径文字印到正文里。
fn resolve_images(
    blocks: &mut Vec<Block>,
    zip: &Zip,
    base: &str,
    cache: &mut reflow::ImgCache,
    budget: &mut usize,
) {
    let mut out: Vec<Block> = Vec::with_capacity(blocks.len());
    for mut b in blocks.drain(..) {
        if b.kind != Kind::Image {
            out.push(b);
            continue;
        }
        let src = std::mem::take(&mut b.text);
        let (rel, _) = split_href(&src);
        if rel.is_empty() {
            continue;
        }
        let path = join_path(base, &rel);
        let Some(img) = load_image(zip, &path, cache, budget) else {
            continue;
        };
        b.text = path;
        b.img = Some(img);
        out.push(b);
    }
    *blocks = out;
}

fn load_image(
    zip: &Zip,
    path: &str,
    cache: &mut reflow::ImgCache,
    budget: &mut usize,
) -> Option<Arc<Img>> {
    if let Some(img) = cache.get(path) {
        return Some(img);
    }
    if *budget == 0 {
        return None;
    }
    let data = zip.read(path).ok().flatten()?;
    let img = Arc::new(reflow::decode_image(&data).ok()?);
    *budget = budget.saturating_sub(img.w * img.h);
    cache.insert(path.to_string(), img.clone());
    Some(img)
}

/// NCX 目录。层级 = `<navPoint>` 的嵌套深度。
fn toc_from_ncx(xml: &str) -> Vec<TocEntry> {
    let mut out = Vec::new();
    let mut depth: u8 = 0;
    let mut in_text = false;
    let mut buf = String::new();
    let mut label: Option<String> = None;
    scan(xml, |tok| match tok {
        Tok::Open { name, attrs } => {
            if name.eq_ignore_ascii_case("navPoint") {
                depth = depth.saturating_add(1);
            } else if name.eq_ignore_ascii_case("text") {
                in_text = true;
                buf.clear();
            } else if name.eq_ignore_ascii_case("content") {
                if let Some(src) = attr(attrs, "src") {
                    let (path, anchor) = split_href(&src);
                    if !path.is_empty() {
                        let title = label.take().unwrap_or_else(|| "Untitled".to_string());
                        out.push(TocEntry {
                            level: depth.clamp(1, 3),
                            title,
                            path,
                            anchor,
                        });
                    }
                }
            }
        }
        Tok::Close { name } => {
            if name.eq_ignore_ascii_case("navPoint") {
                depth = depth.saturating_sub(1);
            } else if name.eq_ignore_ascii_case("text") {
                in_text = false;
                let t = decode_entities(buf.trim());
                if !t.is_empty() {
                    label = Some(t);
                }
            }
        }
        Tok::Text(t) => {
            if in_text {
                buf.push_str(t);
            }
        }
    });
    out
}

/// EPUB3 nav.xhtml 目录。层级 = `<ol>` 嵌套深度。
fn toc_from_nav(xml: &str) -> Vec<TocEntry> {
    let mut out = Vec::new();
    let mut in_nav = false;
    let mut ol: u8 = 0;
    let mut href: Option<String> = None;
    let mut title = String::new();
    scan(xml, |tok| match tok {
        Tok::Open { name, attrs } => {
            if name.eq_ignore_ascii_case("nav") {
                // 只认目录那个 nav（`epub:type="toc"`）；没有 type 的当目录兜底
                in_nav = attrs.to_ascii_lowercase().contains("toc")
                    || !attrs.to_ascii_lowercase().contains("landmarks");
            } else if in_nav && name.eq_ignore_ascii_case("ol") {
                ol = ol.saturating_add(1);
            } else if in_nav && name.eq_ignore_ascii_case("a") {
                href = attr(attrs, "href");
                title.clear();
            }
        }
        Tok::Close { name } => {
            if name.eq_ignore_ascii_case("nav") {
                in_nav = false;
                ol = 0;
            } else if in_nav && name.eq_ignore_ascii_case("ol") {
                ol = ol.saturating_sub(1);
            } else if in_nav && name.eq_ignore_ascii_case("a") {
                if let Some(h) = href.take() {
                    let (path, anchor) = split_href(&h);
                    let t = decode_entities(title.trim());
                    if !path.is_empty() && !t.is_empty() {
                        out.push(TocEntry {
                            level: ol.max(1).min(3),
                            title: t,
                            path,
                            anchor,
                        });
                    }
                }
            }
        }
        Tok::Text(t) => {
            if href.is_some() {
                title.push_str(t);
            }
        }
    });
    out
}

/// 加密检查：只有"字体混淆"（EPUB 正常特性）放行，其余一律拒绝。
fn check_encryption(zip: &Zip) -> Result<(), String> {
    let Some(xml) = zip.read("META-INF/encryption.xml")? else {
        return Ok(());
    };
    let text = decode_text(&xml);
    let mut any = false;
    let mut font_only = true;
    scan(&text, |tok| {
        if let Tok::Open { name, attrs } = tok {
            if name.eq_ignore_ascii_case("EncryptionMethod") {
                any = true;
                let alg = attr(attrs, "Algorithm").unwrap_or_default();
                // IDPF 字体混淆 / Adobe 字体混淆：只影响字体文件，正文还能读
                let font = alg.contains("EmbeddedFontObfuscation") || alg.contains("enc#RC");
                if !font {
                    font_only = false;
                }
            }
        }
    });
    if any && font_only {
        Ok(())
    } else {
        Err("encrypted ebook is not supported".into())
    }
}

struct OpfItem {
    path: String,
    properties: String,
}

struct Opf {
    items: HashMap<String, OpfItem>,
    spine: Vec<String>,
    toc_id: Option<String>,
}

fn parse_opf(xml: &str, opf_path: &str) -> Opf {
    let base = dirname(opf_path);
    let mut items: HashMap<String, OpfItem> = HashMap::new();
    let mut spine: Vec<String> = Vec::new();
    let mut toc_id = None;
    scan(xml, |tok| match tok {
        Tok::Open { name, attrs } => {
            if name.eq_ignore_ascii_case("item") {
                let Some(id) = attr(attrs, "id") else { return };
                let Some(href) = attr(attrs, "href") else {
                    return;
                };
                let (rel, _) = split_href(&href);
                let path = join_path(base, &rel);
                let properties = attr(attrs, "properties").unwrap_or_default();
                items.insert(id, OpfItem { path, properties });
            } else if name.eq_ignore_ascii_case("itemref") {
                if let Some(idref) = attr(attrs, "idref") {
                    spine.push(idref);
                }
            } else if name.eq_ignore_ascii_case("spine") {
                if let Some(toc) = attr(attrs, "toc") {
                    toc_id = Some(toc);
                }
            }
        }
        _ => {}
    });
    Opf {
        items,
        spine,
        toc_id,
    }
}

fn parse_file(path: &str) -> Result<Shared, String> {
    let meta = std::fs::metadata(path).map_err(|_| "cannot read file".to_string())?;
    if meta.len() > MAX_FILE {
        return Err("ebook is too large".into());
    }
    let data = std::fs::read(path).map_err(|_| "cannot read file".to_string())?;
    let zip = Zip::parse(data)?;
    check_encryption(&zip)?;

    let container = zip
        .read("META-INF/container.xml")?
        .ok_or_else(|| "missing META-INF/container.xml".to_string())?;
    let container = decode_text(&container);
    let mut opf_path = None;
    scan(&container, |tok| {
        if let Tok::Open { name, attrs } = tok {
            if name.eq_ignore_ascii_case("rootfile") && opf_path.is_none() {
                opf_path = attr(attrs, "full-path").filter(|p| !p.is_empty());
            }
        }
    });
    let opf_path = opf_path.ok_or_else(|| "no rootfile in container.xml".to_string())?;
    let opf_bytes = zip
        .read(&opf_path)?
        .ok_or_else(|| "missing OPF package".to_string())?;
    let opf = parse_opf(&decode_text(&opf_bytes), &opf_path);

    // spine → 章节文本。读不出来的章节跳过（目录页号跟着对齐到真正读到的章节）
    let mut chapters = Vec::new();
    let mut path_index = HashMap::new();
    let mut total = 0usize;
    // 图片解码缓存：限 8 张，同一张图被多处引用时只解一次（见 `reflow::ImgCache`）。
    // `budget` 是整本书的图片总字节额度，用完后面的图不再解码（`MAX_IMG_TOTAL`）。
    let mut imgs = reflow::ImgCache::new(IMG_CACHE);
    let mut img_budget = reflow::MAX_IMG_TOTAL;
    for idref in &opf.spine {
        let Some(item) = opf.items.get(idref) else {
            continue;
        };
        if !is_html_path(&item.path) {
            continue;
        }
        let Ok(Some(bytes)) = zip.read(&item.path) else {
            continue;
        };
        let html = decode_text(&bytes);
        let mut blocks = blocks_from_html(&html);
        // `<img src>` 是相对**当前 xhtml** 的，`../` 也要能解（图常放在
        // OEBPS/images/ 而正文在 OEBPS/text/）
        resolve_images(
            &mut blocks,
            &zip,
            dirname(&item.path),
            &mut imgs,
            &mut img_budget,
        );
        total += blocks.iter().map(|b| b.text.chars().count()).sum::<usize>();
        if path_index.contains_key(&item.path) {
            continue;
        }
        path_index.insert(item.path.clone(), chapters.len());
        chapters.push(Chapter { blocks });
        if total > MAX_CHARS {
            break;
        }
    }

    // spine 里有条目却一条正文都读不出来（文件缺失/解压失败），当坏书处理：
    // 静默给一本 0 页的书，UI 上就是"点开一片空白"，更难查。
    if !opf.spine.is_empty() && chapters.is_empty() {
        return Err("no readable chapters in spine".into());
    }

    // 目录：优先 NCX（EPUB2），其次 nav.xhtml（EPUB3）
    let toc_path = opf
        .toc_id
        .as_ref()
        .and_then(|id| opf.items.get(id))
        .map(|i| i.path.clone())
        .or_else(|| {
            opf.items
                .values()
                .find(|i| i.properties.split_whitespace().any(|p| p == "nav"))
                .map(|i| i.path.clone())
        });
    let mut toc = Vec::new();
    if let Some(p) = &toc_path {
        if let Ok(Some(bytes)) = zip.read(p) {
            let xml = decode_text(&bytes);
            toc = if p.to_ascii_lowercase().ends_with(".ncx") {
                toc_from_ncx(&xml)
            } else {
                toc_from_nav(&xml)
            };
        }
    }
    // 目录里的 href 按规范是相对 **目录文件自己** 的（ncx 不一定和 opf 同目录）；
    // 解析不到时退回 OPF 目录——现实里的书两种都有。指向 spine 之外的
    // （封面/版权页）就丢掉，免得目录点进去是空页。
    let opf_dir = dirname(&opf_path);
    let toc_dir = toc_path.as_deref().map(dirname).unwrap_or("");
    let mut resolved = Vec::with_capacity(toc.len());
    for mut e in toc {
        let from_toc = join_path(toc_dir, &e.path);
        let from_opf = join_path(opf_dir, &e.path);
        e.path = if path_index.contains_key(&from_toc) {
            from_toc
        } else {
            from_opf
        };
        if path_index.contains_key(&e.path) {
            resolved.push(e);
        }
    }
    let toc = resolved;

    Ok(Shared {
        doc: Book {
            chapters,
            toc,
            path_index,
        },
        pag: RefCell::new(None),
        last: RefCell::new(None),
    })
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- 造 EPUB ----------------

    fn container_xml(opf: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="{opf}" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#
        )
    }

    fn xhtml(body: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<html xmlns="http://www.w3.org/1999/xhtml">
<head><title>t</title><style>p {{ margin: 0; color: red }}</style></head>
<body>{body}</body>
</html>"#
        )
    }

    fn make_zip(entries: &[(String, Vec<u8>, bool)]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        let mut central: Vec<u8> = Vec::new();
        for (name, data, deflate) in entries {
            let off = out.len() as u32;
            let payload = if *deflate {
                miniz_oxide::deflate::compress_to_vec(data, 6)
            } else {
                data.clone()
            };
            let method: u16 = if *deflate { 8 } else { 0 };
            out.extend_from_slice(&SIG_LOC);
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            // crc 不参与解析，写 0 就行
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&payload);

            central.extend_from_slice(&SIG_CEN);
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&20u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&(payload.len() as u32).to_le_bytes());
            central.extend_from_slice(&(data.len() as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u16.to_le_bytes());
            central.extend_from_slice(&0u32.to_le_bytes());
            central.extend_from_slice(&off.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_off = out.len() as u32;
        let cd_size = central.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&SIG_EOCD);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    enum Toc {
        None,
        Ncx(String),
        Nav(String),
    }

    /// 造一本最小 EPUB。`chapters` = (文件名, body html)，都放在 OEBPS/ 下。
    fn epub(chapters: &[(&str, String)], toc: Toc, extra: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut entries: Vec<(String, Vec<u8>, bool)> = Vec::new();
        entries.push((
            "mimetype".to_string(),
            b"application/epub+zip".to_vec(),
            false,
        ));
        entries.push((
            "META-INF/container.xml".to_string(),
            container_xml("OEBPS/content.opf").into_bytes(),
            true,
        ));
        let mut manifest = String::new();
        let mut itemrefs = String::new();
        for (i, (name, body)) in chapters.iter().enumerate() {
            manifest.push_str(&format!(
                r#"<item id="c{i}" href="{name}" media-type="application/xhtml+xml"/>"#
            ));
            itemrefs.push_str(&format!(r#"<itemref idref="c{i}"/>"#));
            entries.push((format!("OEBPS/{name}"), xhtml(body).into_bytes(), true));
        }
        let mut spine_toc = String::new();
        match &toc {
            Toc::None => {}
            Toc::Ncx(src) => {
                manifest.push_str(
                    r#"<item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>"#,
                );
                entries.push(("OEBPS/toc.ncx".to_string(), src.clone().into_bytes(), true));
                spine_toc = r#" toc="ncx""#.to_string();
            }
            Toc::Nav(src) => {
                manifest.push_str(
                    r#"<item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>"#,
                );
                entries.push((
                    "OEBPS/nav.xhtml".to_string(),
                    src.clone().into_bytes(),
                    true,
                ));
            }
        }
        let opf = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>测试书</dc:title></metadata>
  <manifest>{manifest}</manifest>
  <spine{spine_toc}>{itemrefs}</spine>
</package>"#
        );
        entries.push(("OEBPS/content.opf".to_string(), opf.into_bytes(), true));
        for (name, data) in extra {
            entries.push((name.to_string(), data.clone(), true));
        }
        make_zip(&entries)
    }

    fn write_tmp(tag: &str, bytes: &[u8]) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!("rmdev-epub-{}-{tag}.epub", std::process::id()));
        std::fs::write(&p, bytes).expect("写临时文件");
        p.to_string_lossy().into_owned()
    }

    fn page_text(epub: &Epub, idx: usize) -> String {
        let lay = epub.shared.pag.borrow();
        lay.as_ref().unwrap().pages[idx]
            .lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join("")
    }

    fn has_ink(gray: &[u8]) -> bool {
        gray.iter().any(|&p| p < 128)
    }

    fn para(n: usize) -> String {
        let s = "这是一段用来验证中文按字断行和分页的正文内容。".repeat(2);
        format!("<p>{s}{n}</p>")
    }

    // ---------------- 1. 最小 EPUB ----------------

    #[test]
    fn minimal_epub_renders_and_is_deterministic() {
        let body = xhtml(
            "<h1 id=\"top\">第一章</h1>\
             <p>第一段中文：&amp; 实体、&lt;标签&gt; 和 &#20320;&#22909; 都要解码。</p>\
             <p>第二段中文，用来验证段落边界与断行。</p>\
             <p>第三段，结束。</p>",
        );
        let bytes = epub(&[("ch0.xhtml", body)], Toc::None, &[]);
        let path = write_tmp("minimal", &bytes);
        let e = Epub::open(&path).expect("open 最小 epub");
        assert!(e.page_count() >= 1, "至少一页");
        assert!(e.toc().is_empty(), "没有 ncx/nav 时目录为空");

        let g1 = e.render(0, 600, 840).expect("render");
        assert_eq!(g1.len(), 600 * 840);
        assert!(has_ink(&g1), "页面上应该有黑像素（文字）");
        let g2 = e.render(0, 600, 840).expect("render 两次");
        assert_eq!(g1, g2, "同样参数两次 render 必须逐字节相同");

        // 排版单位到像素的缩放不能把字挤出页面：左右留白之外应有墨
        let text = page_text(&e, 0);
        assert!(text.contains("第一章"), "标题应进正文：{text}");
        assert!(text.contains("你好"), "数字实体应解码：{text}");
        let _ = std::fs::remove_file(&path);
    }

    // ---------------- 2. spine 顺序 ----------------

    #[test]
    fn spine_order_and_chapter_pages() {
        let ch0 = xhtml(&format!(
            "<h1>第一章</h1><p>第一章的第一句话应当出现在第一页。</p>{}",
            para(0)
        ));
        let ch1 = xhtml("<h1>第二章</h1><p>第二章的第一句话应当出现在第二页。</p>");
        let ch2 = xhtml("<h1>第三章</h1><p>第三章的第一句话应当出现在第三页。</p>");
        let bytes = epub(
            &[("a.xhtml", ch0), ("b.xhtml", ch1), ("c.xhtml", ch2)],
            Toc::None,
            &[],
        );
        let path = write_tmp("spine", &bytes);
        let e = Epub::open(&path).expect("open");
        assert_eq!(e.page_count(), 3, "短章节一页一章");
        {
            let lay = e.shared.pag.borrow();
            let lay = lay.as_ref().unwrap();
            assert_eq!(lay.chapter_page, vec![0, 1, 2], "spine 顺序 = 物理顺序");
        }
        let p0 = page_text(&e, 0);
        assert!(p0.contains("第一章的第一句话"), "第 1 页是第 1 章：{p0}");
        assert!(!p0.contains("第二章"), "第 1 页不该有第 2 章：{p0}");
        let p1 = page_text(&e, 1);
        assert!(p1.contains("第二章的第一句话"), "第 2 页是第 2 章：{p1}");
        let p2 = page_text(&e, 2);
        assert!(p2.contains("第三章的第一句话"), "第 3 页是第 3 章：{p2}");
        // 越界页号是 Err 而不是 panic
        assert!(e.render(3, 300, 420).is_err());
        let _ = std::fs::remove_file(&path);
    }

    // ---------------- 3. 字号变大 → 页数变多 ----------------

    #[test]
    fn larger_font_makes_more_pages() {
        let mut body = String::from("<h2>长章节</h2>");
        for i in 0..40 {
            body.push_str(&para(i));
        }
        let bytes = epub(&[("long.xhtml", xhtml(&body))], Toc::None, &[]);
        let path = write_tmp("fontsize", &bytes);
        let mut e = Epub::open(&path).expect("open");
        let small = e.page_count();
        assert!(small >= 2, "40 段应当不止一页（实际 {small}）");
        e.set_font_size(36.0);
        let big = e.page_count();
        assert!(big > small, "字号变大页数应变多：{small} -> {big}");
        // 行距变大同样应更多页
        e.set_font_size(DEFAULT_FONT_SIZE);
        let base = e.page_count();
        e.set_line_spacing(1.85);
        assert!(e.page_count() > base, "行距变大页数应变多");
        // 页边距变大也应更多页
        e.set_line_spacing(DEFAULT_LINE_SPACING);
        let base = e.page_count();
        e.set_margin(MARGINS[2]);
        assert!(e.page_count() > base, "页边距变大页数应变多");
        let _ = std::fs::remove_file(&path);
    }

    // ---------------- 4. 目录 ----------------

    #[test]
    fn toc_from_ncx_and_nav() {
        let chapters = vec![
            ("ch0.xhtml", xhtml("<h1>第一章</h1><p>正文一</p>")),
            ("ch1.xhtml", xhtml("<h1>第二章</h1><p>正文二</p>")),
        ];
        let ncx = String::from(
            r#"<?xml version="1.0" encoding="utf-8"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <head><meta name="dtb:depth" content="2"/></head>
  <docTitle><text>测试书</text></docTitle>
  <navMap>
    <navPoint id="n1"><navLabel><text>第一章</text></navLabel><content src="ch0.xhtml"/></navPoint>
    <navPoint id="n2"><navLabel><text>第二章</text></navLabel><content src="ch1.xhtml"/>
      <navPoint id="n21"><navLabel><text>第二章第一节</text></navLabel><content src="ch1.xhtml#sec"/></navPoint>
    </navPoint>
  </navMap>
</ncx>"#,
        );
        let path = write_tmp("ncx", &epub(&chapters, Toc::Ncx(ncx), &[]));
        let e = Epub::open(&path).expect("open ncx");
        let toc = e.toc();
        assert_eq!(toc.len(), 3, "三个 navPoint");
        assert_eq!(toc[0], (1, "第一章".to_string(), 0));
        assert_eq!(toc[1], (1, "第二章".to_string(), 1));
        assert_eq!(toc[2].0, 2, "嵌套 navPoint 是第 2 层");
        assert_eq!(toc[2].1, "第二章第一节");
        assert_eq!(toc[2].2, 1, "锚点找不到时退回章首页");
        let _ = std::fs::remove_file(&path);

        // EPUB3 nav.xhtml
        let nav = String::from(
            r#"<html xmlns="http://www.w3.org/1999/xhtml"><body>
<nav epub:type="toc"><ol>
  <li><a href="ch0.xhtml">First</a></li>
  <li><a href="ch1.xhtml">Second</a>
    <ol><li><a href="ch1.xhtml#s1">Second part</a></li></ol>
  </li>
</ol></nav></body></html>"#,
        );
        let path = write_tmp("nav", &epub(&chapters, Toc::Nav(nav), &[]));
        let e = Epub::open(&path).expect("open nav");
        let toc = e.toc();
        assert_eq!(toc.len(), 3);
        assert_eq!((toc[0].0, toc[0].1.as_str(), toc[0].2), (1, "First", 0));
        assert_eq!((toc[1].0, toc[1].1.as_str(), toc[1].2), (1, "Second", 1));
        assert_eq!(toc[2].0, 2);
        assert_eq!(toc[2].1, "Second part");
        let _ = std::fs::remove_file(&path);
    }

    // ---------------- 4b. 嵌套路径 / 百分号编码 ----------------

    #[test]
    fn nested_paths_and_percent_encoded_href() {
        let container = container_xml("OPS/pkg/content.opf");
        let opf = String::from(
            r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>嵌套</dc:title></metadata>
  <manifest>
    <item id="c0" href="../text/ch%201.xhtml" media-type="application/xhtml+xml"/>
    <item id="ncx" href="nav.ncx" media-type="application/x-dtbncx+xml"/>
  </manifest>
  <spine toc="ncx"><itemref idref="c0"/></spine>
</package>"#,
        );
        // ncx 自己的 href 也是相对 ncx 所在目录（OPS/pkg）写的
        let ncx = String::from(
            r#"<?xml version="1.0"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/"><navMap>
  <navPoint id="n1"><navLabel><text>第一章 嵌套</text></navLabel>
    <content src="../text/ch%201.xhtml#top"/></navPoint>
</navMap></ncx>"#,
        );
        let bytes = make_zip(&[
            (
                "mimetype".to_string(),
                b"application/epub+zip".to_vec(),
                false,
            ),
            (
                "META-INF/container.xml".to_string(),
                container.into_bytes(),
                true,
            ),
            ("OPS/pkg/content.opf".to_string(), opf.into_bytes(), true),
            ("OPS/pkg/nav.ncx".to_string(), ncx.into_bytes(), true),
            (
                "OPS/text/ch 1.xhtml".to_string(),
                xhtml("<h1 id=\"top\">第一章</h1><p>嵌套目录里的正文。</p>").into_bytes(),
                true,
            ),
        ]);
        let path = write_tmp("nested", &bytes);
        let e = Epub::open(&path).expect("open 嵌套结构");
        assert_eq!(e.page_count(), 1);
        assert_eq!(e.toc(), vec![(1, "第一章 嵌套".to_string(), 0)]);
        assert!(has_ink(&e.render(0, 400, 560).expect("render")));
        let _ = std::fs::remove_file(&path);
    }

    // ---------------- 4c. 目录锚点落在标题页 ----------------

    #[test]
    fn toc_anchor_points_to_heading_page() {
        let mut body = String::from("<h1 id=\"top\">第一章</h1>");
        for i in 0..30 {
            body.push_str(&para(i));
        }
        body.push_str("<h2 id=\"mid\">中点小节</h2>");
        for i in 0..30 {
            body.push_str(&para(i));
        }
        let ncx = String::from(
            r#"<?xml version="1.0"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/"><navMap>
  <navPoint><navLabel><text>第一章</text></navLabel><content src="ch0.xhtml"/></navPoint>
  <navPoint><navLabel><text>中点小节</text></navLabel><content src="ch0.xhtml#mid"/></navPoint>
</navMap></ncx>"#,
        );
        let bytes = epub(&[("ch0.xhtml", xhtml(&body))], Toc::Ncx(ncx), &[]);
        let path = write_tmp("anchor", &bytes);
        let e = Epub::open(&path).expect("open");
        assert!(e.page_count() >= 3);
        let toc = e.toc();
        assert_eq!(toc.len(), 2);
        assert_eq!(toc[0].2, 0, "章首在第一页");
        let mid_page = toc[1].2;
        assert!(mid_page > 0, "锚点应落到标题真正所在页，实际 {mid_page}");
        assert!(
            page_text(&e, mid_page).contains("中点小节"),
            "锚点页应当包含该标题"
        );
        assert!(
            !page_text(&e, mid_page - 1).contains("中点小节"),
            "上一页还没到该标题"
        );
        let _ = std::fs::remove_file(&path);
    }

    // ---------------- 4d. 图片 ----------------

    #[test]
    fn img_and_svg_image_make_image_blocks() {
        // 纯解析层：`<img src>` 和 SVG 里的 `<image xlink:href>` 都要出图片块，
        // `text` 先保留原始 href（由 `resolve_images` 再换成 zip 路径 + 像素）。
        let html = xhtml(
            r#"<p>图前</p><img src="pics/a.png" alt="A"/>
               <svg xmlns="http://www.w3.org/2000/svg" xmlns:xlink="http://www.w3.org/1999/xlink">
                 <image xlink:href="../pics/b.png" width="10" height="10"/>
               </svg><p>图后</p>"#,
        );
        let blocks = blocks_from_html(&html);
        let kinds: Vec<Kind> = blocks.iter().map(|b| b.kind).collect();
        assert_eq!(
            kinds,
            vec![Kind::Body, Kind::Image, Kind::Image, Kind::Body],
            "块序列 = {:?}",
            blocks.iter().map(|b| (&b.text, b.kind)).collect::<Vec<_>>()
        );
        assert_eq!(blocks[1].text, "pics/a.png");
        assert_eq!(blocks[2].text, "../pics/b.png", "封面图常包在 svg 里");
        // 没有 src 的 img 不能凭 alt 造出图片块
        let no_src = blocks_from_html(&xhtml(r#"<img alt="x"/>"#));
        assert!(no_src.iter().all(|b| b.kind != Kind::Image));
    }

    #[test]
    fn epub_resolves_parent_relative_image_and_decodes() {
        // 章节在 OPS/text/，图在 OPS/images/，href 写 ../images/p.png
        let container = container_xml("OPS/content.opf");
        let opf = String::from(
            r#"<?xml version="1.0" encoding="utf-8"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0" unique-identifier="id">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:title>图</dc:title></metadata>
  <manifest>
    <item id="c0" href="text/ch0.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="c0"/></spine>
</package>"#,
        );
        let bytes = make_zip(&[
            (
                "mimetype".to_string(),
                b"application/epub+zip".to_vec(),
                false,
            ),
            (
                "META-INF/container.xml".to_string(),
                container.into_bytes(),
                true,
            ),
            ("OPS/content.opf".to_string(), opf.into_bytes(), true),
            (
                "OPS/text/ch0.xhtml".to_string(),
                xhtml(r#"<p>带图的一章。</p><img src="../images/p.png"/>"#).into_bytes(),
                true,
            ),
            (
                "OPS/images/p.png".to_string(),
                reflow::test_png_1x1(),
                true,
            ),
        ]);
        let path = write_tmp("img-rel", &bytes);
        let e = Epub::open(&path).expect("open");
        let blocks = &e.shared.doc.chapters[0].blocks;
        let im = blocks
            .iter()
            .find(|b| b.kind == Kind::Image)
            .expect("应该有图片块");
        assert_eq!(im.text, "OPS/images/p.png", "`../` 要相对当前 xhtml 解析");
        let img = im.img.as_ref().expect("图片要解码好");
        assert_eq!((img.w, img.h), (1, 1));
        // 1×1 会被等比放大到内容宽：页面上应该有一大片灰，不是只有几行字
        let g = e.render(0, 400, 560).expect("render");
        assert!(
            g.iter().filter(|&&v| v < 255).count() > 5000,
            "图片没画出来"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_or_gif_image_is_skipped_without_panic() {
        // 一张不存在的 png + 一张 GIF（明确不支持）：都整块丢掉，正文照排
        let gif = b"GIF89a\x01\x00\x01\x00\x80\x00\x00\x00\x00\x00\xff\xff\xff!\xf9\x04\x01\x00\
\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;"
            .to_vec();
        let bytes = epub(
            &[(
                "ch0.xhtml",
                xhtml(
                    r#"<p>有图的段落。</p><img src="nope.png"/><img src="a.gif"/>
                       <p>段落结束。</p>"#,
                ),
            )],
            Toc::None,
            &[("OEBPS/a.gif", gif)],
        );
        let path = write_tmp("img-missing", &bytes);
        let e = Epub::open(&path).expect("坏图不能拖垮整本书");
        let blocks = &e.shared.doc.chapters[0].blocks;
        assert!(
            blocks.iter().all(|b| b.kind != Kind::Image),
            "解不出的图应当整块丢掉"
        );
        assert!(e.page_count() >= 1);
        let t = page_text(&e, 0);
        assert!(t.contains("有图的段落"));
        assert!(!t.contains("nope.png"), "不许把图片路径印进正文");
        assert!(has_ink(&e.render(0, 400, 560).expect("render")));
        let _ = std::fs::remove_file(&path);
    }

    // ---------------- 5. 坏文件 ----------------

    #[test]
    fn broken_files_are_errors_not_panics() {
        // 随机字节
        let path = write_tmp("random", &vec![0x5au8; 4096]);
        assert!(Epub::open(&path).is_err(), "随机字节应当打不开");
        let _ = std::fs::remove_file(&path);

        // 缺 container.xml
        let zip = make_zip(&[(
            "mimetype".to_string(),
            b"application/epub+zip".to_vec(),
            false,
        )]);
        let path = write_tmp("nocontainer", &zip);
        assert!(Epub::open(&path).is_err(), "缺 container.xml 应当打不开");
        let _ = std::fs::remove_file(&path);

        // 加密标记（AES 加密正文）
        let enc = String::from(
            r#"<?xml version="1.0"?>
<encryption xmlns="urn:oasis:names:tc:opendocument:xmlns:container"
            xmlns:enc="http://www.w3.org/2001/04/xmlenc#">
  <enc:EncryptedData>
    <enc:EncryptionMethod Algorithm="http://www.w3.org/2001/04/xmlenc#aes256-cbc"/>
  </enc:EncryptedData>
</encryption>"#,
        );
        let bytes = epub(
            &[("ch0.xhtml", xhtml("<p>看不见的字</p>"))],
            Toc::None,
            &[("META-INF/encryption.xml", enc.into_bytes())],
        );
        let path = write_tmp("encrypted", &bytes);
        assert!(Epub::open(&path).is_err(), "加密 epub 应当打不开");
        let _ = std::fs::remove_file(&path);

        // 只有字体混淆的 encryption.xml 要能读（很多正常书都有）
        let font_enc = String::from(
            r#"<?xml version="1.0"?>
<encryption xmlns:enc="http://www.w3.org/2001/04/xmlenc#">
  <enc:EncryptedData>
    <enc:EncryptionMethod Algorithm="http://www.idpf.org/2008/embedding#EmbeddedFontObfuscation"/>
  </enc:EncryptedData>
</encryption>"#,
        );
        let bytes = epub(
            &[("ch0.xhtml", xhtml("<p>字体被混淆但正文可读</p>"))],
            Toc::None,
            &[("META-INF/encryption.xml", font_enc.into_bytes())],
        );
        let path = write_tmp("fontobf", &bytes);
        let e = Epub::open(&path).expect("字体混淆不该拦");
        assert!(e.page_count() >= 1);
        let _ = std::fs::remove_file(&path);

        // spine 指向的文件根本不存在（zip 里没有）→ 没有可读章节，应当 Err
        let mut entries: Vec<(String, Vec<u8>, bool)> = Vec::new();
        entries.push((
            "mimetype".to_string(),
            b"application/epub+zip".to_vec(),
            false,
        ));
        entries.push((
            "META-INF/container.xml".to_string(),
            container_xml("OEBPS/content.opf").into_bytes(),
            true,
        ));
        entries.push((
            "OEBPS/content.opf".to_string(),
            String::from(
                r#"<package xmlns="http://www.idpf.org/2007/opf"><manifest>
<item id="c0" href="missing.xhtml" media-type="application/xhtml+xml"/>
</manifest><spine><itemref idref="c0"/></spine></package>"#,
            )
            .into_bytes(),
            true,
        ));
        let path = write_tmp("nospinefiles", &make_zip(&entries));
        assert!(Epub::open(&path).is_err(), "章节文件都不存在应当打不开");
        let _ = std::fs::remove_file(&path);

        // 不存在的路径
        assert!(Epub::open("/nonexistent/rmdev-epub-test.epub").is_err());
    }

    // ---------------- 6. 空 spine ----------------

    #[test]
    fn empty_spine_has_no_pages() {
        let path = write_tmp("empty", &epub(&[], Toc::None, &[]));
        let e = Epub::open(&path).expect("空 spine 仍然算打开");
        assert_eq!(e.page_count(), 0);
        assert!(e.render(0, 200, 280).is_err(), "没有页可渲染，但必须是 Err");
        assert!(e.toc().is_empty());
        assert!((e.aspect(0) - PAGE_W / PAGE_H).abs() < 1e-6);
        let _ = std::fs::remove_file(&path);
    }

    // ---------------- 7. 性能 ----------------

    #[test]
    fn one_megabyte_epub_is_fast_enough() {
        // 造一本 1 MB 出头的书：40 章 × 190 段 × 80 字，中文随机取字让压缩率接近真实
        let pool: Vec<char> = "的一是在不了有和人这中大为上个国我以要他时来用们生\
                               到作地于出就分对成会可主发年动同工也能下过子说产种面而方\
                               后多定行学法所民得经十三之进着等部度家电力里如水化高自二\
                               理起小物现实加量都两体制机当使点从业本去把性好应开它合还\
                               因由其些然前外天政四日那社义事平形相全表间样与关各重新线\
                               内数正心反你明看原又么利比或但质气第向道命此变条只没结解\
                               问意建月公无系军很情者最立代想已通并提直题党程展五果料象\
                               员革位入常文总次品式活设及管特件长求老头基资边流路级少图"
            .chars()
            .collect();
        let mut seed: u64 = 0x1234_5678_9abc_def0;
        let n_pool = pool.len();
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed as usize) % n_pool
        };
        let mut chapters: Vec<(String, String)> = Vec::new();
        for c in 0..40 {
            let mut body = format!("<h1>第 {c} 章</h1>");
            for _ in 0..190 {
                body.push_str("<p>");
                for _ in 0..80 {
                    body.push(pool[next()]);
                }
                body.push_str("。</p>");
            }
            chapters.push((format!("ch{c}.xhtml"), xhtml(&body)));
        }
        let refs: Vec<(&str, String)> = chapters
            .iter()
            .map(|(n, b)| (n.as_str(), b.clone()))
            .collect();
        let bytes = epub(&refs, Toc::None, &[]);
        assert!(
            bytes.len() > 1_000_000,
            "测试样本要在 1 MB 量级，实际 {}",
            bytes.len()
        );
        let path = write_tmp("big", &bytes);
        let t0 = std::time::Instant::now();
        let e = Epub::open(&path).expect("open 大书");
        let d_open = t0.elapsed();
        let pages = e.page_count();
        assert!(pages > 100, "40 章应当有上百页（{pages}）");
        let t1 = std::time::Instant::now();
        let _ = e.render(pages / 2, 1000, 1400).expect("render");
        let d_render = t1.elapsed();
        println!(
            "[epub] {} 字节：open={}ms（{} 页）render={}ms",
            bytes.len(),
            d_open.as_millis(),
            pages,
            d_render.as_millis()
        );
        // spec：open ≤ 300ms（本机 debug 放宽到 2s），render ≤ 50ms（debug 放宽到 300ms）
        assert!(d_open.as_millis() < 2000, "open 太慢：{d_open:?}");
        assert!(d_render.as_millis() < 300, "render 太慢：{d_render:?}");
        let _ = std::fs::remove_file(&path);
    }
}
