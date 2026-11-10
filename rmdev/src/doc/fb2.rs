//! FictionBook 2（`.fb2`）后端：一个 XML 文件 → [`Book`] → [`reflow::ReflowDoc`]。
//!
//! # 为什么是「扫标签流」而不是建树
//!
//! FB2 是单文件格式，正文全在 `<body>` 里，`<section>` 可以嵌很多层；一本书
//! 少则几百 KB，多则几十 MB，其中九成体积是 `<binary>` 里的 base64 插图。
//! 建 DOM 既没必要也吃内存：这里顺着字节扫一遍，遇到开始标签就压栈计数、
//! 遇到文本就往当前段落里塞，`<binary>` / `<description>` / `<body name="notes">`
//! 整块跳过（连 base64 都不解码）。内存里只留最终的 [`Book`]。
//!
//! # 章节怎么切
//!
//! * `<body>` 的直接子 `<section>` → 一章，它的 `<title>` 是 H1；
//! * 再往里一层 `<section><title>` → H2，第三层 → H3（封顶，更深的也按 H3）；
//! * `<body>` 自己的 `<title>`（部/卷名）、section 之外散落的段落，归到**第一章
//!   开头**，不单独占一章——否则每本书第一页都是一行卷名，看着像 bug。
//!
//! 目录只从 `<section><title>` 出：`path` 是所属章节下标的字符串，`anchor` 留空，
//! 页号由 [`reflow::ReflowDoc::outline`] 按「章节首页」算，和章节页号天然一致。
//!
//! # 编码
//!
//! 俄文原版 FB2 十有八九声明 `windows-1251`，还有 `koi8-r` / `iso-8859-5` /
//! `ibm866` / `cp1252`；设备上没有 locale、也不能拉 `encoding_rs`（多一个 crate
//! 就多一份交叉编译和几 MB 静态表），所以按 XML 声明用手写码表解。UTF-16 按
//! BOM 解。认不出来的编码按 UTF-8 lossy 兜底——宁可出来几个乱码字，也不能 panic。
//!
//! # 不支持的
//!
//! `.fb2.zip` 直接报错（gzip 的 `.fb2.gz` 是支持的）；插图不渲染，`<binary>`
//! 里的图片一律丢掉。

use std::collections::HashMap;
use std::fs;

use crate::doc::reflow::{self, Block, Book, Chapter, Cfg, TocEntry};

/// 文件大小上限。正常 FB2 远小于这个数（上限之外基本都是坏文件或根本不是书），
/// 超了直接报错，比解到一半 OOM 强。
const MAX_BYTES: usize = 64 * 1024 * 1024;

/// 单个标签允许扫过的最大字节数。属性值里可以合法地出现 `>`，扫这么久还没
/// 遇到结尾就说明文件坏了，别把剩下的文件当成一个标签吞掉。
const MAX_TAG: usize = 100_000;

// ---------------------------------------------------------------------------
// 对外类型
// ---------------------------------------------------------------------------

/// 一本 FB2。分页/渲染/目录/搜索全在 [`reflow::ReflowDoc`] 里，这里只多存
/// 书名和作者（库视图要用，`reflow::Book` 没有这两个字段）。
pub struct Fb2Doc {
    inner: reflow::ReflowDoc,
    title: Option<String>,
    author: Option<String>,
}

