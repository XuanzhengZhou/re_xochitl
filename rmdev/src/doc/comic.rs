//! CBZ 漫画后端：一个 zip 里的图片序列 → 按页灰度位图。
//!
//! # 解码器选择（实测，不是拍脑袋）
//!
//! 约束是**纯 Rust**、能编到 `armv7-unknown-linux-gnueabihf`、并且支持**渐进式
//! JPEG**（扫描版漫画大量是 progressive）。两套候选（`zune-jpeg`/`jpeg-decoder`、
//! `zune-png`/`png`）都满足前两条，所以直接用**本项目自己的二进制**量增量：
//! 在本 worktree 的副本里让 `main()` 调 `Comic::open/aspect/render`（保证代码真的
//! 被链接），按项目原配置（`opt-level=2` + `lto` + `strip`）编 release：
//!
//! | 组合 | 二进制 | 相对 baseline |
//! |---|---|---|
//! | baseline（只有 epub/pdf） | 1 680 176 B | — |
//! | **zune-jpeg + zune-png** | **1 895 184 B** | **+210 KB** |
//! | jpeg-decoder + zune-png | 1 912 224 B | +227 KB |
//! | zune-jpeg + png 0.18 | 1 928 304 B | +242 KB |
//!
//! 所以走 zune 系：两处都更小（JPEG 省 17 KB，PNG 省 33 KB），两个 crate 共用
//! `zune-core`，PNG 的 inflate 用 `zune-inflate`，不会像 `png` crate 那样再拖一份
//! `miniz_oxide` 0.8 和本项目已有的 0.9 并存；`zune-jpeg` 的 progressive 路径是
//! 完整的（测试里有一张 baseline、一张 progressive 的夹具）。
//!
//! zip 容器本身不引新依赖：中央目录自己扫，deflate 用项目已有的 `miniz_oxide`。
//!
//! # 为什么不做 CBR
//!
//! `.cbr` 是 RAR。当前生态里成气候的 RAR 解压都是 **C 绑定**：
//! `unrar` / `unrar_sys`（内置 UnRAR C++ 源码，build.rs 调 `cc`）、
//! `compress-tools`（libarchive，`pkg-config`/`vcpkg` 找 C 库）。
//! 纯 Rust 的 RAR 实现在 2026-11 才刚出现且都不可用：`rar-codec` 是 0.1.x、
//! 发布于调研当天、没有使用者；`rars` 拖 `rayon` + 一堆 crypto；`unrar-rs` 拖
//! `aws-lc-rs`（C/汇编）。这些要么过重、要么无法在本机造夹具验证，所以
//! **不为了 CBR 引 C 库**，`open()` 直接给出可操作的错误信息。
//!
//! # 性能约定
//!
//! * `open()` 只读 zip 尾部的 EOCD + 中央目录，**一个图片字节都不解压**：
//!   几百页的 CBZ 也是毫秒级；
//! * `aspect()` 才读该页的**前缀**（先 64 KiB，找不到 SOF/IHDR 才放宽到 1 MiB）
//!   解析尺寸，结果缓存；
//! * `render()` 才整页解压 + 解码，且连续同一 (页, w, h) 直接吃缓存
//!   （UI 一帧里常问两次），只留最近 `CACHE_PAGES`（2）页：1404×1872 灰度
//!   2.6 MB/页。
//!
//! # 缩放
//!
//! 目标页最多 1404×1872，源图常是 1600×2400 的扫描件。这里用**整数面积平均**
//! （16.16 定点、每个源像素按覆盖面积加权），不是逐像素浮点双线性：
//! Cortex-A7 没有 FPU 意义上的快路径，逐像素 f32 会被拖成几百毫秒，而面积平均
//! 顺带把扫描件的网点（screentone）平均掉，比双线性抽样的摩尔纹好看。
//!
//! 为了避开每像素一次整数除法，先把每个目标列的横向跨度倒数、每行的纵向跨度
//! 倒数预先算好（都是 u32），最后 `((acc * 1/xspan >> 32) * 1/yspan) >> 32`，
//! 除法次数 = 目标列数 + 目标行数，而不是像素数。
//!
//! 实测（M 系列 Mac，1600×2400 源 → 1404×1872）：inflate ≈ 6 ms、JPEG 解码
//! 46–72 ms、缩放 ≈ 25 ms；同一页命中缓存 ≈ 0.3 ms。设备（Cortex-A7）没有
//! 可比数据，这里只保证缩放本身没有浮点、没有逐像素除法。
//!
//! 灰度用感知加权 `(299r + 587g + 114b) / 1000`（u32 乘法，除 1000 被编译器
//! 常量折叠成乘法+移位，没有 idiv）。带 alpha 的 PNG 按白底合成（255=白）。

use std::cell::RefCell;
use std::cmp::Ordering;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

use miniz_oxide::inflate::{decompress_to_vec_with_limit, TINFLStatus};
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;
use zune_jpeg::JpegDecoder;
use zune_png::PngDecoder;

/// `.cbr` 的固定错误：CBR 需要外部解压器（见模块注释里的调研结论）。
const CBR_MSG: &str = "CBR (RAR) 需要外部解压器，请转成 CBZ";

/// 条目数上限：恶意 zip 可以有几十万个小条目，中央目录能把内存吃光。
const MAX_ENTRIES: usize = 20_000;
/// 中央目录本身的上限（20000 条 + 长文件名放到 16 MiB 已经非常宽松）。
const MAX_CD: usize = 16 << 20;
/// 单个图片条目解压后的上限（扫描件正常 2–8 MiB）。
const MAX_ENTRY: usize = 32 << 20;
/// 一页的像素数上限：30 MP × 3 B ≈ 90 MB，再大设备扛不住。
const MAX_PIXELS: usize = 30_000_000;
/// 解码器允许的单边像素上限（长条 webtoon 可能上万像素高）。
const MAX_DIM: usize = 65_535;
/// 解析图片头先试这么多字节（JPEG 的 SOF 通常在头 2 KiB 内）。
const HEADER_PREFIX: usize = 64 << 10;
/// 前缀里找不到 SOF/IHDR 时（EXIF/ICC 特别大）再放宽到 1 MiB。
const HEADER_LIMIT: usize = 1 << 20;

/// 最近渲染缓存保留的页数。
const CACHE_PAGES: usize = 2;

