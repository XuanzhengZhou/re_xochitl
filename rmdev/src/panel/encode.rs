//! 波形编码：把「目标灰度 + 上一帧灰度」变成一帧 32-bit cell 码。
//!
//! 面板一行 260 个 cell，每个 cell 占 4 字节：
//!
//! ```text
//! bit  15..0  8 个面板行 × 2-bit 驱动码（bit (7-K)*2 对应 lane K）
//! bit  23..16 命令字节（由固定模板给出，运行时只改低 16 位）
//! bit  31..24 未用
//! ```
//!
//! 驱动码：0 = 不驱动，1 = 推黑，2 = 推白。
//!
//! 编码顺序有三处反直觉，都是真机试出来的，别"顺手改对"：
//!
//! * 面板列 `pc` 对应**用户 x = COLS-1-pc**（左右镜像）；
//! * cell `s` 覆盖用户 y ∈ `[UH-8-s*8, UH-1-s*8]`（从下往上）；
//! * 一个 cell 里 `b[0]`（最小 y）放 bit 0，`b[7]` 放 bit 14。

use super::{CELLS, COLS, CSTR, NCELL, ROWS, SLOT, UH};

/// 建一块模板槽（RAM 里），命令字节固定，运行时只改内容 cell 的低 16 位。
///
/// 前 4 行是"命令行"（0 行和 1/2/3 行模板不同），第 4 行起才是内容列。
pub fn slab_new() -> Vec<u32> {
    let mut s = vec![0u32; SLOT / 4];

    // 第 0 行：0x43 基址，20..143 置位 0x00040000，40..103 清 0x00020000。
    for i in 0..CELLS {
        s[i] = 0x0043_0000;
    }
    for i in 20..143 {
        s[i] |= 0x0004_0000;
    }
    for i in 40..103 {
        s[i] &= 0xFFFD_FFFF;
    }

    // 第 1/2 行：0x41 基址，8..19 置 0x00200000，55..255 置 0x00020000。
    let mut line = vec![0x0041_0000u32; CELLS];
    for i in 8..19 {
        line[i] |= 0x0020_0000;
    }
    for i in 55..255 {
        line[i] |= 0x0002_0000;
    }
    let mut content = line.clone();
    // 内容行（第 3 行起，含第 3 行）：26..260 再置 0x00100000。
    for i in NCELL..CELLS {
        content[i] |= 0x0010_0000;
    }
    s[CELLS..2 * CELLS].copy_from_slice(&line);
    s[2 * CELLS..3 * CELLS].copy_from_slice(&line);
    for r in 3..ROWS {
        s[r * CELLS..(r + 1) * CELLS].copy_from_slice(&content);
    }
    s
}

/// 内容 cell 全部清零 = 一帧"无驱动"。每次更新结束必须来一帧。
pub fn zero_content(slab: &mut [u32]) {
    for pc in 0..COLS {
        let row = 4 + pc;
        let base = row * CELLS + NCELL;
        for v in &mut slab[base..base + CSTR] {
            *v &= 0xFFFF_0000;
        }
    }
}

/// 把一列（234 个 cell 码）写进模板槽。
pub fn put_col(slab: &mut [u32], pc: usize, one: &[u16]) {
    let base = (4 + pc) * CELLS + NCELL;
    let src = &one[pc * CSTR..(pc + 1) * CSTR];
    let dst = &mut slab[base..base + CSTR];
    // 命令字节是固定的，直接整字覆盖，省掉一次读：
    //   cell 26..54  → 0x0051_0000（0x41 基址 | 内容位）
    //   cell 55..254 → 0x0053_0000（再加 0x0002_0000）
    //   cell 255..259→ 0x0051_0000
    let a = 55 - NCELL; // 55 号 cell 在 CSTR 里的下标
    let b = 255 - NCELL;
    for (k, (&v, d)) in src.iter().zip(dst.iter_mut()).enumerate() {
        let hi = if k >= a && k < b { 0x0053_0000 } else { 0x0051_0000 };
        *d = hi | v as u32;
    }
}

/// 只写脏列（局部刷新用；其余列在上一帧已经被 `zero_content` 清过）。
pub fn put_cols(slab: &mut [u32], one: &[u16], pclo: usize, pchi: usize) {
    for pc in pclo..=pchi {
        put_col(slab, pc, one);
    }
}

