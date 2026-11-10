//! 最小 PNG 编解码 + 裁剪 + 缩图。**零外部依赖**（cloud 不加依赖，生产机不装 convert）。
//!
//! 用途：agent 网关要把整页渲染图物化到磁盘、把框选区域裁出来，交给 dsh 的
//! `read_image` 直接看（见 `agent_api::prepare`）。所以只需要：
//!
//! - **解码**：8 位深、非隔行、color type 0（灰度）/ 2（真彩）/ 6（真彩+alpha），
//!   filter 0–4 全实现。调色板、16 位、Adam7 一律拒（面板渲染不会出这些）。
//! - **编码**：同一套三种 color type；行内用 filter 0/2，deflate 走自写的
//!   固定 Huffman + LZ77（白底手写页能压一个数量级；压不动就退回 stored 块）。
//! - **裁剪**：像素坐标，越界自动 clamp 进图内；clamp 后小于 8×8 返回 `None`。
//! - **缩图**：长边等比缩到上限（面积平均，不做最近邻——细笔画会被采样丢掉）。
//!
//! 任何坏输入都返回 [`PngError`]，不 panic、不无限申请内存：IHDR 里的尺寸先过
//! [`MAX_DIM`]/[`MAX_BYTES`]，解压输出长度也被扫描线总长卡住。

use std::fmt;
use std::sync::OnceLock;

/// PNG 签名。
const SIGNATURE: [u8; 8] = [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
/// 单边像素上限。真机渲染图最大见过 3810×4311（≈ A3 300dpi），16384 够宽。
const MAX_DIM: u32 = 16384;
/// 解码后像素缓冲上限（128 MiB）：3810×4311 灰度只占 16 MB，但 20 万×20 万的
/// 恶意 IHDR 会被这里挡住，不会去申请几十 GB。
const MAX_BYTES: u64 = 128 << 20;
/// 裁图任一边小于这个值就放弃——给模型一张 3×3 的图没意义。
const MIN_CROP: i64 = 8;

/// deflate 的长度码表（码 257..=285）。
const LEN_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
/// 长度码的额外位数。
const LEN_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
/// deflate 的距离码表（码 0..=29）。
const DIST_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
/// 距离码的额外位数。
const DIST_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// 解不了的图。原因用中文短句，直接能拼进 prompt（"图不可用（…）"）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PngError {
    /// 不是 PNG / 块结构坏了 / 被截断。
    Bad(&'static str),
    /// 是 PNG，但用了我们没实现的子格式（16 位、调色板、隔行……）。
    Unsupported(&'static str),
    /// deflate/zlib 流坏了。
    Inflate(&'static str),
    /// 超过 [`MAX_DIM`] / [`MAX_BYTES`]，或者解压出来比扫描线还长。
    TooLarge,
}

impl fmt::Display for PngError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PngError::Bad(m) => write!(f, "{m}"),
            PngError::Unsupported(m) => write!(f, "{m}"),
            PngError::Inflate(m) => write!(f, "zlib 解压失败：{m}"),
            PngError::TooLarge => write!(f, "图太大"),
        }
    }
}

impl std::error::Error for PngError {}

/// 一张 8 位图，像素按 color type 紧凑打包（每像素 `bpp` 字节）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub color: Color,
    pub pixels: Vec<u8>,
}

impl Image {
    /// 每像素字节数。
    pub fn bpp(&self) -> usize {
        self.color.bpp()
    }

    /// 裁一块（像素坐标，`x0<x1`、`y0<y1` 不要求，越界自动 clamp）。
    ///
    /// clamp 后任一边小于 [`MIN_CROP`] 返回 `None`（整块在图外、或选区太小）。
    pub fn crop(&self, x0: i64, y0: i64, x1: i64, y1: i64) -> Option<Image> {
        let (w, h) = (self.width as i64, self.height as i64);
        let (x0, x1) = (x0.min(x1).clamp(0, w), x0.max(x1).clamp(0, w));
        let (y0, y1) = (y0.min(y1).clamp(0, h), y0.max(y1).clamp(0, h));
        if x1 - x0 < MIN_CROP || y1 - y0 < MIN_CROP {
            return None;
        }
        let bpp = self.bpp();
        let (cw, ch) = ((x1 - x0) as u32, (y1 - y0) as u32);
        let src_stride = self.width as usize * bpp;
        let dst_stride = cw as usize * bpp;
        let mut pixels = Vec::with_capacity(dst_stride * ch as usize);
        for y in y0..y1 {
            let from = y as usize * src_stride + x0 as usize * bpp;
            pixels.extend_from_slice(&self.pixels[from..from + dst_stride]);
        }
        Some(Image {
            width: cw,
            height: ch,
            color: self.color,
            pixels,
        })
    }

    /// 长边缩到 ≤ `max_side`（等比、面积平均）。本来就不超就原样克隆。
    ///
    /// 面积平均（而不是最近邻）是刻意的：16 级灰的手写笔画缩一半后要是被采样
    /// 丢掉，模型就看不见那根线了；平均之后笔画会变淡但还在。
    pub fn fit_within(&self, max_side: u32) -> Image {
        let (ow, oh) = scaled_dims(self.width, self.height, max_side);
        if (ow, oh) == (self.width, self.height) {
            return self.clone();
        }
        let bpp = self.bpp();
        let (sw, sh) = (self.width as usize, self.height as usize);
        // 每个输出行/列覆盖的源区间：整数边界，既不重叠也不漏。
        let span = |out: usize, src: usize, n: usize| {
            let from = out * src / n;
            let to = (((out + 1) * src + n - 1) / n).max(from + 1).min(src);
            (from, to)
        };
        let xs: Vec<(usize, usize)> = (0..ow as usize).map(|x| span(x, sw, ow as usize)).collect();
        let ys: Vec<(usize, usize)> = (0..oh as usize).map(|y| span(y, sh, oh as usize)).collect();
        let mut pixels = vec![0u8; ow as usize * oh as usize * bpp];
        for (y, &(sy0, sy1)) in ys.iter().enumerate() {
            for (x, &(sx0, sx1)) in xs.iter().enumerate() {
                let count = ((sy1 - sy0) * (sx1 - sx0)) as u32;
                let dst = (y * ow as usize + x) * bpp;
                for ch in 0..bpp {
                    let mut sum = 0u32;
                    for sy in sy0..sy1 {
                        let row = (sy * sw + sx0) * bpp + ch;
                        for sx in sx0..sx1 {
                            sum += self.pixels[row + (sx - sx0) * bpp] as u32;
                        }
                    }
                    pixels[dst + ch] = ((sum + count / 2) / count) as u8;
                }
            }
        }
        Image {
            width: ow,
            height: oh,
            color: self.color,
            pixels,
        }
    }
}

