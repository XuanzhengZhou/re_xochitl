//! `.wbf` 波形文件解析（端口自 `epd.c`，已真机验证过的那份）。
//!
//! 文件是 E Ink 标准波形容器，本机是
//! `320_R355_AF0A11_ED103TC2U2_VB3300-KCD_TC.wbf`。
//!
//! 布局（偏移都是文件绝对偏移）：
//!
//! ```text
//! 0x00 u32 crc32 | 0x04 u32 file_length | 0x0E u16 fpl_lot | 0x10 mode_version
//! 0x11 wf_version | 0x12 wf_subversion | 0x17 frame_rate | 0x1C u24 xwia
//! 0x20 u24 wmta   | 0x24 luts  | 0x25 mc  | 0x26 trc
//! 0x30 u8 bounds[trc+2]        ← 温度边界 + 1 字节校验
//! xwia: u8 长度 + 字符串 + 1 字节校验
//! wmta: 模式表 {u24 地址} × (mc+1)，每个地址再指向 trc+1 个 u24（每温度档一个）
//! 表体：RLE 压缩的 32×32 LUT，每相位 256 B
//! ```
//!
//! RLE 规则（和 `epd.c` 的 `decode_table` 一致）：
//! 读一个字节 `c`；`0xFC` 翻转 RLE 开关；`0xFF` 结束（下一字节是累加校验和）；
//! 开关开时紧跟一个长度字节 `len`，输出 `len+1` 份 `c`；开关关时原样输出 `c`。

use std::fs;
use std::path::Path;

/// 一个相位展开后 256 字节（32×32 表，每字节 4 个 2-bit 码）。
pub const BYTES_PER_PHASE: usize = 256;

/// 模式名表：`mode_version` → 该文件里模式下标的含义。
///
/// 同一份波形文件里，下标 3/4/5 可能都叫 GL16（不同温度档/不同闪光策略），
/// 所以按名字找模式时必须能取"第 nth 个同名模式"。
pub fn mode_name(mv: u8, i: usize) -> Option<&'static str> {
    let t: &'static [&'static str] = match mv {
        0x01 => &["INIT", "DU", "GL8", "GC8"],
        0x03 => &["INIT", "DU", "GL16", "GL16", "A2"],
        0x09 => &["INIT", "DU", "GC16", "A2"],
        0x12 => &["INIT", "DU", "NULL", "GC16", "A2", "GL16", "GL16", "DU4"],
        0x15 => &["INIT", "DU", "GC16", "GL16", "A2", "DU4", "GC4"],
        0x16 => &["INIT", "DU", "GC16", "GL16", "GL16", "GC16", "A2"],
        0x18 => &["INIT", "DU", "GC16", "GL16", "GL16", "GL16", "A2"],
        0x19 => &["INIT", "DU", "GC16", "GL16", "GL16", "GL16", "A2", "DU4"],
        0x20 => &["INIT", "DU", "GC16", "GL16", "GL16", "GL16", "A2"],
        0x23 => &["INIT", "DU", "GC16", "GL16", "A2", "DU4"],
        0x43 => &["INIT", "DU", "GC16", "GL16", "GL16", "GL16", "A2", "DU4"],
        0x54 => &["INIT", "DU", "GC16", "GL16", "GL16", "A2"],
        0x48 => &["INIT", "DU", "GC16", "GL16", "GL16", "NULL", "A2"],
        0x83 => &["INIT", "DU", "GC16", "GL16", "A2", "GL16", "GL16"],
        _ => return None,
    };
    t.get(i).copied()
}

fn u16_at(b: &[u8], o: usize) -> u32 {
    u16::from_le_bytes([b[o], b[o + 1]]) as u32
}
fn u24_at(b: &[u8], o: usize) -> u32 {
    b[o] as u32 | (b[o + 1] as u32) << 8 | (b[o + 2] as u32) << 16
}