impl Fb2Doc {
    /// 打开一个 `.fb2` / `.fb2.gz`。打不开一律 `Err`（英文短句，UI 直接当
    /// "打不开"处理）。
    pub fn open(path: &str) -> Result<Fb2Doc, String> {
        let raw = load(path)?;
        Ok(Fb2Doc::build(parse(&raw)?))
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

    /// `<description>/<title-info>/<book-title>`。没有就是 `None`。
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// 第一个作者的「名 姓」。没有就是 `None`。
    pub fn author(&self) -> Option<&str> {
        self.author.as_deref()
    }

    fn build(p: Parsed) -> Fb2Doc {
        Fb2Doc {
            inner: reflow::ReflowDoc::new(p.book, Cfg::default()),
            title: p.title,
            author: p.author,
        }
    }
}

/// 解析产物：正文 + 库视图要的两条元数据。
struct Parsed {
    book: Book,
    title: Option<String>,
    author: Option<String>,
}

// ---------------------------------------------------------------------------
// 读文件 / 解压
// ---------------------------------------------------------------------------

fn load(path: &str) -> Result<Vec<u8>, String> {
    let meta = fs::metadata(path).map_err(|e| format!("cannot open {path}: {e}"))?;
    if meta.len() > MAX_BYTES as u64 {
        return Err(format!(
            "FB2 file is too large: {} bytes (limit {} MB)",
            meta.len(),
            MAX_BYTES / (1024 * 1024)
        ));
    }
    let raw = fs::read(path).map_err(|e| format!("cannot read {path}: {e}"))?;
    unpack(raw)
}

/// gzip 就地解，其余原样返回。
///
/// 解压后的字数也要卡上限：压缩比随便就上百，不夹的话一个 1 MB 的 zip bomb
/// 能把设备内存吃光。`miniz_oxide` 是纯 Rust，设备上已经在用（EPUB 的 zip）。
fn unpack(raw: Vec<u8>) -> Result<Vec<u8>, String> {
    if raw.starts_with(&[0x1f, 0x8b]) {
        let body = gzip_payload(&raw).ok_or_else(|| "corrupt .fb2.gz header".to_string())?;
        return miniz_oxide::inflate::decompress_to_vec_with_limit(body, MAX_BYTES)
            .map_err(|_| "cannot inflate .fb2.gz (corrupt, or larger than 64 MB)".to_string());
    }
    if raw.starts_with(b"PK\x03\x04") {
        return Err(".fb2.zip is not supported; unzip it first".into());
    }
    Ok(raw)
}

/// 跳过 gzip 头，返回里面的裸 deflate 数据。
///
/// `miniz_oxide` 的 `decompress_to_vec*` 只认裸 deflate / zlib，gzip 那 10 字节
/// 头和可选的「文件名/注释/额外字段」段得自己跳（`gzip` 命令默认就带文件名）。
/// 尾部 8 字节 CRC32+ISIZE 不用管：inflate 遇到结束块就停了。
fn gzip_payload(raw: &[u8]) -> Option<&[u8]> {
    // ID1 ID2 CM=8（deflate）。CM 别的值（比如老的 compress）都不支持。
    if raw.len() < 10 || raw[0] != 0x1f || raw[1] != 0x8b || raw[2] != 8 {
        return None;
    }
    let flags = raw[3];
    let mut i = 10;
    if flags & 0x04 != 0 {
        // FEXTRA：2 字节小端长度 + 内容
        let xlen = u16::from_le_bytes([*raw.get(i)?, *raw.get(i + 1)?]) as usize;
        i = i.checked_add(2 + xlen)?;
    }
    // FNAME / FCOMMENT：以 NUL 结尾的字符串
    for mask in [0x08u8, 0x10] {
        if flags & mask != 0 {
            let end = raw.get(i..)?.iter().position(|&b| b == 0)?;
            i = i.checked_add(end + 1)?;
        }
    }
    if flags & 0x02 != 0 {
        // FHCRC
        i = i.checked_add(2)?;
    }
    raw.get(i..)
}

// ---------------------------------------------------------------------------
// 解析
// ---------------------------------------------------------------------------

fn parse(raw: &[u8]) -> Result<Parsed, String> {
    let xml = decode(raw);
    // 先确认这真是个 FB2：随机字节喂进来时，扫完只会得到一本空书，
    // 调用方就分不清「空书」和「不是书」了。
    let head = xml.get(..4096).unwrap_or(&xml);
    if !head.contains("FictionBook") {
        return Err("not a FictionBook 2 file (no <FictionBook>)".into());
    }
    let mut p = Parser::default();
    scan(&xml, |t| p.handle(t));
    Ok(p.finish())
}

// ---------------------------------------------------------------------------
// 编码
// ---------------------------------------------------------------------------

/// 按 BOM / XML 声明把字节解成 `String`，永不失败。
fn decode(raw: &[u8]) -> String {
    if let Some(rest) = raw.strip_prefix(&[0xef, 0xbb, 0xbf][..]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    if let Some(rest) = raw.strip_prefix(&[0xff, 0xfe][..]) {
        return from_utf16(rest, true);
    }
    if let Some(rest) = raw.strip_prefix(&[0xfe, 0xff][..]) {
        return from_utf16(rest, false);
    }
    // 没有 BOM 的 UTF-16：`<?xml` 在 UTF-16LE 里是 3C 00 3F 00
    if raw.starts_with(&[b'<', 0, b'?', 0]) {
        return from_utf16(raw, true);
    }
    if raw.starts_with(&[0, b'<', 0, b'?']) {
        return from_utf16(raw, false);
    }
    let head = raw.get(..4096).unwrap_or(raw);
    match declared_encoding(head).as_deref() {
        Some("windows-1251" | "cp1251" | "win-1251" | "x-cp1251") => single_byte(raw, &CP1251),
        Some("koi8-r" | "koi8r" | "koi8") => single_byte(raw, &KOI8R),
        Some("iso-8859-5" | "iso8859-5" | "iso_8859-5") => single_byte(raw, &ISO8859_5),
        Some("ibm866" | "cp866" | "866" | "dos-866") => single_byte(raw, &IBM866),
        Some("windows-1252" | "cp1252" | "win-1252") => single_byte(raw, &CP1252),
        // utf-8 / us-ascii / 声明错了 / 干脆没声明：都按 UTF-8 lossy 收
        _ => String::from_utf8_lossy(raw).into_owned(),
    }
}

/// 从 `<?xml ... encoding="..."?>` 里抠编码名（小写）。没声明返回 `None`。
///
/// 只在文件头里按 ASCII 字节找，所以这里读到的字节一定还是 ASCII——不管
/// 文件本身是哪种单字节编码。
fn declared_encoding(head: &[u8]) -> Option<String> {
    let at = head.windows(8).position(|w| w == b"encoding")?;
    let mut i = at + 8;
    while i < head.len() && (head[i].is_ascii_whitespace() || head[i] == b'=') {
        i += 1;
    }
    let quote = *head.get(i)?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let start = i + 1;
    let mut end = start;
    while end < head.len() && head[end] != quote {
        end += 1;
    }
    let name = String::from_utf8_lossy(&head[start..end]);
    Some(name.trim().to_ascii_lowercase())
}

/// 单字节编码：0x00..=0x7F 是 ASCII，高半区查表。表里 `'\u{FFFD}'` 表示该
/// 字节在这种编码下没有定义。
fn single_byte(raw: &[u8], table: &[char; 128]) -> String {
    let mut out = String::with_capacity(raw.len());
    for &b in raw {
        if b.is_ascii() {
            out.push(b as char);
        } else {
            out.push(table[(b - 0x80) as usize]);
        }
    }
    out
}

/// UTF-16（含代理对）。落单的代理项换成 U+FFFD，不 panic。
fn from_utf16(bytes: &[u8], le: bool) -> String {
    let units = bytes.chunks_exact(2).map(|c| {
        if le {
            u16::from_le_bytes([c[0], c[1]])
        } else {
            u16::from_be_bytes([c[0], c[1]])
        }
    });
    char::decode_utf16(units)
        .map(|r| r.unwrap_or('\u{FFFD}'))
        .collect()
}

// ---------------------------------------------------------------------------
// 单字节码表（0x80..=0xFF）
// ---------------------------------------------------------------------------
//
// 手写数组而不是拉 `encoding_rs`：真机上多一个 crate 就多一份交叉编译和几 MB
// 静态表，而 FB2 实际用到的只有这几页。顺序就是 0x80、0x81、……、0xFF。

const CP1251: [char; 128] = [
    '\u{0402}', '\u{0403}', '\u{201A}', '\u{0453}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{20AC}', '\u{2030}', '\u{0409}', '\u{2039}', '\u{040A}', '\u{040C}', '\u{040B}', '\u{040F}',
    '\u{0452}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{FFFD}', '\u{2122}', '\u{0459}', '\u{203A}', '\u{045A}', '\u{045C}', '\u{045B}', '\u{045F}',
    '\u{00A0}', '\u{040E}', '\u{045E}', '\u{0408}', '\u{00A4}', '\u{0490}', '\u{00A6}', '\u{00A7}',
    '\u{0401}', '\u{00A9}', '\u{0404}', '\u{00AB}', '\u{00AC}', '\u{00AD}', '\u{00AE}', '\u{0407}',
    '\u{00B0}', '\u{00B1}', '\u{0406}', '\u{0456}', '\u{0491}', '\u{00B5}', '\u{00B6}', '\u{00B7}',
    '\u{0451}', '\u{2116}', '\u{0454}', '\u{00BB}', '\u{0458}', '\u{0405}', '\u{0455}', '\u{0457}',
    '\u{0410}', '\u{0411}', '\u{0412}', '\u{0413}', '\u{0414}', '\u{0415}', '\u{0416}', '\u{0417}',
    '\u{0418}', '\u{0419}', '\u{041A}', '\u{041B}', '\u{041C}', '\u{041D}', '\u{041E}', '\u{041F}',
    '\u{0420}', '\u{0421}', '\u{0422}', '\u{0423}', '\u{0424}', '\u{0425}', '\u{0426}', '\u{0427}',
    '\u{0428}', '\u{0429}', '\u{042A}', '\u{042B}', '\u{042C}', '\u{042D}', '\u{042E}', '\u{042F}',
    '\u{0430}', '\u{0431}', '\u{0432}', '\u{0433}', '\u{0434}', '\u{0435}', '\u{0436}', '\u{0437}',
    '\u{0438}', '\u{0439}', '\u{043A}', '\u{043B}', '\u{043C}', '\u{043D}', '\u{043E}', '\u{043F}',
    '\u{0440}', '\u{0441}', '\u{0442}', '\u{0443}', '\u{0444}', '\u{0445}', '\u{0446}', '\u{0447}',
    '\u{0448}', '\u{0449}', '\u{044A}', '\u{044B}', '\u{044C}', '\u{044D}', '\u{044E}', '\u{044F}',
];

const KOI8R: [char; 128] = [
    '\u{2500}', '\u{2502}', '\u{250C}', '\u{2510}', '\u{2514}', '\u{2518}', '\u{251C}', '\u{2524}',
    '\u{252C}', '\u{2534}', '\u{253C}', '\u{2580}', '\u{2584}', '\u{2588}', '\u{258C}', '\u{2590}',
    '\u{2591}', '\u{2592}', '\u{2593}', '\u{2320}', '\u{25A0}', '\u{2219}', '\u{221A}', '\u{2248}',
    '\u{2264}', '\u{2265}', '\u{00A0}', '\u{2321}', '\u{00B0}', '\u{00B2}', '\u{00B7}', '\u{00F7}',
    '\u{2550}', '\u{2551}', '\u{2552}', '\u{0451}', '\u{2553}', '\u{2554}', '\u{2555}', '\u{2556}',
    '\u{2557}', '\u{2558}', '\u{2559}', '\u{255A}', '\u{255B}', '\u{255C}', '\u{255D}', '\u{255E}',
    '\u{255F}', '\u{2560}', '\u{2561}', '\u{0401}', '\u{2562}', '\u{2563}', '\u{2564}', '\u{2565}',
    '\u{2566}', '\u{2567}', '\u{2568}', '\u{2569}', '\u{256A}', '\u{256B}', '\u{256C}', '\u{00A9}',
    '\u{044E}', '\u{0430}', '\u{0431}', '\u{0446}', '\u{0434}', '\u{0435}', '\u{0444}', '\u{0433}',
    '\u{0445}', '\u{0438}', '\u{0439}', '\u{043A}', '\u{043B}', '\u{043C}', '\u{043D}', '\u{043E}',
    '\u{043F}', '\u{044F}', '\u{0440}', '\u{0441}', '\u{0442}', '\u{0443}', '\u{0436}', '\u{0432}',
    '\u{044C}', '\u{044B}', '\u{0437}', '\u{0448}', '\u{044D}', '\u{0449}', '\u{0447}', '\u{044A}',
    '\u{042E}', '\u{0410}', '\u{0411}', '\u{0426}', '\u{0414}', '\u{0415}', '\u{0424}', '\u{0413}',
    '\u{0425}', '\u{0418}', '\u{0419}', '\u{041A}', '\u{041B}', '\u{041C}', '\u{041D}', '\u{041E}',
    '\u{041F}', '\u{042F}', '\u{0420}', '\u{0421}', '\u{0422}', '\u{0423}', '\u{0416}', '\u{0412}',
    '\u{042C}', '\u{042B}', '\u{0417}', '\u{0428}', '\u{042D}', '\u{0429}', '\u{0427}', '\u{042A}',
];

const ISO8859_5: [char; 128] = [
    '\u{0080}', '\u{0081}', '\u{0082}', '\u{0083}', '\u{0084}', '\u{0085}', '\u{0086}', '\u{0087}',
    '\u{0088}', '\u{0089}', '\u{008A}', '\u{008B}', '\u{008C}', '\u{008D}', '\u{008E}', '\u{008F}',
    '\u{0090}', '\u{0091}', '\u{0092}', '\u{0093}', '\u{0094}', '\u{0095}', '\u{0096}', '\u{0097}',
    '\u{0098}', '\u{0099}', '\u{009A}', '\u{009B}', '\u{009C}', '\u{009D}', '\u{009E}', '\u{009F}',
    '\u{00A0}', '\u{0401}', '\u{0402}', '\u{0403}', '\u{0404}', '\u{0405}', '\u{0406}', '\u{0407}',
    '\u{0408}', '\u{0409}', '\u{040A}', '\u{040B}', '\u{040C}', '\u{00AD}', '\u{040E}', '\u{040F}',
    '\u{0410}', '\u{0411}', '\u{0412}', '\u{0413}', '\u{0414}', '\u{0415}', '\u{0416}', '\u{0417}',
    '\u{0418}', '\u{0419}', '\u{041A}', '\u{041B}', '\u{041C}', '\u{041D}', '\u{041E}', '\u{041F}',
    '\u{0420}', '\u{0421}', '\u{0422}', '\u{0423}', '\u{0424}', '\u{0425}', '\u{0426}', '\u{0427}',
    '\u{0428}', '\u{0429}', '\u{042A}', '\u{042B}', '\u{042C}', '\u{042D}', '\u{042E}', '\u{042F}',
    '\u{0430}', '\u{0431}', '\u{0432}', '\u{0433}', '\u{0434}', '\u{0435}', '\u{0436}', '\u{0437}',
    '\u{0438}', '\u{0439}', '\u{043A}', '\u{043B}', '\u{043C}', '\u{043D}', '\u{043E}', '\u{043F}',
    '\u{0440}', '\u{0441}', '\u{0442}', '\u{0443}', '\u{0444}', '\u{0445}', '\u{0446}', '\u{0447}',
    '\u{0448}', '\u{0449}', '\u{044A}', '\u{044B}', '\u{044C}', '\u{044D}', '\u{044E}', '\u{044F}',
    '\u{2116}', '\u{0451}', '\u{0452}', '\u{0453}', '\u{0454}', '\u{0455}', '\u{0456}', '\u{0457}',
    '\u{0458}', '\u{0459}', '\u{045A}', '\u{045B}', '\u{045C}', '\u{00A7}', '\u{045E}', '\u{045F}',
];

const IBM866: [char; 128] = [
    '\u{0410}', '\u{0411}', '\u{0412}', '\u{0413}', '\u{0414}', '\u{0415}', '\u{0416}', '\u{0417}',
    '\u{0418}', '\u{0419}', '\u{041A}', '\u{041B}', '\u{041C}', '\u{041D}', '\u{041E}', '\u{041F}',
    '\u{0420}', '\u{0421}', '\u{0422}', '\u{0423}', '\u{0424}', '\u{0425}', '\u{0426}', '\u{0427}',
    '\u{0428}', '\u{0429}', '\u{042A}', '\u{042B}', '\u{042C}', '\u{042D}', '\u{042E}', '\u{042F}',
    '\u{0430}', '\u{0431}', '\u{0432}', '\u{0433}', '\u{0434}', '\u{0435}', '\u{0436}', '\u{0437}',
    '\u{0438}', '\u{0439}', '\u{043A}', '\u{043B}', '\u{043C}', '\u{043D}', '\u{043E}', '\u{043F}',
    '\u{2591}', '\u{2592}', '\u{2593}', '\u{2502}', '\u{2524}', '\u{2561}', '\u{2562}', '\u{2556}',
    '\u{2555}', '\u{2563}', '\u{2551}', '\u{2557}', '\u{255D}', '\u{255C}', '\u{255B}', '\u{2510}',
    '\u{2514}', '\u{2534}', '\u{252C}', '\u{251C}', '\u{2500}', '\u{253C}', '\u{255E}', '\u{255F}',
    '\u{255A}', '\u{2554}', '\u{2569}', '\u{2566}', '\u{2560}', '\u{2550}', '\u{256C}', '\u{2567}',
    '\u{2568}', '\u{2564}', '\u{2565}', '\u{2559}', '\u{2558}', '\u{2552}', '\u{2553}', '\u{256B}',
    '\u{256A}', '\u{2518}', '\u{250C}', '\u{2588}', '\u{2584}', '\u{258C}', '\u{2590}', '\u{2580}',
    '\u{0440}', '\u{0441}', '\u{0442}', '\u{0443}', '\u{0444}', '\u{0445}', '\u{0446}', '\u{0447}',
    '\u{0448}', '\u{0449}', '\u{044A}', '\u{044B}', '\u{044C}', '\u{044D}', '\u{044E}', '\u{044F}',
    '\u{0401}', '\u{0451}', '\u{0404}', '\u{0454}', '\u{0407}', '\u{0457}', '\u{040E}', '\u{045E}',
    '\u{00B0}', '\u{2219}', '\u{00B7}', '\u{221A}', '\u{2116}', '\u{00A4}', '\u{25A0}', '\u{00A0}',
];

const CP1252: [char; 128] = [
    '\u{20AC}', '\u{FFFD}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{FFFD}', '\u{017D}', '\u{FFFD}',
    '\u{FFFD}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{FFFD}', '\u{017E}', '\u{0178}',
    '\u{00A0}', '\u{00A1}', '\u{00A2}', '\u{00A3}', '\u{00A4}', '\u{00A5}', '\u{00A6}', '\u{00A7}',
    '\u{00A8}', '\u{00A9}', '\u{00AA}', '\u{00AB}', '\u{00AC}', '\u{00AD}', '\u{00AE}', '\u{00AF}',
    '\u{00B0}', '\u{00B1}', '\u{00B2}', '\u{00B3}', '\u{00B4}', '\u{00B5}', '\u{00B6}', '\u{00B7}',
    '\u{00B8}', '\u{00B9}', '\u{00BA}', '\u{00BB}', '\u{00BC}', '\u{00BD}', '\u{00BE}', '\u{00BF}',
    '\u{00C0}', '\u{00C1}', '\u{00C2}', '\u{00C3}', '\u{00C4}', '\u{00C5}', '\u{00C6}', '\u{00C7}',
    '\u{00C8}', '\u{00C9}', '\u{00CA}', '\u{00CB}', '\u{00CC}', '\u{00CD}', '\u{00CE}', '\u{00CF}',
    '\u{00D0}', '\u{00D1}', '\u{00D2}', '\u{00D3}', '\u{00D4}', '\u{00D5}', '\u{00D6}', '\u{00D7}',
    '\u{00D8}', '\u{00D9}', '\u{00DA}', '\u{00DB}', '\u{00DC}', '\u{00DD}', '\u{00DE}', '\u{00DF}',
    '\u{00E0}', '\u{00E1}', '\u{00E2}', '\u{00E3}', '\u{00E4}', '\u{00E5}', '\u{00E6}', '\u{00E7}',
    '\u{00E8}', '\u{00E9}', '\u{00EA}', '\u{00EB}', '\u{00EC}', '\u{00ED}', '\u{00EE}', '\u{00EF}',
    '\u{00F0}', '\u{00F1}', '\u{00F2}', '\u{00F3}', '\u{00F4}', '\u{00F5}', '\u{00F6}', '\u{00F7}',
    '\u{00F8}', '\u{00F9}', '\u{00FA}', '\u{00FB}', '\u{00FC}', '\u{00FD}', '\u{00FE}', '\u{00FF}',
];

// ---------------------------------------------------------------------------
// 标签流扫描
// ---------------------------------------------------------------------------

/// 标签流事件。文本节点只给切片，由调用方决定要不要解码实体。
enum Tok<'a> {
    Open {
        name: &'a str,
        attrs: &'a str,
        /// `<empty-line/>` 这种自闭合标签不参与深度计数
        empty: bool,
    },
    Close {
        name: &'a str,
    },
    Text(&'a str),
}

/// 扫 `>` 的位置，跳过引号里的内容（属性值里合法地出现 `>`）。
fn tag_end(b: &[u8], mut i: usize) -> Option<usize> {
    let mut quote = 0u8;
    let limit = (i + MAX_TAG).min(b.len());
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

/// 把 XML 拍成标签 + 文本事件。注释整段丢掉，CDATA 当文本，
/// `<?...?>`/`<!DOCTYPE>` 跳过。
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
        let body = inner.trim_start_matches('/').trim();
        let empty = body.ends_with('/');
        let body = body.trim_end_matches('/').trim_end();
        let name_end = body.find(char::is_whitespace).unwrap_or(body.len());
        let name = &body[..name_end];
        let attrs = &body[name_end..];
        // 只认本地名（`l:item` → `item`）：各家的命名空间前缀不统一，
        // 带前缀比较会漏标签。
        let name = match name.rfind(':') {
            Some(k) => &name[k + 1..],
            None => name,
        };
        if !name.is_empty() {
            if close {
                f(Tok::Close { name });
            } else {
                f(Tok::Open {
                    name,
                    attrs,
                    empty,
                });
            }
        }
        i = end + 1;
        text_start = i;
    }
    if text_start < b.len() {
        f(Tok::Text(&s[text_start..]));
    }
}

/// 取属性值（名字大小写不敏感）。没这个属性返回 `None`；属性裸写（无 `=`）
/// 返回空串。FB2 里只用来读 `<body name="notes">`。
fn attr_value<'a>(attrs: &'a str, want: &str) -> Option<&'a str> {
    let b = attrs.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < b.len() && b[i] != b'=' && !b[i].is_ascii_whitespace() {
            i += 1;
        }
        let hit = attrs[start..i].eq_ignore_ascii_case(want);
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] != b'=' {
            if hit {
                return Some("");
            }
            if i == start {
                i += 1;
            }
            continue;
        }
        i += 1;
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        let (val, next) = if i < b.len() && (b[i] == b'"' || b[i] == b'\'') {
            let quote = b[i];
            let start = i + 1;
            let mut j = start;
            while j < b.len() && b[j] != quote {
                j += 1;
            }
            (&attrs[start..j], (j + 1).min(b.len()))
        } else {
            let start = i;
            let mut j = i;
            while j < b.len() && !b[j].is_ascii_whitespace() {
                j += 1;
            }
            (&attrs[start..j], j)
        };
        if hit {
            return Some(val);
        }
        i = next.max(i);
    }
    None
}