/// 等比缩到长边 ≤ `max_side` 之后的尺寸（整数四舍五入，最小 1 px）。
///
/// 例：1404×1872 缩到长边 1600 → 1200×1600。
pub fn scaled_dims(w: u32, h: u32, max_side: u32) -> (u32, u32) {
    let long = w.max(h);
    if long <= max_side || long == 0 {
        return (w, h);
    }
    let scale = |v: u32| -> u32 {
        let long = long as u64;
        (((v as u64 * max_side as u64) + long / 2) / long).max(1) as u32
    };
    (scale(w), scale(h))
}

/// PNG color type 里我们支持的三种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    /// 8 位灰度（color type 0），1 B/px。
    Gray,
    /// 8 位真彩（color type 2），3 B/px。
    Rgb,
    /// 8 位真彩 + alpha（color type 6），4 B/px。
    Rgba,
}

impl Color {
    /// 每像素字节数。
    pub fn bpp(self) -> usize {
        match self {
            Color::Gray => 1,
            Color::Rgb => 3,
            Color::Rgba => 4,
        }
    }

    /// IHDR 里的 color type 字节。
    fn code(self) -> u8 {
        match self {
            Color::Gray => 0,
            Color::Rgb => 2,
            Color::Rgba => 6,
        }
    }
}

/// 只读 IHDR 拿尺寸（不解码像素）。判断要不要缩图 / 缓存还有没有效时用。
pub fn dims(png: &[u8]) -> Option<(u32, u32)> {
    if png.len() < 24 || png[..8] != SIGNATURE || &png[12..16] != b"IHDR" {
        return None;
    }
    let (w, h) = (be32(&png[16..20]), be32(&png[20..24]));
    if w == 0 || h == 0 {
        return None;
    }
    Some((w, h))
}

// ---------------------------------------------------------------- 解码

/// 解开一张 PNG。只支持 8 位、非隔行、color type 0/2/6。
pub fn decode(png: &[u8]) -> Result<Image, PngError> {
    if png.len() < 8 || png[..8] != SIGNATURE {
        return Err(PngError::Bad("不是 PNG（魔数不对）"));
    }
    let mut pos = 8usize;
    let mut header: Option<(u32, u32, u8)> = None;
    let mut idat: Vec<u8> = Vec::new();
    let mut ended = false;
    while pos + 8 <= png.len() {
        let len = be32(&png[pos..pos + 4]) as usize;
        let kind = &png[pos + 4..pos + 8];
        let start = pos + 8;
        let end = start.checked_add(len).ok_or(PngError::Bad("块长度越界"))?;
        if end + 4 > png.len() {
            return Err(PngError::Bad("块被截断"));
        }
        match kind {
            b"IHDR" => {
                if header.is_some() || len < 13 {
                    return Err(PngError::Bad("IHDR 坏了"));
                }
                let w = be32(&png[start..start + 4]);
                let h = be32(&png[start + 4..start + 8]);
                let depth = png[start + 8];
                let color = png[start + 9];
                let compression = png[start + 10];
                let filter = png[start + 11];
                let interlace = png[start + 12];
                if depth != 8 {
                    return Err(PngError::Unsupported("只支持 8 位深"));
                }
                if !matches!(color, 0 | 2 | 6) {
                    return Err(PngError::Unsupported(
                        "只支持灰度/真彩/真彩+alpha（不支持调色板）",
                    ));
                }
                if compression != 0 || filter != 0 {
                    return Err(PngError::Unsupported("压缩/过滤方式不支持"));
                }
                if interlace != 0 {
                    return Err(PngError::Unsupported("不支持隔行（Adam7）"));
                }
                header = Some((w, h, color));
            }
            b"IDAT" => idat.extend_from_slice(&png[start..end]),
            b"IEND" => {
                ended = true;
                break;
            }
            _ => {}
        }
        pos = end + 4;
    }
    let Some((w, h, color)) = header else {
        return Err(PngError::Bad("缺 IHDR"));
    };
    if !ended {
        return Err(PngError::Bad("缺 IEND（图被截断）"));
    }
    if idat.is_empty() {
        return Err(PngError::Bad("缺 IDAT（没有像素数据）"));
    }
    if w == 0 || h == 0 {
        return Err(PngError::Bad("宽高为 0"));
    }
    if w > MAX_DIM || h > MAX_DIM {
        return Err(PngError::TooLarge);
    }
    let color = match color {
        0 => Color::Gray,
        2 => Color::Rgb,
        _ => Color::Rgba,
    };
    let bpp = color.bpp();
    if w as u64 * h as u64 * bpp as u64 > MAX_BYTES {
        return Err(PngError::TooLarge);
    }
    let stride = w as usize * bpp;
    let need = (stride + 1) * h as usize;
    // 解压输出上限就是扫描线总长：多出来的字节直接判坏，顺便防 zip 炸弹。
    let raw = zlib_decompress(&idat, need)?;
    if raw.len() != need {
        return Err(PngError::Inflate("解压后长度不对"));
    }

    let mut pixels = vec![0u8; stride * h as usize];
    for y in 0..h as usize {
        let base = y * (stride + 1);
        let ft = raw[base];
        let (head, tail) = pixels.split_at_mut(y * stride);
        let row = &mut tail[..stride];
        row.copy_from_slice(&raw[base + 1..base + 1 + stride]);
        let up: &[u8] = if y == 0 {
            &[]
        } else {
            &head[(y - 1) * stride..y * stride]
        };
        for x in 0..stride {
            let a = if x >= bpp { row[x - bpp] } else { 0 };
            let b = if y == 0 { 0 } else { up[x] };
            let c = if y > 0 && x >= bpp { up[x - bpp] } else { 0 };
            let pred = match ft {
                0 => 0,
                1 => a,
                2 => b,
                3 => ((a as u16 + b as u16) / 2) as u8,
                4 => paeth(a, b, c),
                _ => return Err(PngError::Bad("filter 类型不认识")),
            };
            row[x] = row[x].wrapping_add(pred);
        }
    }
    Ok(Image {
        width: w,
        height: h,
        color,
        pixels,
    })
}

