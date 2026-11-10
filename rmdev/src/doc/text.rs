//! TXT / Markdown / HTML / RTF 四种「文字流」格式的后端。
//!
//! 这四个格式的共同点是**没有排版信息**（最多有标题层级），所以统一走
//! 「字节 → 探测编码 → 解析成 [`reflow::Block`] → [`reflow::ReflowDoc`]」一条路：
//! 分页、渲染、目录、搜索全部复用重排引擎，这里只负责把文字抠干净。
//!
//! 编码是这个模块的重点。中文 TXT 里 GBK/GB18030 占绝大多数，Windows 记事本
//! 存的「Unicode」是带 BOM 的 UTF-16，所以探测顺序是
//! **BOM → UTF-8 全量校验 → GB18030（系统 iconv）→ Latin-1 兜底**。
//! 兜底那层会出乱码，但保证不 panic、不丢字。

use std::collections::HashMap;
use std::ffi::CString;
use std::os::raw::{c_char, c_int, c_void};

use super::reflow::{self, Block, Book, Cfg, Chapter, Kind, TocEntry};
use super::Format;

/// 单个文件上限。设备只有 994 MB 内存，60 MB 以上的「文字」文件
/// 只会是日志/转储，不是书。
const MAX_FILE: u64 = 64 << 20;

/// 正文总字符上限，和 `epub.rs` 的 `MAX_CHARS` 对齐：再长的书也只读到这儿。
const MAX_CHARS: usize = 8 << 20;

/// 块数上限。8M 字符不可能有这么多段，纯粹是防畸形输入把内存吃光。
const MAX_BLOCKS: usize = 200_000;

// ---------------------------------------------------------------------------
// 对外接口
// ---------------------------------------------------------------------------

/// 一份 TXT/Markdown/HTML/RTF。分页/渲染/目录/搜索全部委托给内部的 [`reflow::ReflowDoc`]。
pub struct TextDoc {
    inner: reflow::ReflowDoc,
    fmt: crate::doc::Format,
}

impl TextDoc {
    /// 按 `fmt` 选解析器读一整份文字流文件。
    pub fn open(path: &str, fmt: crate::doc::Format) -> Result<TextDoc, String> {
        let len = std::fs::metadata(path)
            .map_err(|e| format!("{path}: {e}"))?
            .len();
        if len > MAX_FILE {
            return Err(format!("{path}: {len} bytes exceeds text limit {MAX_FILE}"));
        }
        let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
        let blocks = match fmt {
            Format::Txt => parse_txt(&bytes),
            Format::Markdown => parse_markdown(&bytes),
            Format::Html => parse_html(&bytes),
            Format::Rtf => parse_rtf(&bytes),
            other => return Err(format!("text backend does not handle {other:?}")),
        };
        // 本地图片（Markdown/TXT 里整行的 `![](pic.png)`）：相对**文档所在目录**解析
        let base = std::path::Path::new(path).parent().unwrap_or(std::path::Path::new(""));
        let blocks = resolve_local_images(blocks, base);
        Ok(TextDoc {
            inner: reflow::ReflowDoc::new(book_of(blocks), Cfg::default()),
            fmt,
        })
    }

    pub fn format(&self) -> crate::doc::Format {
        self.fmt
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

/// 块序列 → 一本「单章」书。
///
/// 文字流格式没有「章节文件」的概念，硬拆成多个 chapter 只能编路径，反而让
/// `path_index` 变成假数据。一个 chapter + 标题块 id 已经足够：`paginate`
/// 会把每个标题真正落在哪一页记进 `anchors`，`outline()`/`chapter_title()`
/// 因此拿到的是精确页号，不是估算。
fn book_of(blocks: Vec<Block>) -> Book {
    let mut blocks = capped(blocks);
    let mut toc: Vec<TocEntry> = Vec::new();
    for (i, b) in blocks.iter_mut().enumerate() {
        if !b.is_heading() {
            continue;
        }
        let id = format!("h{i}");
        b.id = id.clone();
        let level = match b.kind {
            Kind::H1 => 1,
            Kind::H2 => 2,
            _ => 3,
        };
        toc.push(TocEntry {
            level,
            title: b.text.clone(),
            path: String::new(),
            anchor: id,
        });
    }
    let mut path_index = HashMap::new();
    path_index.insert(String::new(), 0);
    Book {
        chapters: vec![Chapter { blocks }],
        toc,
        path_index,
    }
}

/// 字符数/块数封顶。超了直接截断（长尾部分读不到，但 `open` 有界）。
fn capped(blocks: Vec<Block>) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::with_capacity(blocks.len().min(4096));
    let mut total = 0usize;
    for b in blocks {
        total += b.text.chars().count();
        if total > MAX_CHARS || out.len() >= MAX_BLOCKS {
            break;
        }
        out.push(b);
    }
    out
}

// ---------------------------------------------------------------------------
// 本地图片（TXT / Markdown）
// ---------------------------------------------------------------------------

/// 单个本地插图文件大小上限。比正文上限小：一本文字书里的插图不会几十 MB。
const MAX_IMG_BYTES: u64 = 32 << 20;

/// TXT / Markdown 里**整行就是一个图片引用**（`![alt](pic.png)`）、或者 HTML 里的
/// `<img src>`，都换成图片块。
///
/// Markdown 只认整行：行内图片会把一段话拆成三块，排版反而更难看，收益也小。
/// 相对路径按**文档所在目录**解析；读不到 / 不是 JPEG/PNG / 超过上限就不放图
/// ——Markdown 原样留成文字，HTML 直接不占位（不 panic，也不丢正文）。
fn resolve_local_images(blocks: Vec<Block>, base: &std::path::Path) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::with_capacity(blocks.len());
    // 整篇的图片总字节额度，和 EPUB 一样有界（见 reflow::MAX_IMG_TOTAL）
    let mut budget = reflow::MAX_IMG_TOTAL;
    for b in blocks {
        // HTML 的 `<img>` 只有 src，像素在这里补
        if b.kind == Kind::Image {
            if let Some(img) = load_local_image(base, b.text.trim(), &mut budget) {
                let key = b.text;
                out.push(Block::image(key, img));
            }
            continue;
        }
        if b.kind != Kind::Body {
            out.push(b);
            continue;
        }
        let Some(rel) = md_image_path(&b.text) else {
            out.push(b);
            continue;
        };
        match load_local_image(base, rel, &mut budget) {
            Some(img) => out.push(Block::image(rel, img)),
            None => out.push(b),
        }
    }
    out
}

/// `![alt](path "title")` → `path`。整串不是图片引用就 `None`。
fn md_image_path(text: &str) -> Option<&str> {
    let s = text.trim();
    let rest = s.strip_prefix("![")?;
    let close = rest.find(']')?;
    let rest = rest[close + 1..].strip_prefix('(')?;
    // 不做嵌套括号：图片路径里带 `(` 是极少数，按第一个 `)` 切就够
    let end = rest.find(')')?;
    // `![alt](pic.png "标题")` 只取路径
    let path = rest[..end].split_whitespace().next()?;
    (!path.is_empty()).then_some(path)
}

fn load_local_image(
    base: &std::path::Path,
    rel: &str,
    budget: &mut usize,
) -> Option<std::sync::Arc<reflow::Img>> {
    if *budget == 0 {
        return None;
    }
    let p = base.join(rel);
    let len = std::fs::metadata(&p).ok()?.len();
    if len == 0 || len > MAX_IMG_BYTES {
        return None;
    }
    let data = std::fs::read(&p).ok()?;
    let img = reflow::decode_image(&data).ok()?;
    *budget = budget.saturating_sub(img.w * img.h);
    Some(std::sync::Arc::new(img))
}

// ---------------------------------------------------------------------------
// 编码探测
// ---------------------------------------------------------------------------

extern "C" {
    // `libc` crate 里没有 iconv 绑定，自己声明这 4 个就够（glibc / macOS 都在 libc 里）。
    fn iconv_open(tocode: *const c_char, fromcode: *const c_char) -> *mut c_void;
    fn iconv(
        cd: *mut c_void,
        inbuf: *mut *mut c_char,
        inbytesleft: *mut usize,
        outbuf: *mut *mut c_char,
        outbytesleft: *mut usize,
    ) -> usize;
    fn iconv_close(cd: *mut c_void) -> c_int;
}

/// 打开一个「任意编码 → UTF-8」的转换器。
///
/// 优先 `UTF-8//IGNORE`（坏字节直接丢，不会卡住）；老版本 libiconv 不认这个
/// 后缀时退回纯 `UTF-8`。返回 `None` = 这台机器没这个编码（或干脆没 iconv）。
fn iconv_open_utf8(from: &str) -> Option<*mut c_void> {
    let from = CString::new(from).ok()?;
    for to in ["UTF-8//IGNORE", "UTF-8"] {
        let Ok(to) = CString::new(to) else {
            continue;
        };
        let cd = unsafe { iconv_open(to.as_ptr(), from.as_ptr()) };
        // `iconv_open` 失败返回 (iconv_t)-1
        if cd as isize != -1 {
            return Some(cd);
        }
    }
    None
}