const SIG_LOCAL: u32 = 0x0403_4b50;
const SIG_CEN: u32 = 0x0201_4b50;
const SIG_EOCD: u32 = 0x0605_4b50;

/// zip 中央目录里的一条图片条目（只存元数据，字节按需再读）。
struct Entry {
    name: String,
    method: u16,
    flags: u16,
    comp: u32,
    unc: u32,
    off: u32,
}

/// 最近渲染过的整页灰度（FIFO，最多 [`CACHE_PAGES`] 条）。
struct CacheSlot {
    idx: usize,
    w: usize,
    h: usize,
    gray: Vec<u8>,
}

/// 解码后的一页：像素按 `ch` 通道交错。
struct Decoded {
    w: usize,
    h: usize,
    ch: usize,
    data: Vec<u8>,
}

/// 一本 CBZ。
///
/// 内部是 `RefCell`（设备单线程，UI 一帧内可能连问两次），**不跨线程**。
pub struct Comic {
    file: RefCell<File>,
    /// 图片条目，已按文件名自然排序（= 页序）。
    pages: Vec<Entry>,
    /// 每页尺寸，`aspect()` 首次用到才读图片头。
    dims: RefCell<Vec<Option<(u32, u32)>>>,
    /// 最近渲染结果。
    cache: RefCell<Vec<CacheSlot>>,
}

impl Comic {
    /// 打开一本 CBZ。只解析 zip 中央目录，不解压任何图片。
    pub fn open(path: &str) -> Result<Comic, String> {
        if has_ext(path, "cbr") {
            return Err(CBR_MSG.to_string());
        }
        let mut file = File::open(path).map_err(|e| format!("打不开 {path}: {e}"))?;
        let len = file
            .metadata()
            .map_err(|e| format!("读不到 {path} 的大小: {e}"))?
            .len();
        if len < 22 {
            return Err(format!("{path} 太小，不是 zip（CBZ）"));
        }

        // EOCD 在文件尾部，zip 注释最长 65535 字节。
        let tail_len = len.min(66_000) as usize;
        let mut tail = vec![0u8; tail_len];
        file.seek(SeekFrom::End(-(tail_len as i64)))
            .and_then(|_| file.read_exact(&mut tail))
            .map_err(|e| format!("读 {path} 尾部失败: {e}"))?;
        let eocd = (0..tail_len.saturating_sub(21))
            .rev()
            .find(|&i| le32(&tail, i) == SIG_EOCD)
            .ok_or_else(|| format!("{path} 里找不到 zip 结束记录，可能不是 CBZ"))?;

        let entries = le16(&tail, eocd + 10) as usize;
        let cd_size = le32(&tail, eocd + 12) as u64;
        let cd_off = le32(&tail, eocd + 16);
        if entries == 0xFFFF || cd_size == 0xFFFF_FFFF || cd_off == 0xFFFF_FFFF {
            return Err(format!("{path} 用了 zip64 扩展，暂不支持"));
        }
        if entries > MAX_ENTRIES {
            return Err(format!(
                "{path} 有 {entries} 个条目，超过上限 {MAX_ENTRIES}，疑似恶意文件"
            ));
        }
        if cd_size as usize > MAX_CD {
            return Err(format!("{path} 的中央目录超过 16 MiB，疑似恶意文件"));
        }

        // 中央目录紧挨在 EOCD 前面；用这个反推偏移，顺带兼容前面被塞了数据的 zip。
        let eocd_abs = len - tail_len as u64 + eocd as u64;
        let cd_start = eocd_abs
            .checked_sub(cd_size)
            .ok_or_else(|| format!("{path} 的 zip 目录偏移非法"))?;
        let mut cd = vec![0u8; cd_size as usize];
        file.seek(SeekFrom::Start(cd_start))
            .and_then(|_| file.read_exact(&mut cd))
            .map_err(|e| format!("读 {path} 的中央目录失败: {e}"))?;

        let mut pages: Vec<Entry> = parse_central(&cd)
            .into_iter()
            .filter(|e| is_page_name(&e.name))
            .collect();
        if pages.is_empty() {
            return Err(format!("{path} 里没有 JPEG/PNG 图片"));
        }
        // 自然排序：page2 必须排在 page10 前面。同号时用全名兜底，保证全序。
        pages.sort_by(|a, b| natural_cmp(&a.name, &b.name).then_with(|| a.name.cmp(&b.name)));

        let n = pages.len();
        Ok(Comic {
            file: RefCell::new(file),
            pages,
            dims: RefCell::new(vec![None; n]),
            cache: RefCell::new(Vec::new()),
        })
    }

    /// 页数。
    pub fn page_count(&self) -> usize {
        self.pages.len()
    }

    /// 第 `idx` 页的宽高比 **w/h**（image_width / image_height）。
    ///
    /// 读不到图片头时返回 `1.0`（签名没有 `Result`，不能因为一页坏图让 UI 崩掉）。
    pub fn aspect(&self, idx: usize) -> f32 {
        match self.dims(idx) {
            Some((w, h)) if h > 0 => w as f32 / h as f32,
            _ => 1.0,
        }
    }

    /// 渲染成 `w*h` 灰度（255=白），长度正好 `w*h`。
    ///
    /// 按比例 contain 居中，四周纯白。同一 (页, w, h) 连续调用直接返回缓存副本。
    pub fn render(&self, idx: usize, w: usize, h: usize) -> Result<Vec<u8>, String> {
        if idx >= self.pages.len() {
            return Err(format!("页号越界：{idx}（共 {} 页）", self.pages.len()));
        }
        if w == 0 || h == 0 {
            return Err(format!("渲染尺寸不能为 0：{w}×{h}"));
        }
        let out_px = w
            .checked_mul(h)
            .filter(|&n| n <= MAX_PIXELS)
            .ok_or_else(|| format!("渲染尺寸过大：{w}×{h}"))?;
        // 注意：先取完缓存再放新值，避免同一帧里两次问同一页时重复解码。
        if let Some(hit) = self
            .cache
            .borrow()
            .iter()
            .find(|s| s.idx == idx && s.w == w && s.h == h)
            .map(|s| s.gray.clone())
        {
            return Ok(hit);
        }

        let bytes = self.entry_bytes(idx, MAX_ENTRY, false)?;
        let img = decode_image(&bytes).map_err(|e| format!("第 {} 页：{e}", idx + 1))?;
        let mut gray = vec![255u8; out_px];
        scale_contain(&img, w, h, &mut gray);

        let mut cache = self.cache.borrow_mut();
        if cache.len() >= CACHE_PAGES {
            cache.remove(0);
        }
        cache.push(CacheSlot {
            idx,
            w,
            h,
            gray: gray.clone(),
        });
        Ok(gray)
    }