/// 从 xwi 字符串里抠出 TFT_VID。
///
/// 形如 `320_R355_AF0A11_ED103TC2U2_VB3300-KCD_TC`：偏好 `AF.....` 后面那一段
/// （本机就是 `ED103TC2U2`），退一步找以 `ED` 开头的段。
pub fn parse_tft_vid(xwi: &str) -> String {
    let f: Vec<&str> = xwi.split('_').collect();
    for i in 0..f.len() {
        if f[i].len() >= 2 && f[i].starts_with("AF") && i + 1 < f.len() {
            return f[i + 1].to_string();
        }
    }
    for t in &f {
        if t.len() >= 2 && t.starts_with("ED") {
            return t.to_string();
        }
    }
    String::new()
}

/// 打开并解析好的波形文件。
pub struct Waveform {
    pub path: String,
    data: Vec<u8>,
    pub fpl_lot: u32,
    pub tft_vid: String,
    pub xwi: String,
    pub mode_version: u8,
    pub wf_version: u8,
    pub wf_subversion: u8,
    pub frame_rate: u8,
    pub luts: u8,
    pub nmode: usize,
    pub nband: usize,
    /// `bounds[nband]`：温度边界，`bounds[j]` 是第 j 档的下界。
    pub bounds: Vec<u8>,
    mode_table_off: usize,
}

impl Waveform {
    pub fn open(path: &Path) -> Result<Waveform, String> {
        let data = fs::read(path).map_err(|e| format!("读 {}: {e}", path.display()))?;
        if data.len() < 64 || data.len() > (32 << 20) {
            return Err(format!("{} 大小不对：{} 字节", path.display(), data.len()));
        }
        let b = &data;
        let fpl_lot = u16_at(b, 0x0E);
        let mode_version = b[0x10];
        let luts = b[0x24];
        let mc = b[0x25] as usize;
        let trc = b[0x26] as usize;
        let nmode = mc + 1;
        let nband = trc + 1;
        if nmode > 64 || nband > 64 {
            return Err(format!("模式/温度档太多：{nmode} × {nband}"));
        }
        if luts != 0x04 {
            return Err(format!("只支持 5-bit 双向 LUT（luts=0x{luts:02X}）"));
        }
        if 48 + nband + 2 > b.len() {
            return Err("温度边界表越界".into());
        }
        let bounds = b[48..48 + nband + 1].to_vec();

        let xwia = u24_at(b, 0x1C) as usize;
        let wmta = u24_at(b, 0x20) as usize;
        let mut xwi = String::new();
        if xwia != 0 && xwia < b.len() {
            let ln = b[xwia] as usize;
            if xwia + 1 + ln <= b.len() {
                xwi = String::from_utf8_lossy(&b[xwia + 1..xwia + 1 + ln]).into_owned();
            }
        }
        let tft_vid = parse_tft_vid(&xwi);

        // 模式表：优先用头里的 wmta，坏掉就按布局算。
        let mut computed = 48 + nband + 2;
        if !xwi.is_empty() {
            computed += 1 + xwi.len() + 1;
        }
        let mut off = if wmta != 0 { wmta } else { computed };
        if !(off > 0 && off + 4 * nmode <= b.len()) {
            off = computed;
        }
        if off + 4 * nmode > b.len() {
            return Err("模式表越界".into());
        }

        Ok(Waveform {
            path: path.display().to_string(),
            fpl_lot,
            tft_vid,
            xwi,
            mode_version,
            wf_version: b[0x11],
            wf_subversion: b[0x12],
            frame_rate: b[0x17],
            luts,
            nmode,
            nband,
            bounds,
            mode_table_off: off,
            data,
        })
    }

    /// 按名字找第 `nth` 个同名模式的下标。
    pub fn mode_index(&self, want: &str, nth: usize) -> Option<usize> {
        let mut left = nth;
        for i in 0..self.nmode {
            if mode_name(self.mode_version, i) == Some(want) {
                if left == 0 {
                    return Some(i);
                }
                left -= 1;
            }
        }
        None
    }

