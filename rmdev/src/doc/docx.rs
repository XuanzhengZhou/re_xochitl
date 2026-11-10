//! DOCX（Word）后端：zip 里抽 `word/document.xml`，按 XML 标签流拍成重排层的块。
//!
//! 只读两条路径：正文 `word/document.xml`、书名 `docProps/core.xml`。
//! 不建 DOM、不解析样式表——顺序扫标签，`<w:p>` 收一段文字，`<w:tbl>` 里的
//! `<w:tr>` 拍成一段（单元格用 `" | "` 连）。列表编号（`word/numbering.xml`）
//! 不做，列表项就是普通段落：编号丢了，正文不丢。
//!
//! zip 也是自己读的（`epub.rs` 里那套是私有的）：EOCD → 中央目录 → 本地头 →
//! deflate 流（`miniz_oxide`）。只支持 stored(0) / deflate(8)，zip64 直接报错。
//!
//! 解析完只是一本 [`Book`]，分页/渲染/目录/搜索全交给 [`ReflowDoc`]。

use std::collections::HashMap;

use super::reflow::{Block, Book, Cfg, Chapter, ReflowDoc, TocEntry};

/// 单个 zip 条目解压上限：防压缩炸弹把设备内存吃光。
const MAX_ENTRY: usize = 64 << 20;
/// 正文总字符上限，对齐 `epub.rs::MAX_CHARS`：再长的书也只读到这儿。
const MAX_CHARS: usize = 8 << 20;

// ---------------------------------------------------------------------------
// 对外接口
// ---------------------------------------------------------------------------

/// 一本 Word 文档。真正的分页/渲染在 [`ReflowDoc`] 里，这里只多存一个书名。
pub struct DocxDoc {
    inner: ReflowDoc,
    /// `docProps/core.xml` 的 `<dc:title>`，没有就是 `None`（打开照常成功）。
    title: Option<String>,
}

impl DocxDoc {
    pub fn open(path: &str) -> Result<DocxDoc, String> {
        let data = std::fs::read(path).map_err(|e| format!("cannot read {path}: {e}"))?;
        let zip = Zip::parse(data)?;
        let xml = zip
            .read("word/document.xml")?
            .ok_or_else(|| format!("{path}: word/document.xml not found"))?;
        let book = Parser::parse(&decode_utf8(&xml));
        // 书名取不到不算错：很多工具导出的 docx 没有 core.xml
        let title = zip
            .read("docProps/core.xml")
            .ok()
            .flatten()
            .and_then(|b| parse_title(&decode_utf8(&b)));
        Ok(DocxDoc {
            inner: ReflowDoc::new(book, Cfg::default()),
            title,
        })
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn page_count(&self) -> usize {
        self.inner.page_count()
    }

    pub fn aspect(&self, idx: usize) -> f32 {
        self.inner.aspect(idx)
    }

    pub fn render(&self, idx: usize, w: usize, h: usize) -> Result<Vec<u8>, String> {
        self.inner.render(idx, w, h)
    }

    pub fn outline(&self) -> Vec<crate::doc::Toc> {
        self.inner.outline()
    }

    pub fn find(&self, needle: &str, limit: usize) -> Vec<(usize, String)> {
        self.inner.find(needle, limit)
    }

    pub fn chapter_title(&self, idx: usize) -> Option<String> {
        self.inner.chapter_title(idx)
    }

    pub fn apply_layout(&mut self, cfg: crate::doc::LayoutCfg) -> bool {
        self.inner.apply_layout(cfg)
    }

    pub fn current_layout(&self) -> crate::doc::LayoutCfg {
        self.inner.current_layout()
    }
}

// ---------------------------------------------------------------------------
// ZIP（只读，够用就行）
// ---------------------------------------------------------------------------

const SIG_LOC: [u8; 4] = [0x50, 0x4b, 0x03, 0x04];
const SIG_CEN: [u8; 4] = [0x50, 0x4b, 0x01, 0x02];
const SIG_EOCD: [u8; 4] = [0x50, 0x4b, 0x05, 0x06];

struct Entry {
    name: String,
    method: u16,
    comp_size: usize,
    local_off: usize,
}

/// 整份文件都留在内存里：docx 通常几 MB，读一个条目就够，不做流式。
struct Zip {
    data: Vec<u8>,
    entries: Vec<Entry>,
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
    /// EOCD（文件尾往前找）→ 中央目录。目录里的名字是唯一索引，别的一概不认。
    fn parse(data: Vec<u8>) -> Result<Zip, String> {
        if data.len() < 22 {
            return Err("not a zip archive".into());
        }
        // EOCD 后面最多 65535 字节注释
        let last = data.len() - 22;
        let first = last.saturating_sub(0xffff);
        let eocd = (first..=last)
            .rev()
            .find(|&i| data[i..i + 4] == SIG_EOCD)
            .ok_or_else(|| "not a zip archive".to_string())?;
        let count = u16_at(&data, eocd + 10).unwrap_or(0) as usize;
        let cd_off = u32_at(&data, eocd + 16).unwrap_or(0) as usize;
        if count == 0 || cd_off == 0 || cd_off >= data.len() {
            return Err("zip archive has no entries".into());
        }
        let mut entries = Vec::with_capacity(count.min(4096));
        let mut p = cd_off;
        for _ in 0..count {
            if data.get(p..p + 4) != Some(&SIG_CEN[..]) {
                // 中央目录被截断：能读几条算几条，一条都没有下面会报错
                break;
            }
            let Some(method) = u16_at(&data, p + 10) else {
                break;
            };
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
            // 目录条目（名字以 `/` 结尾）没用
            if !name.is_empty() && !name.ends_with('/') {
                entries.push(Entry {
                    name,
                    method,
                    comp_size,
                    local_off,
                });
            }
        }
        if entries.is_empty() {
            return Err("zip archive has no entries".into());
        }
        Ok(Zip { data, entries })
    }