// ---------------------------------------------------------------------------
// 实体
// ---------------------------------------------------------------------------

/// `&amp; &lt; &gt; &quot; &apos; &#NNN; &#xHH;`。认不出来的实体原样保留
/// （不吞字），XML 里未定义的裸 `&` 也不会把后面的文字吃掉。
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 1..];
        let hit = tail
            .find(';')
            .filter(|k| *k <= 10)
            .and_then(|k| entity_char(&tail[..k]).map(|c| (c, k + 1)));
        match hit {
            Some((c, skip)) => {
                out.push(c);
                rest = &tail[skip..];
            }
            None => {
                out.push('&');
                rest = tail;
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
        // 西文排版里到处都是 &nbsp;，解成普通空格好让断行吃掉
        "nbsp" => Some(' '),
        _ => {
            let num = name.strip_prefix('#')?;
            let code = match num.strip_prefix('x').or_else(|| num.strip_prefix('X')) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => num.parse::<u32>().ok()?,
            };
            char::from_u32(code).filter(|c| !c.is_control())
        }
    }
}

// ---------------------------------------------------------------------------
// 解析状态机
// ---------------------------------------------------------------------------

/// 正在收集的元数据。这几个标签不会互相嵌套，一个槽就够。
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Cap {
    #[default]
    None,
    BookTitle,
    FirstName,
    LastName,
}