    pub fn mode_label(&self, i: usize) -> &'static str {
        mode_name(self.mode_version, i).unwrap_or("?")
    }

    /// 取某 (模式, 温度档) 的波形表地址。
    pub fn table_addr(&self, mode: usize, band: usize) -> Option<usize> {
        let b = &self.data;
        let off = self.mode_table_off + 4 * mode;
        if off + 4 > b.len() {
            return None;
        }
        let base = u24_at(b, off) as usize;
        if base >= b.len() {
            return None;
        }
        let off = base + 4 * band;
        if off + 4 > b.len() {
            return None;
        }
        let addr = u24_at(b, off) as usize;
        if addr >= b.len() {
            None
        } else {
            Some(addr)
        }
    }

    /// 展开一张表，返回 `nph × 256` 字节。
    pub fn decode_table(&self, addr: usize) -> Result<Vec<u8>, String> {
        let b = &self.data;
        let n = b.len();
        let mut p = addr;
        let mut out = Vec::with_capacity(BYTES_PER_PHASE * 64);
        let mut rle = true;
        let mut cs = 0u32;
        loop {
            if p >= n {
                return Err("表体越界（没有 0xFF 结束符）".into());
            }
            let c = b[p];
            p += 1;
            cs = (cs + c as u32) & 0xFF;
            match c {
                0xFC => rle = !rle,
                0xFF => return Ok(out),
                _ if !rle => out.push(c),
                _ => {
                    if p >= n {
                        return Err("RLE 长度字节缺失".into());
                    }
                    let ln = b[p] as usize;
                    p += 1;
                    cs = (cs + ln as u32) & 0xFF;
                    out.resize(out.len() + ln + 1, c);
                }
            }
        }
    }

    /// 当前温度对应哪一档。
    pub fn band_for(&self, temp_c: i32) -> usize {
        let mut band = 0;
        for j in 0..self.nband {
            if temp_c >= self.bounds[j] as i32 {
                band = j;
            }
        }
        band
    }
}

// ------------------------------------------------------------------ 选文件

/// 本机已由官方 xochitl 日志证实的面板指纹。
pub const DEFAULT_FPL_LOT: u32 = 0x163; // 355 -> R355
pub const DEFAULT_TFT_VID: &str = "ED103TC2U2";
const CONF_FILE: &str = "/etc/rm2epd.conf";
const DIRS: [&str; 2] = ["/usr/share/remarkable", "/var/lib/uboot"];

fn read_conf() -> (Option<u32>, Option<String>) {
    let mut fpl = None;
    let mut tft = None;
    let Ok(txt) = fs::read_to_string(CONF_FILE) else {
        return (fpl, tft);
    };
    for line in txt.lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let (k, v) = (k.trim(), v.trim());
        if k == "fpl_lot" {
            let v = v.trim_start_matches("0x").trim_start_matches("0X");
            fpl = u32::from_str_radix(v, 16)
                .ok()
                .or_else(|| v.parse::<u32>().ok());
        } else if k == "tft_vid" && !v.is_empty() {
            tft = Some(v.to_string());
        }
    }
    (fpl, tft)
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub path: String,
    pub fpl_lot: u32,
    pub tft_vid: String,
    pub mode_version: u8,
    pub nmode: usize,
    pub nband: usize,
}