    /// 按名字取条目。大小写不一致时退一步做不敏感匹配（Windows 上打的包常见）。
    fn read(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        let entry = self
            .entries
            .iter()
            .find(|e| e.name == name)
            .or_else(|| {
                self.entries
                    .iter()
                    .find(|e| e.name.eq_ignore_ascii_case(name))
            });
        let Some(e) = entry else {
            return Ok(None);
        };
        let off = e.local_off;
        if self.data.get(off..off + 4) != Some(&SIG_LOC[..]) {
            return Err("corrupt zip local header".into());
        }
        // 本地头里的名字/扩展区和中央目录的 comp_size 配着用
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
// 极简 XML 标签流
// ---------------------------------------------------------------------------

enum Tok<'a> {
    Open {
        /// 已去掉命名空间前缀（`w:p` → `p`）。
        name: &'a str,
        attrs: &'a str,
        /// 自闭合（`<w:tab/>`），不会有对应的 Close。
        empty: bool,
    },
    Close {
        name: &'a str,
    },
    Text(&'a str),
}

/// 找 `>`，跳过引号里的内容（属性值里合法地出现 `>`）。
fn tag_end(b: &[u8], mut i: usize) -> Option<usize> {
    let mut quote = 0u8;
    while i < b.len() {
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

/// 把 XML 拍成标签 + 文本事件。注释/DOCTYPE/PI 丢掉，CDATA 当文本。
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
            i = rest.find("-->").map_or(s.len(), |k| i + k + 3);
        } else if rest.starts_with("<![CDATA[") {
            let start = i + 9;
            match s[start..].find("]]>") {
                Some(k) => {
                    f(Tok::Text(&s[start..start + k]));
                    i = start + k + 3;
                }
                None => {
                    f(Tok::Text(&s[start..]));
                    i = s.len();
                }
            }
        } else if rest.starts_with("<!") || rest.starts_with("<?") {
            i = tag_end(b, i + 2).map_or(b.len(), |e| e + 1);
        } else {
            let Some(end) = tag_end(b, i + 1) else {
                text_start = b.len();
                break;
            };
            let inner = &s[i + 1..end];
            i = end + 1;
            let close = inner.starts_with('/');
            let mut body = inner.trim_start_matches('/').trim_end();
            let empty = body.ends_with('/');
            if empty {
                body = body[..body.len() - 1].trim_end();
            }
            let nend = body.find(|c: char| c.is_whitespace()).unwrap_or(body.len());
            // 命名空间前缀各家不统一，只认本地名
            let name = body[..nend].rsplit(':').next().unwrap_or("");
            if !name.is_empty() {
                if close {
                    f(Tok::Close { name });
                } else {
                    f(Tok::Open {
                        name,
                        attrs: &body[nend..],
                        empty,
                    });
                }
            }
        }
        text_start = i;
    }
    if text_start < b.len() {
        f(Tok::Text(&s[text_start..]));
    }
}

// ---------------------------------------------------------------------------
// document.xml → Book
// ---------------------------------------------------------------------------

/// 扫文档时攒出来的状态机。字段全是「当前正在处理什么」。
#[derive(Default)]
struct Parser {
    chapters: Vec<Chapter>,
    toc: Vec<TocEntry>,
    path_index: HashMap<String, usize>,
    /// 已经收进书的字符数（用来卡 [`MAX_CHARS`]）。
    total: usize,
    /// 到上限了，后面一律不收。
    full: bool,
    /// 当前段落正攒的文字。
    para: String,
    /// `<w:pStyle w:val="...">` 认出来的级别（1..=3）。
    style_level: Option<u8>,
    /// `<w:outlineLvl w:val="0..2">`，没有 pStyle 时才用。
    outline_level: Option<u8>,
    in_body: bool,
    in_t: bool,
    /// 正在跳过的元素（instrText/delText/drawing/pict/del）。
    skip: Option<String>,
    /// 表格嵌套深度。
    tbl_depth: usize,
    in_row: bool,
    cells: usize,
    /// 当前表格行正攒的文字。
    row: String,
}

impl Parser {
    fn parse(xml: &str) -> Book {
        let mut p = Parser::default();
        scan(xml, |tok| p.feed(tok));
        Book {
            chapters: p.chapters,
            toc: p.toc,
            path_index: p.path_index,
        }
    }