/// PNG 的 Paeth 预测器。
fn paeth(a: u8, b: u8, c: u8) -> u8 {
    let p = a as i16 + b as i16 - c as i16;
    let (pa, pb, pc) = (
        (p - a as i16).abs(),
        (p - b as i16).abs(),
        (p - c as i16).abs(),
    );
    if pa <= pb && pa <= pc {
        a
    } else if pb <= pc {
        b
    } else {
        c
    }
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

// ---------------------------------------------------------------- 编码

/// 编码成 PNG：首行 filter 0（不预测），其余行 filter 2（Up）。
///
/// 白底手写页用 Up 之后大片空白全是 0，LZ77 一压就下去；万一压不动
/// （噪声图），[`zlib_compress`] 会退回 stored 块，不会比原图更大。
pub fn encode(img: &Image) -> Vec<u8> {
    debug_assert_eq!(
        img.pixels.len(),
        img.width as usize * img.height as usize * img.bpp()
    );
    let bpp = img.bpp();
    let stride = img.width as usize * bpp;
    let mut raw = Vec::with_capacity((stride + 1) * img.height as usize);
    for y in 0..img.height as usize {
        let row = &img.pixels[y * stride..(y + 1) * stride];
        if y == 0 {
            raw.push(0);
            raw.extend_from_slice(row);
        } else {
            raw.push(2);
            let up = &img.pixels[(y - 1) * stride..y * stride];
            for (v, u) in row.iter().zip(up) {
                raw.push(v.wrapping_sub(*u));
            }
        }
    }

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&img.width.to_be_bytes());
    ihdr.extend_from_slice(&img.height.to_be_bytes());
    ihdr.extend_from_slice(&[8, img.color.code(), 0, 0, 0]);

    let mut out = Vec::new();
    out.extend_from_slice(&SIGNATURE);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &zlib_compress(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

/// 写一个 PNG 块：长度 + 类型 + 数据 + CRC32（CRC 覆盖类型和数据）。
fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    out.extend_from_slice(&crc32(&[kind, data]).to_be_bytes());
}

/// CRC32（IEEE，PNG 用的那个）。数据量就是一张图，位运算实现够了。
fn crc32(parts: &[&[u8]]) -> u32 {
    let mut c = 0xffff_ffffu32;
    for part in parts {
        for &b in *part {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    (c >> 1) ^ 0xedb8_8320
                } else {
                    c >> 1
                };
            }
        }
    }
    !c
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &x in data {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// 挑小的那种：固定 Huffman 压得动就用它，压不动（噪声图）退回 stored。
fn zlib_compress(raw: &[u8]) -> Vec<u8> {
    let packed = zlib_fixed(raw);
    let stored = zlib_store(raw);
    if packed.len() < stored.len() {
        packed
    } else {
        stored
    }
}

/// 只有一个 fixed Huffman 块的 zlib 流（LZ77 见 [`zlib_fixed`]）。
fn zlib_fixed(raw: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01]; // CMF/FLG：deflate、32K 窗口、无字典，校验位通过
    out.extend_from_slice(&deflate_fixed(raw));
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

/// zlib 流：只用一个或多个 stored（不压缩）deflate 块，末尾接 adler32。
fn zlib_store(raw: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01];
    let mut i = 0usize;
    loop {
        let n = (raw.len() - i).min(0xffff);
        let last = i + n == raw.len();
        out.push(if last { 0x01 } else { 0x00 }); // BFINAL + BTYPE=00，随后补到字节边界
        out.extend_from_slice(&(n as u16).to_le_bytes());
        out.extend_from_slice(&(!(n as u16)).to_le_bytes());
        out.extend_from_slice(&raw[i..i + n]);
        i += n;
        if last {
            break;
        }
    }
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

// ---------------------------------------------------------------- deflate（固定 Huffman + LZ77）

/// LZ77 的回溯窗口（deflate 上限）。
const WINDOW: usize = 32 * 1024;
/// 最短匹配；短于 3 个字节直接出字面量更省。
const MIN_MATCH: usize = 3;
/// 最长匹配（deflate 上限）。
const MAX_MATCH: usize = 258;
/// 3 字节哈希表大小（2^15 个槽）。
const HASH_BITS: u32 = 15;
/// 每个位置的哈希链最多回溯这么多候选，防病态数据退化成 O(n²)。
const MAX_CHAIN: usize = 32;

/// LSB-first 的位写入器；Huffman 码按 MSB 先写（deflate 的规定）。
struct BitWriter {
    out: Vec<u8>,
    acc: u32,
    nbits: u32,
}

impl BitWriter {
    fn new() -> BitWriter {
        BitWriter {
            out: Vec::new(),
            acc: 0,
            nbits: 0,
        }
    }

    /// 写 `n` 位整数，低位先出（长度码的额外位、块头走这个）。
    fn put(&mut self, value: u32, n: u32) {
        self.acc |= (value & ((1u32 << n) - 1)) << self.nbits;
        self.nbits += n;
        while self.nbits >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.nbits -= 8;
        }
    }

    /// 写 Huffman 码，高位先出（和 `put` 相反，这里最容易写错）。
    fn code(&mut self, code: u32, n: u32) {
        for i in (0..n).rev() {
            self.put((code >> i) & 1, 1);
        }
    }

    /// 补零到字节边界，收工。
    fn finish(mut self) -> Vec<u8> {
        if self.nbits > 0 {
            self.out.push(self.acc as u8);
        }
        self.out
    }
}

/// 固定 Huffman 表里某个符号的码值和位数（RFC 1951 §3.2.6）。
fn fixed_code(sym: u16) -> (u32, u32) {
    match sym {
        0..=143 => (0x30 + sym as u32, 8),
        144..=255 => (0x190 + (sym as u32 - 144), 9),
        256..=279 => (sym as u32 - 256, 7),
        _ => (0xc0 + (sym as u32 - 280), 8),
    }
}

/// 匹配长度 → (长度码, 额外位数, 额外位值)。
fn len_code(len: usize) -> (u16, u32, u32) {
    for i in (0..LEN_BASE.len()).rev() {
        if len >= LEN_BASE[i] as usize {
            return (
                257 + i as u16,
                LEN_EXTRA[i] as u32,
                (len - LEN_BASE[i] as usize) as u32,
            );
        }
    }
    (257, 0, 0)
}

/// 回溯距离 → (距离码, 额外位数, 额外位值)。
fn dist_code(dist: usize) -> (u16, u32, u32) {
    for i in (0..DIST_BASE.len()).rev() {
        if dist >= DIST_BASE[i] as usize {
            return (
                i as u16,
                DIST_EXTRA[i] as u32,
                (dist - DIST_BASE[i] as usize) as u32,
            );
        }
    }
    (0, 0, 0)
}

