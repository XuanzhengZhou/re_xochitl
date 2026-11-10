//! MOBI / AZW3（Kindle）后端。
//!
//! 三层结构，从下往上：
//!
//! 1. **PalmDB 容器**（[`PalmDb`]）：78 字节头 + 每记录 8 字节表项，
//!    记录 N 的数据是 `[offset[N], offset[N+1])`，最后一条到文件尾。
//! 2. **record 0**：前 16 字节是 PalmDOC header（压缩方式 / 正文长度 /
//!    正文记录数 / 加密标志），offset 16 起是 MOBI header（编码、类型、版本、
//!    HUFF/CDIC 记录位置、trailing data flags、KF8 的 FDST 位置）。
//! 3. **正文**：记录 1 起逐条剥掉尾部附加数据再解压（不压缩 / PalmDOC LZ77 /
//!    HUFF-CDIC），按 header 里的编码解成字符串，最后当 HTML 子集扫成块。
//!
//! # 算法出处
//!
//! * PalmDOC LZ77 与 HUFF/CDIC：照 **KindleUnpack** 的
//!   `lib/mobi_uncompress.py`（`PalmdocReader` / `HuffcdicReader`）重写，
//!   并和 **mobi 0.8.0**（MIT）的 `src/compression/huff.rs` 对过一遍表结构。
//! * 尾部附加数据（trailing entries / multibyte overlap）：
//!   照 KindleUnpack 的 `MobiHeader.getRawML()`，**不剥就会解压出垃圾**。
//! * KF8 判定：KindleUnpack 的 `isK8()`（`version >= 8 || mobi_type == 248`）。
//!
//! # 已知边界
//!
//! * 合订本（kindlegen 出的 `.mobi`：KF7 段 + `BOUNDARY` + KF8 段）只读
//!   record 0 的 KF7 段——它是完整的旧 MOBI 正文，够读。
//! * 独立 KF8（`.azw3`）直接读 record 0 的 KF8 正文；正文里 skeleton /
//!   fragment 是**顺序拼接**的，没有解析 INDX 表去重排，但行文顺序是对的，
//!   用 FDST 只取 flow 0（xhtml），不会混进 CSS。
//! * HUFF/CDIC 递归展开有 32 层上限；自引用的短语会报错而不是栈溢出。
//! * 目录靠 `<h1>/<h2>/<h3>` 自动生成，不解析 `<guide>` / `filepos` 锚点。

use std::collections::HashMap;

use crate::doc::reflow::{Block, Book, Cfg, Chapter, Kind, ReflowDoc, TocEntry};
use crate::doc::{Format, LayoutCfg, Toc};

/// 文件上限：128 MB。设备内存不到 1 GB，再大手就没意义了。
const MAX_FILE: usize = 128 << 20;
/// 解压后正文上限：32 MB（对齐 `epub.rs` 的 `MAX_ENTRY`）。
const MAX_TEXT: usize = 32 << 20;
/// PalmDB 记录数上限。
const MAX_RECORDS: usize = 20_000;
/// HUFF/CDIC 短语递归展开深度上限。
const MAX_HUFF_DEPTH: usize = 32;

// ---------------------------------------------------------------------------
// 对外接口
// ---------------------------------------------------------------------------

/// 一本 MOBI / AZW3。分页、渲染、目录、搜索全交给 [`ReflowDoc`]。
pub struct MobiDoc {
    inner: ReflowDoc,
    fmt: Format,
}

impl MobiDoc {
    /// 打开一本 MOBI / AZW3。文件头不认识、DRM、压缩方式不支持都返回 `Err`。
    pub fn open(path: &str) -> Result<MobiDoc, String> {
        let meta = std::fs::metadata(path).map_err(|e| format!("cannot read {path}: {e}"))?;
        if meta.len() > MAX_FILE as u64 {
            return Err(format!(
                "file too large ({} bytes, max {} MB): {path}",
                meta.len(),
                MAX_FILE >> 20
            ));
        }
        let data = std::fs::read(path).map_err(|e| format!("cannot read {path}: {e}"))?;
        MobiDoc::from_bytes(&data)
    }

    /// 按 header 判：record 0 是 KF8 头就是 AZW3，否则 MOBI。
    ///
    /// 合订本（KF7 + KF8）读的是 KF7 段，所以报 MOBI。
    pub fn format(&self) -> Format {
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

    pub fn outline(&self) -> Vec<Toc> {
        self.inner.outline()
    }

    pub fn find(&self, needle: &str, limit: usize) -> Vec<(usize, String)> {
        self.inner.find(needle, limit)
    }

    pub fn chapter_title(&self, idx: usize) -> Option<String> {
        self.inner.chapter_title(idx)
    }

    pub fn apply_layout(&mut self, cfg: LayoutCfg) -> bool {
        self.inner.apply_layout(cfg)
    }

    pub fn current_layout(&self) -> LayoutCfg {
        self.inner.current_layout()
    }

    /// 从内存里的字节打开（[`MobiDoc::open`] 的实体；测试也走这条）。
    fn from_bytes(data: &[u8]) -> Result<MobiDoc, String> {
        let db = PalmDb::parse(data)?;
        let part = Part::parse(db.record(0)?)?;
        let html = decompress(&part, &db)?;
        let book = build_book(&html);
        let fmt = if part.kf8 {
            Format::Azw3
        } else {
            Format::Mobi
        };
        Ok(MobiDoc {
            inner: ReflowDoc::new(book, Cfg::default()),
            fmt,
        })
    }
}

// ---------------------------------------------------------------------------
// PalmDB 容器
// ---------------------------------------------------------------------------

/// Palm 数据库容器：只管记录边界，不管内容。
struct PalmDb<'a> {
    data: &'a [u8],
    /// 记录起点；末尾多塞一个 `data.len()`，两两相减就是记录数据。
    offsets: Vec<usize>,
}

impl<'a> PalmDb<'a> {
    fn parse(data: &'a [u8]) -> Result<PalmDb<'a>, String> {
        if data.len() < 78 {
            return Err("not a PalmDB file (shorter than 78 bytes)".into());
        }
        let count = be16(data, 76)? as usize;
        if count == 0 {
            return Err("PalmDB has no records".into());
        }
        if count > MAX_RECORDS {
            return Err(format!(
                "too many PalmDB records ({count}, max {MAX_RECORDS})"
            ));
        }
        let table_end = 78usize
            .checked_add(count * 8)
            .ok_or_else(|| "PalmDB record table size overflow".to_string())?;
        if table_end > data.len() {
            return Err("truncated PalmDB record table".into());
        }
        let mut offsets = Vec::with_capacity(count + 1);
        for i in 0..count {
            let off = be32(data, 78 + i * 8)? as usize;
            // 记录不能压在头部/表上，也不能越过文件尾
            if off < table_end || off > data.len() {
                return Err(format!(
                    "PalmDB record {i} offset {off} out of range ({}..={})",
                    table_end,
                    data.len()
                ));
            }
            if let Some(&prev) = offsets.last() {
                if off < prev {
                    return Err(format!("PalmDB record {i} offset goes backwards"));
                }
            }
            offsets.push(off);
        }
        offsets.push(data.len());
        Ok(PalmDb { data, offsets })
    }

    fn count(&self) -> usize {
        self.offsets.len() - 1
    }

    fn record(&self, i: usize) -> Result<&'a [u8], String> {
        if i + 1 >= self.offsets.len() {
            return Err(format!(
                "MOBI record {i} out of range (0..{})",
                self.count()
            ));
        }
        Ok(&self.data[self.offsets[i]..self.offsets[i + 1]])
    }
}