    fn feed(&mut self, tok: Tok<'_>) {
        match tok {
            Tok::Text(t) => {
                if self.in_t && self.skip.is_none() {
                    self.push_text(t);
                }
            }
            Tok::Open {
                name,
                attrs,
                empty,
            } => {
                if self.skip.is_some() {
                    return;
                }
                // 域代码 / 修订删除 / 图片：里面的文字一律不要
                if !empty
                    && matches!(name, "instrText" | "delText" | "drawing" | "pict" | "del")
                {
                    self.skip = Some(name.to_string());
                    return;
                }
                match name {
                    "body" => self.in_body = true,
                    "p" if self.in_body => {
                        self.para.clear();
                        self.style_level = None;
                        self.outline_level = None;
                    }
                    "pStyle" if self.in_body => {
                        self.style_level = attr(attrs, "val").and_then(style_level);
                    }
                    "outlineLvl" if self.in_body => {
                        // 0 基，0..=2 对应 H1..H3，更深的忽略
                        self.outline_level = attr(attrs, "val")
                            .and_then(|v| v.trim().parse::<u8>().ok())
                            .filter(|v| *v <= 2);
                    }
                    "t" if self.in_body && !empty => self.in_t = true,
                    "tab" if self.in_body => self.push_char(' '),
                    "br" if self.in_body => self.push_char('\n'),
                    "tbl" if self.in_body && !empty => {
                        self.tbl_depth += 1;
                        if self.tbl_depth == 1 {
                            self.row.clear();
                            self.in_row = false;
                        }
                    }
                    "tr" if self.in_body && self.tbl_depth == 1 && !empty => {
                        self.in_row = true;
                        self.cells = 0;
                        self.row.clear();
                    }
                    "tc" if self.in_body && self.tbl_depth == 1 && self.in_row && !empty => {
                        if self.cells > 0 {
                            self.row.push_str(" | ");
                        }
                        self.cells += 1;
                    }
                    _ => {}
                }
            }
            Tok::Close { name } => {
                if self.skip.is_some() {
                    if self.skip.as_deref() == Some(name) {
                        self.skip = None;
                    }
                    return;
                }
                match name {
                    "body" => self.in_body = false,
                    "t" => self.in_t = false,
                    "p" if self.in_body => self.flush_para(),
                    "tr" if self.in_body && self.tbl_depth == 1 => self.flush_row(),
                    "tbl" if self.in_body => {
                        self.tbl_depth = self.tbl_depth.saturating_sub(1);
                    }
                    _ => {}
                }
            }
        }
    }

    /// 只在 `<w:t>` 里调：实体解码 + 字符上限。
    fn push_text(&mut self, raw: &str) {
        for ch in decode_entities(raw).chars() {
            self.push_char(ch);
        }
    }

    fn push_char(&mut self, ch: char) {
        if self.full {
            return;
        }
        if self.total >= MAX_CHARS {
            self.full = true;
            return;
        }
        self.total += 1;
        self.para.push(ch);
    }