/// 用系统 `iconv` 把 `bytes` 从 `from` 转成 UTF-8；打不开/一个字符都没转出来给 `None`。
///
/// `iconv_t` 是 `*mut c_void`，`iconv` 失败返回 `(size_t)-1`。这里不读 `errno`：
/// 转换失败时看有没有产出，**没产出就跳过一字节强推**，这样无论 E2BIG 还是
/// EILSEQ 都一定收敛，不会死循环。
fn iconv_to_utf8(bytes: &[u8], from: &str) -> Option<String> {
    if bytes.is_empty() {
        return Some(String::new());
    }
    let cd = iconv_open_utf8(from)?;
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len() + bytes.len() / 2 + 8);
    let mut chunk = vec![0u8; 8192];
    let mut in_ptr = bytes.as_ptr() as *mut c_char;
    let mut in_left = bytes.len();
    loop {
        let mut out_ptr = chunk.as_mut_ptr() as *mut c_char;
        let mut out_left = chunk.len();
        let r = unsafe {
            iconv(
                cd,
                &mut in_ptr,
                &mut in_left,
                &mut out_ptr,
                &mut out_left,
            )
        };
        let produced = chunk.len() - out_left;
        out.extend_from_slice(&chunk[..produced]);
        if r != usize::MAX {
            break; // 输入全部吃完
        }
        if in_left == 0 {
            break; // 结尾是半个多字节序列，丢掉
        }
        if produced == 0 {
            // EILSEQ / EINVAL 且毫无产出：跳一字节，保证进度
            in_ptr = unsafe { in_ptr.add(1) };
            in_left -= 1;
        }
    }
    unsafe { iconv_close(cd) };
    if out.is_empty() {
        return None; // 全丢了 → 让上层退回 Latin-1，至少还看得见字节
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

/// 环境里有没有能解 GB18030 的 iconv。测试用它决定要不要跳过 GBK 断言。
#[cfg(test)]
fn iconv_available() -> bool {
    match iconv_open_utf8("GB18030") {
        Some(cd) => {
            unsafe { iconv_close(cd) };
            true
        }
        None => false,
    }
}

/// Latin-1（ISO-8859-1）逐字节映射：0x00..=0xFF 正好对应 U+0000..=U+00FF。
/// 乱码但永不失败，是所有解码路径的最后兜底。
fn decode_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| b as char).collect()
}

/// UTF-16 → String。`little == true` 是小端（Windows「Unicode」）。
///
/// 不用 `from_utf8`（那是 UTF-8 的），按 16 位单元合并 + 代理对还原；
/// 落单的半个代理对用 U+FFFD 代替。
fn decode_utf16(bytes: &[u8], little: bool) -> String {
    let mut units: Vec<u16> = Vec::with_capacity(bytes.len() / 2);
    for p in bytes.chunks_exact(2) {
        units.push(if little {
            u16::from_le_bytes([p[0], p[1]])
        } else {
            u16::from_be_bytes([p[0], p[1]])
        });
    }
    char::decode_utf16(units)
        .map(|r| r.unwrap_or('\u{fffd}'))
        .collect()
}

/// 无 BOM 的编码探测：UTF-8 全量校验 → GB18030 → Latin-1。
fn decode_text(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF][..]) {
        return decode_utf8_lenient(rest);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE][..]) {
        return decode_utf16(rest, true);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF][..]) {
        return decode_utf16(rest, false);
    }
    // 全量校验：只要有一处非法就整份当 GBK 试——UTF-8 碰巧通过 GBK 解出的
    // 概率极低，反过来（GBK 通过 UTF-8 校验）也极低，所以这两种是互斥的。
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    // 先用自带的 GBK 表：设备上 `/usr/lib/gconv` 只剩 IBM437，iconv 必然失败，
    // 而中文 TXT 绝大多数就是 GBK。表覆盖 GBK 双字节全集（21791 个有效码位）。
    if let Some(s) = super::gbk::decode(bytes) {
        return s;
    }
    // 再试系统 iconv：桌面/服务器上的 gconv 更全，能多认一些扩展编码
    if let Some(s) = iconv_to_utf8(bytes, "GB18030") {
        return s;
    }
    decode_latin1(bytes)
}

/// 已知是 UTF-8 时用：非法字节换 U+FFFD，别整份退回 GBK。
fn decode_utf8_lenient(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Windows-1252 相对 Latin-1 只有 0x80..=0x9F 这一段不同（引号/破折号/™）。
const CP1252_HIGH: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8d}', 'Ž', '\u{8f}',
    '\u{90}', '‘', '’', '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9d}', 'ž', 'Ÿ',
];

fn decode_cp1252(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if (0x80..=0x9f).contains(&b) {
                CP1252_HIGH[(b - 0x80) as usize]
            } else {
                b as char
            }
        })
        .collect()
}

/// 按 codepage 解一段原始字节（RTF 的 `\'hh` 用）。
/// 只做中文书里会出现的几种；其余一律当 cp1252。
fn decode_codepage(bytes: &[u8], cp: u16) -> String {
    match cp {
        // GBK/GB2312 都是 GB18030 的子集，用超集解不会错
        936 => super::gbk::decode(bytes)
            .or_else(|| iconv_to_utf8(bytes, "GB18030"))
            .unwrap_or_else(|| decode_latin1(bytes)),
        950 => iconv_to_utf8(bytes, "BIG5").unwrap_or_else(|| decode_latin1(bytes)),
        65001 => decode_utf8_lenient(bytes),
        _ => decode_cp1252(bytes),
    }
}

// ---------------------------------------------------------------------------
// TXT
// ---------------------------------------------------------------------------

fn parse_txt(bytes: &[u8]) -> Vec<Block> {
    let text = decode_text(bytes);
    let text = text.trim_start_matches('\u{feff}');
    let unfolded = unfold_hard_wraps(text);
    let marked = mark_chapter_headings(&unfolded);
    // 分段/标题规则跟 EPUB/FB2/PDF 抽出来的文字完全一致，直接用共享实现。
    // 但「每段一行、段间不空行」的文件要先拆行，否则整章会挤成一段。
    if lines_are_paragraphs(&marked) {
        return reflow::blocks_from_plain(&one_line_per_paragraph(&marked), MAX_BLOCKS);
    }
    reflow::blocks_from_plain(&marked, MAX_BLOCKS)
}

/// 「每行就是一个段落」的判据。
///
/// 中文小说最常见的存法就是这样：每段一行、段与段之间**不空行**。
/// `blocks_from_plain` 的规则是「连续非空行并成一段」，碰上这种文件会把
/// 整章挤成一大段，所有段间距消失。
///
/// 判据：几乎没有空行（<5%）+ 多数行都够长（≥30 字，说明不是排版折行）。
/// 两个条件都满足才按行分段——太激进会把真正的折行文件切碎。
fn lines_are_paragraphs(text: &str) -> bool {
    let all: Vec<&str> = text.lines().collect();
    let lines: Vec<&str> = all.iter().map(|l| l.trim()).filter(|l| !l.is_empty()).collect();
    if lines.len() < 5 {
        return false;
    }
    let blank = all.iter().filter(|l| l.trim().is_empty()).count();
    if blank * 100 > all.len().max(1) * 5 {
        return false;
    }
    let long = lines.iter().filter(|l| l.chars().count() >= 30).count();
    long * 10 >= lines.len() * 6
}

/// 每行后面补一个空行，让 `blocks_from_plain` 把它当独立段落。
fn one_line_per_paragraph(text: &str) -> String {
    let mut out = String::with_capacity(text.len() * 2);
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        out.push_str(t);
        out.push_str("\n\n");
    }
    out
}

/// 行尾标点。硬折行的判据之一是「这一行没说完」，所以标点要认得宽一点。
const LINE_PUNCT: [char; 30] = [
    '。', '，', '、', '；', '：', '！', '？', '…', '—', '·', '）', '》', '」', '』', '】', '”', '’',
    '.', ',', ';', ':', '!', '?', '-', '—', '"', '\'', ')', ']', '}',
];

fn ends_with_punct(s: &str) -> bool {
    s.chars().next_back().is_some_and(|c| LINE_PUNCT.contains(&c))
}