// ---------------------------------------------------------------------------
// record 0：PalmDOC header + MOBI header
// ---------------------------------------------------------------------------

/// record 0 里我们真正用得上的字段。
struct Part {
    compression: u16,
    text_len: usize,
    text_records: usize,
    encoding: u32,
    /// `version >= 8 || mobi_type == 248`：正文是 KF8。
    kf8: bool,
    /// HUFF/CDIC 记录起始下标 + 条数（含 HUFF 自己）。
    huff_start: usize,
    huff_count: usize,
    /// 尾部附加数据开关（MOBI header @0xF2）。
    extra_flags: u16,
    /// KF8 的 FDST 记录下标；只有 `version >= 8` 才有意义。
    fdst: Option<usize>,
}

impl Part {
    fn parse(rec0: &[u8]) -> Result<Part, String> {
        need(rec0, 0, 16)?;
        let compression = be16(rec0, 0)?;
        let text_len = be32(rec0, 4)? as usize;
        let text_records = be16(rec0, 8)? as usize;
        let encryption = be16(rec0, 12)?;
        if encryption != 0 {
            return Err("this book is DRM-protected".into());
        }
        if rec0.len() < 20 || &rec0[16..20] != b"MOBI" {
            return Err("not a MOBI book (missing MOBI header)".into());
        }
        let mobi_len = be32(rec0, 20)? as usize;
        if mobi_len < 0x18 {
            return Err(format!("invalid MOBI header length ({mobi_len})"));
        }
        if mobi_len > rec0.len().saturating_sub(16) {
            return Err("truncated MOBI header".into());
        }
        let mobi_type = be32(rec0, 24)?;
        let encoding = be32(rec0, 28)?;
        let version = be32(rec0, 36)?;
        if encoding != 65001 && encoding != 1252 {
            return Err(format!("unsupported MOBI text encoding ({encoding})"));
        }
        if !matches!(compression, 1 | 2 | 17480) {
            return Err(format!("unsupported MOBI compression ({compression})"));
        }

        // DRM 第二道确认：MOBI 头里的 DRM offset/count 不是 0xFFFFFFFF。
        // 只认「两个字段都有效且 count > 0」，免得把正常书误判成加密。
        if rec0.len() >= 0xB0 {
            let drm_off = be32(rec0, 0xA8)?;
            let drm_cnt = be32(rec0, 0xAC)?;
            if drm_off != 0xFFFF_FFFF && drm_cnt != 0xFFFF_FFFF && drm_cnt > 0 {
                return Err("this book is DRM-protected".into());
            }
        }

        let (huff_off, huff_num) = if rec0.len() >= 0x78 {
            (be32(rec0, 0x70)?, be32(rec0, 0x74)?)
        } else {
            (0xFFFF_FFFF, 0)
        };
        let (huff_start, huff_count) = if huff_off == 0xFFFF_FFFF || huff_num == 0 {
            (0, 0)
        } else {
            (huff_off as usize, huff_num as usize)
        };

        let extra_flags = if mobi_len >= 0xE4 && version >= 5 {
            be16(rec0, 0xF2)?
        } else {
            0
        };

        let kf8 = version >= 8 || mobi_type == 248;
        let fdst = if version >= 8 && rec0.len() >= 0xC4 {
            let off = be32(rec0, 0xC0)?;
            if off == 0xFFFF_FFFF || off == 0 {
                None
            } else {
                Some(off as usize)
            }
        } else {
            None
        };

        Ok(Part {
            compression,
            text_len,
            text_records,
            encoding,
            kf8,
            huff_start,
            huff_count,
            extra_flags,
            fdst,
        })
    }
}

// ---------------------------------------------------------------------------
// 正文：剥尾部 → 解压 → 按编码解码
// ---------------------------------------------------------------------------

fn decompress(part: &Part, db: &PalmDb<'_>) -> Result<String, String> {
    if part.text_records == 0 {
        return Ok(String::new());
    }
    if 1 + part.text_records > db.count() {
        return Err(format!(
            "MOBI text records run past end of file ({} records, {} text)",
            db.count(),
            part.text_records
        ));
    }

    // extra_data_flags：bit0 = 多字节字符重叠，其余每置一位就是一条尾部附加数据
    let multibyte = part.extra_flags & 1 != 0;
    let trailers = (part.extra_flags >> 1).count_ones() as usize;

    let mut huff = if part.compression == 17480 {
        Some(HuffCdic::load(part, db)?)
    } else {
        None
    };

    // text_len 是权威总长；为 0 时退化成「有多少收多少」
    let target = if part.text_len == 0 {
        MAX_TEXT
    } else {
        part.text_len.min(MAX_TEXT)
    };
    let mut out: Vec<u8> = Vec::new();
    for i in 0..part.text_records {
        let rec = db.record(1 + i)?;
        let rec = trim_trailing(rec, trailers, multibyte);
        match part.compression {
            1 => out.extend_from_slice(rec),
            2 => out.extend_from_slice(&palmdoc(rec)?),
            17480 => {
                let huff = huff
                    .as_mut()
                    .ok_or_else(|| "HUFF/CDIC state missing".to_string())?;
                out.extend_from_slice(&huff.unpack(rec, 0)?);
            }
            _ => return Err(format!("unsupported MOBI compression ({})", part.compression)),
        }
        if out.len() >= target {
            break;
        }
    }
    out.truncate(target);
    // KF8 的正文按 flow 分片：flow 0 是 xhtml，后面是 CSS / SVG 之类的资源。
    // 只取 flow 0，不然样式表会被当正文读出来。
    if let Some((start, end)) = first_flow(part, db, out.len()) {
        out.truncate(end);
        if start > 0 {
            out.drain(..start);
        }
    }
    Ok(decode_text(&out, part.encoding))
}

/// FDST 表里的第一个 flow 的 `[start, end)`；没有表或不合法就返回 `None`。
fn first_flow(part: &Part, db: &PalmDb<'_>, raw_len: usize) -> Option<(usize, usize)> {
    let idx = part.fdst?;
    let rec = db.record(idx).ok()?;
    if rec.len() < 20 || &rec[..4] != b"FDST" {
        return None;
    }
    let flows = be32(rec, 8).ok()? as usize;
    let table_end = flows.checked_mul(8).and_then(|n| n.checked_add(12))?;
    if flows == 0 || table_end > rec.len() {
        return None;
    }
    let start = (be32(rec, 12).ok()? as usize).min(raw_len);
    let end = (be32(rec, 16).ok()? as usize).min(raw_len);
    if start >= end {
        return None;
    }
    Some((start, end))
}

/// 一条记录尾部的附加数据长度（照 KindleUnpack 的 `getSizeOfTrailingDataEntry`）。
///
/// 尾部是「大端分组的变长整数」：最高位组带 `0x80`，字节顺序从组高位到低位。
/// 只看最后 4 字节——KDP 的编码器不会让它超过 4 字节，`getRawML` 也这么干。
fn trailing_entry_size(data: &[u8]) -> usize {
    let start = data.len().saturating_sub(4);
    let mut num = 0usize;
    for &v in &data[start..] {
        if v & 0x80 != 0 {
            num = 0;
        }
        num = (num << 7) | (v & 0x7f) as usize;
    }
    num
}