    // ---- 块/章节 ----

    fn new_chapter(&mut self) -> usize {
        let ci = self.chapters.len();
        self.chapters.push(Chapter::default());
        self.path_index.insert(ci.to_string(), ci);
        ci
    }

    /// 当前章；还没有（正文前没有标题）就先开一章。
    fn cur_chapter(&mut self) -> usize {
        if self.chapters.is_empty() {
            self.new_chapter()
        } else {
            self.chapters.len() - 1
        }
    }

    fn push_body(&mut self, text: String) {
        let ci = self.cur_chapter();
        self.chapters[ci].blocks.push(Block::body(text));
    }

    fn push_heading(&mut self, level: u8, text: String) {
        // 每个 H1 起新章；全文没有 H1 时整篇一章（走 push_body 的 cur_chapter）
        let ci = if level == 1 {
            self.new_chapter()
        } else {
            self.cur_chapter()
        };
        self.chapters[ci]
            .blocks
            .push(Block::heading(level, text.clone()));
        self.toc.push(TocEntry {
            level,
            title: text,
            path: ci.to_string(),
            anchor: String::new(),
        });
    }

    /// 段落收尾：表格里并进行，正文里成一个块。
    fn flush_para(&mut self) {
        let level = self
            .style_level
            .or_else(|| self.outline_level.map(|v| v.saturating_add(1)))
            .unwrap_or(0)
            .min(3);
        self.style_level = None;
        self.outline_level = None;
        let text = squash(&self.para);
        self.para.clear();
        if self.tbl_depth > 0 {
            if self.in_row && !text.is_empty() {
                if !self.row.is_empty() && !self.row.ends_with(' ') {
                    self.row.push(' ');
                }
                self.row.push_str(&text);
            }
            return;
        }
        if text.is_empty() {
            return;
        }
        if level == 0 {
            self.push_body(text);
        } else {
            self.push_heading(level, text);
        }
    }