    /// 第 `idx` 页的图片尺寸，首次访问才读（并解析图片头）。
    fn dims(&self, idx: usize) -> Option<(u32, u32)> {
        if idx >= self.pages.len() {
            return None;
        }
        let cached = self.dims.borrow().get(idx).copied().flatten();
        if let Some(d) = cached {
            return Some(d);
        }
        // 只解压前缀：SOF/IHDR 在文件开头，大图也不用全解。
        let d = match self
            .entry_bytes(idx, HEADER_PREFIX, true)
            .ok()
            .and_then(|head| image_dims(&head))
        {
            Some(d) => d,
            // 少见：APP 段（EXIF/ICC）比 64 KiB 还大，放宽一次再试。
            None => {
                let head = self.entry_bytes(idx, HEADER_LIMIT, true).ok()?;
                image_dims(&head)?
            }
        };
        if let Some(slot) = self.dims.borrow_mut().get_mut(idx) {
            *slot = Some(d);
        }
        Some(d)
    }

    /// 取第 `idx` 个条目的字节（zip 条目解压后的内容）。
    ///
    /// `allow_truncated = true` 时，超出 `limit` 的部分直接丢掉（解析图片头够用），
    /// 这样 `aspect()` 不用为了一个 SOF 把整页解出来。
    fn entry_bytes(
        &self,
        idx: usize,
        limit: usize,
        allow_truncated: bool,
    ) -> Result<Vec<u8>, String> {
        let e = &self.pages[idx];
        let no = idx + 1;
        if e.flags & 1 != 0 {
            return Err(format!("第 {no} 页在 zip 里是加密的，读不了"));
        }
        if e.method != 0 && e.method != 8 {
            return Err(format!(
                "第 {no} 页用了不支持的 zip 压缩方法 {}（只支持 store/deflate）",
                e.method
            ));
        }
        let limit = limit.max(1);
        if !allow_truncated {
            if e.unc as usize > limit {
                return Err(format!("第 {no} 页解压后 {} 字节，超过上限 {limit}", e.unc));
            }
            if e.comp as usize > limit {
                return Err(format!("第 {no} 页压缩后 {} 字节，超过上限", e.comp));
            }
        }
        // 只解析图片头时按 limit 截断着读：大页也只碰前面一小段。
        let take = (e.comp as usize).min(limit);

        let mut file = self.file.borrow_mut();
        let hdr =
            read_at(&mut file, e.off as u64, 30).map_err(|err| format!("第 {no} 页：{err}"))?;
        if le32(&hdr, 0) != SIG_LOCAL {
            return Err(format!("第 {no} 页的 zip 局部头损坏"));
        }
        let name_len = le16(&hdr, 26) as u64;
        let extra_len = le16(&hdr, 28) as u64;
        let data_off = e.off as u64 + 30 + name_len + extra_len;
        let raw = read_at(&mut file, data_off, take).map_err(|err| format!("第 {no} 页：{err}"))?;

        if e.method == 0 {
            // 上面已经挡过 unc/comp 超过 limit 的整页读，这里 raw 一定在限内（或本就要截断）。
            return Ok(raw);
        }
        match decompress_to_vec_with_limit(&raw, limit) {
            Ok(v) => Ok(v),
            // 截断的输入/输出：miniz 会把已解出的部分放在 err.output 里，图片头就在开头。
            Err(err)
                if allow_truncated
                    && matches!(
                        err.status,
                        TINFLStatus::HasMoreOutput | TINFLStatus::NeedsMoreInput
                    ) =>
            {
                Ok(err.output)
            }
            Err(err) => Err(format!("第 {no} 页解压失败: {err}")),
        }
    }
}

/// 从中央目录解析条目。遇到不认识的记录就停（有些 zip 尾部还跟着数字签名）。
fn parse_central(cd: &[u8]) -> Vec<Entry> {
    let mut out = Vec::new();
    let mut p = 0usize;
    while p + 46 <= cd.len() && le32(cd, p) == SIG_CEN {
        let name_len = le16(cd, p + 28) as usize;
        let extra_len = le16(cd, p + 30) as usize;
        let comment_len = le16(cd, p + 32) as usize;
        let name_end = p + 46 + name_len;
        if name_end > cd.len() {
            break;
        }
        out.push(Entry {
            name: String::from_utf8_lossy(&cd[p + 46..name_end]).into_owned(),
            flags: le16(cd, p + 8),
            method: le16(cd, p + 10),
            comp: le32(cd, p + 20),
            unc: le32(cd, p + 24),
            off: le32(cd, p + 42),
        });
        p = name_end + extra_len + comment_len;
    }
    out
}

/// 是不是一页漫画：非目录、非隐藏、非 `__MACOSX`、扩展名是 jpg/jpeg/png。
fn is_page_name(name: &str) -> bool {
    if name.ends_with('/') {
        return false;
    }
    // 目录项/隐藏文件：任何一级以 `.` 开头（`.DS_Store`、`.hidden/x.jpg`）；
    // macOS 打包附带的 `__MACOSX/` 资源分叉也一律丢掉。
    if name
        .split('/')
        .any(|seg| seg.starts_with('.') || seg == "__MACOSX")
    {
        return false;
    }
    match name.rsplit_once('.') {
        Some((_, ext)) => {
            ext.eq_ignore_ascii_case("jpg")
                || ext.eq_ignore_ascii_case("jpeg")
                || ext.eq_ignore_ascii_case("png")
        }
        None => false,
    }
}

/// 文件名的自然排序：数字段按数值比，`page2 < page10`。
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (ca, cb) = (a[i], b[j]);
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            let za = i; // 前导零起点
            let zb = j;
            while i < a.len() && a[i] == b'0' {
                i += 1;
            }
            while j < b.len() && b[j] == b'0' {
                j += 1;
            }
            let ea = digits_end(a, i);
            let eb = digits_end(b, j);
            let (na, nb) = (&a[i..ea], &b[j..eb]);
            match na.len().cmp(&nb.len()).then_with(|| na.cmp(nb)) {
                Ordering::Equal => {
                    // 数值一样（如 2 与 02）：前导零少的在前，然后继续比后面。
                    let (zca, zcb) = (i - za, j - zb);
                    if zca != zcb {
                        return zca.cmp(&zcb);
                    }
                    i = ea;
                    j = eb;
                }
                ord => return ord,
            }
        } else if ca != cb {
            return ca.cmp(&cb);
        } else {
            i += 1;
            j += 1;
        }
    }
    a.len().cmp(&b.len())
}