fn trim_trailing(mut data: &[u8], trailers: usize, multibyte: bool) -> &[u8] {
    for _ in 0..trailers {
        let n = trailing_entry_size(data);
        if n == 0 || n > data.len() {
            break;
        }
        data = &data[..data.len() - n];
    }
    if multibyte && !data.is_empty() {
        let n = (data[data.len() - 1] & 3) as usize + 1;
        if n <= data.len() {
            data = &data[..data.len() - n];
        }
    }
    data
}

/// PalmDOC LZ77。回溯拷贝允许自重叠（`dist` 可以小于 `len`），所以逐字节推。
fn palmdoc(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut out: Vec<u8> = Vec::with_capacity(data.len() * 2);
    let mut i = 0usize;
    while i < data.len() {
        let c = data[i];
        i += 1;
        match c {
            0x00 => out.push(0),
            0x01..=0x08 => {
                let n = c as usize;
                if i + n > data.len() {
                    return Err("truncated PalmDOC literal run".into());
                }
                out.extend_from_slice(&data[i..i + n]);
                i += n;
            }
            0x09..=0x7f => out.push(c),
            0x80..=0xbf => {
                let Some(&c2) = data.get(i) else {
                    return Err("truncated PalmDOC back-reference".into());
                };
                i += 1;
                let pair = ((c as u16) << 8) | c2 as u16;
                let dist = ((pair >> 3) & 0x07ff) as usize;
                let len = (pair & 7) as usize + 3;
                if dist == 0 || dist > out.len() {
                    return Err(format!(
                        "bad PalmDOC back-reference (distance {dist}, {} bytes decoded)",
                        out.len()
                    ));
                }
                // 故意允许自重叠：dist=1 时就是把刚写进去的字节复制 len 次
                for _ in 0..len {
                    let b = out[out.len() - dist];
                    out.push(b);
                }
            }
            _ => {
                out.push(b' ');
                out.push(c ^ 0x80);
            }
        }
        if out.len() > MAX_TEXT {
            return Err("MOBI record expands past 32 MB".into());
        }
    }
    Ok(out)
}

/// 字节 → 字符串。`65001` = UTF-8（坏字节替换掉），`1252` = windows-1252。
fn decode_text(bytes: &[u8], encoding: u32) -> String {
    if encoding == 65001 {
        String::from_utf8_lossy(bytes)
            .trim_start_matches('\u{feff}')
            .to_string()
    } else {
        let mut s = String::with_capacity(bytes.len());
        for &b in bytes {
            s.push(cp1252_char(b));
        }
        s
    }
}

/// windows-1252：只有 0x80..0x9F 这 32 个和 Latin-1 不同，其余直通。
fn cp1252_char(b: u8) -> char {
    const HIGH: [char; 32] = [
        '\u{20ac}', '\u{0081}', '\u{201a}', '\u{0192}', '\u{201e}', '\u{2026}', '\u{2020}',
        '\u{2021}', '\u{02c6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008d}',
        '\u{017d}', '\u{008f}', '\u{0090}', '\u{2018}', '\u{2019}', '\u{201c}', '\u{201d}',
        '\u{2022}', '\u{2013}', '\u{2014}', '\u{02dc}', '\u{2122}', '\u{0161}', '\u{203a}',
        '\u{0153}', '\u{009d}', '\u{017e}', '\u{0178}',
    ];
    if (0x80..=0x9f).contains(&b) {
        HIGH[(b - 0x80) as usize]
    } else {
        b as char
    }
}

// ---------------------------------------------------------------------------
// HUFF / CDIC
// ---------------------------------------------------------------------------

/// HUFF 码表 + 递归展开后的短语字典。
///
/// 字段含义照 KindleUnpack：
/// * `code[i] = (codelen, term, maxcode)`，`i = code 的高 8 位`；`maxcode`
///   已经归一化成 32 位码空间里的实际值。
/// * `min[len]` / `max[len]`：该码长下的码字区间，`term == false` 时用。
/// * `phrases[r] = (bytes, 已展开)`；`0x8000` 位没置的短语本身还是 HUFF 流。
struct HuffCdic {
    code: [(u8, bool, u64); 256],
    min: [u64; 33],
    max: [u64; 33],
    phrases: Vec<Option<(Vec<u8>, bool)>>,
}

impl HuffCdic {
    fn load(part: &Part, db: &PalmDb<'_>) -> Result<HuffCdic, String> {
        if part.huff_count == 0 {
            return Err(
                "HUFF/CDIC compression is not supported (no HUFF/CDIC records)".into(),
            );
        }
        let mut huff = None;
        let mut cdics: Vec<&[u8]> = Vec::new();
        for k in 0..part.huff_count {
            let idx = part.huff_start.checked_add(k).ok_or("HUFF record index overflow")?;
            let rec = db.record(idx)?;
            if k == 0 {
                huff = Some(rec);
            } else {
                cdics.push(rec);
            }
        }
        HuffCdic::from_records(huff.unwrap_or(&[]), &cdics)
    }

    fn from_records(huff: &[u8], cdics: &[&[u8]]) -> Result<HuffCdic, String> {
        let mut dec = HuffCdic {
            code: [(0, false, 0); 256],
            min: [0; 33],
            max: [0; 33],
            phrases: Vec::new(),
        };
        dec.load_huff(huff)?;
        for cdic in cdics {
            dec.load_cdic(cdic)?;
        }
        Ok(dec)
    }

    fn load_huff(&mut self, huff: &[u8]) -> Result<(), String> {
        if huff.len() < 24 || &huff[..4] != b"HUFF" {
            return Err("invalid HUFF record".into());
        }
        // @8 = base 表（256 个码长项），@12 = min/max 表
        let off1 = be32(huff, 8)? as usize;
        let off2 = be32(huff, 12)? as usize;
        if off1 > huff.len().saturating_sub(1024) || off2 > huff.len().saturating_sub(256) {
            return Err("truncated HUFF tables".into());
        }
        for i in 0..256 {
            let v = be32(huff, off1 + i * 4)?;
            let codelen = (v & 0x1f) as u8;
            let term = v & 0x80 != 0;
            let maxcode = (v >> 8) as u64;
            if codelen == 0 {
                return Err("invalid HUFF code length 0".into());
            }
            if codelen <= 8 && !term {
                return Err("invalid HUFF table (short code without term bit)".into());
            }
            // maxcode = ((maxcode + 1) << (32 - codelen)) - 1
            let norm = (maxcode + 1) << (32 - codelen as u32);
            if norm == 0 || norm - 1 > u32::MAX as u64 {
                return Err("invalid HUFF maxcode".into());
            }
            self.code[i] = (codelen, term, norm - 1);
        }
        for len in 1..=32usize {
            let min = be32(huff, off2 + (len - 1) * 8)? as u64;
            let max = be32(huff, off2 + (len - 1) * 8 + 4)? as u64;
            let min = min << (32 - len);
            let max = ((max + 1) << (32 - len)) - 1;
            if min > u32::MAX as u64 || max > u32::MAX as u64 {
                return Err("invalid HUFF min/max table".into());
            }
            self.min[len] = min;
            self.max[len] = max;
        }
        Ok(())
    }