#[derive(Default)]
struct Parser {
    /// 输出
    chapters: Vec<Chapter>,
    toc: Vec<TocEntry>,
    path_index: HashMap<String, usize>,
    title: Option<String>,
    author: Option<String>,
    /// 元数据临时槽
    first_name: String,
    last_name: String,
    in_title_info: bool,
    /// 结构
    desc: u32,
    skip_tag: Option<&'static str>,
    skip_depth: u32,
    in_body: bool,
    section_depth: u32,
    /// `<poem>` 里的 `<title>`（诗题）不是章节标题，不进目录
    poem: u32,
    in_title: bool,
    in_block: bool,
    cur: Option<usize>,
    /// section 之前的内容（部/卷名、题记…）先放这儿，第一章建出来时并进去
    preamble: Vec<Block>,
    /// 当前段落/标题的文字
    buf: String,
    cap: Cap,
}

impl Parser {
    fn handle(&mut self, tok: Tok<'_>) {
        match tok {
            Tok::Text(s) => self.text(s),
            Tok::Open { name, attrs, empty } => self.open(name, attrs, empty),
            Tok::Close { name } => self.close(name),
        }
    }

    fn text(&mut self, s: &str) {
        if self.skip_tag.is_some() {
            return;
        }
        let wanted = self.in_block || self.in_title || self.cap != Cap::None;
        if !wanted {
            return;
        }
        if s.contains('&') {
            self.buf.push_str(&decode_entities(s));
        } else {
            self.buf.push_str(s);
        }
    }