fn digits_end(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && s[i].is_ascii_digit() {
        i += 1;
    }
    i
}

/// 从图片字节里读尺寸，不完整解码。
fn image_dims(d: &[u8]) -> Option<(u32, u32)> {
    if d.starts_with(&[0xFF, 0xD8]) {
        jpeg_dims(d)
    } else if d.starts_with(b"\x89PNG\r\n\x1a\n") {
        png_dims(d)
    } else {
        None
    }
}

fn jpeg_dims(d: &[u8]) -> Option<(u32, u32)> {
    let mut i = 2usize;
    while i + 9 <= d.len() {
        if d[i] != 0xFF {
            // 段间填充/垃圾字节，往后挪一格。
            i += 1;
            continue;
        }
        let marker = d[i + 1];
        // 填充 0xFF、无长度的独立标记（RST/TEM/SOI/EOI）。
        if marker == 0xFF {
            i += 1;
            continue;
        }
        if marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            i += 2;
            continue;
        }
        let len = u16::from_be_bytes([d[i + 2], d[i + 3]]) as usize;
        // SOF0..SOF15，但 C4=DHT、C8=JPG、CC=DAC 不是 SOF。
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
            let h = u16::from_be_bytes([d[i + 5], d[i + 6]]) as u32;
            let w = u16::from_be_bytes([d[i + 7], d[i + 8]]) as u32;
            return (w > 0 && h > 0).then_some((w, h));
        }
        i += 2 + len;
    }
    None
}

fn png_dims(d: &[u8]) -> Option<(u32, u32)> {
    // 8 字节签名 + 4 长度 + 4 "IHDR" + w/h 各 4 字节
    if d.len() < 24 || &d[12..16] != b"IHDR" {
        return None;
    }
    let w = be32(d, 16);
    let h = be32(d, 20);
    (w > 0 && h > 0).then_some((w, h))
}

/// 解码一页：JPEG 走 zune-jpeg，PNG 走 zune-png（两者都纯 Rust）。
fn decode_image(data: &[u8]) -> Result<Decoded, String> {
    let (w, h) = image_dims(data).ok_or("既不是 JPEG 也不是 PNG")?;
    let (w, h) = (w as usize, h as usize);
    if w.saturating_mul(h) > MAX_PIXELS {
        return Err(format!("图片 {w}×{h} 超过像素上限"));
    }

    let opts = DecoderOptions::default()
        .set_max_width(MAX_DIM)
        .set_max_height(MAX_DIM);

    if data.starts_with(&[0xFF, 0xD8]) {
        let mut dec = JpegDecoder::new_with_options(ZCursor::new(data), opts);
        let px = dec.decode().map_err(|e| format!("JPEG 解码失败: {e}"))?;
        let ch = channel_count(px.len(), w, h, "JPEG")?;
        if ch == 4 {
            return Err("不支持的 JPEG 色彩模式（CMYK/YCCK）".to_string());
        }
        return Ok(Decoded { w, h, ch, data: px });
    }

    let mut dec =
        PngDecoder::new_with_options(ZCursor::new(data), opts.png_set_strip_to_8bit(true));
    dec.decode_headers()
        .map_err(|e| format!("PNG 头解析失败: {e}"))?;
    let dim = dec.dimensions().ok_or("PNG 缺尺寸")?;
    if dim.0.saturating_mul(dim.1) > MAX_PIXELS {
        return Err(format!("图片 {}×{} 超过像素上限", dim.0, dim.1));
    }
    let ch = match dec.colorspace().ok_or("PNG 缺色彩空间")? {
        ColorSpace::Luma => 1,
        ColorSpace::LumaA => 2,
        ColorSpace::RGB => 3,
        ColorSpace::RGBA => 4,
        other => return Err(format!("不支持的 PNG 色彩空间 {other:?}")),
    };
    let px = dec.decode_raw().map_err(|e| format!("PNG 解码失败: {e}"))?;
    if px.len() != dim.0 * dim.1 * ch {
        return Err(format!(
            "PNG 像素长度不符：{} != {}×{}×{}",
            px.len(),
            dim.0,
            dim.1,
            ch
        ));
    }
    Ok(Decoded {
        w: dim.0,
        h: dim.1,
        ch,
        data: px,
    })
}

/// 由解码输出长度反推通道数（JPEG 的灰度图只返回 1 通道）。
fn channel_count(len: usize, w: usize, h: usize, what: &str) -> Result<usize, String> {
    let px = w * h;
    if px == 0 || !len.is_multiple_of(px) || !(1..=4).contains(&(len / px)) {
        return Err(format!("{what} 像素数据长度异常：{len}"));
    }
    Ok(len / px)
}