/// 「硬折行」检测 + 接行。
///
/// TXT 里的换行有两种意思：**段落边界**（空行分段、每段一行）和**排版折行**
/// （记事本/OCR/PDF 拷贝出来的固定宽度），后者按行分段会把一句话切成一堆
/// 残句。判据（用户定的）：整份文件 90% 以上的非空行都短于 30 字符**且**
/// 行尾不带标点——正常写作不可能 90% 的句子都在 30 字以内外加没有句号。
///
/// 命中后把连续行按中英文规则接回一整段（中文直接接，拉丁文补空格），
/// 章节行和空行仍然断开。**代价**：短句诗/歌词/对话体也会被判成硬折行，
/// 见文件末尾「已知限制」。
fn unfold_hard_wraps(text: &str) -> String {
    let lines: Vec<&str> = text.lines().map(|l| l.trim_end_matches('\r')).collect();
    let nonempty: Vec<&str> = lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect();
    // 样本太小时统计没意义，直接当正常文件
    if nonempty.len() < 20 {
        return text.to_string();
    }
    let short = nonempty.iter().filter(|l| l.chars().count() < 30).count();
    let bare = nonempty.iter().filter(|l| !ends_with_punct(l)).count();
    let n = nonempty.len();
    if short * 10 < n * 9 || bare * 10 < n * 9 {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut para = String::new();
    for line in lines {
        let t = line.trim();
        if t.is_empty() {
            flush_unfolded(&mut out, &mut para);
            continue;
        }
        if is_chapter_heading(t) {
            flush_unfolded(&mut out, &mut para);
            out.push_str(t);
            out.push_str("\n\n");
            continue;
        }
        if para.is_empty() {
            para.push_str(t);
        } else {
            // 和 blocks_from_plain 同一条接行规则：中文之间不补空格
            let prev_cjk = para.chars().next_back().is_some_and(reflow::is_cjk);
            let next_cjk = t.chars().next().is_some_and(reflow::is_cjk);
            if !prev_cjk && !next_cjk {
                para.push(' ');
            }
            para.push_str(t);
        }
    }
    flush_unfolded(&mut out, &mut para);
    out
}

fn flush_unfolded(out: &mut String, para: &mut String) {
    let t = para.trim();
    if !t.is_empty() {
        out.push_str(t);
        out.push_str("\n\n");
    }
    para.clear();
}

/// 给认出来的章节行加 `# `——`blocks_from_plain` 只认 Markdown 风格标题，
/// 这里把「第X章 / Chapter N / 序章 / 全大写短行」翻译成它认得的写法。
fn mark_chapter_headings(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 32);
    for line in text.lines() {
        let t = line.trim();
        if !t.is_empty() && is_chapter_heading(t) {
            out.push_str("# ");
            out.push_str(t);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// 章节行判定，用来给长篇 TXT 生成目录。
///
/// 认这几类：
/// * 中文 `第X章/回/节/卷/篇/部`（X 是阿拉伯或中文数字）
/// * `序章`/`楔子`/`引子`/`前言`/`后记`/`尾声`/`终章`/`番外`/`外传`/`附录`
/// * `Chapter N` / `Chapter IV` / `Chapter One` / `Prologue` / `Epilogue`
/// * 全大写短行（网页转出来的 TXT 里 `PART ONE`、`THE END` 很常见）
///
/// 判据刻意收紧（长度、句末标点、词后必须有编号）：TXT 没结构，误判的代价是
/// 正文里凭空多一条目录，比漏判更难接受。
fn is_chapter_heading(t: &str) -> bool {
    let n = t.chars().count();
    if n == 0 || n > 60 || t.starts_with('#') {
        return false;
    }
    if t.chars().any(|c| matches!(c, '。' | '！' | '？' | '；')) {
        return false;
    }
    cn_numbered(t) || fixed_cn_heading(t) || en_chapter(t) || all_caps_line(t)
}

fn is_cn_numeral(c: char) -> bool {
    matches!(
        c,
        '0'..='9'
            | '〇'
            | '零'
            | '一'
            | '二'
            | '两'
            | '三'
            | '四'
            | '五'
            | '六'
            | '七'
            | '八'
            | '九'
            | '十'
            | '百'
            | '千'
            | '万'
            | '廿'
            | '卅'
            | '之'
            | ' '
    )
}

/// `第X章`：`第` 和单位字之间只能是数字（1..=6 个字）。
fn cn_numbered(t: &str) -> bool {
    let Some(rest) = t.strip_prefix('第') else {
        return false;
    };
    let Some(pos) = rest.find(['章', '回', '节', '卷', '篇', '部']) else {
        return false;
    };
    let num = &rest[..pos];
    let count = num.chars().count();
    (1..=6).contains(&count) && num.chars().all(is_cn_numeral)
}

const CN_FIXED: [&str; 12] = [
    "序章", "序言", "楔子", "引子", "前言", "后记", "尾声", "尾聲", "终章", "番外", "外传", "附录",
];

/// 固定词章节：词后面必须紧跟编号/分隔符/结束，防止「前言不搭后语」这种句子。
fn fixed_cn_heading(t: &str) -> bool {
    if t.chars().count() > 14 {
        return false;
    }
    CN_FIXED.iter().any(|w| {
        t.strip_prefix(w).is_some_and(|rest| match rest.chars().next() {
            None => true,
            Some(c) => matches!(c, ' ' | '：' | ':' | '—' | '-' | '·' | '（' | '(' | '　')
                || is_cn_numeral(c),
        })
    })
}

/// `Chapter 12` / `Chapter IV` / `Chapter One` / `Prologue`。
fn en_chapter(t: &str) -> bool {
    let mut it = t.split_whitespace();
    let Some(first) = it.next() else {
        return false;
    };
    let word = first
        .trim_end_matches([':', '.', ','])
        .to_ascii_lowercase();
    match word.as_str() {
        "prologue" | "epilogue" => true,
        "chapter" | "part" | "book" => match it.next() {
            Some(second) => {
                let c = second.chars().next().unwrap_or(' ');
                c.is_ascii_digit()
                    || "IVXLCDMivxlcdm".contains(c)
                    || is_number_word(&second.to_ascii_lowercase())
            }
            None => false,
        },
        _ => false,
    }
}

fn is_number_word(w: &str) -> bool {
    matches!(
        w.trim_end_matches([':', '.', ',']),
        "one"
            | "two"
            | "three"
            | "four"
            | "five"
            | "six"
            | "seven"
            | "eight"
            | "nine"
            | "ten"
            | "eleven"
            | "twelve"
            | "first"
            | "second"
            | "third"
    )
}

/// 全大写短行（至少 2 个字母，其余只能是数字/空白/标点）。
fn all_caps_line(t: &str) -> bool {
    let n = t.chars().count();
    if !(3..=40).contains(&n) {
        return false;
    }
    let mut letters = 0;
    for c in t.chars() {
        if c.is_alphabetic() {
            if !c.is_uppercase() {
                return false;
            }
            letters += 1;
        } else if !(c.is_numeric()
            || c.is_whitespace()
            || c.is_ascii_punctuation()
            || matches!(c, '·' | '—' | '－' | '＆' | '、'))
        {
            return false;
        }
    }
    letters >= 2
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

fn parse_markdown(bytes: &[u8]) -> Vec<Block> {
    let text = decode_text(bytes);
    let text = text.trim_start_matches('\u{feff}');
    blocks_from_md(text)
}

fn blocks_from_md(text: &str) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut para = String::new();
    // 代码围栏的标记（``` 或 ~~~）；出现时里面一行一块，不合并、不解析
    let mut fence: Option<&'static str> = None;

    for raw in text.lines() {
        let line = raw.trim_end_matches('\r');
        let t = line.trim();
        if let Some(marker) = fence {
            if t.starts_with(marker) {
                fence = None;
            } else if !t.is_empty() {
                out.push(Block::body(line));
            }
            continue;
        }
        if let Some(marker) = fence_start(t) {
            flush_md(&mut out, &mut para);
            fence = Some(marker);
            continue;
        }
        if t.is_empty() {
            flush_md(&mut out, &mut para);
            continue;
        }
        if let Some((level, title)) = md_heading(t) {
            flush_md(&mut out, &mut para);
            let title = strip_inline(title, 0);
            if !title.is_empty() {
                out.push(Block::heading(level, title));
            }
            continue;
        }
        // 水平线（`---`/`***`/`___`）整行丢掉。注意要在列表之前判，否则
        // `- - -` 会被当成三个空列表项。
        if md_rule(t) {
            flush_md(&mut out, &mut para);
            continue;
        }
        // 列表项：保留符号当文字前缀（排版引擎左边界恒为 0，做不了缩进）
        if let Some(item) = md_list_item(t) {
            flush_md(&mut out, &mut para);
            out.push(Block::body(item));
            continue;
        }
        // 表格：一行一段，单元格用 " | " 连
        if t.contains('|') {
            flush_md(&mut out, &mut para);
            if md_table_sep(t) {
                continue;
            }
            let row = md_table_row(t);
            if !row.is_empty() {
                out.push(Block::body(row));
            }
            continue;
        }
        // 引用：去掉 `>`，文字照样并进段落
        let body = t.trim_start_matches('>').trim();
        push_md_line(&mut para, body);
    }
    flush_md(&mut out, &mut para);
    out
}

fn flush_md(out: &mut Vec<Block>, para: &mut String) {
    let t = para.trim();
    if !t.is_empty() {
        out.push(Block::body(t));
    }
    para.clear();
}

/// 一行正文并进当前段落（行内标记先剥掉）。中文之间不加空格，拉丁文之间加。
fn push_md_line(para: &mut String, line: &str) {
    let text = strip_inline(line, 0);
    let t = text.trim();
    if t.is_empty() {
        return;
    }
    if !para.is_empty() {
        let prev_cjk = para.chars().next_back().is_some_and(reflow::is_cjk);
        let next_cjk = t.chars().next().is_some_and(reflow::is_cjk);
        if !prev_cjk && !next_cjk {
            para.push(' ');
        }
    }
    para.push_str(t);
}

fn fence_start(t: &str) -> Option<&'static str> {
    if t.starts_with("```") {
        Some("```")
    } else if t.starts_with("~~~") {
        Some("~~~")
    } else {
        None
    }
}

/// `#`~`######` + 空格/行尾。4 级以上按 H3（排版引擎只有 3 级）。
fn md_heading(t: &str) -> Option<(u8, &str)> {
    let n = t.chars().take_while(|c| *c == '#').count();
    if n == 0 || n > 6 {
        return None;
    }
    let rest = &t[n..];
    if rest.is_empty() {
        return Some((n as u8, ""));
    }
    if !rest.starts_with(' ') {
        return None; // `#include <stdio.h>` 之类不当标题
    }
    Some((n as u8, rest.trim_start()))
}

fn md_rule(t: &str) -> bool {
    let mut chars = t.chars().filter(|c| !c.is_whitespace());
    let Some(first) = chars.next() else {
        return false;
    };
    if !matches!(first, '-' | '*' | '_') {
        return false;
    }
    let mut n = 1usize;
    for c in chars {
        if c != first {
            return false;
        }
        n += 1;
    }
    n >= 3
}

/// 无序列表 → `• `；有序列表保留原编号（`1. `）。
fn md_list_item(t: &str) -> Option<String> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = t.strip_prefix(marker) {
            let text = strip_inline(rest, 0);
            let text = text.trim();
            if text.is_empty() {
                return None;
            }
            return Some(format!("• {text}"));
        }
    }
    // 有序：`1.` / `1)`，编号最长 3 位（再长是一行以数字开头的正文）
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 || digits > 3 {
        return None;
    }
    let rest = &t[digits..];
    let mut it = rest.chars();
    let mark = it.next()?;
    if !matches!(mark, '.' | ')') || !rest[1..].starts_with(' ') {
        return None;
    }
    let text = strip_inline(rest[1..].trim(), 0);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(format!("{} {text}", &t[..digits + 1]))
}