/// 只写「脏列 × 脏 cell 区间」。
///
/// 一个内容 cell 覆盖 8 个用户行，所以脏行也可以折成 cell 区间。以前这里
/// 每帧都写满整列 234 个 cell：一条横贯整屏但只有半屏高的矩形（翻页时的
/// 工具条、`panelrec` 里的宽笔迹）明明只脏 68 个 cell，却要写 234 个 ——
/// 3.4 倍的 DRAM 流量，帧周期直接被顶过 11.93 ms。
pub fn put_cells(
    slab: &mut [u32],
    one: &[u16],
    pclo: usize,
    pchi: usize,
    slo: usize,
    shi: usize,
) {
    let a = 55 - NCELL; // 55 号 cell 在 CSTR 里的下标
    let b = 255 - NCELL;
    let shi = shi.min(CSTR - 1);
    let n = shi + 1 - slo;
    for pc in pclo..=pchi {
        let base = (4 + pc) * CELLS + NCELL + slo;
        let src = &one[pc * CSTR + slo..pc * CSTR + slo + n];
        for (k, (&v, d)) in src.iter().zip(&mut slab[base..base + n]).enumerate() {
            let i = slo + k;
            let hi = if i >= a && i < b { 0x0053_0000 } else { 0x0051_0000 };
            *d = hi | v as u32;
        }
    }
}

/// `kp[i] = (目标灰度 << 4) | 上一帧灰度`，只算「脏列 × 脏行」。
///
/// 布局是 x-major：`kp[x*UH + y]`，和 `img`/`prv` 一致。
///
/// **行范围一定要传**：以前这里对每列都扫满整列 1872 行，而笔迹往往只有
/// 几十像素高。一次 `prepare(639×607)` 就是这样花掉 9 ms，帧周期被顶到
/// 37.8 ms —— 波形被拉长 3 倍，手感是"快写完全跟不上"。
#[allow(clippy::too_many_arguments)]
pub fn build_kp_cols(
    img: &[u8],
    prv: &[u8],
    kp: &mut [u8],
    pclo: usize,
    pchi: usize,
    ylo: usize,
    yhi: usize,
) {
    let yhi = yhi.min(UH);
    if ylo >= yhi {
        return;
    }
    for pc in pclo..=pchi {
        let x = COLS - 1 - pc;
        let base = x * UH + ylo;
        let n = yhi - ylo;
        for k in 0..n {
            let i = base + k;
            kp[i] = ((img[i] & 0xF) << 4) | (prv[i] & 0xF);
        }
    }
}

/// 从 256 字节的 LUT 表体里给 16×16 个 (目标,上一帧) 组合取 2-bit 码。
///
/// `idx = (cur*32 + prev)`，`cur = 2*gray`、`prev = 2*gray`（5-bit 空间），
/// 即 `idx = (gc*2)<<5 | gp*2`。
pub fn prep(src: &[u8], t: &mut [u8; 256]) {
    for gc in 0..16usize {
        for gp in 0..16usize {
            let idx = ((gc * 2) << 5) | (gp * 2);
            let byte = src.get(idx >> 2).copied().unwrap_or(0);
            t[(gc << 4) | gp] = (byte >> ((idx & 3) * 2)) & 3;
        }
    }
}

/// 编码面板列 `[pclo, pchi]`，结果写进 `out`（按列索引整块的 `u16`）。
pub fn encode_cols(t: &[u8; 256], kp: &[u8], out: &mut [u16], pclo: usize, pchi: usize) -> bool {
    encode_cells_range(t, kp, out, pclo, pchi, 0, CSTR - 1)
}

/// 2-bit 码 × 8（整块同码时的 cell 字）：0 → 0，1 → 0x5555，2 → 0xAAAA。
const DUP: [u16; 4] = [0x0000, 0x5555, 0xAAAA, 0xFFFF];

/// 从用户行区间 `[y, y+h)` 折出内容 cell 区间 `[slo, shi]`。
///
/// cell `s` 覆盖用户行 `UH-8-s*8 ..= UH-1-s*8`（从下往上），所以：
/// 顶行 `y` 落在 cell `(UH-1-y)/8`，底行 `y+h-1` 落在 `(UH-y-h)/8`。
pub fn cell_window(y: usize, h: usize) -> (usize, usize) {
    let yhi = (y + h).min(UH);
    if y >= yhi {
        return (0, 0);
    }
    let shi = ((UH - 1 - y) / 8).min(CSTR - 1);
    let slo = ((UH - yhi) / 8).min(CSTR - 1);
    (slo.min(shi), shi)
}