/// 面积平均缩放 + contain 居中，写进已填白的 `out`（`w*h`）。
///
/// 两步分离：先按行做纵向加权平均得到 `vrow`（源宽），再按列做横向加权平均。
/// 权重是 16.16 定点，卷积累加用 u64；每目标像素两次乘法代替一次除法。
fn scale_contain(img: &Decoded, w: usize, h: usize, out: &mut [u8]) {
    let (sw, sh, ch) = (img.w, img.h, img.ch);
    if sw == 0 || sh == 0 || w == 0 || h == 0 {
        return;
    }
    let (aw, ah) = fit(sw, sh, w, h);
    let ox = (w - aw) / 2;
    let oy = (h - ah) / 2;

    // 每目标列覆盖的源列区间（16.16 定点）与其跨度倒数。
    let mut spans: Vec<(u64, u64)> = Vec::with_capacity(aw);
    let mut inv_x: Vec<u64> = Vec::with_capacity(aw);
    for x in 0..aw {
        let x0 = ((x as u64) << 16) * sw as u64 / aw as u64;
        let x1 = (((x as u64) + 1) << 16) * sw as u64 / aw as u64;
        let span = (x1 - x0).max(1);
        spans.push((x0, x1));
        inv_x.push((1u64 << 32) / span);
    }

    let mut gray_row = vec![0u8; sw];
    let mut vrow = vec![0u64; sw];

    for dy in 0..ah {
        let y0 = ((dy as u64) << 16) * sh as u64 / ah as u64;
        let y1 = (((dy as u64) + 1) << 16) * sh as u64 / ah as u64;
        let yspan = (y1 - y0).max(1);
        vrow.iter_mut().for_each(|v| *v = 0);

        // 纵向：本目标行覆盖的源行，按覆盖比例加权累加灰度。
        let sy0 = (y0 >> 16) as usize;
        let sy1 = (((y1 + 0xFFFF) >> 16) as usize).min(sh);
        for sy in sy0..sy1 {
            let wy = overlap(y0, y1, sy);
            if wy == 0 {
                continue;
            }
            let start = sy * sw * ch;
            gray_row_of(&img.data[start..start + sw * ch], ch, &mut gray_row);
            for (v, g) in vrow.iter_mut().zip(gray_row.iter()) {
                *v += *g as u64 * wy;
            }
        }

        // 横向：每目标列在自己的源列区间里加权平均。
        let inv_y = (1u64 << 32) / yspan;
        let row_start = (oy + dy) * w + ox;
        for (dx, px_out) in out[row_start..row_start + aw].iter_mut().enumerate() {
            let (x0, x1) = spans[dx];
            let sx0 = (x0 >> 16) as usize;
            let sx1 = (((x1 + 0xFFFF) >> 16) as usize).min(sw);
            let mut acc = 0u64;
            for (sx, &v) in vrow.iter().enumerate().take(sx1).skip(sx0) {
                acc += v * overlap(x0, x1, sx);
            }
            // acc / ((x1-x0)*(y1-y0))，用两个倒数乘代替除法；
            // 两级都 +2^31 做四舍五入，否则纯白 255 会掉成 254。
            let half = 1u64 << 31;
            let g = (((acc * inv_x[dx] + half) >> 32) * inv_y + half) >> 32;
            *px_out = g.min(255) as u8;
        }
    }
}

/// 源图在 `w×h` 里 contain 后的实际尺寸（至少 1 像素）。
fn fit(sw: usize, sh: usize, w: usize, h: usize) -> (usize, usize) {
    let (sw, sh, w, h) = (sw as u64, sh as u64, w as u64, h as u64);
    if sw * h <= sh * w {
        // 源图更瘦，按高度贴合。
        let aw = (sw * h / sh).clamp(1, w) as usize;
        (aw, h as usize)
    } else {
        let ah = (sh * w / sw).clamp(1, h) as usize;
        (w as usize, ah)
    }
}

/// `[a,b)`（16.16）与第 `i` 个源像素 `[i<<16,(i+1)<<16)` 的重叠长度。
#[inline]
fn overlap(a: u64, b: u64, i: usize) -> u64 {
    let lo = a.max((i as u64) << 16);
    let hi = b.min(((i as u64) + 1) << 16);
    hi.saturating_sub(lo)
}

/// 把一行像素转成灰度（带 alpha 的按白底合成）。
fn gray_row_of(row: &[u8], ch: usize, out: &mut [u8]) {
    match ch {
        1 => out.copy_from_slice(&row[..out.len()]),
        3 => {
            for (o, px) in out.iter_mut().zip(row.chunks_exact(3)) {
                *o = luma(px[0], px[1], px[2]);
            }
        }
        4 => {
            for (o, px) in out.iter_mut().zip(row.chunks_exact(4)) {
                *o = over_white(luma(px[0], px[1], px[2]), px[3]);
            }
        }
        2 => {
            for (o, px) in out.iter_mut().zip(row.chunks_exact(2)) {
                *o = over_white(px[0], px[1]);
            }
        }
        _ => out.fill(255),
    }
}

/// 感知加权灰度：`(299r + 587g + 114b) / 1000`。除 1000 会被编译成乘法+移位。
#[inline]
fn luma(r: u8, g: u8, b: u8) -> u8 {
    ((299 * r as u32 + 587 * g as u32 + 114 * b as u32) / 1000) as u8
}

/// 灰度值 `g` 在透明度 `a` 下与白底合成。
#[inline]
fn over_white(g: u8, a: u8) -> u8 {
    if a == 255 {
        return g;
    }
    ((g as u32 * a as u32 + 255 * (255 - a as u32) + 127) / 255) as u8
}

fn has_ext(path: &str, want: &str) -> bool {
    path.rsplit_once('.')
        .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case(want))
}