    /// 表格行收尾：整行一个段落。
    fn flush_row(&mut self) {
        self.in_row = false;
        self.cells = 0;
        let text = squash(&self.row);
        self.row.clear();
        if !text.is_empty() {
            self.push_body(text);
        }
    }
}

// ---------------------------------------------------------------------------
// 文字处理
// ---------------------------------------------------------------------------

/// 段落文字归一：连续空白压成一个空格，`<w:br/>` 的换行保留（重排层靠它断行）。
fn squash(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut space = false;
    for ch in s.chars() {
        if ch == '\n' {
            while out.ends_with(' ') {
                out.pop();
            }
            out.push('\n');
            space = true;
        } else if ch.is_whitespace() {
            if !space {
                out.push(' ');
                space = true;
            }
        } else {
            out.push(ch);
            space = false;
        }
    }
    out.trim().to_string()
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(pos) = rest.find('&') {
        out.push_str(&rest[..pos]);
        rest = &rest[pos..];
        // 实体名最长就到 `&hellip;` 这个量级，10 个字符够
        let end = rest.find(';').filter(|&k| k <= 10);
        match end.and_then(|k| entity_char(&rest[1..k]).map(|c| (c, k))) {
            Some((c, k)) => {
                out.push(c);
                rest = &rest[k + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity_char(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => {
            let code = if let Some(h) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                u32::from_str_radix(h, 16).ok()?
            } else {
                name.strip_prefix('#')?.parse::<u32>().ok()?
            };
            char::from_u32(code).or(Some('\u{fffd}'))
        }
    }
}

/// 取属性值；属性名按本地名比（`w:val` 也能用 `val` 问）。
fn attr<'a>(attrs: &'a str, want: &str) -> Option<&'a str> {
    let mut s = attrs;
    loop {
        let eq = s.find('=')?;
        let key = s[..eq].trim();
        let key = key
            .rsplit(|c: char| c == ':' || c.is_whitespace())
            .next()
            .unwrap_or("");
        let rest = s[eq + 1..].trim_start();
        let (val, tail) = match rest.chars().next()? {
            q @ ('"' | '\'') => {
                let body = &rest[1..];
                let end = body.find(q)?;
                (&body[..end], &body[end + 1..])
            }
            _ => {
                let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
                (&rest[..end], &rest[end..])
            }
        };
        if key.eq_ignore_ascii_case(want) {
            return Some(val);
        }
        s = tail;
    }
}

/// Word 内置标题样式名 → 级别。`Title` 当 H1，Heading4..6 压到 H3。
fn style_level(val: &str) -> Option<u8> {
    let lower = val.trim().to_ascii_lowercase();
    if lower == "title" {
        return Some(1);
    }
    let n: u8 = lower.strip_prefix("heading")?.trim().parse().ok()?;
    match n {
        1 => Some(1),
        2 => Some(2),
        3..=6 => Some(3),
        _ => None,
    }
}

/// XML 声明的是 UTF-8（个别导出工具会带 BOM）；非法字节按 lossy 兜底。
fn decode_utf8(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// `docProps/core.xml` 的 `<dc:title>`。没有 / 空 → `None`。
fn parse_title(xml: &str) -> Option<String> {
    let mut out = None;
    let mut cur = String::new();
    let mut inside = false;
    scan(xml, |tok| match tok {
        Tok::Open {
            name,
            empty: false,
            ..
        } if name.eq_ignore_ascii_case("title") => {
            inside = true;
            cur.clear();
        }
        Tok::Close { name } if name.eq_ignore_ascii_case("title") && inside => {
            inside = false;
            let t = squash(&decode_entities(&cur));
            if !t.is_empty() {
                out = Some(t);
            }
        }
        Tok::Text(t) if inside => cur.push_str(t),
        _ => {}
    });
    out
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- 造 DOCX ----------------

    /// 手写 zip：`deflate` = false 时全部 stored，用来覆盖两条解压路径。
    fn build_zip(entries: &[(&str, Vec<u8>)], deflate: bool) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        let mut central: Vec<u8> = Vec::new();
        for (name, data) in entries {
            let off = out.len() as u32;
            let (method, payload): (u16, Vec<u8>) = if deflate {
                (8, miniz_oxide::deflate::compress_to_vec(data, 6))
            } else {
                (0, data.clone())
            };
            out.extend_from_slice(&SIG_LOC);
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            // crc 不参与解析
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
        let cd_len = central.len() as u32;
        out.extend_from_slice(&central);
        out.extend_from_slice(&SIG_EOCD);
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_len.to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    fn document_xml(body: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body>{body}</w:body></w:document>"#
        )
    }

    fn docx_bytes(body: &str) -> Vec<u8> {
        build_zip(&[("word/document.xml", document_xml(body).into_bytes())], true)
    }

    fn write_tmp(tag: &str, bytes: &[u8]) -> String {
        let mut p = std::env::temp_dir();
        p.push(format!("rmdev-docx-{}-{tag}.docx", std::process::id()));
        std::fs::write(&p, bytes).expect("写临时文件");
        p.to_string_lossy().into_owned()
    }

    fn open_body(tag: &str, body: &str) -> DocxDoc {
        DocxDoc::open(&write_tmp(tag, &docx_bytes(body))).expect("打开 docx")
    }

    fn p(text: &str) -> String {
        format!("<w:p><w:r><w:t>{text}</w:t></w:r></w:p>")
    }

    fn heading(style: &str, text: &str) -> String {
        format!(
            r#"<w:p><w:pPr><w:pStyle w:val="{style}"/></w:pPr><w:r><w:t>{text}</w:t></w:r></w:p>"#
        )
    }

    fn page_text(doc: &DocxDoc, idx: usize) -> String {
        doc.inner.page_text(idx).expect("页不存在")
    }

    // ---------------- 测试 ----------------

    #[test]
    fn minimal_docx_has_pages_and_text() {
        let doc = open_body("minimal", &format!("{}{}", p("Hello"), p("World")));
        assert!(doc.page_count() > 0);
        let text = page_text(&doc, 0);
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("World"), "{text}");
        // 搜索也走通了
        let hits = doc.find("Hello", 10);
        assert_eq!(hits.first().map(|h| h.0), Some(0));
    }

    #[test]
    fn stored_zip_also_works() {
        let bytes = build_zip(
            &[
                ("word/document.xml", document_xml(&p("stored entry")).into_bytes()),
                ("[Content_Types].xml", b"<Types/>".to_vec()),
            ],
            false,
        );
        let doc = DocxDoc::open(&write_tmp("stored", &bytes)).expect("打开 stored docx");
        assert!(doc.page_count() > 0);
        assert!(page_text(&doc, 0).contains("stored entry"));
    }

    #[test]
    fn heading1_makes_toc_entry_with_right_page() {
        let mut body = String::new();
        for i in 0..60 {
            body.push_str(&p(&format!("filler paragraph {i} with a bit of text")));
        }
        body.push_str(&heading("Heading1", "Chapter Two"));
        body.push_str(&p("body of chapter two"));
        let doc = open_body("heading1", &body);

        let toc = doc.outline();
        assert_eq!(toc.len(), 1);
        assert_eq!(toc[0].depth, 1);
        assert_eq!(toc[0].title, "Chapter Two");
        // H1 起新章 → 新页，标题就该落在这一页上
        assert!(toc[0].page > 0, "page = {}", toc[0].page);
        assert!(page_text(&doc, toc[0].page).contains("Chapter Two"));
        assert!(doc.page_count() > toc[0].page);
        assert_eq!(doc.chapter_title(toc[0].page).as_deref(), Some("Chapter Two"));
    }

    #[test]
    fn heading_levels_and_outline_lvl() {
        let body = format!(
            "{}{}{}{}",
            heading("Heading1", "Part"),
            heading("Heading2", "Section"),
            r#"<w:p><w:pPr><w:outlineLvl w:val="0"/></w:pPr><w:r><w:t>Outlined</w:t></w:r></w:p>"#,
            p("tail"),
        );
        let doc = open_body("levels", &body);
        let toc = doc.outline();
        assert_eq!(toc.len(), 3);
        assert_eq!(toc[0].depth, 1);
        assert_eq!(toc[1].depth, 2);
        assert_eq!(toc[2].depth, 1);
        assert_eq!(toc[2].title, "Outlined");
    }

    #[test]
    fn h2_does_not_split_chapter() {
        let body = format!(
            "{}{}{}{}",
            heading("Heading1", "One"),
            p("a"),
            heading("Heading2", "Sub"),
            p("b"),
        );
        let doc = open_body("h2", &body);
        assert_eq!(doc.outline().len(), 2);
        // 只有 H1 起新章：全文 2 章是不可能的，应该是 1 章 1 页
        assert_eq!(doc.page_count(), 1);
    }

    #[test]
    fn three_runs_join_without_loss() {
        let body = r#"<w:p><w:r><w:t>Hello </w:t></w:r><w:r><w:t>big </w:t></w:r><w:r><w:t>world</w:t></w:r></w:p>"#;
        let doc = open_body("runs", body);
        let text = page_text(&doc, 0);
        assert!(text.contains("Hello big world"), "{text}");
    }

    #[test]
    fn tabs_and_breaks_inside_paragraph() {
        let body = r#"<w:p><w:r><w:t>one</w:t><w:tab/><w:t>two</w:t><w:br/><w:t>three</w:t></w:r></w:p>"#;
        let doc = open_body("tabbr", body);
        assert_eq!(page_text(&doc, 0), "one two three");
    }

    #[test]
    fn table_rows_join_cells_with_pipes() {
        let body = r#"<w:tbl>
  <w:tr><w:tc><w:p><w:r><w:t>A1</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>B1</w:t></w:r></w:p></w:tc></w:tr>
  <w:tr><w:tc><w:p><w:r><w:t>A2</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>B2</w:t></w:r></w:p></w:tc></w:tr>
</w:tbl>"#;
        let doc = open_body("table", body);
        let text = page_text(&doc, 0);
        assert!(text.contains("A1 | B1"), "{text}");
        assert!(text.contains("A2 | B2"), "{text}");
        assert_eq!(text.matches('|').count(), 2, "{text}");
    }

    #[test]
    fn entities_are_decoded() {
        let body = p("A &amp; B &lt;C&gt; &quot;D&quot; &#65; &#x42; &apos;E&apos;");
        let doc = open_body("entities", &body);
        assert_eq!(page_text(&doc, 0), "A & B <C> \"D\" A B 'E'");
    }

    #[test]
    fn field_codes_deletions_and_drawings_are_skipped() {
        let body = format!(
            "{}{}",
            r#"<w:p>
                 <w:r><w:fldChar w:fldCharType="begin"/></w:r>
                 <w:r><w:instrText>PAGE</w:instrText></w:r>
                 <w:r><w:t>visible</w:t></w:r>
                 <w:r><w:delText>deleted</w:delText></w:r>
                 <w:r><w:drawing><a:t>drawn</a:t></w:drawing></w:r>
               </w:p>"#,
            p("after"),
        );
        let doc = open_body("skip", &body);
        let text = page_text(&doc, 0);
        assert!(text.contains("visible"), "{text}");
        assert!(text.contains("after"), "{text}");
        assert!(!text.contains("PAGE"), "{text}");
        assert!(!text.contains("deleted"), "{text}");
        assert!(!text.contains("drawn"), "{text}");
    }

    #[test]
    fn empty_document_has_zero_pages() {
        let doc = open_body("empty", "");
        assert_eq!(doc.page_count(), 0);
        assert!(doc.outline().is_empty());
        // 没有页的时候渲染越界，但不 panic
        assert!(doc.render(0, 200, 280).is_err());
    }

    #[test]
    fn invalid_zip_is_err_not_panic() {
        let p = write_tmp("badzip", b"this is definitely not a zip archive");
        assert!(DocxDoc::open(&p).is_err());
        // 空文件同样
        let p = write_tmp("emptyzip", b"");
        assert!(DocxDoc::open(&p).is_err());
    }

    #[test]
    fn missing_document_xml_is_err() {
        let bytes = build_zip(&[("word/other.xml", b"<x/>".to_vec())], true);
        let p = write_tmp("nodoc", &bytes);
        let err = match DocxDoc::open(&p) {
            Ok(_) => panic!("缺少 document.xml 应当报错"),
            Err(e) => e,
        };
        assert!(err.contains("word/document.xml"), "{err}");
    }

    #[test]
    fn out_of_range_page_is_err() {
        let doc = open_body("oor", &p("only page"));
        assert!(doc.render(doc.page_count(), 100, 140).is_err());
        assert!(doc.render(999, 100, 140).is_err());
    }

    #[test]
    fn render_is_deterministic() {
        let doc = open_body("deterministic", &p("hello world"));
        let a = doc.render(0, 200, 280).expect("render");
        let b = doc.render(0, 200, 280).expect("render");
        assert_eq!(a.len(), 200 * 280);
        assert_eq!(a, b);
    }

    #[test]
    fn title_comes_from_core_xml() {
        let entries = vec![
            ("word/document.xml", document_xml(&p("hi")).into_bytes()),
            (
                "docProps/core.xml",
                br#"<?xml version="1.0" encoding="UTF-8"?>
<cp:coreProperties xmlns:cp="http://schemas.openxmlformats.org/package/2006/metadata/core-properties" xmlns:dc="http://purl.org/dc/elements/1.1/">
  <dc:title>  My   Word   Book  </dc:title>
</cp:coreProperties>"#
                    .to_vec(),
            ),
        ];
        let doc = DocxDoc::open(&write_tmp("title", &build_zip(&entries, true))).expect("打开");
        assert_eq!(doc.title(), Some("My Word Book"));
    }

    #[test]
    fn title_missing_is_none() {
        let doc = open_body("notitle", &p("hi"));
        assert_eq!(doc.title(), None);
    }

    #[test]
    fn apply_layout_delegates() {
        let mut doc = open_body("layout", &p("hello world"));
        let default = doc.current_layout();
        assert!(!doc.apply_layout(default));
        let bigger = crate::doc::LayoutCfg {
            font_size: 36.0,
            ..default
        };
        assert!(doc.apply_layout(bigger));
        assert_eq!(doc.current_layout().font_size, 36.0);
        assert!(doc.page_count() > 0);
    }

    #[test]
    fn no_heading_means_single_chapter() {
        let body = format!("{}{}", p("first"), p("second"));
        let doc = open_body("single", &body);
        assert!(doc.outline().is_empty());
        assert_eq!(doc.chapter_title(0), None);
        assert_eq!(doc.page_count(), 1);
    }

    #[test]
    fn whitespace_is_squashed() {
        let body = p("  hello    big\n\t world  ");
        let doc = open_body("spaces", &body);
        assert_eq!(page_text(&doc, 0), "hello big world");
    }

    #[test]
    fn zip_read_rejects_truncated_entries() {
        // 中央目录里把压缩大小吹到文件外面去
        let mut bytes = docx_bytes(&p("x"));
        let n = bytes.len();
        let at = (0..n - 4)
            .find(|&i| bytes[i..i + 4] == SIG_CEN)
            .expect("central directory");
        bytes[at + 20..at + 24].copy_from_slice(&0x0fff_ffffu32.to_le_bytes());
        let p = write_tmp("truncated", &bytes);
        assert!(DocxDoc::open(&p).is_err());
    }
}