/// 表格分隔行（`|---|:--:|`）本身没有文字，丢掉。
fn md_table_sep(t: &str) -> bool {
    let mut dash = false;
    for c in t.chars() {
        match c {
            '-' => dash = true,
            '|' | ':' | ' ' | '\t' => {}
            _ => return false,
        }
    }
    dash
}

fn md_table_row(t: &str) -> String {
    let mut s = t.trim();
    s = s.strip_prefix('|').unwrap_or(s);
    s = s.strip_suffix('|').unwrap_or(s);
    let cells: Vec<String> = s
        .split('|')
        .map(|c| strip_inline(c, 0))
        .map(|c| c.trim().to_string())
        .filter(|c| !c.is_empty())
        .collect();
    cells.join(" | ")
}

/// 剥掉行内标记，只留文字。
///
/// 我们没有富文本渲染，留着 `**` 只会显示成星号，所以：
/// 图片 `![alt](url)` 整段丢、链接 `[文字](url)` 只留文字、`*`/`_`/`` ` ``
/// 一律删掉。`depth` 是嵌套上限，防止 `[[[[[...` 这种输入递归到爆栈。
fn strip_inline(s: &str, depth: usize) -> String {
    if depth > 6 {
        return s.to_string();
    }
    let b: Vec<char> = s.chars().collect();
    // 先处理图片：`![alt](url)` 整段丢（包括感叹号）
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < b.len() {
        let c = b[i];
        if c == '!' && i + 1 < b.len() && b[i + 1] == '[' {
            if let Some((_, end)) = bracket_span(&b, i + 1) {
                i = end;
                continue;
            }
        }
        if c == '[' {
            if let Some((close, end)) = bracket_span(&b, i) {
                let label: String = b[i + 1..close].iter().collect();
                out.push_str(&strip_inline(&label, depth + 1));
                i = end;
                continue;
            }
        }
        if matches!(c, '*' | '_' | '`') {
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    // 图片/链接被抠掉后可能留下连续空格，折一个
    let mut compact = String::with_capacity(out.len());
    let mut space = false;
    for c in out.chars() {
        if c == ' ' {
            if !space {
                compact.push(' ');
            }
            space = true;
        } else {
            compact.push(c);
            space = false;
        }
    }
    compact
}

/// `b[open] == '['`，返回 `(匹配的 ']' 下标, 整段之后的下标)`；后面没跟
/// `(url)` 就不算链接，返回 `None`。
fn bracket_span(b: &[char], open: usize) -> Option<(usize, usize)> {
    let mut j = open + 1;
    let mut depth = 0usize;
    while j < b.len() {
        match b[j] {
            '[' => depth += 1,
            ']' => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
        j += 1;
    }
    if j >= b.len() || b[j] != ']' {
        return None;
    }
    let close = j;
    if j + 1 >= b.len() || b[j + 1] != '(' {
        return None;
    }
    let mut k = j + 2;
    let mut pdepth = 0usize;
    while k < b.len() {
        match b[k] {
            '(' => pdepth += 1,
            ')' => {
                if pdepth == 0 {
                    break;
                }
                pdepth -= 1;
            }
            _ => {}
        }
        k += 1;
    }
    if k >= b.len() {
        return None;
    }
    Some((close, k + 1))
}

// ---------------------------------------------------------------------------
// HTML
// ---------------------------------------------------------------------------

fn parse_html(bytes: &[u8]) -> Vec<Block> {
    let text = decode_html(bytes);
    blocks_from_html(&text)
}

/// 按 `<meta charset>` 声明的编码解（一律转成 UTF-8），没声明就走 TXT 的探测。
fn decode_html(bytes: &[u8]) -> String {
    if let Some(enc) = sniff_charset(bytes) {
        let e = enc.to_ascii_lowercase();
        if matches!(e.as_str(), "utf-8" | "utf8") {
            return strip_bom_char(decode_utf8_lenient(bytes));
        }
        if is_latin1_name(&e) {
            return strip_bom_char(decode_latin1(bytes));
        }
        if let Some(iconv_name) = iconv_encoding(&e) {
            if let Some(s) = iconv_to_utf8(bytes, iconv_name) {
                return strip_bom_char(s);
            }
        }
    }
    strip_bom_char(decode_text(bytes))
}

fn strip_bom_char(s: String) -> String {
    s.trim_start_matches('\u{feff}').to_string()
}

/// HTML 里常见的编码名 → iconv 名。GBK/GB2312 都用超集 GB18030 解。
fn iconv_encoding(name: &str) -> Option<&'static str> {
    match name {
        "gb2312" | "gbk" | "gb18030" | "gb_2312" | "cp936" | "windows-936" | "x-gbk"
        | "gb-2312-80" => Some("GB18030"),
        "big5" | "big-5" | "cp950" | "x-x-big5" => Some("BIG5"),
        "shift_jis" | "shift-jis" | "sjis" | "ms_kanji" => Some("SHIFT_JIS"),
        "euc-jp" => Some("EUC-JP"),
        "euc-kr" | "ks_c_5601-1987" => Some("EUC-KR"),
        _ => None,
    }
}

fn is_latin1_name(name: &str) -> bool {
    matches!(
        name,
        "latin1"
            | "latin-1"
            | "iso-8859-1"
            | "iso8859-1"
            | "iso_8859-1"
            | "windows-1252"
            | "cp1252"
            | "us-ascii"
            | "ascii"
    )
}

/// 在文件头里找 `charset=xxx`。`<meta charset="utf-8">` 和
/// `<meta http-equiv="Content-Type" content="text/html; charset=gb2312">` 都认。
fn sniff_charset(bytes: &[u8]) -> Option<String> {
    let head = &bytes[..bytes.len().min(4096)];
    let s = String::from_utf8_lossy(head).to_ascii_lowercase();
    let mut pos = 0usize;
    while let Some(k) = s[pos..].find("charset") {
        let mut rest = &s[pos + k + "charset".len()..];
        rest = rest.trim_start();
        rest = rest.strip_prefix('=').unwrap_or(rest).trim_start();
        rest = rest.trim_start_matches(['"', '\'']);
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            .collect();
        if !name.is_empty() {
            return Some(name);
        }
        pos += k + "charset".len();
    }
    None
}

fn blocks_from_html(html: &str) -> Vec<Block> {
    // `head` 没闭合时不能把后面全丢了（现实里 `<head>` 漏闭合有，丢了就是白页）
    let html = strip_section(html, "head", false);
    // `script`/`style` 没闭合就把剩下的当代码丢掉，比把 JS 塞进正文强
    let html = strip_section(&html, "script", true);
    let html = strip_section(&html, "style", true);
    let html = strip_comments(&html);

    let mut out: Vec<Block> = Vec::new();
    let mut buf = String::new();
    let mut level: Option<u8> = None;
    let mut pos = 0usize;

    while pos < html.len() {
        let Some(k) = html[pos..].find('<') else {
            buf.push_str(&html[pos..]);
            break;
        };
        let open = pos + k;
        if open > pos {
            buf.push_str(&html[pos..open]);
        }
        let Some(e) = html[open..].find('>') else {
            // 没闭合的 `<`：剩下全是文本
            buf.push_str(&html[open..]);
            break;
        };
        let tag = &html[open + 1..open + e];
        pos = open + e + 1;
        handle_html_tag(tag, &mut buf, &mut level, &mut out);
    }
    flush_html(&mut buf, level.take(), &mut out);
    out
}

fn handle_html_tag(
    tag: &str,
    buf: &mut String,
    level: &mut Option<u8>,
    out: &mut Vec<Block>,
) {
    let t = tag.trim();
    let closing = t.starts_with('/');
    let body = t.trim_start_matches('/').trim_start();
    let name: String = body
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase();
    if name.is_empty() {
        return;
    }
    // `<img src>`：先收掉当前段落，再占一个整块。像素由 `resolve_local_images`
    // 补——这里只扫标签流，不知道这个 HTML 文件在哪个目录。
    if !closing && name == "img" {
        if let Some(src) = html_attr(t, "src").filter(|s| !s.trim().is_empty()) {
            flush_html(buf, *level, out);
            *level = None;
            out.push(Block {
                kind: Kind::Image,
                text: src,
                id: String::new(),
                img: None,
            });
        }
        return;
    }
    // 标题：`<h1>`~`<h6>`，4 级以下按 H3
    if name.len() == 2 && name.starts_with('h') {
        if let Some(d) = name.as_bytes()[1].checked_sub(b'0') {
            if (1..=6u8).contains(&d) {
                flush_html(buf, *level, out);
                *level = if closing {
                    None
                } else {
                    Some(d.min(3))
                };
                return;
            }
        }
    }
    // 断块的标签：开/闭都断，空块会被 flush 吃掉
    const BREAK: [&str; 20] = [
        "p", "div", "br", "li", "tr", "td", "th", "ul", "ol", "dl", "dt", "dd", "blockquote",
        "section", "article", "header", "footer", "nav", "hr", "pre",
    ];
    if BREAK.contains(&name.as_str()) {
        flush_html(buf, *level, out);
        *level = None;
    }
    // meta/link/input 之类没有文字，其余行内标签（span/a/b/i/em...）直接忽略，
    // 它们的文字会自然流进当前段落。
}

/// 从标签串（`img src="a.png" alt=x`）里取属性值：大小写不敏感、去引号、解实体。
///
/// 名字前后做了边界检查，`data-src="..."` 不会被当成 `src`。
fn html_attr(tag: &str, want: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let b = tag.as_bytes();
    let mut from = 0usize;
    while let Some(k) = lower[from..].find(want) {
        let at = from + k;
        from = at + want.len();
        let before = at > 0 && (b[at - 1] as char).is_whitespace();
        let mut j = at + want.len();
        while j < b.len() && (b[j] as char).is_whitespace() {
            j += 1;
        }
        if !before || b.get(j) != Some(&b'=') {
            continue;
        }
        j += 1;
        while j < b.len() && (b[j] as char).is_whitespace() {
            j += 1;
        }
        if j >= b.len() {
            return Some(String::new());
        }
        if b[j] == b'"' || b[j] == b'\'' {
            let q = b[j] as char;
            let start = j + 1;
            let end = start + tag[start..].find(q)?;
            return Some(decode_entities(&tag[start..end]));
        }
        let start = j;
        while j < b.len() && !(b[j] as char).is_whitespace() {
            j += 1;
        }
        return Some(decode_entities(&tag[start..j]));
    }
    None
}

/// 把 `buf` 里的原始 HTML 文本收尾成一个块：解实体 → 折空白 → 去首尾。
fn flush_html(buf: &mut String, level: Option<u8>, out: &mut Vec<Block>) {
    let text = clean_html_text(buf);
    buf.clear();
    if text.is_empty() {
        return;
    }
    match level {
        Some(l) => out.push(Block::heading(l, text)),
        None => out.push(Block::body(text)),
    }
}

fn clean_html_text(s: &str) -> String {
    let decoded = decode_entities(s);
    let mut out = String::with_capacity(decoded.len());
    let mut space = true; // 行首空白直接吃掉
    for c in decoded.chars() {
        if c.is_whitespace() {
            if !space {
                out.push(' ');
                space = true;
            }
        } else {
            out.push(c);
            space = false;
        }
    }
    // 被中文夹住的空格删掉（源文件里的换行/缩进不该出现在中文句子中间）
    let chars: Vec<char> = out.chars().collect();
    let mut res = String::with_capacity(out.len());
    for (i, &c) in chars.iter().enumerate() {
        if c == ' ' && i > 0 && i + 1 < chars.len() && reflow::is_cjk(chars[i - 1]) && reflow::is_cjk(chars[i + 1]) {
            continue;
        }
        res.push(c);
    }
    res.trim().to_string()
}

/// HTML 实体：命名实体一小张表，外加 `&#NNN;` / `&#xHH;`。
/// 认不出来的原样留着（`&foo;` 显示成 `&foo;` 比变成空更诚实）。
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < s.len() {
        if s.as_bytes()[i] == b'&' {
            if let Some((ch, len)) = entity_at(&s[i..]) {
                out.push(ch);
                i += len;
                continue;
            }
        }
        let Some(c) = s[i..].chars().next() else {
            break;
        };
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// 解 `s` 开头的一个实体，返回 `(字符, 消耗字节数)`。
fn entity_at(s: &str) -> Option<(char, usize)> {
    let semi = s.find(';')?;
    if semi > 12 {
        return None;
    }
    let name = &s[1..semi];
    let len = semi + 1;
    if let Some(num) = name.strip_prefix('#') {
        let code = if let Some(hex) = num.strip_prefix(['x', 'X']) {
            u32::from_str_radix(hex, 16).ok()?
        } else {
            num.parse::<u32>().ok()?
        };
        return char::from_u32(code).map(|c| (c, len));
    }
    let c = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "mdash" => '—',
        "ndash" => '–',
        "hellip" => '…',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "laquo" => '«',
        "raquo" => '»',
        "bull" => '•',
        "middot" => '·',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "times" => '×',
        "divide" => '÷',
        "deg" => '°',
        "plusmn" => '±',
        "frac12" => '½',
        "ensp" | "emsp" | "thinsp" => ' ',
        _ => return None,
    };
    Some((c, len))
}

/// 去掉 `<tag ...> ... </tag>` 整段（大小写不敏感）。
///
/// 标签名必须自带边界（`<head>`/`<head x>`/`</head>`），否则 `<header>` 会被
/// 当成 `<head>` 的开头，把整篇正文吞掉。`drop_rest` = 没有闭合时把剩下的
/// 全丢（对 `script`/`style` 是对的，对 `head` 是灾难）。
fn strip_section(src: &str, tag: &str, drop_rest: bool) -> String {
    // 只折叠 ASCII，字节长度不变 → low 上的下标可以直接用在 src 上
    let low = src.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}");
    let mut out = String::with_capacity(src.len());
    let mut pos = 0usize;
    while pos < src.len() {
        let Some(start) = find_tag(&low, pos, &open) else {
            break;
        };
        match find_tag(&low, start, &close) {
            Some(cstart) => match low[cstart..].find('>') {
                Some(e) => {
                    out.push_str(&src[pos..start]);
                    pos = cstart + e + 1;
                }
                None => {
                    out.push_str(&src[pos..start]);
                    pos = src.len();
                }
            },
            None => {
                // 没闭合：只吃掉这个开标签（head），或者把剩下的全丢（script）
                out.push_str(&src[pos..start]);
                pos = if drop_rest {
                    src.len()
                } else {
                    start + open.len()
                };
            }
        }
    }
    out.push_str(&src[pos.min(src.len())..]);
    out
}

/// 在 `low[from..]` 里找 `pat`，并要求后面跟的是标签名字符边界。
fn find_tag(low: &str, from: usize, pat: &str) -> Option<usize> {
    let mut pos = from;
    while pos < low.len() {
        let k = low[pos..].find(pat)?;
        let at = pos + k;
        let after = at + pat.len();
        let boundary = match low.as_bytes().get(after) {
            None => true,
            Some(&b) => matches!(b, b'>' | b'/' | b' ' | b'\t' | b'\r' | b'\n'),
        };
        if boundary {
            return Some(at);
        }
        pos = after;
    }
    None
}

fn strip_comments(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut pos = 0usize;
    while pos < src.len() {
        let Some(k) = src[pos..].find("<!--") else {
            break;
        };
        out.push_str(&src[pos..pos + k]);
        match src[pos + k..].find("-->") {
            Some(e) => pos = pos + k + e + 3,
            None => {
                pos = src.len();
            }
        }
    }
    out.push_str(&src[pos.min(src.len())..]);
    out
}

// ---------------------------------------------------------------------------
// RTF
// ---------------------------------------------------------------------------

fn parse_rtf(bytes: &[u8]) -> Vec<Block> {
    // RTF 的正文可以是任意 iconv 编码（`\'hh`），但结构字符全是 ASCII，
    // 所以直接按字节扫，不做整体解码。
    blocks_from_rtf(bytes)
}

fn blocks_from_rtf(bytes: &[u8]) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut cp: u16 = 1252; // 默认 cp1252
    let mut uc: usize = 1; // `\ucN`：每个 `\uN` 后面跟几个替代字符
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'{' => {
                // 字体表/颜色表/样式表/文档信息/`{\*\...}` 这些组整段丢
                if let Some(after) = skip_dest_group(bytes, i) {
                    i = after;
                } else {
                    i += 1;
                }
            }
            b'}' => i += 1,
            b'\\' => i = rtf_control(bytes, i, &mut buf, &mut out, &mut cp, &mut uc),
            // RTF 源文件里的裸换行只是排版，不是段落
            b'\r' | b'\n' => i += 1,
            _ => {
                buf.push(bytes[i]);
                i += 1;
            }
        }
    }
    flush_rtf(&mut out, &mut buf, cp);
    out
}