    fn open(&mut self, name: &str, attrs: &str, empty: bool) {
        // 跳过区里只数同名标签的深度，别的什么都不做
        if let Some(tag) = self.skip_tag {
            if !empty && name == tag {
                self.skip_depth += 1;
            }
            return;
        }
        if self.desc > 0 {
            if empty {
                return;
            }
            match name {
                "description" => self.desc += 1,
                "title-info" => self.in_title_info = true,
                "book-title" => self.cap = Cap::BookTitle,
                "first-name" if self.in_title_info => self.cap = Cap::FirstName,
                "last-name" if self.in_title_info => self.cap = Cap::LastName,
                _ => {}
            }
            if self.cap != Cap::None {
                self.buf.clear();
            }
            return;
        }
        match name {
            "description" => {
                if !empty {
                    self.desc = 1;
                }
                return;
            }
            // 插图：九成体积在这儿，整块跳过连 base64 都不看一眼
            "binary" => {
                if !empty {
                    self.skip_tag = Some("binary");
                    self.skip_depth = 1;
                }
                return;
            }
            "body" => {
                if empty {
                    return;
                }
                // 脚注/注释 body 不混进正文
                if attr_value(attrs, "name").is_some_and(|v| v.eq_ignore_ascii_case("notes")) {
                    self.skip_tag = Some("body");
                    self.skip_depth = 1;
                } else {
                    self.in_body = true;
                }
                return;
            }
            _ => {}
        }
        if !self.in_body {
            return;
        }
        match name {
            "section" if !empty => {
                self.section_depth += 1;
                if self.section_depth == 1 {
                    self.new_chapter();
                }
            }
            "poem" if !empty => self.poem += 1,
            "title" if !empty => {
                self.begin();
                self.in_title = true;
            }
            // 一段文字。`<poem>`/`<stanza>`/`<cite>`/`<epigraph>` 只是容器，
            // 里面的 `<p>`/`<v>` 自动走到这里。
            "p" | "v" | "subtitle" | "text-author" | "td" if !empty => {
                self.begin();
                self.in_block = true;
            }
            // 空行 = 段落分隔。块模型里段落本来就一块一块的，这里只需把
            // 前面积着的裸文字收掉。
            "empty-line" => self.flush(),
            _ => {}
        }
    }