/// 同 `encode_cols`，但编「列 `[pclo, pchi]` × cell `[slo, shi]`」这一块。
///
/// 输出按 `out[pc * CSTR + s]` 寻址（和 `put_cols`/`put_cells` 一致），
/// 区间外的位置不碰 —— 调用方负责先清零。
///
/// 返回"这一相位有没有真的驱动"：GL16 前 21 个相位对任何像素都是 0，
/// 整相位为空就可以剪掉（波形是先亮后驱，剪掉不改变物理效果）。
///
/// 两个性能点都在这里：
///
/// 1. **同键连跑**：纯白/纯黑区一整列 234 个 cell 的键完全一样，以前每个
///    cell 都要查一次表再写一个字。现在遇到同一个 8 字节键就一路向后数，
///    然后 `fill` 一整段（编译成 memset）。翻页那种整屏更新里 60%+ 的
///    cell 落在这种区里，编码直接掉一个数量级。
/// 2. **整块同码快路径**：8 个 lane 的键一样时一次查表，`DUP` 给出重复码。
pub fn encode_cells_range(
    t: &[u8; 256],
    kp: &[u8],
    out: &mut [u16],
    pclo: usize,
    pchi: usize,
    slo: usize,
    shi: usize,
) -> bool {
    let mut any = false;
    let shi = shi.min(CSTR - 1);
    if slo > shi {
        return false;
    }
    for pc in pclo..=pchi {
        let x = COLS - 1 - pc;
        let col = &kp[x * UH..(x + 1) * UH];
        let base = pc * CSTR;
        let mut s = slo;
        while s <= shi {
            // 一个 cell 覆盖 8 个面板行：`b[0]` 是最小 y，落 bit 0。
            //
            // 这里刻意写成「先切出 8 字节定长切片，再用常量下标」：
            // 动态下标会让 LLVM 每条都插边界检查（整屏一相位要多花 5 ms）。
            let lo = UH - 8 - s * 8;
            let Some(b) = col.get(lo..lo + 8) else { break };
            let w = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
            let v = if w == (w & 0xFF).wrapping_mul(0x0101_0101_0101_0101) {
                DUP[t[(w & 0xFF) as usize] as usize]
            } else {
                (t[b[0] as usize] as u16)
                    | ((t[b[1] as usize] as u16) << 2)
                    | ((t[b[2] as usize] as u16) << 4)
                    | ((t[b[3] as usize] as u16) << 6)
                    | ((t[b[4] as usize] as u16) << 8)
                    | ((t[b[5] as usize] as u16) << 10)
                    | ((t[b[6] as usize] as u16) << 12)
                    | ((t[b[7] as usize] as u16) << 14)
            };
            any |= v != 0;
            // 同键连跑：键相同就一定编出同一个码，可以一次填满。
            let mut e = s + 1;
            while e <= shi {
                let l2 = UH - 8 - e * 8;
                let Some(b2) = col.get(l2..l2 + 8) else { break };
                let w2 = u64::from_le_bytes([b2[0], b2[1], b2[2], b2[3], b2[4], b2[5], b2[6], b2[7]]);
                if w2 != w {
                    break;
                }
                e += 1;
            }
            out[base + s..base + e].fill(v);
            s = e;
        }
    }
    any
}