    fn load_cdic(&mut self, cdic: &[u8]) -> Result<(), String> {
        if cdic.len() < 16 || &cdic[..4] != b"CDIC" {
            return Err("invalid CDIC record".into());
        }
        let phrases = be32(cdic, 8)? as usize;
        let bits = be32(cdic, 12)? as usize;
        if bits == 0 || bits > 16 {
            return Err(format!("invalid CDIC code length ({bits})"));
        }
        let n = (1usize << bits).min(phrases.saturating_sub(self.phrases.len()));
        if n == 0 {
            return Ok(());
        }
        let table_end = 16 + n * 2;
        if table_end > cdic.len() {
            return Err("truncated CDIC offset table".into());
        }
        for k in 0..n {
            let off = be16(cdic, 16 + k * 2)? as usize;
            let at = 16 + off;
            if at + 2 > cdic.len() {
                return Err("CDIC phrase offset out of range".into());
            }
            let blen = be16(cdic, at)?;
            let len = (blen & 0x7fff) as usize;
            if at + 2 + len > cdic.len() {
                return Err("truncated CDIC phrase".into());
            }
            self.phrases
                .push(Some((cdic[at + 2..at + 2 + len].to_vec(), blen & 0x8000 != 0)));
        }
        Ok(())
    }

    /// 解一条正文记录。返回的是展开后的字节。
    fn unpack(&mut self, data: &[u8], depth: usize) -> Result<Vec<u8>, String> {
        if depth > MAX_HUFF_DEPTH {
            return Err("HUFF/CDIC phrase recursion too deep".into());
        }
        let mut bits_left = data.len() as i64 * 8;
        let mut pos = 0usize;
        let mut x = window64(data, 0);
        let mut n: i32 = 32;
        let mut out: Vec<u8> = Vec::new();
        loop {
            // 32 bit 滑窗用完了：往后挪 4 字节重新装 64 bit
            if n <= 0 {
                pos += 4;
                x = window64(data, pos);
                n += 32;
            }
            let code = ((x >> n) & 0xffff_ffff) as u32;
            let (codelen, term, mut maxcode) = self.code[(code >> 24) as usize];
            let mut codelen = codelen as usize;
            if !term {
                // 码长不够就顺着 mincode 往上找，直到落进某个长度区间
                while codelen <= 32 && (code as u64) < self.min[codelen] {
                    codelen += 1;
                }
                if codelen > 32 {
                    return Err("invalid HUFF code".into());
                }
                maxcode = self.max[codelen];
            }
            n -= codelen as i32;
            bits_left -= codelen as i64;
            if bits_left < 0 {
                break;
            }
            if maxcode < code as u64 {
                return Err("invalid HUFF code range".into());
            }
            let r = ((maxcode - code as u64) >> (32 - codelen)) as usize;
            if r >= self.phrases.len() {
                return Err(format!(
                    "HUFF/CDIC phrase {r} out of range ({} phrases)",
                    self.phrases.len()
                ));
            }
            // 先 take 掉：短语自引用时会变成 None → 报错，不会无限递归
            let Some((slice, expanded)) = self.phrases[r].take() else {
                return Err("recursive HUFF/CDIC phrase".into());
            };
            let bytes = if expanded {
                slice
            } else {
                self.unpack(&slice, depth + 1)?
            };
            out.extend_from_slice(&bytes);
            self.phrases[r] = Some((bytes, true));
            if out.len() > MAX_TEXT {
                return Err("HUFF/CDIC output too large".into());
            }
        }
        Ok(out)
    }
}

/// 从 `pos` 起读 8 字节大端，越界补 0（KindleUnpack 也是补 8 个零字节）。
fn window64(b: &[u8], pos: usize) -> u64 {
    let mut v = 0u64;
    for k in 0..8 {
        v = (v << 8) | b.get(pos + k).copied().unwrap_or(0) as u64;
    }
    v
}

// ---------------------------------------------------------------------------
// MOBI HTML → 块
// ---------------------------------------------------------------------------

const BLOCK_TAGS: [&str; 25] = [
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
    "address",
    "hr",
];

fn is_block_tag(name: &str) -> bool {
    BLOCK_TAGS.iter().any(|t| name.eq_ignore_ascii_case(t))
}

fn is_skip_tag(name: &str) -> bool {
    ["script", "style", "head", "title"]
        .iter()
        .any(|t| name.eq_ignore_ascii_case(t))
}

fn kind_level(k: Kind) -> u8 {
    match k {
        Kind::Body => 0,
        Kind::H1 => 1,
        Kind::H2 => 2,
        Kind::H3 => 3,
        // MOBI 的内嵌图（recindex 资源）还没解，这里只保证 match 穷尽
        Kind::Image => 0,
    }
}

fn heading_kind(name: &str) -> Option<Kind> {
    let n = name.as_bytes();
    if n.len() != 2 || n[0] != b'h' || !(b'1'..=b'6').contains(&n[1]) {
        return None;
    }
    Some(match n[1] {
        b'1' => Kind::H1,
        b'2' => Kind::H2,
        _ => Kind::H3,
    })
}

/// HTML → 块。标签流扫描，不建树：MOBI 里 `<p>` 常常不闭合。
fn blocks_from_html(html: &str) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut cur = String::new();
    let mut kind = Kind::Body;
    let mut skip: Option<String> = None;
    scan(html, |tok| match tok {
        Tok::Text(t) => {
            if skip.is_none() {
                push_text(&mut cur, t);
            }
        }
        Tok::Open(name) => {
            if skip.is_some() {
                // 烂书可能不写 </head>：见到 <body> 就强制结束 head
                if matches!(skip.as_deref(), Some(s) if s.eq_ignore_ascii_case("head"))
                    && name.eq_ignore_ascii_case("body")
                {
                    skip = None;
                } else {
                    return;
                }
            }
            if is_skip_tag(name) {
                skip = Some(name.to_string());
                return;
            }
            if name.eq_ignore_ascii_case("br") {
                cur.push('\n');
                return;
            }
            if name.eq_ignore_ascii_case("pagebreak") {
                flush(&mut out, &mut cur, &mut kind);
                return;
            }
            if let Some(h) = heading_kind(name) {
                flush(&mut out, &mut cur, &mut kind);
                kind = h;
                return;
            }
            if is_block_tag(name) {
                flush(&mut out, &mut cur, &mut kind);
            }
        }
        Tok::Close(name) => {
            if let Some(s) = skip.as_deref() {
                if name.eq_ignore_ascii_case(s) {
                    skip = None;
                }
                return;
            }
            if heading_kind(name).is_some() || is_block_tag(name) {
                flush(&mut out, &mut cur, &mut kind);
            }
        }
    });
    flush(&mut out, &mut cur, &mut kind);
    out
}

fn flush(out: &mut Vec<Block>, cur: &mut String, kind: &mut Kind) {
    let text = cur.trim();
    if !text.is_empty() {
        out.push(Block {
            kind: *kind,
            text: text.to_string(),
            id: String::new(),
            img: None,
        });
    }
    cur.clear();
    *kind = Kind::Body;
}