/// 扫一遍所有 .wbf，返回能解析的。
///
/// 解析失败的文件直接跳过——目录里可能有半截/损坏的文件，不该因此起不来。
pub fn scan() -> Vec<Candidate> {
    let mut out = Vec::new();
    for dir in DIRS {
        let Ok(rd) = fs::read_dir(dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            let is_wbf = p
                .extension()
                .map(|x| x.to_string_lossy().eq_ignore_ascii_case("wbf"))
                .unwrap_or(false);
            if !is_wbf {
                continue;
            }
            if let Ok(w) = Waveform::open(&p) {
                out.push(Candidate {
                    path: p.display().to_string(),
                    fpl_lot: w.fpl_lot,
                    tft_vid: w.tft_vid.clone(),
                    mode_version: w.mode_version,
                    nmode: w.nmode,
                    nband: w.nband,
                });
            }
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// 选一个波形文件。
///
/// 优先级：`RM2EPD_WBF` 显式路径 → 指纹精确匹配（fpl_lot + tft_vid）
/// → 只 fpl_lot → 只 tft_vid → 第一个（并告警）。
///
/// 返回值里带上"为什么选它"，方便日志里一眼看出是不是回退了。
pub fn pick() -> Result<(Candidate, &'static str), String> {
    if let Ok(p) = std::env::var("RM2EPD_WBF") {
        if !p.is_empty() {
            let w = Waveform::open(Path::new(&p))?;
            return Ok((
                Candidate {
                    path: p,
                    fpl_lot: w.fpl_lot,
                    tft_vid: w.tft_vid.clone(),
                    mode_version: w.mode_version,
                    nmode: w.nmode,
                    nband: w.nband,
                },
                "环境变量 RM2EPD_WBF",
            ));
        }
    }
    let (cf, ct) = read_conf();
    let ef = std::env::var("RM2EPD_FPL_LOT")
        .ok()
        .and_then(|v| {
            let v = v.trim().trim_start_matches("0x").to_string();
            u32::from_str_radix(&v, 16).ok().or_else(|| v.parse().ok())
        })
        .or(cf)
        .unwrap_or(DEFAULT_FPL_LOT);
    let et = std::env::var("RM2EPD_TFT_VID")
        .ok()
        .filter(|v| !v.is_empty())
        .or(ct)
        .unwrap_or_else(|| DEFAULT_TFT_VID.to_string());

    let all = scan();
    if all.is_empty() {
        return Err(format!("{:?} 里没有任何可解析的 .wbf", DIRS));
    }
    if let Some(c) = all.iter().find(|c| c.fpl_lot == ef && c.tft_vid == et) {
        return Ok((c.clone(), "精确匹配 FPL_LOT+TFT_VID"));
    }
    if let Some(c) = all.iter().find(|c| c.fpl_lot == ef) {
        return Ok((c.clone(), "回退：只匹配 FPL_LOT"));
    }
    if let Some(c) = all.iter().find(|c| c.tft_vid == et) {
        return Ok((c.clone(), "回退：只匹配 TFT_VID"));
    }
    Ok((all[0].clone(), "警告：无指纹匹配，任选一个"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vid_from_real_xwi() {
        assert_eq!(
            parse_tft_vid("320_R355_AF0A11_ED103TC2U2_VB3300-KCD_TC"),
            "ED103TC2U2"
        );
        assert_eq!(
            parse_tft_vid("320_R259_AFAB21_ED103TC2M1_TC"),
            "ED103TC2M1"
        );
        // 没有 AF 段时退到 ED 段
        assert_eq!(parse_tft_vid("320_R001_ED103TC2XX"), "ED103TC2XX");
        assert_eq!(parse_tft_vid(""), "");
    }

    #[test]
    fn names_by_version() {
        assert_eq!(mode_name(0x19, 0), Some("INIT"));
        assert_eq!(mode_name(0x19, 5), Some("GL16"));
        assert_eq!(mode_name(0x19, 7), Some("DU4"));
        assert_eq!(mode_name(0x19, 8), None);
        assert_eq!(mode_name(0x77, 0), None);
    }

    /// 用一段手工 RLE 校验解码器：RLE 默认是**开**的，
    /// `AA 03` = 4 个 0xAA，`FC` 关掉 RLE，之后 0x11 0x22 原样，`FF` 结束。
    #[test]
    fn rle_decode() {
        let mut data = vec![0u8; 64];
        let body: [u8; 6] = [0xAA, 0x03, 0xFC, 0x11, 0x22, 0xFF];
        data.extend_from_slice(&body);
        let w = Waveform {
            path: "mem".into(),
            fpl_lot: 0,
            tft_vid: String::new(),
            xwi: String::new(),
            mode_version: 0,
            wf_version: 0,
            wf_subversion: 0,
            frame_rate: 0,
            luts: 4,
            nmode: 1,
            nband: 1,
            bounds: vec![0, 0],
            mode_table_off: 0,
            data,
        };
        assert_eq!(w.decode_table(64).unwrap(), vec![0xAA, 0xAA, 0xAA, 0xAA, 0x11, 0x22]);
    }

    #[test]
    fn band_boundaries() {
        let w = Waveform {
            path: "mem".into(),
            fpl_lot: 0,
            tft_vid: String::new(),
            xwi: String::new(),
            mode_version: 0x19,
            wf_version: 0,
            wf_subversion: 0,
            frame_rate: 0,
            luts: 4,
            nmode: 8,
            nband: 14,
            bounds: vec![0, 3, 6, 9, 12, 15, 18, 21, 24, 27, 30, 33, 38, 43, 48],
            mode_table_off: 0,
            data: vec![0; 64],
        };
        assert_eq!(w.band_for(0), 0);
        assert_eq!(w.band_for(21), 7);
        assert_eq!(w.band_for(23), 7);
        assert_eq!(w.band_for(24), 8);
        assert_eq!(w.band_for(99), 13);
    }
}

#[cfg(test)]
mod dev_dump {
    //! 波形表实测工具：在本机直接解任意一张 `.wbf`，把 (目标, 起点) 全部
    //! 组合的逐相位驱动码打出来。设备上的 `paneldbg enc` 只能打固定几张组合，
    //! 而且要先占住 `/dev/fb0`；这个只要文件，不受限。
    //!
    //! ```text
    //! WBF=/path/to/xxx.wbf cargo test --lib dump_pairs -- --ignored --nocapture
    //! ```
    //!
    //! 2026-02 在 320_R355_AF0A11（22 ℃ → band 7）上实测到的关键几行（码 0 =
    //! 不驱动，1 = 推黑，2 = 推白，串长 = 相位数）：
    //!
    //! * `GC16 白←白` = 推黑 22 + 推白 20 → **GC16 会重绘"目标没变"的像素**，
    //!   这正是"整块重刷 GC16 能自愈残影/空洞"的依据（`fb.rs::repaints`）。
    //! * `GL16 白←白` = 全 0 → GL16 是"不变像素完全不驱"的那个变体。
    //! * `GC16 黑←黑` = 先推白 18 再推黑 → 自愈同样成立。
    //! * `DU 中灰←*`、`A2 中灰←*` = 全 0 → 二值波形驱不动中间灰。
    //!
    //!   **别在驱动里量化成 0/15**：那会把矩形里别人的羽化边/图标一起翻成
    //!   纯黑纯白（`fb.rs::refresh` 有一段注释记着这个坑）。上层要画灰阶
    //!   就得自己挑 GL16/GC16 —— `app` 的设置页就是拿 DU 画 `tone 20`
    //!   （= 灰阶 1）的字，结果整页只剩纯黑方块、文字全空。
    #[test]
    #[ignore]
    fn dump_pairs() {
        use super::*;
        let path = std::env::var("WBF").unwrap_or_else(|_| "/tmp/dev.wbf".into());
        let w = Waveform::open(std::path::Path::new(&path)).unwrap();
        let band = w.band_for(22);
        for mode in ["GC16", "GL16", "DU", "A2", "DU4"] {
            let Some(mi) = w.mode_index(mode, 0) else { continue };
            let Some(addr) = w.table_addr(mi, band) else { continue };
            let tab = w.decode_table(addr).unwrap();
            let nph = tab.len() / BYTES_PER_PHASE;
            println!("== {mode} band {band} {nph} 相位");
            for tgt in [0u8, 1, 2, 8, 15] {
                for prv in [0u8, 8, 15] {
                    let mut all = String::new();
                    for p in 0..nph {
                        let mut t = [0u8; 256];
                        crate::panel::encode::prep(&tab[p * BYTES_PER_PHASE..], &mut t);
                        let c = t[((tgt << 4) | prv) as usize];
                        all.push(char::from(b'0' + c));
                    }
                    println!("  目标 {tgt:2} ← {prv:2} : {all}");
                }
            }
        }
    }
}