fn le16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn be32(b: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// 从当前位置读 `n` 字节（用 `seek` 而不是整文件读，几百 MB 的 CBZ 也不吃内存）。
fn read_at(file: &mut File, off: u64, n: usize) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; n];
    file.seek(SeekFrom::Start(off))?;
    file.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- 造测试数据：不依赖任何外部图片文件 ----------

    /// PNG 块 CRC32（只测试用，项目本身不需要）。
    fn crc32(data: &[u8]) -> u32 {
        let mut c = 0xFFFF_FFFFu32;
        for &b in data {
            c ^= b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
        !c
    }

    fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    }

    /// 手写最小 PNG（8 bit 真彩、无交错、filter=0），整张纯色。
    fn png_rgb(w: usize, h: usize, rgb: [u8; 3]) -> Vec<u8> {
        let mut row = Vec::with_capacity(1 + w * 3);
        row.push(0); // 每行一个 filter 字节：0 = None
        for _ in 0..w {
            row.extend_from_slice(&rgb);
        }
        let mut raw = Vec::with_capacity(h * row.len());
        for _ in 0..h {
            raw.extend_from_slice(&row);
        }
        let mut png = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
        let mut ihdr = Vec::new();
        ihdr.extend_from_slice(&(w as u32).to_be_bytes());
        ihdr.extend_from_slice(&(h as u32).to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        png_chunk(&mut png, b"IHDR", &ihdr);
        png_chunk(
            &mut png,
            b"IDAT",
            &miniz_oxide::deflate::compress_to_vec_zlib(&raw, 6),
        );
        png_chunk(&mut png, b"IEND", &[]);
        png
    }

    /// 拼一个 zip；`files` = (条目名, 内容, 是否 deflate)。CRC 写 0，本模块不校验。
    fn zip_of(files: &[(&str, &[u8], bool)]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cd = Vec::new();
        for (name, data, deflate) in files {
            let body = if *deflate {
                miniz_oxide::deflate::compress_to_vec(data, 6)
            } else {
                data.to_vec()
            };
            let method: u16 = if *deflate { 8 } else { 0 };
            let off = out.len() as u32;

            // 局部头 30 字节 + 名字 + 数据
            out.extend_from_slice(&SIG_LOCAL.to_le_bytes());
            out.extend_from_slice(&20u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&method.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u32.to_le_bytes());
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(&(name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(name.as_bytes());
            out.extend_from_slice(&body);

            // 中央目录 46 字节 + 名字
            cd.extend_from_slice(&SIG_CEN.to_le_bytes());
            cd.extend_from_slice(&20u16.to_le_bytes());
            cd.extend_from_slice(&20u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&method.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u32.to_le_bytes());
            cd.extend_from_slice(&(body.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(data.len() as u32).to_le_bytes());
            cd.extend_from_slice(&(name.len() as u16).to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u16.to_le_bytes());
            cd.extend_from_slice(&0u32.to_le_bytes());
            cd.extend_from_slice(&off.to_le_bytes());
            cd.extend_from_slice(name.as_bytes());
        }
        let cd_off = out.len() as u32;
        let cd_size = cd.len() as u32;
        out.extend_from_slice(&cd);
        out.extend_from_slice(&SIG_EOCD.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&(files.len() as u16).to_le_bytes());
        out.extend_from_slice(&cd_size.to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// 写临时 cbz/cbr，返回路径（测试结束由调用方删）。
    fn temp_file(tag: &str, ext: &str, data: &[u8]) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("rmdev_comic_{}_{tag}.{ext}", std::process::id()));
        std::fs::write(&p, data).unwrap();
        p
    }

    fn open_zip(tag: &str, files: &[(&str, &[u8], bool)]) -> (Comic, std::path::PathBuf) {
        let path = temp_file(tag, "cbz", &zip_of(files));
        let comic = Comic::open(path.to_str().unwrap()).unwrap();
        (comic, path)
    }

    fn close(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
    }

    /// `unwrap_err` 需要 `Comic: Debug`，这里不为了测试给公开类型加 Debug。
    fn expect_err(r: Result<Comic, String>) -> String {
        match r {
            Ok(_) => panic!("这里应当返回 Err"),
            Err(e) => e,
        }
    }

    // ---------- 用例 ----------

    #[test]
    fn natural_sort_p1_p2_p10() {
        let img = png_rgb(4, 6, [0, 0, 0]);
        let (c, p) = open_zip(
            "sort",
            &[
                ("p10.jpg", &img, false),
                ("p1.jpg", &img, false),
                ("p2.jpg", &img, false),
            ],
        );
        let names: Vec<&str> = c.pages.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["p1.jpg", "p2.jpg", "p10.jpg"]);
        close(&p);
    }

    #[test]
    fn ignores_non_images_and_macosx() {
        let img = png_rgb(4, 6, [0, 0, 0]);
        let (c, p) = open_zip(
            "filter",
            &[
                ("001.jpg", &img, false),
                ("ComicInfo.xml", b"<ComicInfo/>", false),
                ("readme.txt", b"hi", false),
                ("__MACOSX/001.jpg", &img, false),
                ("__MACOSX/._001.jpg", b"junk", false),
                (".DS_Store", b"junk", false),
                ("sub/.hidden.jpg", &img, false),
                ("dir/", b"", false),
            ],
        );
        assert_eq!(c.page_count(), 1);
        assert_eq!(c.pages[0].name, "001.jpg");
        close(&p);
    }

    #[test]
    fn render_size_border_and_center() {
        // 30×40 的深色页 → 60×60 画布：contain 后 45×60，左右各白 7 像素。
        let img = png_rgb(30, 40, [10, 20, 30]);
        let (c, p) = open_zip("render", &[("001.png", &img, true)]);
        let (w, h) = (60usize, 60usize);
        let g = c.render(0, w, h).unwrap();
        assert_eq!(g.len(), w * h);
        // contain 后内容占 x∈[7,52)，四角与左右边缘必须是白的
        assert_eq!(g[0], 255, "左上角");
        assert_eq!(g[w - 1], 255, "右上角");
        assert_eq!(g[(h - 1) * w], 255, "左下角");
        assert_eq!(g[h * w - 1], 255, "右下角");
        for y in 0..h {
            for x in 0..7 {
                assert_eq!(g[y * w + x], 255, "左白边 y={y} x={x}");
                assert_eq!(g[y * w + 52 + x], 255, "右白边 y={y} x={x}");
            }
        }
        // 中间有非白像素（灰度 = (299*10+587*20+114*30)/1000 = 18）
        assert!(g[(h / 2) * w + w / 2] < 255, "中间应是非白");
        // 1:1 渲染必须逐像素保持灰度值（顺带验证面积平均的定点倒数没有掉精度）
        let native = c.render(0, 30, 40).unwrap();
        assert!(native.iter().all(|&v| v == 18), "1:1 灰度应为 18");
        close(&p);
    }

    #[test]
    fn render_is_deterministic_and_cached() {
        let img = png_rgb(30, 40, [200, 100, 60]);
        let (c, p) = open_zip(
            "determin",
            &[
                ("001.png", &img, true),
                ("002.png", &png_rgb(20, 20, [0, 0, 0]), true),
                ("003.png", &png_rgb(20, 20, [10, 10, 10]), true),
            ],
        );
        let a = c.render(0, 40, 40).unwrap();
        let b = c.render(0, 40, 40).unwrap();
        assert_eq!(a, b);
        // 只留最近 2 页
        c.render(1, 20, 20).unwrap();
        c.render(2, 20, 20).unwrap();
        assert_eq!(c.cache.borrow().len(), CACHE_PAGES);
        assert_eq!(c.cache.borrow()[1].idx, 2);
        close(&p);
    }

    #[test]
    fn out_of_range_is_err_not_panic() {
        let img = png_rgb(4, 6, [0, 0, 0]);
        let (c, p) = open_zip("oob", &[("001.png", &img, false)]);
        assert!(c.render(1, 10, 10).is_err());
        assert!(c.render(usize::MAX, 10, 10).is_err());
        assert_eq!(c.aspect(9), 1.0); // aspect 不返回 Result，退回 1.0
        close(&p);
    }

    #[test]
    fn aspect_uses_image_ratio() {
        let img = png_rgb(30, 40, [1, 2, 3]);
        let (c, p) = open_zip("aspect", &[("001.png", &img, true)]);
        assert!((c.aspect(0) - 0.75).abs() < 1e-6);
        close(&p);
    }

    #[test]
    fn cbr_reports_clear_error() {
        let p = temp_file("cbr", "cbr", b"Rar!\x1a\x07\x00");
        let err = expect_err(Comic::open(p.to_str().unwrap()));
        assert_eq!(err, "CBR (RAR) 需要外部解压器，请转成 CBZ");
        close(&p);
    }

    #[test]
    fn too_many_entries_is_err() {
        let mut files: Vec<(String, Vec<u8>)> = Vec::new();
        for i in 0..=MAX_ENTRIES {
            files.push((format!("p{i}.jpg"), Vec::new()));
        }
        let refs: Vec<(&str, &[u8], bool)> = files
            .iter()
            .map(|(n, d)| (n.as_str(), d.as_slice(), false))
            .collect();
        let p = temp_file("many", "cbz", &zip_of(&refs));
        let err = expect_err(Comic::open(p.to_str().unwrap()));
        assert!(err.contains("超过上限"), "err = {err}");
        close(&p);
    }

    #[test]
    fn not_a_zip_is_err() {
        let p = temp_file("junk", "cbz", &[0u8; 128]);
        assert!(Comic::open(p.to_str().unwrap()).is_err());
        let err = expect_err(Comic::open("/nonexistent/xx.cbz"));
        assert!(err.contains("打不开"), "err = {err}");
        close(&p);
    }

    /// baseline JPEG（16×24，LibJPEG 编码）。
    const MINI_JPG: &[u8] = &[
        255, 216, 255, 224, 0, 16, 74, 70, 73, 70, 0, 1, 1, 0, 0, 1, 0, 1, 0, 0, 255, 219, 0, 67,
        0, 20, 14, 15, 18, 15, 13, 20, 18, 16, 18, 23, 21, 20, 24, 30, 50, 33, 30, 28, 28, 30, 61,
        44, 46, 36, 50, 73, 64, 76, 75, 71, 64, 70, 69, 80, 90, 115, 98, 80, 85, 109, 86, 69, 70,
        100, 136, 101, 109, 119, 123, 129, 130, 129, 78, 96, 141, 151, 140, 125, 150, 115, 126,
        129, 124, 255, 219, 0, 67, 1, 21, 23, 23, 30, 26, 30, 59, 33, 33, 59, 124, 83, 70, 83, 124,
        124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124,
        124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124,
        124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 255, 192, 0, 17, 8, 0, 24,
        0, 16, 3, 1, 34, 0, 2, 17, 1, 3, 17, 1, 255, 196, 0, 31, 0, 0, 1, 5, 1, 1, 1, 1, 1, 1, 0,
        0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 255, 196, 0, 181, 16, 0, 2, 1, 3,
        3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 125, 1, 2, 3, 0, 4, 17, 5, 18, 33, 49, 65, 6, 19, 81, 97,
        7, 34, 113, 20, 50, 129, 145, 161, 8, 35, 66, 177, 193, 21, 82, 209, 240, 36, 51, 98, 114,
        130, 9, 10, 22, 23, 24, 25, 26, 37, 38, 39, 40, 41, 42, 52, 53, 54, 55, 56, 57, 58, 67, 68,
        69, 70, 71, 72, 73, 74, 83, 84, 85, 86, 87, 88, 89, 90, 99, 100, 101, 102, 103, 104, 105,
        106, 115, 116, 117, 118, 119, 120, 121, 122, 131, 132, 133, 134, 135, 136, 137, 138, 146,
        147, 148, 149, 150, 151, 152, 153, 154, 162, 163, 164, 165, 166, 167, 168, 169, 170, 178,
        179, 180, 181, 182, 183, 184, 185, 186, 194, 195, 196, 197, 198, 199, 200, 201, 202, 210,
        211, 212, 213, 214, 215, 216, 217, 218, 225, 226, 227, 228, 229, 230, 231, 232, 233, 234,
        241, 242, 243, 244, 245, 246, 247, 248, 249, 250, 255, 196, 0, 31, 1, 0, 3, 1, 1, 1, 1, 1,
        1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 255, 196, 0, 181, 17, 0,
        2, 1, 2, 4, 4, 3, 4, 7, 5, 4, 4, 0, 1, 2, 119, 0, 1, 2, 3, 17, 4, 5, 33, 49, 6, 18, 65, 81,
        7, 97, 113, 19, 34, 50, 129, 8, 20, 66, 145, 161, 177, 193, 9, 35, 51, 82, 240, 21, 98,
        114, 209, 10, 22, 36, 52, 225, 37, 241, 23, 24, 25, 26, 38, 39, 40, 41, 42, 53, 54, 55, 56,
        57, 58, 67, 68, 69, 70, 71, 72, 73, 74, 83, 84, 85, 86, 87, 88, 89, 90, 99, 100, 101, 102,
        103, 104, 105, 106, 115, 116, 117, 118, 119, 120, 121, 122, 130, 131, 132, 133, 134, 135,
        136, 137, 138, 146, 147, 148, 149, 150, 151, 152, 153, 154, 162, 163, 164, 165, 166, 167,
        168, 169, 170, 178, 179, 180, 181, 182, 183, 184, 185, 186, 194, 195, 196, 197, 198, 199,
        200, 201, 202, 210, 211, 212, 213, 214, 215, 216, 217, 218, 226, 227, 228, 229, 230, 231,
        232, 233, 234, 242, 243, 244, 245, 246, 247, 248, 249, 250, 255, 218, 0, 12, 3, 1, 0, 2,
        17, 3, 17, 0, 63, 0, 201, 68, 171, 8, 148, 34, 85, 132, 74, 82, 145, 84, 102, 34, 37, 88,
        68, 162, 138, 198, 76, 230, 162, 217, 255, 217,
    ];

    /// 同尺寸的**渐进式** JPEG（PIL progressive=True）：扫描版漫画的主力格式。
    const MINI_PROG_JPG: &[u8] = &[
        255, 216, 255, 224, 0, 16, 74, 70, 73, 70, 0, 1, 1, 0, 0, 1, 0, 1, 0, 0, 255, 219, 0, 67,
        0, 20, 14, 15, 18, 15, 13, 20, 18, 16, 18, 23, 21, 20, 24, 30, 50, 33, 30, 28, 28, 30, 61,
        44, 46, 36, 50, 73, 64, 76, 75, 71, 64, 70, 69, 80, 90, 115, 98, 80, 85, 109, 86, 69, 70,
        100, 136, 101, 109, 119, 123, 129, 130, 129, 78, 96, 141, 151, 140, 125, 150, 115, 126,
        129, 124, 255, 219, 0, 67, 1, 21, 23, 23, 30, 26, 30, 59, 33, 33, 59, 124, 83, 70, 83, 124,
        124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124,
        124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124,
        124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 124, 255, 194, 0, 17, 8, 0, 24,
        0, 16, 3, 1, 34, 0, 2, 17, 1, 3, 17, 1, 255, 196, 0, 23, 0, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 3, 1, 4, 255, 196, 0, 22, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 1, 2, 3, 255, 218, 0, 12, 3, 1, 0, 2, 16, 3, 16, 0, 0, 1, 228, 162, 131, 148, 35, 47,
        255, 196, 0, 20, 16, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 32, 255, 218, 0, 8, 1,
        1, 0, 1, 5, 2, 95, 255, 196, 0, 22, 17, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        2, 1, 255, 218, 0, 8, 1, 3, 1, 1, 63, 1, 141, 67, 255, 196, 0, 21, 17, 1, 1, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 255, 218, 0, 8, 1, 2, 1, 1, 63, 1, 89, 191, 255, 196, 0,
        20, 16, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 32, 255, 218, 0, 8, 1, 1, 0, 6, 63,
        2, 95, 255, 196, 0, 21, 16, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 255, 218,
        0, 8, 1, 1, 0, 1, 63, 33, 8, 32, 130, 8, 47, 255, 218, 0, 12, 3, 1, 0, 2, 0, 3, 0, 0, 0,
        16, 11, 255, 0, 255, 196, 0, 21, 17, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,
        255, 218, 0, 8, 1, 3, 1, 1, 63, 16, 53, 95, 255, 196, 0, 21, 17, 1, 1, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 1, 255, 218, 0, 8, 1, 2, 1, 1, 63, 16, 20, 255, 196, 0, 20, 16, 1,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 32, 255, 218, 0, 8, 1, 1, 0, 1, 63, 16, 95,
        255, 0, 255, 217,
    ];

    #[test]
    fn jpeg_baseline_and_progressive() {
        let (c, p) = open_zip(
            "jpeg",
            &[
                ("001.jpg", MINI_JPG, false),
                ("002.jpg", MINI_PROG_JPG, true),
            ],
        );
        assert_eq!(c.page_count(), 2);
        assert!((c.aspect(0) - 16.0 / 24.0).abs() < 1e-6, "aspect 从 SOF 读");
        assert!(
            (c.aspect(1) - 16.0 / 24.0).abs() < 1e-6,
            "渐进式也要能读 SOF"
        );
        for idx in 0..2 {
            let g = c.render(idx, 64, 96).unwrap();
            assert_eq!(g.len(), 64 * 96);
            assert!(g.iter().any(|&v| v < 255), "第 {idx} 页应有内容");
            // 夹具是左上暗、右下亮的渐变：检查解码方向 + 灰度符号没搞反。
            let top = g[2 * 64 + 2] as i32;
            let bottom = g[93 * 64 + 61] as i32;
            assert!(top < bottom, "第 {idx} 页灰度梯度反了：{top} vs {bottom}");
            assert!(bottom > 100 && bottom < 255, "第 {idx} 页右下应为中灰");
        }
        close(&p);
    }

    #[test]
    fn render_upscale_and_odd_sizes() {
        let img = png_rgb(5, 7, [255, 255, 255]);
        let (c, p) = open_zip("odd", &[("001.png", &img, true)]);
        // 放大到远大于源图：全是白的，但不能 panic，长度要对。
        let g = c.render(0, 101, 37).unwrap();
        assert_eq!(g.len(), 101 * 37);
        assert!(g.iter().all(|&v| v == 255));
        // 极小画布
        let one = c.render(0, 1, 1).unwrap();
        assert_eq!(one.len(), 1);
        close(&p);
    }

    #[test]
    fn aspect_works_on_entries_bigger_than_header_limit() {
        // 4×6 的 PNG 后面接 1.2 MB 垃圾（> HEADER_LIMIT）：aspect 只读前缀。
        let mut big = png_rgb(4, 6, [0, 0, 0]);
        big.resize(big.len() + 1_200_000, 0);
        let (c, p) = open_zip("big", &[("001.png", &big, false)]);
        assert!(big.len() > HEADER_LIMIT);
        assert!((c.aspect(0) - 4.0 / 6.0).abs() < 1e-6);
        // 整页渲染也照常（IEND 之后的垃圾解码器会忽略）
        let g = c.render(0, 8, 12).unwrap();
        assert_eq!(g.len(), 8 * 12);
        assert!(g.iter().all(|&v| v == 0));
        close(&p);
    }

    #[test]
    fn aspect_finds_sof_beyond_first_64k() {
        // 在 SOI 后面插一个 65 KB 的 APP2 段（大 EXIF/ICC 的极端情况），
        // SOF 落在 64 KiB 之外 → 走 HEADER_LIMIT 的第二档前缀。
        let mut jpg = Vec::with_capacity(MINI_JPG.len() + 65_540);
        jpg.extend_from_slice(&MINI_JPG[..2]); // SOI
        let payload = 65_533usize;
        jpg.extend_from_slice(&[0xFF, 0xE2]);
        jpg.extend_from_slice(&((payload + 2) as u16).to_be_bytes());
        jpg.extend(std::iter::repeat_n(0u8, payload));
        jpg.extend_from_slice(&MINI_JPG[2..]);
        assert!(jpg.len() > HEADER_PREFIX);
        let (c, p) = open_zip("bigapp", &[("001.jpg", &jpg, false)]);
        assert!((c.aspect(0) - 16.0 / 24.0).abs() < 1e-6);
        let g = c.render(0, 32, 48).unwrap();
        assert!(g.iter().any(|&v| v < 255));
        close(&p);
    }

    #[test]
    fn empty_render_dims_is_err() {
        let img = png_rgb(4, 6, [0, 0, 0]);
        let (c, p) = open_zip("zero", &[("001.png", &img, false)]);
        assert!(c.render(0, 0, 10).is_err());
        assert!(c.render(0, 10, 0).is_err());
        close(&p);
    }
}