fn push_text(cur: &mut String, raw: &str) {
    let decoded = decode_entities(raw);
    let mut prev_space = cur.is_empty() || cur.ends_with(' ') || cur.ends_with('\n');
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

// ---------------------------------------------------------------------------
// 极简 HTML 扫描
// ---------------------------------------------------------------------------

enum Tok<'a> {
    Open(&'a str),
    Close(&'a str),
    Text(&'a str),
}

/// 扫标签流。注释 / CDATA / `<!DOCTYPE>` / `<?xml?>` 都跳过；
/// 标签名只留本地名（`mbp:pagebreak` → `pagebreak`）。
fn scan(s: &str, mut f: impl FnMut(Tok<'_>)) {
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
        text_start = i;
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
        let Some(end) = tag_end(b, i + 1) else {
            break;
        };
        let inner = &s[i + 1..end];
        let close = inner.starts_with('/');
        let body = inner.trim_start_matches('/').trim_end();
        let body = body.trim_end_matches('/').trim_end();
        let name = body
            .split(|c: char| c.is_whitespace())
            .next()
            .unwrap_or("");
        let name = match name.rfind(':') {
            Some(k) => &name[k + 1..],
            None => name,
        };
        if !name.is_empty() {
            if close {
                f(Tok::Close(name));
            } else {
                f(Tok::Open(name));
            }
        }
        i = end + 1;
        text_start = i;
    }
    if text_start < b.len() {
        f(Tok::Text(&s[text_start..]));
    }
}

/// 找标签的 `>`，跳过引号里的内容（属性值里可以有 `>`）。
fn tag_end(b: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
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

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < s.len() {
        let ch = match s[i..].chars().next() {
            Some(c) => c,
            None => break,
        };
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
        // 断行时要把 nbsp 当普通空格吃掉
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

// ---------------------------------------------------------------------------
// 块 → 章节 + 目录
// ---------------------------------------------------------------------------

fn build_book(html: &str) -> Book {
    let blocks = blocks_from_html(html);
    if blocks.is_empty() {
        return Book::default();
    }
    let split = pick_split_level(&blocks);
    let mut chapters: Vec<Chapter> = Vec::new();
    if split == 0 {
        // 一个标题都没有：每 100 段切一章，至少让人能跳
        for chunk in blocks.chunks(100) {
            chapters.push(Chapter {
                blocks: chunk.to_vec(),
            });
        }
    } else {
        let mut cur: Vec<Block> = Vec::new();
        for b in blocks {
            if kind_level(b.kind) == split && !cur.is_empty() {
                chapters.push(Chapter {
                    blocks: std::mem::take(&mut cur),
                });
            }
            cur.push(b);
        }
        if !cur.is_empty() {
            chapters.push(Chapter { blocks: cur });
        }
    }
    if chapters.is_empty() {
        return Book::default();
    }
    let (toc, path_index) = build_toc(&chapters);
    Book {
        chapters,
        toc,
        path_index,
    }
}

/// 用最外层的标题切章：有 h1 用 h1，没有用 h2，再没有就 0（按段数切）。
fn pick_split_level(blocks: &[Block]) -> u8 {
    for level in 1..=3u8 {
        if blocks.iter().any(|b| kind_level(b.kind) == level) {
            return level;
        }
    }
    0
}

fn build_toc(chapters: &[Chapter]) -> (Vec<TocEntry>, HashMap<String, usize>) {
    let mut toc = Vec::new();
    let mut path_index = HashMap::new();
    for (ci, ch) in chapters.iter().enumerate() {
        let path = ci.to_string();
        path_index.insert(path.clone(), ci);
        let mut headings = 0;
        for b in &ch.blocks {
            let level = kind_level(b.kind);
            if level > 0 {
                toc.push(TocEntry {
                    level,
                    title: b.text.clone(),
                    path: path.clone(),
                    anchor: String::new(),
                });
                headings += 1;
            }
        }
        if headings == 0 {
            // 没有标题的章（前言 / 按 100 段切的块）也给个目录项，
            // 否则长书翻起来没参照。
            let title = ch
                .blocks
                .iter()
                .find(|b| !b.text.trim().is_empty())
                .map(|b| short_title(&b.text))
                .unwrap_or_else(|| format!("Section {}", ci + 1));
            toc.push(TocEntry {
                level: 1,
                title,
                path: path.clone(),
                anchor: String::new(),
            });
        }
    }
    (toc, path_index)
}

fn short_title(text: &str) -> String {
    let mut s: String = text.chars().take(30).collect();
    if text.chars().count() > 30 {
        s.push('…');
    }
    s
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn need(b: &[u8], off: usize, n: usize) -> Result<(), String> {
    if off.checked_add(n).is_none_or(|end| end > b.len()) {
        return Err(format!(
            "truncated MOBI data (need {n} bytes at {off}, have {})",
            b.len()
        ));
    }
    Ok(())
}

fn be16(b: &[u8], off: usize) -> Result<u16, String> {
    need(b, off, 2)?;
    Ok(u16::from_be_bytes([b[off], b[off + 1]]))
}

fn be32(b: &[u8], off: usize) -> Result<u32, String> {
    need(b, off, 4)?;
    Ok(u32::from_be_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
    ]))
}

// ---------------------------------------------------------------------------
// 测试：自己造 MOBI 字节流，不依赖外部文件
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- 造 PalmDB / MOBI ----------------

    #[derive(Clone)]
    struct Opts {
        compression: u16,
        encoding: u32,
        encryption: u16,
        version: u32,
        mobi_type: u32,
        extra_flags: u16,
        /// 每条正文记录尾部加上 trailing entry + 多字节重叠（测剥离）
        trailing: bool,
        /// KF8：正文里 flow1 的起点（测 FDST 只取 flow 0）
        flow_split: Option<usize>,
        /// HUFF/CDIC：是否真的写 HUFF/CDIC 记录
        with_huff: bool,
        /// 合订本：正文记录后面再塞 BOUNDARY + 一条假的 KF8 record 0
        combo: bool,
    }

    impl Default for Opts {
        fn default() -> Opts {
            Opts {
                compression: 1,
                encoding: 65001,
                encryption: 0,
                version: 6,
                mobi_type: 2,
                extra_flags: 0,
                trailing: false,
                flow_split: None,
                with_huff: true,
                combo: false,
            }
        }
    }

    /// PalmDOC LZ77 编码器：只为了测解码，够用就行。
    ///
    /// * 重复片段（>= 3 字节、距离 <= 2047）发回溯对
    /// * 其它字节按每段最多 8 个字的「字面量段」发（`0x01..=0x08`）
    fn palmdoc_compress(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < data.len() {
            let mut best = (0usize, 0usize);
            let start = i.saturating_sub(2047);
            for j in start..i {
                let mut len = 0usize;
                while len < 10 && i + len < data.len() && data[j + len] == data[i + len] {
                    len += 1;
                }
                if len >= 3 && len > best.1 {
                    best = (i - j, len);
                }
            }
            if best.1 >= 3 {
                let pair = ((best.0 as u16) << 3) | (best.1 as u16 - 3);
                out.push(0x80 | (pair >> 8) as u8);
                out.push((pair & 0xff) as u8);
                i += best.1;
            } else {
                let n = (data.len() - i).min(8);
                out.push(n as u8);
                out.extend_from_slice(&data[i..i + n]);
                i += n;
            }
        }
        out
    }

    fn pdb(records: &[Vec<u8>]) -> Vec<u8> {
        let n = records.len();
        let mut out = vec![0u8; 78 + 8 * n];
        out[..4].copy_from_slice(b"TEST");
        out[60..64].copy_from_slice(b"BOOK");
        out[64..68].copy_from_slice(b"MOBI");
        out[76..78].copy_from_slice(&(n as u16).to_be_bytes());
        let mut pos = 78 + 8 * n;
        for (i, r) in records.iter().enumerate() {
            out[78 + i * 8..78 + i * 8 + 4].copy_from_slice(&(pos as u32).to_be_bytes());
            pos += r.len();
        }
        for r in records {
            out.extend_from_slice(r);
        }
        out
    }

    /// 造一张「8 bit 定长码」的 HUFF 表 + CDIC 字典，字节进字节出，
    /// 这样 HUFF/CDIC 的位读取、字典索引、短语展开都能被测到。
    ///
    /// 码字 `i`（= 输入字节）解出的字典下标是 `255 - i`，短语内容 `255 - r`，
    /// 合起来就是原字节。`recursive` 时把 `r=255` 那条改成未展开短语，
    /// 里面放一个还是 HUFF 流的 `0xFF`，解出来是 `Q`（测递归展开）。
    fn huff_records(recursive: bool) -> Vec<Vec<u8>> {
        let mut huff = vec![0u8; 24 + 256 * 4 + 64 * 4];
        huff[0..4].copy_from_slice(b"HUFF");
        huff[4..8].copy_from_slice(&24u32.to_be_bytes());
        huff[8..12].copy_from_slice(&24u32.to_be_bytes());
        huff[12..16].copy_from_slice(&(24u32 + 1024).to_be_bytes());
        for i in 0..256 {
            // codelen=8, term=1, maxcode=0xFF → 归一化后 0xFFFFFFFF
            let v: u32 = 0x80 | 0x08 | (0xff << 8);
            let o = 24 + i * 4;
            huff[o..o + 4].copy_from_slice(&v.to_be_bytes());
        }

        let mut cdic = vec![0u8; 16 + 256 * 2 + 256 * 3];
        cdic[0..4].copy_from_slice(b"CDIC");
        cdic[4..8].copy_from_slice(&16u32.to_be_bytes());
        cdic[8..12].copy_from_slice(&256u32.to_be_bytes());
        cdic[12..16].copy_from_slice(&8u32.to_be_bytes());
        for r in 0..256usize {
            let off = (512 + r * 3) as u16;
            cdic[16 + r * 2..16 + r * 2 + 2].copy_from_slice(&off.to_be_bytes());
            let p = 16 + off as usize;
            let (blen, byte) = if recursive && r == 255 {
                (0x0001u16, 0xffu8)
            } else if recursive && r == 0 {
                (0x8001u16, b'Q')
            } else {
                (0x8001u16, (255 - r) as u8)
            };
            cdic[p..p + 2].copy_from_slice(&blen.to_be_bytes());
            cdic[p + 2] = byte;
        }
        vec![huff, cdic]
    }

    fn build_mobi(opts: &Opts, body: &[u8]) -> Vec<u8> {
        let mut records: Vec<Vec<u8>> = Vec::new();
        for chunk in body.chunks(4096) {
            let mut rec = if opts.compression == 2 {
                palmdoc_compress(chunk)
            } else {
                chunk.to_vec()
            };
            if opts.trailing {
                // 多字节字符重叠 2 字节，再放 1 条 trailing entry：
                // 载荷 "TR" + 变长整数 0x83（= 含自己在内共 3 字节）
                rec.extend_from_slice(&[0x41, 0x01]);
                rec.extend_from_slice(b"TR\x83");
            }
            records.push(rec);
        }
        let text_records = records.len();

        let mut huff_off = 0xFFFF_FFFFu32;
        let mut huff_num = 0u32;
        if opts.compression == 17480 && opts.with_huff {
            let huff = huff_records(false);
            huff_off = (1 + text_records) as u32;
            huff_num = huff.len() as u32;
            records.extend(huff);
        }

        let mut fdst_off = 0xFFFF_FFFFu32;
        if let Some(split) = opts.flow_split {
            let mut f = Vec::new();
            f.extend_from_slice(b"FDST");
            f.extend_from_slice(&12u32.to_be_bytes());
            f.extend_from_slice(&2u32.to_be_bytes());
            for v in [0usize, split, split, body.len()] {
                f.extend_from_slice(&(v as u32).to_be_bytes());
            }
            fdst_off = (1 + records.len()) as u32;
            records.push(f);
        }
        if opts.combo {
            // 合订本结构：KF7 正文之后是 BOUNDARY，再后面才是 KF8 record 0。
            // 我们只认 record 0 的 KF7 段，这两条应该被无视。
            records.push(b"BOUNDARY".to_vec());
            records.push(vec![0u8; 64]);
        }

        // record 0：PalmDOC header(16) + MOBI header(0xE4 = 228) = 244 字节
        let mut r0 = vec![0u8; 244];
        r0[0..2].copy_from_slice(&opts.compression.to_be_bytes());
        r0[4..8].copy_from_slice(&(body.len() as u32).to_be_bytes());
        r0[8..10].copy_from_slice(&(text_records as u16).to_be_bytes());
        r0[10..12].copy_from_slice(&4096u16.to_be_bytes());
        r0[12..14].copy_from_slice(&opts.encryption.to_be_bytes());
        r0[16..20].copy_from_slice(b"MOBI");
        r0[20..24].copy_from_slice(&0xE4u32.to_be_bytes());
        r0[24..28].copy_from_slice(&opts.mobi_type.to_be_bytes());
        r0[28..32].copy_from_slice(&opts.encoding.to_be_bytes());
        r0[32..36].copy_from_slice(&0x1234_5678u32.to_be_bytes());
        r0[36..40].copy_from_slice(&opts.version.to_be_bytes());
        r0[0x70..0x74].copy_from_slice(&huff_off.to_be_bytes());
        r0[0x74..0x78].copy_from_slice(&huff_num.to_be_bytes());
        r0[0x80..0x84].copy_from_slice(&0u32.to_be_bytes()); // 无 EXTH
        r0[0xA8..0xAC].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        r0[0xAC..0xB0].copy_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
        r0[0xC0..0xC4].copy_from_slice(&fdst_off.to_be_bytes());
        r0[0xF2..0xF4].copy_from_slice(&opts.extra_flags.to_be_bytes());
        records.insert(0, r0);
        pdb(&records)
    }

    /// 直接从字节走一遍「解压 + 解码」，方便断言原文。
    fn body_of(opts: &Opts, body: &[u8]) -> Result<String, String> {
        let bytes = build_mobi(opts, body);
        let db = PalmDb::parse(&bytes)?;
        let part = Part::parse(db.record(0)?)?;
        decompress(&part, &db)
    }

    fn doc_of(opts: &Opts, body: &[u8]) -> Result<MobiDoc, String> {
        MobiDoc::from_bytes(&build_mobi(opts, body))
    }

    fn page_text(doc: &MobiDoc, idx: usize) -> String {
        doc.inner.page_text(idx).unwrap_or_default()
    }

    // ---------------- 容器 / 无压缩 ----------------

    #[test]
    fn uncompressed_mobi_opens_and_reads_text() {
        let doc = doc_of(&Opts::default(), b"<p>Hello MOBI reader</p>").expect("open");
        assert_eq!(doc.format(), Format::Mobi);
        assert_eq!(doc.page_count(), 1);
        let t = page_text(&doc, 0);
        assert!(t.contains("Hello MOBI reader"), "page_text = {t:?}");
    }

    #[test]
    fn palmdb_offsets_and_render_shape() {
        let doc = doc_of(&Opts::default(), b"<p>one two three</p>").expect("open");
        let w = 300;
        let h = 420;
        let g = doc.render(0, w, h).expect("render");
        assert_eq!(g.len(), w * h);
        assert!(g.iter().any(|&p| p < 200), "页面上应该有字");
        assert!((doc.aspect(0) - crate::doc::reflow::PAGE_W / crate::doc::reflow::PAGE_H).abs() < 1e-6);
    }

    #[test]
    fn trailing_data_is_stripped_before_decompression() {
        let opts = Opts {
            extra_flags: 0b11, // multibyte + 1 条 trailing entry
            trailing: true,
            ..Opts::default()
        };
        let body = b"<p>trailing data stripped</p>";
        assert_eq!(body_of(&opts, body).expect("decompress"), "<p>trailing data stripped</p>");
    }

    // ---------------- PalmDOC LZ77 ----------------

    #[test]
    fn palmdoc_roundtrip_with_backrefs() {
        let text = "<p>abcabcabcabcabcabcabcabc</p><p>abcabcabcabcabcabcabcabc</p>";
        let packed = palmdoc_compress(text.as_bytes());
        // 有回溯对（0x80..=0xBF 作首字节）才算测到
        assert!(
            packed.iter().any(|b| (0x80..=0xbf).contains(b)),
            "编码器没产生回溯"
        );
        assert_eq!(palmdoc(&packed).expect("decode"), text.as_bytes());
    }

    #[test]
    fn palmdoc_mobi_decompresses() {
        let opts = Opts {
            compression: 2,
            ..Opts::default()
        };
        // 跨记录重复，让回溯 + 分块都走到
        let mut text = String::from("<h1>Compressed</h1><p>");
        for _ in 0..200 {
            text.push_str("repeat repeat repeat ");
        }
        text.push_str("</p>");
        let got = body_of(&opts, text.as_bytes()).expect("decompress");
        assert_eq!(got, text);
        let doc = doc_of(&opts, text.as_bytes()).expect("open");
        assert!(page_text(&doc, 0).contains("repeat repeat"));
    }

    #[test]
    fn palmdoc_bad_backref_is_err_not_panic() {
        // 0x80 0x00：dist = 0，非法
        assert!(palmdoc(&[0x80, 0x00]).is_err());
        // 回溯对缺第二个字节
        assert!(palmdoc(&[0x80]).is_err());
    }

    // ---------------- HUFF / CDIC ----------------

    #[test]
    fn huffcdic_decodes_whole_book() {
        let opts = Opts {
            compression: 17480,
            ..Opts::default()
        };
        let body = "<h1>HUFF</h1><p>Hello HUFF/CDIC world</p>";
        assert_eq!(body_of(&opts, body.as_bytes()).expect("decompress"), body);
        let doc = doc_of(&opts, body.as_bytes()).expect("open");
        assert!(
            page_text(&doc, 0).contains("Hello HUFF/CDIC world"),
            "page_text = {:?}",
            page_text(&doc, 0)
        );
    }

    #[test]
    fn huffcdic_recursive_phrase() {
        let recs = huff_records(true);
        let mut dec = HuffCdic::from_records(&recs[0], &[&recs[1]]).expect("load");
        // 0x00 → 字典 r=255（未展开，里面是 0xFF）→ r=0 的 "Q"
        assert_eq!(dec.unpack(b"\x00", 0).expect("unpack"), b"Q");
    }

    #[test]
    fn huffcdic_missing_records_is_clear_error() {
        let opts = Opts {
            compression: 17480,
            with_huff: false,
            ..Opts::default()
        };
        let err = body_of(&opts, b"<p>x</p>").expect_err("should fail");
        assert!(err.contains("HUFF/CDIC"), "{err}");
    }

    // ---------------- 编码 ----------------

    #[test]
    fn cp1252_is_decoded() {
        let opts = Opts {
            encoding: 1252,
            ..Opts::default()
        };
        let body = b"<p>Price \x80 and \x93quotes\x94</p>";
        let got = body_of(&opts, body).expect("decompress");
        assert!(got.contains('€'), "{got:?}");
        assert!(got.contains('\u{201c}') && got.contains('\u{201d}'), "{got:?}");
        assert!(got.contains("Price"), "{got:?}");
    }

    #[test]
    fn utf8_text_is_decoded() {
        let doc = doc_of(&Opts::default(), "<p>简体中文 · café</p>".as_bytes()).expect("open");
        let t = page_text(&doc, 0);
        assert!(t.contains("简体中文"), "{t:?}");
        assert!(t.contains("café"), "{t:?}");
    }

    // ---------------- 目录 / 章节 ----------------

    #[test]
    fn h1_headings_become_toc() {
        let html = "<h1>Chapter One</h1><p>first body</p><h1>Chapter Two</h1><p>second body</p>";
        let doc = doc_of(&Opts::default(), html.as_bytes()).expect("open");
        let toc = doc.outline();
        assert_eq!(toc.len(), 2, "{:?}", toc.iter().map(|t| &t.title).collect::<Vec<_>>());
        assert_eq!(toc[0].title, "Chapter One");
        assert_eq!(toc[0].depth, 1);
        assert_eq!(toc[0].page, 0);
        assert_eq!(toc[1].title, "Chapter Two");
        assert_eq!(toc[1].page, 1, "章首另起一页");
        assert_eq!(doc.chapter_title(1).as_deref(), Some("Chapter Two"));
        assert_eq!(doc.page_count(), 2);
    }

    #[test]
    fn h2_fallback_and_subheadings_in_toc() {
        let html = "<h2>Part A</h2><p>a</p><h3>Sub A1</h3><p>a1</p><h2>Part B</h2><p>b</p>";
        let doc = doc_of(&Opts::default(), html.as_bytes()).expect("open");
        let toc = doc.outline();
        assert_eq!(toc.len(), 3);
        assert_eq!(toc[0].title, "Part A");
        assert_eq!(toc[0].depth, 2);
        assert_eq!(toc[1].title, "Sub A1");
        assert_eq!(toc[1].depth, 3);
    }

    #[test]
    fn no_headings_falls_back_to_hundred_block_chapters() {
        let mut html = String::new();
        for i in 0..250 {
            html.push_str(&format!("<p>paragraph number {i}</p>"));
        }
        let doc = doc_of(&Opts::default(), html.as_bytes()).expect("open");
        let toc = doc.outline();
        assert_eq!(toc.len(), 3, "250 段 → 3 章");
        assert_eq!(toc[0].page, 0);
    }

    #[test]
    fn entities_and_br_are_handled() {
        let html = "<p>a &amp; b &lt;tag&gt; &quot;q&quot; &#39;s&#39; x&nbsp;y &mdash; z</p>\
                    <p>line1<br/>line2</p>";
        let doc = doc_of(&Opts::default(), html.as_bytes()).expect("open");
        let t = page_text(&doc, 0);
        assert!(t.contains("a & b <tag> \"q\" 's' x y — z"), "{t:?}");
        assert!(t.contains("line1") && t.contains("line2"), "{t:?}");
    }

    #[test]
    fn unclosed_head_does_not_swallow_body() {
        let html = "<html><head><meta charset=\"utf-8\"><body><p>still visible</p></body></html>";
        let doc = doc_of(&Opts::default(), html.as_bytes()).expect("open");
        assert!(page_text(&doc, 0).contains("still visible"));
    }

    #[test]
    fn script_and_style_are_dropped() {
        let html = "<style>p { color: red }</style><script>alert('x')</script><p>visible</p>";
        let doc = doc_of(&Opts::default(), html.as_bytes()).expect("open");
        let t = page_text(&doc, 0);
        assert!(t.contains("visible"));
        assert!(!t.contains("color"), "{t:?}");
        assert!(!t.contains("alert"), "{t:?}");
    }

    // ---------------- KF8 / AZW3 ----------------

    #[test]
    fn kf8_header_is_azw3_and_flow0_only() {
        let first = b"<h1>KF8 Flow</h1><p>keep this paragraph</p>";
        let mut body = first.to_vec();
        body.extend_from_slice(b"body { color: red } .junk-css { display: none }");
        let opts = Opts {
            version: 8,
            mobi_type: 248,
            flow_split: Some(first.len()),
            ..Opts::default()
        };
        let got = body_of(&opts, &body).expect("decompress");
        assert_eq!(got, String::from_utf8_lossy(first));
        let doc = doc_of(&opts, &body).expect("open");
        assert_eq!(doc.format(), Format::Azw3);
        let t = page_text(&doc, 0);
        assert!(t.contains("keep this paragraph"), "{t:?}");
        assert!(!t.contains("junk-css"), "flow 1 不该进正文: {t:?}");
    }

    #[test]
    fn kf7_header_is_mobi() {
        let doc = doc_of(&Opts::default(), b"<p>x</p>").expect("open");
        assert_eq!(doc.format(), Format::Mobi);
    }

    #[test]
    fn combo_file_reads_kf7_part_and_ignores_kf8_records() {
        let opts = Opts {
            combo: true,
            ..Opts::default()
        };
        let body = b"<h1>KF7 Part</h1><p>old mobi content</p>";
        let bytes = build_mobi(&opts, body);
        let db = PalmDb::parse(&bytes).expect("palmdb");
        assert!(db.count() > 3, "BOUNDARY/KF8 记录没塞进去");
        let doc = MobiDoc::from_bytes(&bytes).expect("open");
        assert_eq!(doc.format(), Format::Mobi);
        assert_eq!(doc.outline()[0].title, "KF7 Part");
        assert!(page_text(&doc, 0).contains("old mobi content"));
    }

    // ---------------- 错误路径 ----------------

    #[test]
    fn drm_is_rejected_with_clear_message() {
        let opts = Opts {
            encryption: 2,
            ..Opts::default()
        };
        let err = doc_of(&opts, b"<p>x</p>")
            .err()
            .expect("DRM should fail");
        assert!(err.contains("DRM"), "{err}");
    }

    #[test]
    fn truncated_file_is_err_not_panic() {
        let bytes = build_mobi(&Opts::default(), b"<p>hello</p>");
        for cut in [0usize, 40, 77, 78, 120, bytes.len() / 2] {
            assert!(
                MobiDoc::from_bytes(&bytes[..cut]).is_err(),
                "cut at {cut} should fail"
            );
        }
    }

    #[test]
    fn short_record0_is_err_not_panic() {
        for n in 0..=40usize {
            assert!(Part::parse(&vec![0u8; n]).is_err(), "len {n}");
        }
        // 有 MOBI magic，但 header 长度标到记录外
        let mut rec = vec![0u8; 24];
        rec[16..20].copy_from_slice(b"MOBI");
        rec[20..24].copy_from_slice(&0xE4u32.to_be_bytes());
        assert!(Part::parse(&rec).is_err());
        // 16 字节的 PalmDOC 头 + 刚好够放 magic 的残头
        let mut rec = vec![0u8; 20];
        rec[16..20].copy_from_slice(b"MOBI");
        assert!(Part::parse(&rec).is_err());
    }

    #[test]
    fn bad_record_offset_is_err() {
        let mut bytes = build_mobi(&Opts::default(), b"<p>hello</p>");
        // 记录 1 的 offset 改成越过文件尾
        let at = 78 + 8;
        bytes[at..at + 4].copy_from_slice(&0xFFFF_FFF0u32.to_be_bytes());
        assert!(MobiDoc::from_bytes(&bytes).is_err());
    }

    #[test]
    fn too_many_records_is_err() {
        let mut bytes = build_mobi(&Opts::default(), b"<p>hello</p>");
        bytes[76..78].copy_from_slice(&(MAX_RECORDS as u16 + 1).to_be_bytes());
        let err = MobiDoc::from_bytes(&bytes).err().expect("should fail");
        assert!(err.contains("too many"), "{err}");
    }

    #[test]
    fn zero_records_is_err() {
        let mut bytes = build_mobi(&Opts::default(), b"<p>hello</p>");
        bytes[76..78].copy_from_slice(&0u16.to_be_bytes());
        assert!(MobiDoc::from_bytes(&bytes).is_err());
    }

    #[test]
    fn out_of_range_page_is_err() {
        let doc = doc_of(&Opts::default(), b"<p>only one page</p>").expect("open");
        let n = doc.page_count();
        assert!(doc.render(n, 300, 420).is_err());
        assert!(doc.render(usize::MAX, 300, 420).is_err());
        assert!(doc.render(0, 0, 420).is_err());
    }

    #[test]
    fn empty_body_does_not_panic() {
        let doc = doc_of(&Opts::default(), b"").expect("open");
        assert_eq!(doc.page_count(), 0);
        assert!(doc.outline().is_empty());
        assert!(doc.chapter_title(0).is_none());
        let _ = doc.render(0, 100, 140); // 不 panic 就行
        let _ = doc.find("x", 5);
    }

    #[test]
    fn render_twice_is_identical() {
        let doc = doc_of(&Opts::default(), b"<p>same pixels please</p>").expect("open");
        let a = doc.render(0, 400, 560).expect("render 1");
        let b = doc.render(0, 400, 560).expect("render 2");
        assert_eq!(a, b);
    }

    #[test]
    fn find_and_layout_delegate() {
        let mut doc = doc_of(&Opts::default(), b"<p>needle in here</p>").expect("open");
        let hits = doc.find("needle", 10);
        assert!(!hits.is_empty());
        assert_eq!(hits[0].0, 0);

        let before = doc.current_layout();
        let mut cfg = before;
        cfg.font_size = 36.0;
        assert!(doc.apply_layout(cfg));
        assert_eq!(doc.current_layout().font_size, 36.0);
        assert!(!doc.apply_layout(cfg), "参数没变应该返回 false");
    }

    #[test]
    fn open_missing_file_is_err() {
        assert!(MobiDoc::open("/nonexistent/definitely/not/a/book.mobi").is_err());
    }

    #[test]
    fn open_from_disk_works() {
        let bytes = build_mobi(&Opts::default(), b"<p>from disk</p>");
        let path = std::env::temp_dir().join(format!("rmdev-mobi-{}.mobi", std::process::id()));
        std::fs::write(&path, &bytes).expect("write temp");
        let doc = MobiDoc::open(path.to_str().expect("utf-8 path")).expect("open");
        assert_eq!(doc.page_count(), 1);
        assert!(page_text(&doc, 0).contains("from disk"));
        let _ = std::fs::remove_file(&path);
    }
}