fn hash3(b: &[u8]) -> usize {
    (((b[0] as usize) << 10) ^ ((b[1] as usize) << 5) ^ b[2] as usize) & ((1 << HASH_BITS) - 1)
}

/// 一个固定 Huffman 的 deflate 块（BFINAL=1），LZ77 贪心匹配。
///
/// 只求"比 stored 小得多"，不追求压缩率：白底页面的长 0 段用 dist=1 的
/// 258 字节匹配就能吃掉。
fn deflate_fixed(raw: &[u8]) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.put(1, 1); // BFINAL
    w.put(1, 2); // BTYPE=01，固定 Huffman

    let n = raw.len();
    let mut head = vec![usize::MAX; 1 << HASH_BITS];
    let mut prev = vec![usize::MAX; n];
    let mut i = 0usize;
    while i < n {
        let (mut best_len, mut best_dist) = (0usize, 0usize);
        if i + MIN_MATCH <= n {
            let mut cand = head[hash3(&raw[i..i + 3])];
            let mut chain = 0;
            while cand != usize::MAX && chain < MAX_CHAIN {
                let dist = i - cand;
                if dist > WINDOW {
                    break; // 链按时间倒序，再往前只会更远
                }
                let max = MAX_MATCH.min(n - i);
                let mut len = 0;
                while len < max && raw[cand + len] == raw[i + len] {
                    len += 1;
                }
                if len > best_len {
                    best_len = len;
                    best_dist = dist;
                    if len == max {
                        break;
                    }
                }
                cand = prev[cand];
                chain += 1;
            }
        }
        if best_len >= MIN_MATCH {
            let (sym, extra, value) = len_code(best_len);
            let (code, bits) = fixed_code(sym);
            w.code(code, bits);
            if extra > 0 {
                w.put(value, extra);
            }
            let (dsym, dextra, dvalue) = dist_code(best_dist);
            w.code(dsym as u32, 5); // 固定距离码就是符号值本身
            if dextra > 0 {
                w.put(dvalue, dextra);
            }
            for k in i..i + best_len {
                if k + MIN_MATCH <= n {
                    let h = hash3(&raw[k..k + 3]);
                    prev[k] = head[h];
                    head[h] = k;
                }
            }
            i += best_len;
        } else {
            let (code, bits) = fixed_code(raw[i] as u16);
            w.code(code, bits);
            if i + MIN_MATCH <= n {
                let h = hash3(&raw[i..i + 3]);
                prev[i] = head[h];
                head[h] = i;
            }
            i += 1;
        }
    }
    let (code, bits) = fixed_code(256);
    w.code(code, bits); // 块结束
    w.finish()
}

// ---------------------------------------------------------------- inflate

/// 解 zlib 流；输出超过 `limit` 字节判定为坏图（防炸内存）。
fn zlib_decompress(data: &[u8], limit: usize) -> Result<Vec<u8>, PngError> {
    if data.len() < 6 {
        return Err(PngError::Inflate("zlib 流太短"));
    }
    let (cmf, flg) = (data[0], data[1]);
    if cmf & 0x0f != 8 {
        return Err(PngError::Inflate("不是 deflate"));
    }
    if (((cmf as u16) << 8) | flg as u16) % 31 != 0 {
        return Err(PngError::Inflate("zlib 头校验失败"));
    }
    if flg & 0x20 != 0 {
        return Err(PngError::Unsupported("不支持 zlib 预设字典"));
    }
    let mut bits = Bits {
        data,
        pos: 2,
        bit: 0,
    };
    let out = inflate(&mut bits, limit)?;
    let tail = &data[data.len() - 4..];
    if adler32(&out) != be32(tail) {
        return Err(PngError::Inflate("adler32 校验失败"));
    }
    Ok(out)
}

/// LSB-first 的位读取器。
struct Bits<'a> {
    data: &'a [u8],
    /// 下一个要读的字节。
    pos: usize,
    /// 当前字节里已经消费的位数（0..8）。
    bit: u32,
}

impl Bits<'_> {
    fn bit(&mut self) -> Result<u32, PngError> {
        let Some(byte) = self.data.get(self.pos) else {
            return Err(PngError::Inflate("数据提前结束"));
        };
        let v = (*byte >> self.bit) & 1;
        self.bit += 1;
        if self.bit == 8 {
            self.bit = 0;
            self.pos += 1;
        }
        Ok(v as u32)
    }

    /// 读 `n`（≤ 16）位，低位先出。
    fn bits(&mut self, n: u32) -> Result<u32, PngError> {
        let mut v = 0u32;
        for i in 0..n {
            v |= self.bit()? << i;
        }
        Ok(v)
    }

    /// 丢到下一个字节边界。
    fn align(&mut self) {
        if self.bit != 0 {
            self.bit = 0;
            self.pos += 1;
        }
    }

    /// 读 `n` 个整字节（调用方保证已对齐）。
    fn bytes(&mut self, n: usize) -> Result<&[u8], PngError> {
        let end = self.pos.checked_add(n).ok_or(PngError::Inflate("长度越界"))?;
        let Some(s) = self.data.get(self.pos..end) else {
            return Err(PngError::Inflate("数据提前结束"));
        };
        self.pos = end;
        Ok(s)
    }
}

fn inflate(bits: &mut Bits, limit: usize) -> Result<Vec<u8>, PngError> {
    let mut out = Vec::new();
    loop {
        let last = bits.bits(1)? == 1;
        match bits.bits(2)? {
            0 => stored(bits, &mut out, limit)?,
            1 => {
                let (lit, dist) = fixed();
                block(bits, &mut out, limit, lit, dist)?;
            }
            2 => {
                let (lit, dist) = dynamic(bits)?;
                block(bits, &mut out, limit, &lit, &dist)?;
            }
            _ => return Err(PngError::Inflate("块类型 3 非法")),
        }
        if last {
            return Ok(out);
        }
    }
}

/// stored 块：先补齐到字节，再 LEN/NLEN + 原始字节。
fn stored(bits: &mut Bits, out: &mut Vec<u8>, limit: usize) -> Result<(), PngError> {
    bits.align();
    let len = bits.bits(16)? as u16;
    let nlen = bits.bits(16)? as u16;
    if len != !nlen {
        return Err(PngError::Inflate("stored 块长度校验失败"));
    }
    if out.len() + len as usize > limit {
        return Err(PngError::TooLarge);
    }
    let data = bits.bytes(len as usize)?;
    out.extend_from_slice(data);
    Ok(())
}