/// 同 [`encode_cells_range`]，但**相位按 cell 自己算**。
///
/// `lut` 是 `(nph+1) × 256` 的查表矩阵：前 `nph` 行是各相位的「键 → 2-bit 码」
/// 表，第 `nph` 行全 0（"已到位，不驱动"）。`phc[x*CSTR + s]` 记着这个 cell
/// 的相位 0 出现在哪一帧，于是相位 = `cur - phc`。
///
/// 为什么要这样：官方 swtcon 的每个 `WorkItem` 自带 `phase` / `frameCursor`
/// （`display.cpp:748` `new_phase = item->phase + frame_count`），新区域从相位 0
/// 开始，老区域继续数自己的相位，**互不重置**。我们以前只有一个全局相位，
/// 一来新墨就归零，于是连续输入时永远停在相位 0 —— 实测一次 DU 刷新跑了
/// 350 帧 / 4.1 s（本该 25 帧 / 300 ms）。
///
/// 改成按 cell 记相位后还顺带解决了两个老问题：
///
/// * 相位到 `nph` 自动变 0 码，不用再"等一整轮没有新墨"才提交（那个规则在
///   连续输入下永远等不到，屏上就是浓拖尾）；
/// * 每个像素都能连续吃满 `nph` 帧（`fb.rs` 的滑动窗口保证至少编 `nph` 帧），
///   不会被中途踢出驱动区。
/// 一帧里**每个 cell 各用各的波形表**（官方 swtcon 那个模型：每个 `WorkItem`
/// 自带 `phase` 和自己的波形，同一帧里互不干扰）。
///
/// `luts[mi] = (nph, 表)`：`mi` 是 `pmc` 里存的波形下标，`nph` 是该波形的相位
/// 数（DU 25 / A2 12 / GC16·GL16 46 / DU4 29），表是 `(nph+1) × 256`，多出来的
/// 那一行全 0 = "这一格吃完了，不驱动"。
///
/// 和 `encode_cells_range_aged` 的唯一区别就是每个 cell 多读一个 `pmc`：
/// 相位不能用"全局的一张表 + 全局 nph"来查，否则两种波形并行时短的那个会
/// 把长的截断（反过来长的会让短的多吃相位），正是"菜单只出来一半"那类毛病。
#[allow(clippy::too_many_arguments)]
pub fn encode_cells_range_multi(
    luts: &[(usize, Vec<u8>)],
    pmc: &[u8],
    kp: &[u8],
    phc: &[u32],
    cur: u32,
    out: &mut [u16],
    pclo: usize,
    pchi: usize,
    slo: usize,
    shi: usize,
) -> bool {
    let mut any = false;
    let shi = shi.min(CSTR - 1);
    if slo > shi || luts.is_empty() {
        return false;
    }
    let last = (luts.len() - 1) as u8;
    for pc in pclo..=pchi {
        let x = COLS - 1 - pc;
        let col = &kp[x * UH..(x + 1) * UH];
        let cb = x * CSTR;
        let base = pc * CSTR;
        let mut s = slo;
        // 波形表按"上一次用过的模式"缓存住：同一块矩形里 mode 只在区域边界
        // 变一次，逐 cell 去 `luts[m]` 两次索引（Vec<(usize, Vec<u8>)> 是
        // 指针 → 堆上再一层）纯属浪费 —— 整屏实测 9.7 ns/px，其中一大截是它。
        let mut cur_m = usize::MAX;
        let mut cur_nph = 0usize;
        let mut cur_lut: &[u8] = &[];
        while s <= shi {
            let m = (pmc[cb + s] & 0x7F).min(last) as usize;
            if m != cur_m {
                let (n, l) = &luts[m];
                cur_m = m;
                cur_nph = *n;
                cur_lut = &l[..];
            }
            let age = cur.wrapping_sub(phc[cb + s]) as usize;
            if age >= cur_nph {
                // 这一格吃完了：码是 0。连跑的格一起填，别做无用的 8 次查表。
                let mut e = s + 1;
                while e <= shi {
                    let mm = (pmc[cb + e] & 0x7F).min(last) as usize;
                    let n2 = if mm != cur_m { luts[mm].0 } else { cur_nph };
                    let age2 = cur.wrapping_sub(phc[cb + e]) as usize;
                    if age2 < n2 {
                        break;
                    }
                    e += 1;
                }
                out[base + s..base + e].fill(0);
                s = e;
                continue;
            }
            let ai = age;
            let row = &cur_lut[ai * 256..ai * 256 + 256];
            let lo = UH - 8 - s * 8;
            let Some(b) = col.get(lo..lo + 8) else { break };
            let w = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
            let v = if w == (w & 0xFF).wrapping_mul(0x0101_0101_0101_0101) {
                DUP[row[(w & 0xFF) as usize] as usize]
            } else {
                (row[b[0] as usize] as u16)
                    | ((row[b[1] as usize] as u16) << 2)
                    | ((row[b[2] as usize] as u16) << 4)
                    | ((row[b[3] as usize] as u16) << 6)
                    | ((row[b[4] as usize] as u16) << 8)
                    | ((row[b[5] as usize] as u16) << 10)
                    | ((row[b[6] as usize] as u16) << 12)
                    | ((row[b[7] as usize] as u16) << 14)
            };
            any |= v != 0;
            // 同键 + 同波形 + 同相位连跑：码一定一样，可以一次填满。
            let mut e = s + 1;
            while e <= shi {
                let mm = (pmc[cb + e] & 0x7F).min(last) as usize;
                if mm != cur_m {
                    break;
                }
                let l2 = UH - 8 - e * 8;
                let Some(b2) = col.get(l2..l2 + 8) else { break };
                let w2 = u64::from_le_bytes([b2[0], b2[1], b2[2], b2[3], b2[4], b2[5], b2[6], b2[7]]);
                if w2 != w {
                    break;
                }
                let age2 = cur.wrapping_sub(phc[cb + e]) as usize;
                if age2 != ai {
                    break;
                }
                e += 1;
            }
            out[base + s..base + e].fill(v);
            s = e;
        }
    }
    any
}