fn flush_rtf(out: &mut Vec<Block>, buf: &mut Vec<u8>, cp: u16) {
    if buf.is_empty() {
        return;
    }
    let raw = std::mem::take(buf);
    // `\uN` 的字符是以 UTF-8 塞进缓冲的，纯 `\'hh` 则是原始字节：先按 UTF-8
    // 试，失败再按 codepage 解。两种混在一段里（罕见）会退化成 codepage。
    let text = match std::str::from_utf8(&raw) {
        Ok(s) => s.to_string(),
        Err(_) => decode_codepage(&raw, cp),
    };
    let t = text.trim();
    if !t.is_empty() {
        out.push(Block::body(t));
    }
}

/// `bytes[i] == '\\'`：解一个控制字/转义，返回下一个待处理的字节下标。
fn rtf_control(
    bytes: &[u8],
    i: usize,
    buf: &mut Vec<u8>,
    out: &mut Vec<Block>,
    cp: &mut u16,
    uc: &mut usize,
) -> usize {
    let mut j = i + 1;
    if j >= bytes.len() {
        return j;
    }
    match bytes[j] {
        b'\'' => {
            // `\'hh`：一个原始字节，连续几个要在 flush 时一起解（多字节编码）
            if j + 2 < bytes.len() {
                if let (Some(hi), Some(lo)) = (hex_val(bytes[j + 1]), hex_val(bytes[j + 2])) {
                    buf.push(hi * 16 + lo);
                    return j + 3;
                }
            }
            return (j + 3).min(bytes.len());
        }
        b'\\' | b'{' | b'}' => {
            buf.push(bytes[j]);
            return j + 1;
        }
        b'~' => {
            buf.push(b' ');
            return j + 1;
        }
        // `\-` 可选连字符、`\_` 不换行连字符：没有可见文字
        b'-' | b'_' => return j + 1,
        // `\*` 标记「这是未知目的组」，文字本身不该出现
        b'*' => return j + 1,
        // `\<newline>` 等价 `\par`
        b'\r' | b'\n' => {
            flush_rtf(out, buf, *cp);
            return j + 1;
        }
        _ => {}
    }
    let start = j;
    while j < bytes.len() && bytes[j].is_ascii_alphabetic() {
        j += 1;
    }
    let word = std::str::from_utf8(&bytes[start..j]).unwrap_or("");
    let pstart = j;
    if j < bytes.len() && (bytes[j] == b'-' || bytes[j].is_ascii_digit()) {
        j += 1;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
    }
    let param = std::str::from_utf8(&bytes[pstart..j])
        .ok()
        .and_then(|s| s.parse::<i32>().ok());
    // 控制字后面的一个空格是分隔符，不是文字
    if j < bytes.len() && bytes[j] == b' ' {
        j += 1;
    }
    match word {
        "par" | "line" | "sect" | "page" => flush_rtf(out, buf, *cp),
        "tab" => buf.push(b' '),
        "u" => {
            let n = param.unwrap_or(0);
            // 负数表示大于 0x7FFF 的码位（RTF 用有符号 16 位存）
            let code = if n < 0 { n + 65536 } else { n } as u32;
            if let Some(c) = char::from_u32(code) {
                let mut tmp = [0u8; 4];
                buf.extend_from_slice(c.encode_utf8(&mut tmp).as_bytes());
            }
            return skip_fallbacks(bytes, j, *uc);
        }
        "ansicpg" => {
            if let Some(n) = param {
                if (0..=65535).contains(&n) {
                    *cp = n as u16;
                }
            }
        }
        "uc" => {
            if let Some(n) = param {
                if (0..=16).contains(&n) {
                    *uc = n as usize;
                }
            }
        }
        // 字体名/加粗/斜体/字号/颜色… 全部丢掉，取文字用不上
        _ => {}
    }
    j
}