/// 一个 Huffman 块：字面量/长度码 + 距离码，直到 256。
fn block(
    bits: &mut Bits,
    out: &mut Vec<u8>,
    limit: usize,
    lit: &Huffman,
    dist: &Huffman,
) -> Result<(), PngError> {
    loop {
        match lit.decode(bits)? {
            256 => return Ok(()),
            sym @ 0..=255 => push(out, sym as u8, limit)?,
            sym @ 257..=285 => {
                let i = (sym - 257) as usize;
                let len = LEN_BASE[i] as usize + bits.bits(LEN_EXTRA[i] as u32)? as usize;
                let d = dist.decode(bits)? as usize;
                let Some(&base) = DIST_BASE.get(d) else {
                    return Err(PngError::Inflate("距离码非法"));
                };
                let back = base as usize + bits.bits(DIST_EXTRA[d] as u32)? as usize;
                if back == 0 || back > out.len() {
                    return Err(PngError::Inflate("回溯距离越界"));
                }
                if out.len() + len > limit {
                    return Err(PngError::TooLarge);
                }
                // 逐字节复制，天然支持 len > back 的重叠（LZ77 语义）。
                for _ in 0..len {
                    let b = out[out.len() - back];
                    out.push(b);
                }
            }
            _ => return Err(PngError::Inflate("长度码非法")),
        }
    }
}

fn push(out: &mut Vec<u8>, b: u8, limit: usize) -> Result<(), PngError> {
    if out.len() >= limit {
        return Err(PngError::TooLarge);
    }
    out.push(b);
    Ok(())
}

/// 固定的字面量/距离码表（deflate 规范里写死的那两张）。
fn fixed() -> &'static (Huffman, Huffman) {
    static FIXED: OnceLock<(Huffman, Huffman)> = OnceLock::new();
    FIXED.get_or_init(|| {
        let mut lit = [0u8; 288];
        for (i, l) in lit.iter_mut().enumerate() {
            *l = match i {
                0..=143 => 8,
                144..=255 => 9,
                256..=279 => 7,
                _ => 8,
            };
        }
        let lit = Huffman::build(&lit).expect("固定码表一定完备");
        let dist = Huffman::build(&[5u8; 32]).expect("固定距离表一定完备");
        (lit, dist)
    })
}

/// 动态码表：先读码长码表（19 个 3 位码长），再用它解出 lit/dist 的码长。
fn dynamic(bits: &mut Bits) -> Result<(Huffman, Huffman), PngError> {
    /// 码长码表在流里的顺序（不是 0..19）。
    const ORDER: [usize; 19] = [
        16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
    ];
    let hlit = bits.bits(5)? as usize + 257;
    let hdist = bits.bits(5)? as usize + 1;
    let hclen = bits.bits(4)? as usize + 4;
    // 规范的合法上限是 HLIT ≤ 286 / HDIST ≤ 30；多出来的槽位历史上允许编码，
    // 只要里面是 0 码长就无害，所以放宽到 288/32 再靠解码时拒非法符号。
    if hlit > 288 || hdist > 32 {
        return Err(PngError::Inflate("码表长度越界"));
    }
    let mut code_lens = [0u8; 19];
    for &i in ORDER.iter().take(hclen) {
        code_lens[i] = bits.bits(3)? as u8;
    }
    let code = Huffman::build(&code_lens)?;

    let mut lens = vec![0u8; hlit + hdist];
    let mut i = 0usize;
    while i < lens.len() {
        match code.decode(bits)? {
            n @ 0..=15 => {
                lens[i] = n as u8;
                i += 1;
            }
            16 => {
                let Some(prev) = i.checked_sub(1).map(|j| lens[j]) else {
                    return Err(PngError::Inflate("重复码没有前一个码长"));
                };
                let n = 3 + bits.bits(2)? as usize;
                fill(&mut lens, i, n, prev)?;
                i += n;
            }
            17 => {
                let n = 3 + bits.bits(3)? as usize;
                fill(&mut lens, i, n, 0)?;
                i += n;
            }
            18 => {
                let n = 11 + bits.bits(7)? as usize;
                fill(&mut lens, i, n, 0)?;
                i += n;
            }
            _ => return Err(PngError::Inflate("码长符号非法")),
        }
    }
    let lit = Huffman::build(&lens[..hlit])?;
    let dist = Huffman::build(&lens[hlit..])?;
    Ok((lit, dist))
}

fn fill(lens: &mut [u8], at: usize, n: usize, value: u8) -> Result<(), PngError> {
    let Some(slot) = lens.get_mut(at..at + n) else {
        return Err(PngError::Inflate("码长重复越界"));
    };
    slot.fill(value);
    Ok(())
}

/// 规范 Huffman 表：`count[len]` 是该长度的码数，`symbol` 按码值顺序排。
///
/// 解码是 puff 的做法：逐位读、跟 `first`/`index` 对，15 层封顶。
struct Huffman {
    count: [u16; 16],
    symbol: Vec<u16>,
}

impl Huffman {
    fn build(lengths: &[u8]) -> Result<Huffman, PngError> {
        let mut count = [0u16; 16];
        for &l in lengths {
            if l > 15 {
                return Err(PngError::Inflate("码长超过 15 位"));
            }
            count[l as usize] += 1;
        }
        // 除 0 码长外所有码加起来不能超过对应深度的容量（超了就是坏表）。
        let mut left: i32 = 1;
        for len in 1..=15 {
            left <<= 1;
            left -= count[len] as i32;
            if left < 0 {
                return Err(PngError::Inflate("Huffman 表超配"));
            }
        }
        let mut offset = [0u16; 16];
        for len in 1..15 {
            offset[len + 1] = offset[len] + count[len];
        }
        let mut symbol = vec![0u16; lengths.len()];
        for (sym, &len) in lengths.iter().enumerate() {
            if len != 0 {
                symbol[offset[len as usize] as usize] = sym as u16;
                offset[len as usize] += 1;
            }
        }
        Ok(Huffman { count, symbol })
    }