    fn close(&mut self, name: &str) {
        if let Some(tag) = self.skip_tag {
            if name == tag {
                self.skip_depth = self.skip_depth.saturating_sub(1);
                if self.skip_depth == 0 {
                    self.skip_tag = None;
                }
            }
            return;
        }
        if self.desc > 0 {
            match name {
                "description" => {
                    self.desc = self.desc.saturating_sub(1);
                    self.cap = Cap::None;
                    self.in_title_info = false;
                }
                "title-info" => self.in_title_info = false,
                "book-title" => self.take_cap(Cap::BookTitle),
                "first-name" => self.take_cap(Cap::FirstName),
                "last-name" => self.take_cap(Cap::LastName),
                "author" if self.in_title_info => self.join_author(),
                _ => {}
            }
            return;
        }
        match name {
            "body" if self.in_body => {
                self.flush();
                self.in_body = false;
                self.in_title = false;
                self.in_block = false;
                self.section_depth = 0;
                self.poem = 0;
                self.cur = None;
            }
            "section" => {
                self.flush();
                self.section_depth = self.section_depth.saturating_sub(1);
            }
            "poem" => {
                self.flush();
                self.poem = self.poem.saturating_sub(1);
            }
            "title" => {
                self.flush();
                self.in_title = false;
            }
            "p" | "v" | "subtitle" | "text-author" | "td" => {
                self.flush();
                self.in_block = false;
            }
            _ => {}
        }
    }

    /// 开始一个新块/标题：先把前面积着的文字收掉。
    fn begin(&mut self) {
        self.flush();
        self.buf.clear();
    }

    /// 把 `buf` 收成一个块。标题按 section 嵌套深度定级：最外层 H1，
    /// 往下 H2/H3，封顶 H3。
    fn flush(&mut self) {
        if self.buf.trim().is_empty() {
            self.buf.clear();
            return;
        }
        let text = clean(&std::mem::take(&mut self.buf));
        let block = if self.in_title {
            let level = if self.section_depth == 0 {
                1
            } else {
                self.section_depth.min(3) as u8
            };
            // 诗题不是章节标题，别往目录里塞
            if self.section_depth > 0 && self.poem == 0 {
                if let Some(ci) = self.cur {
                    self.toc.push(TocEntry {
                        level,
                        title: text.clone(),
                        path: ci.to_string(),
                        anchor: String::new(),
                    });
                }
            }
            Block::heading(level, text)
        } else {
            Block::body(text)
        };
        self.push_block(block);
    }

    fn push_block(&mut self, b: Block) {
        match self.cur {
            Some(i) => self.chapters[i].blocks.push(b),
            None => self.preamble.push(b),
        }
    }

    fn new_chapter(&mut self) {
        let idx = self.add_chapter();
        self.cur = Some(idx);
    }

    fn add_chapter(&mut self) -> usize {
        let blocks = std::mem::take(&mut self.preamble);
        let idx = self.chapters.len();
        self.chapters.push(Chapter { blocks });
        self.path_index.insert(idx.to_string(), idx);
        idx
    }

    fn take_cap(&mut self, want: Cap) {
        let text = clean(&std::mem::take(&mut self.buf));
        let hit = self.cap == want;
        self.cap = Cap::None;
        if !hit || text.is_empty() {
            return;
        }
        match want {
            Cap::BookTitle => {
                if self.title.is_none() {
                    self.title = Some(text);
                }
            }
            Cap::FirstName => self.first_name = text,
            Cap::LastName => self.last_name = text,
            Cap::None => {}
        }
    }

    /// 只留第一个作者（`title-info` 里可能有并列作者）。
    fn join_author(&mut self) {
        let first = std::mem::take(&mut self.first_name);
        let last = std::mem::take(&mut self.last_name);
        if self.author.is_some() {
            return;
        }
        let name = match (first.is_empty(), last.is_empty()) {
            (false, false) => format!("{first} {last}"),
            (false, true) => first,
            (true, false) => last,
            (true, true) => return,
        };
        self.author = Some(name);
    }