/// `\uN` 后面的 ANSI 替代字符要跳过（`\ucN` 说跳几个）。
/// 替代字符可能是 `\'hh`、一个转义、一个控制字或一个普通字符。
fn skip_fallbacks(bytes: &[u8], mut j: usize, n: usize) -> usize {
    for _ in 0..n {
        if j >= bytes.len() {
            break;
        }
        match bytes[j] {
            // 组边界不能吃，否则后面的组结构会错位
            b'{' | b'}' => break,
            b'\\' => {
                if j + 2 < bytes.len() && bytes[j + 1] == b'\'' {
                    j += 4;
                } else if j + 1 < bytes.len() && matches!(bytes[j + 1], b'\\' | b'{' | b'}') {
                    j += 2;
                } else {
                    j += 2; // 控制字：最少吃掉反斜杠 + 一个字母
                }
            }
            _ => j += utf8_len(bytes[j]),
        }
    }
    j.min(bytes.len())
}

fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// `bytes[i] == '{'`：这一组是要整段丢掉的（字体表/颜色表/样式/文档信息/`\*`）时
/// 返回组之后的下标，否则 `None`（当普通组处理）。
fn skip_dest_group(bytes: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < bytes.len() && matches!(bytes[j], b' ' | b'\r' | b'\n' | b'\t') {
        j += 1;
    }
    if j >= bytes.len() || bytes[j] != b'\\' {
        return None;
    }
    j += 1;
    // `{\*\...}`：未知目的组，整段不要
    if j < bytes.len() && bytes[j] == b'*' {
        return Some(end_of_group(bytes, i));
    }
    let start = j;
    while j < bytes.len() && bytes[j].is_ascii_alphabetic() {
        j += 1;
    }
    if j == start {
        return None;
    }
    let word = std::str::from_utf8(&bytes[start..j]).unwrap_or("");
    let drop = matches!(
        word,
        "fonttbl" | "colortbl" | "stylesheet" | "info" | "pict" | "object" | "header"
            | "headerl" | "headerr" | "headerf" | "footer" | "footerl" | "footerr" | "footerf"
            | "listtable" | "listoverridetable" | "rsidtbl" | "generator" | "filetbl"
    );
    if drop {
        Some(end_of_group(bytes, i))
    } else {
        None
    }
}