    fn decode(&self, bits: &mut Bits) -> Result<u16, PngError> {
        let (mut first, mut index) = (0i32, 0i32);
        let mut code = 0i32;
        for len in 1..=15 {
            code |= bits.bit()? as i32;
            let count = self.count[len] as i32;
            if code - count < first {
                let Some(sym) = self.symbol.get((index + (code - first)) as usize) else {
                    return Err(PngError::Inflate("Huffman 符号越界"));
                };
                return Ok(*sym);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(PngError::Inflate("Huffman 码非法"))
    }
}

// ---------------------------------------------------------------- 单测

#[cfg(test)]
mod tests {
    use super::*;

    /// 4×4 灰度图：值跨满 0..=255，四个象限各不同。
    fn gray4() -> Image {
        Image {
            width: 4,
            height: 4,
            color: Color::Gray,
            pixels: vec![
                0, 17, 34, 51, 68, 85, 102, 119, 136, 153, 170, 187, 204, 221, 238, 255,
            ],
        }
    }

    /// 16×16 灰度，值 = (x*13 + y*7) % 256（下面嵌入的真实 PNG 就是它）。
    fn gradient16() -> Image {
        let mut pixels = Vec::with_capacity(256);
        for y in 0..16usize {
            for x in 0..16usize {
                pixels.push(((x * 13 + y * 7) % 256) as u8);
            }
        }
        Image {
            width: 16,
            height: 16,
            color: Color::Gray,
            pixels,
        }
    }

    /// 128×128 白底黑笔迹（下面动态 Huffman 那张 PNG 的原图）。
    fn ink128() -> Image {
        let (w, h) = (128usize, 128usize);
        let mut pixels = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                pixels.push(if (x * 7 + y * 11) % 23 < 3 { 0 } else { 255 });
            }
        }
        Image {
            width: w as u32,
            height: h as u32,
            color: Color::Gray,
            pixels,
        }
    }