    fn finish(mut self) -> Parsed {
        self.flush();
        self.join_author();
        // 没有 section 的 body（整本只有段落）也得成一章
        if !self.preamble.is_empty() {
            self.add_chapter();
        }
        Parsed {
            book: Book {
                chapters: self.chapters,
                toc: self.toc,
                path_index: self.path_index,
            },
            title: self.title,
            author: self.author,
        }
    }
}

/// 折叠空白：XML 源码里的换行和缩进不该变成段内硬换行，多空格压成一个。
fn clean(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut gap = false;
    for c in s.chars() {
        if c.is_whitespace() {
            gap = true;
            continue;
        }
        if gap && !out.is_empty() {
            out.push(' ');
        }
        gap = false;
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(xml: &str) -> Fb2Doc {
        Fb2Doc::build(parse(xml.as_bytes()).expect("parse"))
    }

    fn doc_bytes(raw: &[u8]) -> Fb2Doc {
        Fb2Doc::build(parse(raw).expect("parse"))
    }

    fn page_text(d: &Fb2Doc, idx: usize) -> String {
        d.inner.page_text(idx).unwrap_or_default()
    }

    fn all_text(d: &Fb2Doc) -> String {
        (0..d.page_count())
            .map(|i| page_text(d, i))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 现造一个 gzip 容器（10 字节头 + 裸 deflate + CRC32 + 原始长度），
    /// 头部按 `gzip` 命令的默认样式带上文件名。
    fn gzip(xml: &str, name: &str) -> Vec<u8> {
        let body = xml.as_bytes();
        let mut out = vec![0x1f, 0x8b, 8, 0x08, 0, 0, 0, 0, 0, 0xff];
        out.extend_from_slice(name.as_bytes());
        out.push(0);
        out.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(body, 6));
        let mut crc = 0xffff_ffffu32;
        for &b in body {
            crc ^= b as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb8_8320 & (crc & 1).wrapping_neg());
            }
        }
        out.extend_from_slice(&(!crc).to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out
    }

    #[test]
    fn gzipped_fb2_is_inflated() {
        let xml = unpack(gzip(MINIMAL, "book.fb2")).expect("unpack gz");
        let d = Fb2Doc::build(parse(&xml).expect("parse gz"));
        assert_eq!(d.page_count(), 2);
        assert_eq!(d.title(), Some("测试书"));
        assert!(page_text(&d, 0).contains("第一章"));
        // 头被截断 / CM 不是 deflate：Err，不 panic
        assert!(unpack(vec![0x1f, 0x8b, 8]).is_err());
        assert!(unpack(vec![0x1f, 0x8b, 9, 0, 0, 0, 0, 0, 0, 0]).is_err());
    }

    const MINIMAL: &str = r##"<?xml version="1.0" encoding="utf-8"?>
<FictionBook xmlns="http://www.gribuser.ru/xml/fictionbook/2.0" xmlns:l="http://www.w3.org/1999/xlink">
  <description>
    <title-info>
      <book-title>测试书</book-title>
      <author><first-name>列夫</first-name><last-name>托尔斯泰</last-name></author>
      <annotation><p>简介属于 description，不该进正文。</p></annotation>
      <coverpage><image l:href="#cover.jpg"/></coverpage>
    </title-info>
  </description>
  <body>
    <section id="s1"><title><p>第一章</p></title><p>第一章的正文。</p></section>
    <section id="s2"><title><p>第二章</p></title><p>第二章的正文。</p></section>
  </body>
</FictionBook>"##;

    #[test]
    fn minimal_book_has_pages_and_chapters() {
        let d = doc(MINIMAL);
        assert_eq!(d.page_count(), 2, "两个 section = 两章两页");
        let p0 = page_text(&d, 0);
        assert!(p0.contains("第一章"), "第一页要有第一章标题：{p0}");
        assert!(!p0.contains("第二章"), "第一章和第二章不该同页：{p0}");
        assert!(page_text(&d, 1).contains("第二章的正文"));
        assert_eq!(d.chapter_title(0).as_deref(), Some("第一章"));
        assert_eq!(d.chapter_title(1).as_deref(), Some("第二章"));
        // 简介、封面都不该混进正文
        assert!(!all_text(&d).contains("简介属于"));
        // 目录：两条一级，页号和章节首页一致
        let toc = d.outline();
        assert_eq!(toc.len(), 2);
        assert_eq!((toc[0].depth, toc[0].title.as_str(), toc[0].page), (1, "第一章", 0));
        assert_eq!((toc[1].depth, toc[1].title.as_str(), toc[1].page), (1, "第二章", 1));
    }

    #[test]
    fn find_delegates_to_reflow() {
        let xml = r##"<FictionBook><body><section><title><p>Chapter One</p></title>
<p>alpha beta gamma</p></section></body></FictionBook>"##;
        let d = doc(xml);
        let hits = d.find("gamma", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, 0);
        assert!(hits[0].1.contains("gamma"));
        assert!(d.find("zzz", 10).is_empty());
    }

    #[test]
    fn title_and_author_are_parsed() {
        let d = doc(MINIMAL);
        assert_eq!(d.title(), Some("测试书"));
        assert_eq!(d.author(), Some("列夫 托尔斯泰"));
        // 没有 description 的书：两个都是 None，不 panic
        let bare = doc("<FictionBook><body><section><p>x</p></section></body></FictionBook>");
        assert_eq!(bare.title(), None);
        assert_eq!(bare.author(), None);
    }

    #[test]
    fn nested_sections_give_three_levels_of_toc() {
        let xml = r##"<FictionBook><body>
<section><title><p>第一部</p></title><p>开场。</p>
  <section><title><p>1.1</p></title><p>小节。</p>
    <section><title><p>1.1.1</p></title><p>更深。</p>
      <section><title><p>1.1.1.1</p></title><p>最深。</p></section>
    </section>
  </section>
</section>
<section><title><p>第二部</p></title><p>正文。</p>
  <section><title><p>2.1</p></title><p>小节。</p></section>
</section>
<section><title><p>第三部</p></title><p>正文。</p></section>
</body></FictionBook>"##;
        let d = doc(xml);
        let toc = d.outline();
        let depths: Vec<u8> = toc.iter().map(|t| t.depth).collect();
        assert_eq!(depths, vec![1, 2, 3, 3, 1, 2, 1], "三层目录，第四层封顶 H3");
        assert_eq!(toc.len(), 7);
        // 页号只增不减；每个顶层 section 各占一章首
        assert!(toc.windows(2).all(|w| w[0].page <= w[1].page));
        let tops: Vec<usize> = toc.iter().filter(|t| t.depth == 1).map(|t| t.page).collect();
        assert_eq!(tops, vec![0, 1, 2], "三个 section = 三章，页号递增");
        assert!(toc[3].title == "1.1.1.1" && toc[3].depth == 3);
    }

    #[test]
    fn notes_body_is_skipped() {
        let xml = r##"<FictionBook>
<description><title-info><book-title>带脚注</book-title></title-info></description>
<body><section><title><p>正文</p></title><p>正文段落。</p></section></body>
<body name="notes">
  <section id="n1"><title><p>1</p></title><p>脚注内容不该出现。</p></section>
</body>
</FictionBook>"##;
        let d = doc(xml);
        assert_eq!(d.page_count(), 1);
        assert!(page_text(&d, 0).contains("正文段落"));
        assert!(!all_text(&d).contains("脚注"), "notes body 不该进正文");
        assert_eq!(d.outline().len(), 1, "脚注 section 也不进目录");
        assert_eq!(d.outline()[0].title, "正文");
    }

    #[test]
    fn entities_are_decoded() {
        let xml = r##"<FictionBook><body><section><title><p>实体</p></title>
<p>A &amp; B &lt;x&gt; &#1055;&#x440;&#1080; &quot;q&quot; &apos;s&apos;</p>
</section></body></FictionBook>"##;
        let d = doc(xml);
        let text = page_text(&d, 0);
        assert!(text.contains("A & B <x> При \"q\" 's'"), "实体没解对：{text}");
    }

    #[test]
    fn windows_1251_is_decoded_by_declaration() {
        // 0xCF 0xF0 0xE8 0xE2 0xE5 0xF2 = "Привет"
        let mut raw = Vec::new();
        raw.extend_from_slice(
            b"<?xml version=\"1.0\" encoding=\"windows-1251\"?>\
              <FictionBook><description><title-info><book-title>",
        );
        raw.extend_from_slice(&[0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2]);
        raw.extend_from_slice(
            b"</book-title></title-info></description>\
              <body><section><title><p>",
        );
        raw.extend_from_slice(&[0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2]);
        raw.extend_from_slice(b"</p></section></body></FictionBook>");
        let d = doc_bytes(&raw);
        assert!(page_text(&d, 0).contains("Привет"), "1251 没解对");
        assert_eq!(d.title(), Some("Привет"));
    }

    #[test]
    fn utf16_bom_is_decoded() {
        let xml = "<?xml version=\"1.0\" encoding=\"utf-16\"?><FictionBook><body>\
                   <section><title><p>宽字符</p></title><p>正文。</p></section>\
                   </body></FictionBook>";
        let mut raw = vec![0xff, 0xfe];
        for u in xml.encode_utf16() {
            raw.extend_from_slice(&u.to_le_bytes());
        }
        let d = doc_bytes(&raw);
        assert!(page_text(&d, 0).contains("宽字符"));
    }

    #[test]
    fn unknown_encoding_falls_back_to_utf8() {
        let raw = b"<?xml version=\"1.0\" encoding=\"shift_jis\"?>\
                    <FictionBook><body><section><title><p>x</p></title>\
                    <p>fallback</p></section></body></FictionBook>";
        let d = doc_bytes(raw);
        assert_eq!(d.page_count(), 1);
        assert!(page_text(&d, 0).contains("fallback"));
    }

    #[test]
    fn huge_binary_block_is_skipped() {
        let mut xml = String::from(
            "<FictionBook><body><section><title><p>有图的书</p></title>\
             <p>图片不该影响正文。</p></section></body>",
        );
        xml.push_str("<binary id=\"cover.jpg\" content-type=\"image/jpeg\">");
        for _ in 0..20_000 {
            xml.push_str("QUJDREVGR0g=");
        }
        xml.push_str("</binary></FictionBook>");
        let d = doc(&xml);
        assert_eq!(d.page_count(), 1);
        let text = page_text(&d, 0);
        assert!(text.contains("图片不该影响正文"));
        assert!(!text.contains("QUJD"), "base64 不该漏进正文");
    }

    #[test]
    fn empty_body_has_no_pages() {
        let d = doc("<FictionBook><body></body></FictionBook>");
        assert_eq!(d.page_count(), 0);
        assert!(d.outline().is_empty());
        assert!(d.render(0, 200, 280).is_err(), "没页可渲染但必须是 Err");
    }

    #[test]
    fn out_of_range_page_is_err() {
        let d = doc(MINIMAL);
        let n = d.page_count();
        assert!(d.render(n, 300, 420).is_err());
        assert!(d.render(usize::MAX, 100, 140).is_err());
    }

    #[test]
    fn body_without_sections_is_one_chapter() {
        let xml = r##"<FictionBook>
<description><title-info><book-title>无章节</book-title></title-info></description>
<body><title><p>全书标题</p></title><p>唯一的一段。</p></body>
</FictionBook>"##;
        let d = doc(xml);
        assert_eq!(d.page_count(), 1);
        let text = page_text(&d, 0);
        assert!(text.contains("全书标题") && text.contains("唯一的一段"));
        // body 自己的 title 不是 section，不进目录
        assert!(d.outline().is_empty());
    }

    #[test]
    fn bad_containers_are_errors_not_panics() {
        // 不是 FB2
        assert!(parse(b"random bytes, not xml at all").is_err());
        // 坏 gzip
        assert!(unpack(vec![0x1f, 0x8b, 1, 2, 3, 4]).is_err());
        // zip 包里的 fb2 不支持，但要给清楚的错
        let err = unpack(b"PK\x03\x04....".to_vec()).err().unwrap_or_default();
        assert!(err.contains(".fb2.zip"), "错误信息要指出 zip：{err}");
    }
}