/// `bytes[open] == '{'`，返回配对 `}` 之后的下标（`\{`/`\}`/`\\` 不算结构）。
fn end_of_group(bytes: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    let mut j = open;
    while j < bytes.len() {
        match bytes[j] {
            b'\\' => {
                j += 2;
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return j + 1;
                }
            }
            _ => {}
        }
        j += 1;
    }
    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 临时文件：测试自己造数据，不依赖仓库里任何外部文件。
    struct TmpFile(String);

    impl TmpFile {
        fn new(bytes: &[u8]) -> TmpFile {
            static N: AtomicUsize = AtomicUsize::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!(
                "rmdev-text-{}-{}-{}.txt",
                std::process::id(),
                std::thread::current().name().unwrap_or("t"),
                n
            ));
            std::fs::write(&p, bytes).expect("写临时文件");
            TmpFile(p.to_string_lossy().into_owned())
        }

        fn open(&self, fmt: Format) -> TextDoc {
            TextDoc::open(&self.0, fmt).expect("open")
        }
    }

    impl Drop for TmpFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn utf16le(s: &str) -> Vec<u8> {
        let mut out = vec![0xff, 0xfe];
        for u in s.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out
    }

    fn body_texts(blocks: &[Block]) -> Vec<String> {
        blocks
            .iter()
            .filter(|b| !b.is_heading())
            .map(|b| b.text.clone())
            .collect()
    }

    #[test]
    fn txt_utf8_chinese_opens_with_pages_and_text() {
        let src = "第一章 起点\n\n这是第一段中文正文，用来验证 UTF-8 被正确识别。\n\n第二段拉丁文 hello world.\n";
        let f = TmpFile::new(src.as_bytes());
        let d = f.open(Format::Txt);
        assert_eq!(d.format(), Format::Txt);
        assert!(d.page_count() >= 1);
        let t = d.inner.page_text(0).expect("page 0");
        assert!(t.contains("第一章"), "page text: {t}");
        assert!(t.contains("这是第一段中文正文"), "page text: {t}");
    }

    /// 无空行、每段一行、行长够（中文小说最常见的存法）：每行该各成一段，
    /// 而不是被 `blocks_from_plain` 并成一大段。
    #[test]
    fn txt_without_blank_lines_keeps_paragraph_breaks() {
        let para = "这一行足够长，长到不可能被当成排版折行处理，所以它应该单独成为一个段落。";
        let src: String = (0..8).map(|i| format!("{para}第{i}段。\n")).collect();
        assert!(!src.contains("\n\n"), "样例必须没有空行");
        assert!(lines_are_paragraphs(&src));

        let f = TmpFile::new(src.as_bytes());
        let d = f.open(Format::Txt);
        // 8 段各占整行，页面上能看到段间距（相邻两段的基线差 > 单倍行高）
        let b = d.inner.book();
        assert_eq!(b.chapters.len(), 1);
        assert_eq!(b.chapters[0].blocks.len(), 8, "应当是 8 段而不是 1 段");
    }

    /// 真正的排版折行（短行 + 行尾无标点）不能被拆成一段一行。
    #[test]
    fn hard_wrapped_txt_is_still_joined() {
        let src: String = (0..30)
            .map(|i| format!("the quick brown fox {i}\n"))
            .collect();
        assert!(!lines_are_paragraphs(&src));
        let f = TmpFile::new(src.as_bytes());
        let d = f.open(Format::Txt);
        let b = d.inner.book();
        assert_eq!(b.chapters[0].blocks.len(), 1, "折行应当接回一段");
    }

    #[test]
    fn txt_utf16le_bom_decodes() {
        let f = TmpFile::new(&utf16le("Hello 中文\n\n第二段文字。\n"));
        let d = f.open(Format::Txt);
        let t = d.inner.page_text(0).expect("page 0");
        assert!(t.contains("中文"), "page text: {t}");
        assert!(t.contains("第二段文字"), "page text: {t}");
        assert!(t.contains("Hello"), "page text: {t}");
    }

    #[test]
    fn txt_utf16be_bom_decodes() {
        let mut bytes = vec![0xfe, 0xff];
        for u in "第一章\n\n正文。".encode_utf16() {
            bytes.extend_from_slice(&u.to_be_bytes());
        }
        let f = TmpFile::new(&bytes);
        let d = f.open(Format::Txt);
        let t = d.inner.page_text(0).expect("page 0");
        assert!(t.contains("正文"), "page text: {t}");
    }

    /// GBK 字节流：「中文测试」的 GBK 是 d6 d0 ce c4 b2 e2 ca d4（硬编码，不依赖
    /// 生成时的工具）。解码要走 iconv，**环境里没有 iconv/GB18030 时按用户要求
    /// 跳过断言**（这里用运行时探测代替 `#[ignore]`，因为能不能用只有在运行时
    /// 才知道，而 `#[ignore]` 是静态的）。
    #[test]
    fn txt_gbk_bytes_decode() {
        if !iconv_available() {
            eprintln!("skipped: no iconv GB18030 support in this environment");
            return;
        }
        let mut bytes = b"\xd6\xd0\xce\xc4\xb2\xe2\xca\xd4\n\n".to_vec();
        // 「第二段」= b5 da b6 fe b6 ce
        bytes.extend_from_slice(b"\xb5\xda\xb6\xfe\xb6\xce\n");
        let f = TmpFile::new(&bytes);
        let d = f.open(Format::Txt);
        let t = d.inner.page_text(0).expect("page 0");
        assert!(t.contains("中文测试"), "page text: {t}");
        assert!(t.contains("第二段"), "page text: {t}");
        assert!(!t.contains("锟"), "不该是 GBK 误解的乱码: {t}");
    }

    #[test]
    fn txt_latin1_fallback_never_panics() {
        // 0x81 在 GB18030 里是非法的单字节，iconv //IGNORE 会丢；至少要能打开
        let bytes: Vec<u8> = (0x80u8..=0xff).collect();
        let f = TmpFile::new(&bytes);
        let d = f.open(Format::Txt);
        assert!(d.page_count() >= 1);
    }

    #[test]
    fn txt_chapter_headings_make_outline() {
        let src = "第一章 开端\n\n正文一，内容。\n\n第二章 终局\n\n正文二，内容。\n\nChapter 3\n\n正文三，内容。\n";
        let f = TmpFile::new(src.as_bytes());
        let d = f.open(Format::Txt);
        let toc = d.outline();
        assert_eq!(toc.len(), 3, "toc: {toc:?}");
        assert_eq!(toc[0].title, "第一章 开端");
        assert_eq!(toc[0].depth, 1);
        assert_eq!(toc[2].title, "Chapter 3");
        assert!(toc[0].page <= toc[2].page, "目录页号应单调不减: {toc:?}");
    }

    #[test]
    fn txt_hard_wraps_are_joined() {
        // 20 行、每行 12 字、行尾没标点 → 判成硬折行，应当接成一段
        let line = "今天天气很好我们出门散步看花";
        let src = vec![line; 20].join("\n");
        assert_eq!(src.lines().count(), 20);
        let unfolded = unfold_hard_wraps(&src);
        assert!(
            unfolded.lines().filter(|l| !l.is_empty()).count() < 20,
            "硬折行没接线: {unfolded}"
        );
        // 中文之间不补空格：整段应当是连续的一串
        assert!(unfolded.contains(&format!("{line}{line}")), "{unfolded}");
    }

    #[test]
    fn txt_normal_file_is_not_unfolded() {
        // 行尾都有标点 → 不判硬折行，段落保持原样
        let src = format!("{}\n\n{}", "第一段文字。".repeat(10), "第二段文字。".repeat(10));
        assert_eq!(unfold_hard_wraps(&src), src);
    }

    #[test]
    fn md_heading_makes_toc_entry_and_bold_is_stripped() {
        let src = "# 标题\n\n正文 **加粗** 和 *斜体*，还有 `代码`。\n";
        let f = TmpFile::new(src.as_bytes());
        let d = f.open(Format::Markdown);
        let toc = d.outline();
        assert_eq!(toc.len(), 1, "toc: {toc:?}");
        assert_eq!(toc[0].title, "标题");
        let t = d.inner.page_text(0).expect("page 0");
        assert!(t.contains("加粗"), "page text: {t}");
        assert!(t.contains("斜体"), "page text: {t}");
        assert!(!t.contains('*'), "不该留下星号标记: {t}");
        assert!(!t.contains('`'), "不该留下反引号: {t}");
    }

    #[test]
    fn md_heading_levels_map_to_three() {
        let blocks = blocks_from_md("# a\n## b\n### c\n#### d\n###### f\n");
        let levels: Vec<Kind> = blocks.iter().map(|b| b.kind).collect();
        assert_eq!(levels, vec![Kind::H1, Kind::H2, Kind::H3, Kind::H3, Kind::H3]);
    }

    #[test]
    fn md_lists_links_images_code_and_tables() {
        let src = "\
- 无序列项
* 星号列项
1. 有序列项

> 引用的文字

[链接文字](https://example.com) 和 ![图片](x.png) 结尾

```code
let a = 1;
let b = 2;
```

| 甲 | 乙 |
|---|:--:|
| 1 | 2 |
";
        let blocks = blocks_from_md(src);
        let texts = body_texts(&blocks);
        let joined = texts.join("\n");
        assert!(texts.iter().any(|t| t == "• 无序列项"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "• 星号列项"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "1. 有序列项"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "引用的文字"), "{texts:?}");
        assert!(joined.contains("链接文字"), "{texts:?}");
        assert!(!joined.contains("example.com"), "链接地址该丢掉: {texts:?}");
        assert!(!joined.contains("图片"), "图片该整段丢掉: {texts:?}");
        // 代码块里的两行不合并
        assert!(texts.iter().any(|t| t == "let a = 1;"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "let b = 2;"), "{texts:?}");
        // 表格：一行一段，单元格用 " | " 连
        assert!(texts.iter().any(|t| t == "甲 | 乙"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "1 | 2"), "{texts:?}");
        assert!(!joined.contains("---"), "分隔行该丢掉: {texts:?}");
    }

    #[test]
    fn md_horizontal_rule_is_dropped() {
        let blocks = blocks_from_md("上\n\n---\n\n下\n");
        assert_eq!(body_texts(&blocks), vec!["上".to_string(), "下".to_string()]);
    }

    #[test]
    fn html_paragraphs_entities_and_script() {
        let src = "<html><head><title>书名</title><style>p{color:red}</style></head>\
<body><p>a</p><p>b&amp;c</p><div>第二段 &#20013;&#x6587; &mdash; ok</div>\
<script>var x = 1;</script></body></html>";
        let blocks = blocks_from_html(src);
        let texts = body_texts(&blocks);
        assert_eq!(texts[0], "a", "{texts:?}");
        assert_eq!(texts[1], "b&c", "{texts:?}");
        let joined = texts.join("\n");
        assert!(joined.contains("中文"), "数字实体没解: {texts:?}");
        assert!(joined.contains('—'), "&mdash; 没解: {texts:?}");
        assert!(!joined.contains("var x"), "script 内容不该出现: {texts:?}");
        assert!(!joined.contains("color"), "style 内容不该出现: {texts:?}");
        assert!(!joined.contains("书名"), "head/title 不该进正文: {texts:?}");
        assert!(!joined.contains('<'), "标签没剥干净: {texts:?}");
    }

    /// `<header>` 不能被当成 `<head>`：否则整篇正文会被当 head 吞掉。
    #[test]
    fn html_header_tag_does_not_eat_body() {
        let src = "<html><body><header>导航</header><p>正文第一段</p>\
<header>另一块</header><p>正文第二段</p></body></html>";
        let texts = body_texts(&blocks_from_html(src));
        assert!(texts.iter().any(|t| t == "正文第一段"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "正文第二段"), "{texts:?}");
    }

    /// 未闭合的 `<head>` 只吃掉标签本身，后面的正文必须还在。
    #[test]
    fn html_unclosed_head_keeps_body() {
        let texts = body_texts(&blocks_from_html("<head><p>甲</p><p>乙</p>"));
        assert!(texts.iter().any(|t| t == "甲"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "乙"), "{texts:?}");
    }

    #[test]
    fn html_headings_and_br() {
        let blocks = blocks_from_html("<h1>一级</h1><p>甲<br>乙</p><h4>四级</h4>");
        assert_eq!(blocks[0].kind, Kind::H1);
        assert_eq!(blocks[0].text, "一级");
        assert_eq!(body_texts(&blocks), vec!["甲", "乙"]);
        assert_eq!(blocks[3].kind, Kind::H3, "4 级以上按 H3");
    }

    #[test]
    fn html_meta_charset_gbk() {
        if !iconv_available() {
            eprintln!("skipped: no iconv GB18030 support in this environment");
            return;
        }
        let mut bytes = b"<html><head><meta charset=\"gbk\"></head><body><p>".to_vec();
        bytes.extend_from_slice(b"\xd6\xd0\xce\xc4"); // 中文
        bytes.extend_from_slice(b"</p></body></html>");
        let f = TmpFile::new(&bytes);
        let d = f.open(Format::Html);
        let t = d.inner.page_text(0).expect("page 0");
        assert!(t.contains("中文"), "page text: {t}");
    }

    #[test]
    fn html_without_charset_uses_probe() {
        let f = TmpFile::new("<p>纯 UTF-8 中文</p>".as_bytes());
        let d = f.open(Format::Html);
        let t = d.inner.page_text(0).expect("page 0");
        assert!(t.contains("纯 UTF-8 中文"), "page text: {t}");
    }

    #[test]
    fn rtf_two_paragraphs() {
        let blocks = blocks_from_rtf(b"{\\rtf1\\ansi Hello\\par World}");
        assert_eq!(body_texts(&blocks), vec!["Hello".to_string(), "World".to_string()]);
        let f = TmpFile::new(b"{\\rtf1\\ansi Hello\\par World}");
        let d = f.open(Format::Rtf);
        let t = d.inner.page_text(0).expect("page 0");
        assert!(t.contains("Hello") && t.contains("World"), "{t}");
    }

    #[test]
    fn rtf_skips_font_table_and_title_info() {
        let src = b"{\\rtf1\\ansi\\deff0{\\fonttbl{\\f0 Arial;}}{\\info{\\title X}}\\fs24 Real text\\par}";
        let blocks = blocks_from_rtf(src);
        let texts = body_texts(&blocks);
        assert_eq!(texts, vec!["Real text".to_string()], "{texts:?}");
        assert!(!texts.join("").contains("Arial"));
        assert!(!texts.join("").contains('X'));
    }

    #[test]
    fn rtf_unicode_and_escapes() {
        // \u20013? = 中，\u25991? = 文；`?` 是替代字符，要跳过
        let blocks = blocks_from_rtf(b"{\\rtf1\\ansi \\u20013?\\u25991?\\tab ok\\par next}");
        assert_eq!(body_texts(&blocks), vec!["中文 ok".to_string(), "next".to_string()]);
    }

    #[test]
    fn rtf_cp1252_escape() {
        let blocks = blocks_from_rtf(b"{\\rtf1\\ansi caf\\'e9 au lait}");
        assert_eq!(body_texts(&blocks), vec!["café au lait".to_string()]);
        // \{ \} \\ 转义还原成单个字符
        let blocks = blocks_from_rtf(b"{\\rtf1 a\\{b\\}c\\\\d}");
        assert_eq!(body_texts(&blocks), vec!["a{b}c\\d".to_string()]);
    }

    #[test]
    fn rtf_gbk_codepage() {
        if !iconv_available() {
            eprintln!("skipped: no iconv GB18030 support in this environment");
            return;
        }
        // \ansicpg936 + 「中文」的 GBK 字节
        let blocks = blocks_from_rtf(b"{\\rtf1\\ansi\\ansicpg936 \\'d6\\'d0\\'ce\\'c4\\par}");
        assert_eq!(body_texts(&blocks), vec!["中文".to_string()]);
    }

    #[test]
    fn empty_file_is_not_a_panic() {
        for fmt in [Format::Txt, Format::Markdown, Format::Html, Format::Rtf] {
            let f = TmpFile::new(b"");
            let d = f.open(fmt);
            let n = d.page_count();
            assert!(n <= 1, "{fmt:?} 空文件页数: {n}");
            if n == 1 {
                // 一页空页也算合格，但不能是半张白纸之外的东西
                let g = d.render(0, 200, 280).expect("空页也要能渲染");
                assert_eq!(g.len(), 200 * 280);
            } else {
                assert!(d.render(0, 200, 280).is_err());
            }
        }
    }

    #[test]
    fn render_out_of_range_is_err() {
        let f = TmpFile::new("只有一页的文字。".as_bytes());
        let d = f.open(Format::Txt);
        assert!(d.render(999, 400, 560).is_err());
        assert!(d.render(0, 400, 560).is_ok());
    }

    #[test]
    fn render_is_deterministic() {
        let f = TmpFile::new("确定性测试：同一页渲染两次必须字节相同。".as_bytes());
        let d = f.open(Format::Txt);
        let a = d.render(0, 400, 560).expect("render");
        let b = d.render(0, 400, 560).expect("render 两次");
        assert_eq!(a, b);
        assert_eq!(a.len(), 400 * 560);
        // aspect 是固定的 1000/1400
        assert!((d.aspect(0) - 1000.0 / 1400.0).abs() < 1e-6);
    }

    #[test]
    fn layout_can_be_changed() {
        let f = TmpFile::new("改字号会让页数变化。".as_bytes());
        let mut d = f.open(Format::Txt);
        let before = d.current_layout();
        let cfg = crate::doc::LayoutCfg {
            font_size: 36.0,
            line_spacing: 1.85,
            margin: 120,
        };
        assert!(d.apply_layout(cfg));
        let after = d.current_layout();
        assert_eq!(after.font_size, 36.0);
        assert_ne!(after, before);
        // 参数没变时返回 false（不用重排）
        assert!(!d.apply_layout(cfg));
    }

    /// `find` 是纯委托，这里只验证接线通。
    ///
    /// **注意测试数据是纯 ASCII**：`reflow::search` 有个既有 bug——它把
    /// `normalize()`（去掉标点/空白后的串）里的字节偏移当成原文偏移去切
    /// snippet，遇到中文会切在 UTF-8 字符中间直接 panic（release 下
    /// `panic = "abort"`）。那是 reflow.rs 的问题，本文件不许改，见交付报告
    /// 的「已知限制」；修好后这条注释可以删。
    #[test]
    fn find_and_chapter_title_work() {
        let filler: String =
            "lorem ipsum dolor sit amet consectetur adipiscing elit sed do. ".repeat(120);
        let src = format!(
            "Chapter 1\n\nhere is a needle inside ascii text.\n\n{filler}\n\nChapter 2\n\nanother paragraph.\n"
        );
        let f = TmpFile::new(src.as_bytes());
        let d = f.open(Format::Txt);
        assert!(d.page_count() >= 2, "页数: {}", d.page_count());
        let hits = d.find("needle", 10);
        assert_eq!(hits.len(), 1, "hits: {hits:?}");
        assert_eq!(hits[0].0, 0);
        assert_eq!(d.chapter_title(0).as_deref(), Some("Chapter 1"));
    }

    #[test]
    fn unknown_format_is_rejected() {
        let f = TmpFile::new(b"x");
        assert!(TextDoc::open(&f.0, Format::Pdf).is_err());
        assert!(TextDoc::open("/nonexistent/rmdev-nope.txt", Format::Txt).is_err());
    }

    #[test]
    fn chapter_heading_detector_is_not_greedy() {
        // 正文句子不该被当成标题
        assert!(!is_chapter_heading("第二天他来到车站，天已经黑了。"));
        assert!(!is_chapter_heading("前言不搭后语"));
        assert!(!is_chapter_heading("Part of the problem is here"));
        assert!(is_chapter_heading("第一章 开端"));
        assert!(is_chapter_heading("第十二回"));
        assert!(is_chapter_heading("楔子"));
        assert!(is_chapter_heading("Chapter 12: The End"));
        assert!(is_chapter_heading("Chapter IV"));
        assert!(is_chapter_heading("Prologue"));
        assert!(is_chapter_heading("THE END"));
        assert!(!is_chapter_heading("这是很长的一句正文，超过了上限所以不该当标题"));
    }

    #[test]
    fn oversized_file_is_rejected() {
        // 用稀疏文件造 70 MB，不真占磁盘也不真读进来：open 必须在读之前就拒绝
        let f = TmpFile::new(b"");
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&f.0)
            .expect("open 临时文件");
        file.set_len(70 << 20).expect("sparse");
        drop(file);
        match TextDoc::open(&f.0, Format::Txt) {
            Ok(_) => panic!("70 MB 的 txt 该被拒绝"),
            Err(e) => assert!(e.contains("exceeds"), "{e}"),
        }
    }

    #[test]
    fn markdown_local_image_becomes_image_block() {
        // 目录结构：<tmp>/pic.png  +  <tmp>/sub/（模拟正文在子目录里）
        let dir = std::env::temp_dir().join(format!(
            "rmdev-md-img-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("t")
        ));
        let sub = dir.join("sub");
        std::fs::create_dir_all(&sub).expect("建临时目录");
        // sub/inner.png 走纯相对路径，外面的 pic.png 走 `../`
        std::fs::write(sub.join("inner.png"), reflow::test_png_1x1()).expect("写 png");
        std::fs::write(dir.join("pic.png"), reflow::test_png_1x1()).expect("写 png");

        let blocks = vec![
            Block::body("正文第一段"),
            Block::body("![](inner.png)"),
            Block::body("![图](../pic.png \"标题\")"),
            Block::body("![](missing.png)"),
            // 标题里的图片引用不处理（标题本来就只有一行）
            Block::heading(1, "![](pic.png)"),
        ];
        let out = resolve_local_images(blocks, &sub);
        assert_eq!(out.len(), 5);
        assert_eq!(out[0].kind, Kind::Body);
        assert_eq!(out[1].kind, Kind::Image);
        assert!(out[1].img.is_some(), "图片要真解码出来");
        assert_eq!(out[1].text, "inner.png");
        assert_eq!(out[2].kind, Kind::Image, "`../` 要能往上找一级");
        assert_eq!(out[2].text, "../pic.png");
        assert_eq!(out[3].kind, Kind::Body, "读不到就留原文，不能 panic");
        assert_eq!(out[3].text, "![](missing.png)");
        assert_eq!(out[4].kind, Kind::H1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn html_img_becomes_image_block() {
        let dir = std::env::temp_dir().join(format!(
            "rmdev-html-img-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("t")
        ));
        std::fs::create_dir_all(&dir).expect("建临时目录");
        std::fs::write(dir.join("p.png"), reflow::test_png_1x1()).expect("写 png");

        let html = "<html><body><p>图前</p><img alt=\"a\" SRC=\"p.png\"><p>图后</p>\
                    <img src='missing.png'><img data-src=\"nope.png\"></body></html>";
        let blocks = blocks_from_html(html);
        let imgs: Vec<&Block> = blocks.iter().filter(|b| b.kind == Kind::Image).collect();
        assert_eq!(imgs.len(), 2, "两个真 img；data-src 不算");
        assert_eq!(imgs[0].text, "p.png", "属性名大小写不敏感");
        assert_eq!(imgs[1].text, "missing.png");

        let out = resolve_local_images(blocks, &dir);
        assert_eq!(out.iter().filter(|b| b.kind == Kind::Image).count(), 1);
        assert!(out
            .iter()
            .any(|b| b.kind == Kind::Image && b.text == "p.png" && b.img.is_some()));
        // 读不到的图不占位，也不留路径文字
        assert!(!out.iter().any(|b| b.text.contains("missing.png")));
        assert!(out.iter().any(|b| b.text.contains("图后")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn md_image_path_parses_markdown_forms() {
        assert_eq!(md_image_path("![](a.png)"), Some("a.png"));
        assert_eq!(md_image_path("  ![图](img/b.jpg)  "), Some("img/b.jpg"));
        assert_eq!(md_image_path("![x](p.png \"标题\")"), Some("p.png"));
        assert_eq!(md_image_path("![](  )"), None);
        assert_eq!(md_image_path("普通段落"), None);
        assert_eq!(md_image_path("![x]没有括号"), None);
    }
}