    /// 手工拼一张 PNG（测试要指定 filter 行 / 指定 IDAT 字节）。
    fn build(w: u32, h: u32, color: Color, raw: &[u8]) -> Vec<u8> {
        assert_eq!(
            raw.len(),
            (w as usize * color.bpp() + 1) * h as usize,
            "扫描线长度不对"
        );
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&w.to_be_bytes());
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, color.code(), 0, 0, 0]);
        let mut out = SIGNATURE.to_vec();
        chunk(&mut out, b"IHDR", &ihdr);
        chunk(&mut out, b"IDAT", &zlib_store(raw));
        chunk(&mut out, b"IEND", &[]);
        out
    }

    /// 按 filter `ft` 把原图正向过滤成扫描线（编码器只写 0/2，这里补全五种）。
    fn filtered(img: &Image, ft: u8) -> Vec<u8> {
        let bpp = img.bpp();
        let stride = img.width as usize * bpp;
        let mut raw = Vec::new();
        for y in 0..img.height as usize {
            raw.push(ft);
            let row = &img.pixels[y * stride..(y + 1) * stride];
            let up: &[u8] = if y == 0 {
                &[]
            } else {
                &img.pixels[(y - 1) * stride..y * stride]
            };
            for x in 0..stride {
                let a = if x >= bpp { row[x - bpp] } else { 0 };
                let b = if y == 0 { 0 } else { up[x] };
                let c = if y > 0 && x >= bpp { up[x - bpp] } else { 0 };
                let pred = match ft {
                    0 => 0,
                    1 => a,
                    2 => b,
                    3 => ((a as u16 + b as u16) / 2) as u8,
                    4 => paeth(a, b, c),
                    _ => unreachable!(),
                };
                raw.push(row[x].wrapping_sub(pred));
            }
        }
        raw
    }

    fn unhex(s: &str) -> Vec<u8> {
        let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        assert!(clean.len() % 2 == 0, "十六进制长度不是偶数");
        (0..clean.len() / 2)
            .map(|i| u8::from_str_radix(&clean[i * 2..i * 2 + 2], 16).expect("hex"))
            .collect()
    }

    // -------------------------------------------------- 往返

    #[test]
    fn gray_4x4_round_trips() {
        let img = gray4();
        let png = encode(&img);
        assert!(png.starts_with(&SIGNATURE), "编码结果不是 PNG");
        assert_eq!(decode(&png).expect("解回来"), img);
        assert_eq!(dims(&png), Some((4, 4)));
    }

    #[test]
    fn rgb_and_rgba_round_trip() {
        let rgb = Image {
            width: 2,
            height: 2,
            color: Color::Rgb,
            pixels: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
        };
        assert_eq!(decode(&encode(&rgb)).expect("RGB"), rgb);

        let rgba = Image {
            width: 2,
            height: 3,
            color: Color::Rgba,
            pixels: (0..24).collect(),
        };
        assert_eq!(decode(&encode(&rgba)).expect("RGBA"), rgba);
    }

    #[test]
    fn encoder_compresses_flat_pages() {
        // 白底 + 稀疏笔迹：和真实手写页同构。
        let (w, h) = (256usize, 256usize);
        let mut pixels = vec![255u8; w * h];
        for y in 0..h {
            for x in 0..w {
                if (x * 5 + y * 3) % 97 < 2 {
                    pixels[y * w + x] = 0;
                }
            }
        }
        let img = Image {
            width: w as u32,
            height: h as u32,
            color: Color::Gray,
            pixels,
        };
        let png = encode(&img);
        let stored = zlib_store(&filtered(&img, 0)).len();
        assert!(
            png.len() < stored / 4,
            "压得不够：PNG {} 字节 vs stored {} 字节",
            png.len(),
            stored
        );
        assert_eq!(decode(&png).expect("解回来"), img);
    }

    #[test]
    fn incompressible_data_round_trips() {
        // 线性同余伪随机：固定 Huffman 压不动，编码器应退回 stored 块。
        let mut x = 0x1234_5678u32;
        let mut pixels = Vec::with_capacity(64 * 64);
        for _ in 0..64 * 64 {
            x = x.wrapping_mul(1664525).wrapping_add(1013904223);
            pixels.push((x >> 24) as u8);
        }
        let img = Image {
            width: 64,
            height: 64,
            color: Color::Gray,
            pixels,
        };
        assert_eq!(decode(&encode(&img)).expect("解回来"), img);
    }

    #[test]
    fn both_zlib_flavours_round_trip() {
        let raw: Vec<u8> = (0..1000u32).map(|i| (i % 7) as u8).collect();
        assert_eq!(zlib_decompress(&zlib_store(&raw), raw.len()).unwrap(), raw);
        assert_eq!(zlib_decompress(&zlib_fixed(&raw), raw.len()).unwrap(), raw);
        // 压不动的数据也得能解
        let noise: Vec<u8> = (0..1000u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
            .collect();
        assert_eq!(
            zlib_decompress(&zlib_fixed(&noise), noise.len()).unwrap(),
            noise
        );
    }

    // -------------------------------------------------- filter 0–4

    #[test]
    fn each_filter_one_to_five_decode() {
        // 用非平凡像素（每行/每列都不同）才能区分 Sub/Up/Avg/Paeth。
        let img = Image {
            width: 4,
            height: 5,
            color: Color::Gray,
            pixels: vec![
                10, 20, 30, 40, 15, 25, 35, 45, 5, 6, 7, 8, 200, 199, 198, 197, 130, 120, 110,
                100,
            ],
        };
        for ft in 0..=4u8 {
            let png = build(4, 5, Color::Gray, &filtered(&img, ft));
            let got = decode(&png).unwrap_or_else(|e| panic!("filter {ft} 解不开：{e}"));
            assert_eq!(got, img, "filter {ft} 解出来的像素不对");
        }
    }

    #[test]
    fn filters_work_for_three_byte_pixels() {
        // bpp=3：Sub/Avg/Paeth 的 `a`/`c` 要按像素而不是按字节回退 1。
        let img = Image {
            width: 3,
            height: 2,
            color: Color::Rgb,
            pixels: vec![
                10, 20, 30, 40, 50, 60, 70, 80, 90, 200, 210, 220, 230, 240, 250, 1, 2, 3,
            ],
        };
        for ft in 0..=4u8 {
            let png = build(3, 2, Color::Rgb, &filtered(&img, ft));
            assert_eq!(decode(&png).expect("RGB filter"), img, "filter {ft}");
        }
    }

    // -------------------------------------------------- 真实压缩流（zlib 出的固定/动态 Huffman）

    /// 16×16 灰度（值 = (x*13+y*7)%256），zlib 用**固定 Huffman** 块压的 IDAT。
    /// 由 PIL 生成，80 字节；用来证明真机渲染图（不是我们自己的编码器）能解。
    const FIXED_PNG: &str = "89504e470d0a1a0a0000000d49484452000000100000001008000000003a98a0bd\
000000134944415478da6364e045054cec6860640900001f59077360fdeac80000000049454e44ae426082";

    /// 128×128 白底黑笔迹，zlib 用**动态 Huffman** 块压（265 字节 PNG）。
    const DYNAMIC_PNG: &str = "89504e470d0a1a0a0000000d4948445200000080000000800800000000e6553e1700\
0000d44944415478daedd6310ac3401003409dfeffe7a4700c09e9ad665c1ddbdc7911624e5ef97ce7fb749e1af7959\
39c24d74beed363e33e7bddffb8839ffe1977b5fafbd46900f2d9c032869d06e0dac036869d062049c73d948e7be8d\
ac032861df7d0d70646afe8b887d2710fa5e31eba37b07b45c73d948e7b2827138afea0741bc38e7b08cbb11ccbb11ccb\
b11ccbb11ccbb11ccbb11ccbb11ccbb11ccbb11ccbb11ccbb11ccbb11ccbb11ccbb11ccbb11ccbb11ccbb11ccbb11ccbb\
11ccbb11ccbb11ccbb11ccbb11ccbb11ccbb11ccbb13c6f898e5b6d85d0031b0000000049454e44ae426082";

    #[test]
    fn decodes_real_zlib_fixed_huffman_png() {
        let img = decode(&unhex(FIXED_PNG)).expect("固定 Huffman 的 PNG");
        assert_eq!(img.color, Color::Gray);
        assert_eq!((img.width, img.height), (16, 16));
        assert_eq!(img, gradient16());
    }

    #[test]
    fn decodes_real_zlib_dynamic_huffman_png() {
        let img = decode(&unhex(DYNAMIC_PNG)).expect("动态 Huffman 的 PNG");
        assert_eq!(img.color, Color::Gray);
        assert_eq!((img.width, img.height), (128, 128));
        assert_eq!(img, ink128());
    }

    // -------------------------------------------------- 坏图：只许 Err

    #[test]
    fn truncated_png_is_err() {
        let png = encode(&gray4());
        for cut in [0usize, 3, 8, 20, 40, png.len() - 1] {
            assert!(decode(&png[..cut]).is_err(), "截到 {cut} 字节还解出来了");
        }
    }

    #[test]
    fn bad_magic_and_garbage_are_err() {
        assert!(decode(b"").is_err());
        assert!(decode(b"not a png at all").is_err());
        assert!(decode(&[0x89, b'P', b'N', b'G', 0, 0, 0, 0]).is_err());
        let mut junk = SIGNATURE.to_vec();
        junk.extend_from_slice(&[0xff; 32]);
        assert!(decode(&junk).is_err());
        assert_eq!(dims(b"not a png"), None);
    }

    #[test]
    fn zero_size_and_bad_ihdr_are_err() {
        for (w, h) in [(0u32, 4u32), (4, 0), (0, 0)] {
            let mut ihdr = Vec::new();
            ihdr.extend_from_slice(&w.to_be_bytes());
            ihdr.extend_from_slice(&h.to_be_bytes());
            ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
            let mut png = SIGNATURE.to_vec();
            chunk(&mut png, b"IHDR", &ihdr);
            chunk(&mut png, b"IDAT", &zlib_store(&[0u8; 16]));
            chunk(&mut png, b"IEND", &[]);
            assert_eq!(
                decode(&png),
                Err(PngError::Bad("宽高为 0")),
                "{w}×{h} 应该被拒"
            );
        }
    }

    #[test]
    fn unsupported_variants_are_err() {
        let bad = |w: u32, h: u32, depth: u8, color: u8, interlace: u8| {
            let mut ihdr = Vec::new();
            ihdr.extend_from_slice(&w.to_be_bytes());
            ihdr.extend_from_slice(&h.to_be_bytes());
            ihdr.extend_from_slice(&[depth, color, 0, 0, interlace]);
            let mut png = SIGNATURE.to_vec();
            chunk(&mut png, b"IHDR", &ihdr);
            chunk(&mut png, b"IDAT", &zlib_store(&[0u8; 8]));
            chunk(&mut png, b"IEND", &[]);
            decode(&png)
        };
        assert!(matches!(bad(4, 4, 16, 0, 0), Err(PngError::Unsupported(_))));
        assert!(matches!(bad(4, 4, 8, 3, 0), Err(PngError::Unsupported(_))));
        assert!(matches!(bad(4, 4, 8, 0, 1), Err(PngError::Unsupported(_))));
        // 没有 IDAT
        let mut png = SIGNATURE.to_vec();
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&4u32.to_be_bytes());
        ihdr.extend_from_slice(&4u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(&mut png, b"IEND", &[]);
        assert!(decode(&png).is_err());
    }

    #[test]
    fn oversized_header_is_rejected_before_allocating() {
        let huge = |w: u32, h: u32| {
            let mut ihdr = Vec::new();
            ihdr.extend_from_slice(&w.to_be_bytes());
            ihdr.extend_from_slice(&h.to_be_bytes());
            ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
            let mut png = SIGNATURE.to_vec();
            chunk(&mut png, b"IHDR", &ihdr);
            chunk(&mut png, b"IDAT", &zlib_store(&[0u8; 8]));
            chunk(&mut png, b"IEND", &[]);
            decode(&png)
        };
        assert_eq!(huge(40_000, 40_000), Err(PngError::TooLarge));
        assert_eq!(huge(16_384, 16_384), Err(PngError::TooLarge)); // 单边没超，字节数超
    }

    #[test]
    fn corrupt_deflate_stream_is_err() {
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&4u32.to_be_bytes());
        ihdr.extend_from_slice(&4u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
        let mut png = SIGNATURE.to_vec();
        chunk(&mut png, b"IHDR", &ihdr);
        chunk(&mut png, b"IDAT", &[0x78, 0x9c, 0xff, 0xff, 0xff, 0xff]);
        chunk(&mut png, b"IEND", &[]);
        assert!(decode(&png).is_err());
    }

    // -------------------------------------------------- 裁剪

    fn checker16() -> Image {
        let mut pixels = Vec::with_capacity(256);
        for y in 0..16usize {
            for x in 0..16usize {
                pixels.push((y * 16 + x) as u8);
            }
        }
        Image {
            width: 16,
            height: 16,
            color: Color::Gray,
            pixels,
        }
    }

    #[test]
    fn crop_takes_exact_pixels() {
        let img = checker16();
        let cut = img.crop(2, 3, 10, 11).expect("8×8 该裁得出来");
        assert_eq!((cut.width, cut.height), (8, 8));
        assert_eq!(cut.color, Color::Gray);
        assert_eq!(cut.pixels[0], (3 * 16 + 2) as u8);
        assert_eq!(cut.pixels[7], (3 * 16 + 9) as u8);
        assert_eq!(cut.pixels[7 * 8], (10 * 16 + 2) as u8);
    }

    #[test]
    fn crop_clamps_out_of_range() {
        let img = checker16();
        let cut = img.crop(-4, -4, 12, 12).expect("负数起点要能 clamp");
        assert_eq!((cut.width, cut.height), (12, 12));
        assert_eq!(cut.pixels[0], 0);
        let all = img.crop(-100, -100, 100, 100).expect("整张图");
        assert_eq!(all, img);
        // 坐标反着给：按 min/max 理解
        let flipped = img.crop(10, 11, 2, 3).expect("反着的矩形");
        assert_eq!(flipped, img.crop(2, 3, 10, 11).unwrap());
    }

    #[test]
    fn crop_rejects_tiny_or_outside() {
        let img = checker16();
        assert!(img.crop(0, 0, 7, 100).is_none(), "宽 7 px 该拒");
        assert!(img.crop(0, 0, 100, 7).is_none(), "高 7 px 该拒");
        assert!(img.crop(0, 0, 8, 8).is_some(), "正好 8×8 保留");
        assert!(img.crop(100, 100, 200, 200).is_none(), "整块在图外");
        assert!(img.crop(9, 9, 30, 30).is_none(), "clamp 后只剩 7×7");
    }

    #[test]
    fn crop_keeps_color_type() {
        let rgb = Image {
            width: 9,
            height: 9,
            color: Color::Rgb,
            pixels: (0..9 * 9 * 3).map(|i| (i % 256) as u8).collect(),
        };
        let cut = rgb.crop(1, 1, 9, 9).expect("RGB 裁图");
        assert_eq!(cut.color, Color::Rgb);
        assert_eq!((cut.width, cut.height), (8, 8));
        assert_eq!(cut.pixels.len(), 8 * 8 * 3);
        assert_eq!(cut.pixels[0], rgb.pixels[(1 * 9 + 1) * 3]);
    }

    #[test]
    fn crop_result_round_trips_through_encode() {
        let cut = checker16().crop(0, 0, 8, 8).expect("8×8");
        assert_eq!(decode(&encode(&cut)).expect("解回来"), cut);
    }

    // -------------------------------------------------- 缩图

    #[test]
    fn scaled_dims_only_shrinks_long_side() {
        assert_eq!(scaled_dims(1404, 1872, 1600), (1200, 1600));
        assert_eq!(scaled_dims(3810, 4311, 1600), (1414, 1600));
        assert_eq!(scaled_dims(1872, 1404, 1600), (1600, 1200));
        // 本来就在范围内：原样
        assert_eq!(scaled_dims(1404, 1872, 4000), (1404, 1872));
        assert_eq!(scaled_dims(16, 16, 1600), (16, 16));
    }

    #[test]
    fn fit_within_averages_blocks() {
        // 4×4 → 长边 2 → 2×2，每格是 2×2 的平均
        let img = Image {
            width: 4,
            height: 4,
            color: Color::Gray,
            pixels: vec![0, 0, 10, 10, 0, 0, 10, 10, 20, 20, 30, 30, 20, 20, 30, 30],
        };
        let small = img.fit_within(2);
        assert_eq!((small.width, small.height), (2, 2));
        assert_eq!(small.pixels, vec![0, 10, 20, 30]);
        // 不需要缩：原样
        assert_eq!(img.fit_within(4), img);
        assert_eq!(img.fit_within(100), img);
    }

    #[test]
    fn fit_within_keeps_color_and_odd_sizes() {
        let img = Image {
            width: 5,
            height: 3,
            color: Color::Rgb,
            pixels: (0..5 * 3 * 3).map(|i| (i * 3) as u8).collect(),
        };
        let small = img.fit_within(3);
        assert_eq!(small.color, Color::Rgb);
        assert!(small.width <= 3 && small.height <= 3, "{small:?}");
        assert_eq!(small.pixels.len(), small.width as usize * small.height as usize * 3);
        // 缩完还要能编码/解码
        assert_eq!(decode(&encode(&small)).expect("解回来"), small);
    }

    #[test]
    fn fit_within_real_page_size() {
        let (w, h) = (3810usize, 4311usize);
        let img = Image {
            width: w as u32,
            height: h as u32,
            color: Color::Gray,
            pixels: vec![255u8; w * h],
        };
        let small = img.fit_within(1600);
        assert_eq!((small.width, small.height), (1414, 1600));
        assert!(small.pixels.iter().all(|&p| p == 255), "全白缩完还是全白");
    }
}