pub fn encode_cells_range_aged(
    lut: &[u8],
    nph: usize,
    kp: &[u8],
    phc: &[u32],
    cur: u32,
    out: &mut [u16],
    pclo: usize,
    pchi: usize,
    slo: usize,
    shi: usize,
) -> bool {
    let mut any = false;
    let shi = shi.min(CSTR - 1);
    if slo > shi || nph == 0 || lut.len() < (nph + 1) * 256 {
        return false;
    }
    for pc in pclo..=pchi {
        let x = COLS - 1 - pc;
        let col = &kp[x * UH..(x + 1) * UH];
        let cb = x * CSTR;
        let base = pc * CSTR;
        let mut s = slo;
        while s <= shi {
            let age = cur.wrapping_sub(phc[cb + s]) as usize;
            let ai = if age < nph { age } else { nph };
            let row = &lut[ai * 256..ai * 256 + 256];
            let lo = UH - 8 - s * 8;
            let Some(b) = col.get(lo..lo + 8) else { break };
            let w = u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]);
            let v = if w == (w & 0xFF).wrapping_mul(0x0101_0101_0101_0101) {
                DUP[row[(w & 0xFF) as usize] as usize]
            } else {
                (row[b[0] as usize] as u16)
                    | ((row[b[1] as usize] as u16) << 2)
                    | ((row[b[2] as usize] as u16) << 4)
                    | ((row[b[3] as usize] as u16) << 6)
                    | ((row[b[4] as usize] as u16) << 8)
                    | ((row[b[5] as usize] as u16) << 10)
                    | ((row[b[6] as usize] as u16) << 12)
                    | ((row[b[7] as usize] as u16) << 14)
            };
            any |= v != 0;
            // 同键 + 同相位连跑：码一定一样，可以一次填满。
            let mut e = s + 1;
            while e <= shi {
                let l2 = UH - 8 - e * 8;
                let Some(b2) = col.get(l2..l2 + 8) else { break };
                let w2 = u64::from_le_bytes([b2[0], b2[1], b2[2], b2[3], b2[4], b2[5], b2[6], b2[7]]);
                if w2 != w {
                    break;
                }
                let age2 = cur.wrapping_sub(phc[cb + e]) as usize;
                let ai2 = if age2 < nph { age2 } else { nph };
                if ai2 != ai {
                    break;
                }
                e += 1;
            }
            out[base + s..base + e].fill(v);
            s = e;
        }
    }
    any
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 按 cell 记相位：同一个键在不同相位必须查不同行，`nph` 行必须是 0。
    #[test]
    fn aged_lookup_follows_cell_phase() {
        // 造一个假波形表：相位 p 的码 = p % 4（只对键 0x0F 生效，其余 0）。
        let nph = 5usize;
        let mut lut = vec![0u8; (nph + 1) * 256];
        for p in 0..nph {
            lut[p * 256 + 0x0F] = (p % 4) as u8;
        }
        // 全屏都是键 0x0F（目标白、上一帧黑），cell 0 的 8 个 lane 同键。
        let mut kp = vec![0u8; UH * COLS];
        for v in kp.iter_mut() {
            *v = 0x0F;
        }
        let mut phc = vec![0u32; CSTR * COLS];
        let cur = 100u32;
        // 面板列 pc 对应 x = COLS-1-pc
        let xs = [COLS - 1, COLS - 2, COLS - 3, COLS - 4];
        phc[xs[0] * CSTR] = 100; // 相位 0
        phc[xs[1] * CSTR] = 97; // 相位 3
        phc[xs[2] * CSTR] = 95; // 超出 nph → 查第 nph 行（全 0）
        phc[xs[3] * CSTR] = 99; // 相位 1
        let mut out = vec![0u16; COLS * CSTR];
        encode_cells_range_aged(&lut, nph, &kp, &phc, cur, &mut out, 0, 3, 0, 0);
        assert_eq!(out[0 * CSTR], DUP[0], "x=1403 相位 0");
        assert_eq!(out[1 * CSTR], DUP[3], "x=1402 相位 3");
        assert_eq!(out[2 * CSTR], 0, "x=1401 已到位");
        assert_eq!(out[3 * CSTR], DUP[1], "x=1400 相位 1");
    }

    /// 屏上**每一个用户行**都必须落在一个被 `cell_window` 覆盖的 cell 里，
    /// 反过来每个被覆盖的 cell 也必须真有行落在请求区间内。
    ///
    /// 这条是"某个角写不上"的第一嫌疑：cell 是 8 行一格，顶边 y=0 落在
    /// s=233（每列最后一个 cell），边界算错一格就是"最上面 8 行永远不驱动"。
    #[test]
    fn cell_window_covers_exactly_the_requested_rows() {
        let ys = [0usize, 1, 7, 8, 9, 119, 120, 121, 935, 1864, 1867, 1871];
        let hs = [1usize, 2, 7, 8, 9, 47, 120, 649, 1056, 1872];
        for &y in &ys {
            for &h in &hs {
                if y + h > UH {
                    continue;
                }
                let (slo, shi) = cell_window(y, h);
                assert!(shi < CSTR, "y={y} h={h} shi={shi} 越界");
                assert!(slo <= shi, "y={y} h={h} slo={slo} shi={shi}");
                // 请求的每一行都被覆盖
                for r in y..y + h {
                    let s = (UH - 1 - r) / 8;
                    assert!(
                        (slo..=shi).contains(&s),
                        "y={y} h={h} 第 {r} 行落在 cell {s}，窗口是 {slo}..={shi}"
                    );
                }
                // 覆盖的每个 cell 都至少有一行在请求区间里
                for s in slo..=shi {
                    let top = UH - 8 - s * 8;
                    let bot = UH - 1 - s * 8;
                    assert!(
                        top < y + h && y <= bot,
                        "y={y} h={h} 窗口里的 cell {s}（行 {top}..={bot}）完全在区间外"
                    );
                }
            }
        }
    }

    /// 四个角（以及四条边）的矩形都必须映射到槽内合法、且属于**内容 cell**
    /// 的地址，一个都不能落到每行开头的命令字上。
    #[test]
    fn corner_rects_land_on_content_cells() {
        // 左上/右上页角、左下/右下页角，加上四条贴边的窄条。
        let rects = [
            (120usize, 0usize, 80usize, 60usize),
            (1300, 0, 104, 60),
            (120, 1812, 80, 60),
            (1300, 1812, 104, 60),
            (0, 0, 1404, 1872),
            (0, 0, 4, 1872),
            (1400, 0, 4, 1872),
            (0, 0, 1404, 8),
            (0, 1864, 1404, 8),
        ];
        let slot_words = SLOT / 4;
        for &(x, y, w, h) in &rects {
            let pclo = COLS - (x + w);
            let pchi = COLS - 1 - x;
            assert!(pclo <= pchi, "x={x} w={w} 列区间反了");
            assert!(pchi < COLS, "x={x} w={w} pchi={pchi} 越界");
            let (slo, shi) = cell_window(y, h);
            assert!(shi < CSTR && slo <= shi, "y={y} h={h} cell 区间 {slo}..={shi}");
            for pc in pclo..=pchi {
                let g = 4 + pc;
                assert!(g < ROWS, "pc={pc} 分组 {g} 超出 ROWS={ROWS}");
                for s in slo..=shi {
                    let idx = g * CELLS + NCELL + s;
                    assert!(idx < slot_words, "pc={pc} s={s} 地址 {idx} 越界");
                    // 必须还落在本分组内，不能溢到下一列
                    assert_eq!(idx / CELLS, g, "pc={pc} s={s} 溢出了分组");
                    // 内容 cell 的命令字只有 0x51/0x53 两种
                    let a = 55 - NCELL;
                    let b = 255 - NCELL;
                    let want = if (a..b).contains(&s) { 0x0053_0000 } else { 0x0051_0000 };
                    let slab = slab_new();
                    assert_eq!(slab[idx] & 0xFFFF_0000, want, "pc={pc} s={s} 命令字不对");
                }
            }
        }
    }

    /// 同一帧里每个 cell 用自己的波形表 + 自己的相位。
    ///
    /// 这是"并行推"的全部正确性依据：一张表 + 一个全局 nph 的写法下，短的
    /// 波形（DU 25）会把长的（GC16 46）截断 —— 那正是"菜单只出来一半"
    /// 和"擦除只擦一半"的来源。
    #[test]
    fn multi_mode_uses_each_cells_own_table() {
        // 表 0：5 相位，码 1；表 1：3 相位，码 2（都只对键 0x0F 非零）。
        let mut l0 = vec![0u8; 6 * 256];
        for p in 0..5 {
            l0[p * 256 + 0x0F] = 1;
        }
        let mut l1 = vec![0u8; 4 * 256];
        for p in 0..3 {
            l1[p * 256 + 0x0F] = 2;
        }
        let luts = vec![(5usize, l0), (3usize, l1)];
        let kp = vec![0x0Fu8; UH * COLS];
        let mut phc = vec![0u32; CSTR * COLS];
        let mut pmc = vec![0u8; CSTR * COLS];
        let cur = 100u32;
        let x = COLS - 1; // pc = 0
        phc[x * CSTR] = 100; // 表 0，相位 0
        pmc[x * CSTR] = 0;
        phc[x * CSTR + 1] = 98; // 表 1，相位 2（最后一相）
        pmc[x * CSTR + 1] = 1;
        phc[x * CSTR + 2] = 95; // 表 1，相位 5 > nph=3 → 查第 3 行（全 0）
        pmc[x * CSTR + 2] = 1;
        phc[x * CSTR + 3] = 70; // 表 0，相位 30 > nph=5 → 全 0
        pmc[x * CSTR + 3] = 0;
        let mut out = vec![0u16; CSTR * COLS];
        let any = encode_cells_range_multi(
            &luts, &pmc, &kp, &phc, cur, &mut out, 0, 0, 0, 3,
        );
        assert!(any);
        assert_eq!(out[0], DUP[1], "表 0 相位 0");
        assert_eq!(out[1], DUP[2], "表 1 相位 2");
        assert_eq!(out[2], 0, "表 1 越界 → 不驱动");
        assert_eq!(out[3], 0, "表 0 越界 → 不驱动");
    }

    /// DU（25 相位）和 GC16（46 相位）在同一帧里并行：DU 的 cell 走到第 30
    /// 帧必须停（码 0），GC16 的 cell 到第 30 帧还得继续驱。
    #[test]
    fn short_waveform_does_not_truncate_long_one() {
        let nph_du = 25usize;
        let nph_gc = 46usize;
        let mut a = vec![0u8; (nph_du + 1) * 256];
        for p in 0..nph_du {
            a[p * 256 + 0x0F] = 1;
        }
        let mut b = vec![0u8; (nph_gc + 1) * 256];
        for p in 0..nph_gc {
            b[p * 256 + 0x0F] = 2;
        }
        let luts = vec![(nph_du, a), (nph_gc, b)];
        let kp = vec![0x0Fu8; UH * COLS];
        let mut phc = vec![0u32; CSTR * COLS];
        let mut pmc = vec![0u8; CSTR * COLS];
        let x = COLS - 1;
        let cur = 200u32;
        // 两个 cell 都在第 170 帧开始，现在第 200 帧 = 相位 30。
        phc[x * CSTR] = 170;
        pmc[x * CSTR] = 0; // DU
        phc[x * CSTR + 1] = 170;
        pmc[x * CSTR + 1] = 1; // GC16
        let mut out = vec![0u16; CSTR * COLS];
        encode_cells_range_multi(&luts, &pmc, &kp, &phc, cur, &mut out, 0, 0, 0, 1);
        assert_eq!(out[0], 0, "DU 25 相位，第 30 帧必须停了");
        assert_eq!(out[1], DUP[2], "GC16 46 相位，第 30 帧还得驱");
    }

    #[test]
    fn slab_size_and_content_bit() {
        let s = slab_new();
        assert_eq!(s.len(), SLOT / 4);
        // 第 0 行命令字
        assert_eq!(s[0], 0x0043_0000);
        assert_eq!(s[20], 0x0043_0000 | 0x0004_0000);
        assert_eq!(s[40], (0x0043_0000 | 0x0004_0000) & 0xFFFD_FFFF);
        // 内容行有 0x00100000，且低 16 位是空的
        assert_eq!(s[3 * CELLS + NCELL] & 0x0010_0000, 0x0010_0000);
        assert_eq!(s[3 * CELLS + NCELL] & 0xFFFF, 0);
        assert_eq!(s[(4 + 10) * CELLS + NCELL] & 0x0010_0000, 0x0010_0000);
        // 命令 cell 没有内容位
        assert_eq!(s[(4 + 10) * CELLS + 25] & 0x0010_0000, 0);
    }

    /// 位序：cell s 覆盖 y = UH-8-s*8 ..= UH-1-s*8，b[0] 是 bit0。
    #[test]
    fn bit_order() {
        let mut t = [0u8; 256];
        t[0x01] = 1; // 目标黑 / 上一帧白
        t[0x02] = 2;
        t[0x00] = 0;
        let mut kp = vec![0u8; COLS * UH];
        let pc = 7;
        let x = COLS - 1 - pc;
        // s=0 的两端
        kp[x * UH + (UH - 1)] = 0x01; // b[7] -> bit 14
        kp[x * UH + (UH - 8)] = 0x02; // b[0] -> bit 0
        let mut out = vec![0u16; COLS * CSTR];
        encode_cols(&t, &kp, &mut out, pc, pc);
        // 输出按面板列**绝对寻址**：`out[pc*CSTR + s]`（和 `put_cells` 一致）。
        // b[7]=0x01 -> 码 1 落在 bit14；b[0]=0x02 -> 码 2 落在 bit0（值 2）。
        let o = pc * CSTR;
        assert_eq!(out[o], (1 << 14) | 2);
        assert_eq!(out[o + 1], 0);
    }

    /// 整块同码的快路径（纯色区）必须和逐 lane 走的结果一模一样。
    #[test]
    fn uniform_cell_fast_path() {
        let mut kp = vec![0u8; COLS * UH];
        let pc = 3;
        let x = COLS - 1 - pc;
        for key in [0x00u8, 0x0F, 0xFF, 0x5A] {
            for y in 0..UH {
                kp[x * UH + y] = key;
            }
            for code in 0..3u8 {
                let mut t = [0u8; 256];
                t[key as usize] = code;
                let mut out = vec![0u16; COLS * CSTR];
                encode_cols(&t, &kp, &mut out, pc, pc);
                let expect = match code {
                    0 => 0,
                    1 => 0x5555,
                    _ => 0xAAAA,
                };
                let o = pc * CSTR;
                assert!(
                    out[o..o + CSTR].iter().all(|&v| v == expect),
                    "key={key:#04X} code={code}: {:04X} != {:04X}",
                    out[o],
                    expect
                );
            }
        }
    }

    #[test]
    fn prep_indexing() {        // idx = (gc*2)<<5 | gp*2；byte = idx>>2，shift = (idx&3)*2
        let mut src = [0u8; 256];
        src[((15 * 2) << 5) >> 2] = 0b0000_0011; // gc=15,gp=0 在 bit0/1
        let mut t = [0u8; 256];
        prep(&src, &mut t);
        assert_eq!(t[(15 << 4) | 0], 3);
        assert_eq!(t[(0 << 4) | 0], 0);
    }

    #[test]
    fn zero_and_put_roundtrip() {
        let mut s = slab_new();
        let mut one = vec![0u16; COLS * CSTR];
        one[3 * CSTR] = 0x5555;
        put_col(&mut s, 3, &one);
        assert_eq!(s[(4 + 3) * CELLS + NCELL] & 0xFFFF, 0x5555);
        zero_content(&mut s);
        assert_eq!(s[(4 + 3) * CELLS + NCELL] & 0xFFFF, 0);
    }
}
