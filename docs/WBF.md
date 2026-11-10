# E Ink `.wbf` 波形文件格式分析（reMarkable 2）

样本：`/tmp/320_R259_AFAB21_ED103TC2M1_TC.wbf`（282 610 B，来自真机
`/usr/share/remarkable/`，共 42 个文件之一）。
解析器：[`reverse/python/wbf.py`](../reverse/python/wbf.py)。

---

## 0. 结论速览

| 项目 | 结果 | 置信度 |
|---|---|---|
| 头结构 | 48 B 定长 + 温区表 + xwi 块，字段/偏移全部确定 | **确定**（4 种校验和全部通过） |
| `mc` / `trc` | `mc = 7` → **8 个模式**；`trc = 13` → **14 个温区** | **确定** |
| 表组织 | 模式表(8×4B) → 每模式温区表(14×4B) → 波形表(RLE) | **确定**（112 个引用全部校验和通过） |
| 波形表编码 | `0xFC` 切换 RLE/原样，`0xFF` 结束，尾随 1 B 校验和 | **确定**（79 张表全部校验和通过） |
| 相位几何 | **32 src × 32 dst**，2 bit/格，4 格/字节 → **256 B/相位** | **确定**（相位计数与 zplash 阶梯逐值吻合） |
| **是否有前一帧状态维度** | **有**，LUT 是二维 `[prev][cur]` | **确定** |
| 有效灰阶 | 只有**偶数** dst 索引被使用；`dst_index = 2 × 灰阶(0..15)` | 高（DU4={0,10,20,30}、A2/DU={0,30}、GC16=全偶数 0..30 三重印证） |
| 模式名 | mode_version=0x19 → INIT, DU, GC16, GL16×3, A2, DU4 | 高（公开工具的模式表 + 面板型号 ES103TC1 对上） |
| 相位码语义 | 只出现 0/1/2 三种值（3 从未出现）；0 = 不驱动 | 中（0 的含义确定，1/2 的极性未定） |
| 字节内位序 LSB/MSB | **未确定**（两种都能自洽，证据见 §11） | 低 |

**自检（最强信号）——两条阶梯都命中：**

```
zplash phase_count : 131,116,102,90,77,66,55,46,38,38,38,38,38,38
wbf  mode 2 (GC16) : 131,116,102,90,77,66,55,46,38,38,38,38,38,38   ✔ 14/14 全等

zplash byte_count  : 149,133,117,103,89,159,135,111,99,87,79,71,63,59
wbf  mode 0 (INIT) : 149,133,117,103,89,159,135,111,99,87,79,71,63,59 ✔ 14/14 全等
```

两条**互相独立**的阶梯逐值相等，说明：头解析、模式表、温区表、RLE
解码、相位/字节几何——**全部正确**。

---

## 1. 文件总体布局

```
0x000  48 B   固定头（waveform_data_header）
0x030  16 B   温区表：14 个上界 + 1 个末界 + 1 B 校验和  (trc+1 界 + 末界 + cs)
0x040  31 B   xwi 块：1 B 长度(0x1D=29) + 29 B ASCII + 1 B 校验和
0x05F  32 B   模式表：8 × {u24 地址, u8 校验和}
0x07F 0x380   模式块：8 × 0x70 B，每块 = 14 × {u24,u8} 温区指针 + 14 × {u24,u8} 第二库
0x3FF  16 B   固定的 16 B 记录（见 §8）
0x40F  ...    波形表区：79 张不同的表，每张 [RLE 数据][0xFF][校验和][16 B 记录]
0x44AD1 ...   最后一模式(DU4)最后一张表
0x44E90 ...   第二指针库指向的描述符 + 8×8 抖动矩阵（1313 B，语义未定）
0x44FF2  = 文件尾 = 头里 file_length ✔
```

> 修正两个早期锚点（我之前搞错过，实测如下）：
> * xwi **长度字节在 `0x40`**，字符串 `0x41..0x5D`（29 B），校验和在 `0x5E`。
>   头里的 `xwia = 0x40` 指向长度字节，不是字符串。
> * `0x5E` 不是 u32 偏移表起点。真正的表在 **`0x5F`**（= 头里的 `wmta`），
>   每项是 **4 B = u24 地址 + u8 校验和**（校验和 = 前 3 字节之和 & 0xFF），
>   共 8 项。`0x5E` 看到的 `0x7f88,0xef7f,...` 是错位一字节的伪 u32。

---

## 2. 48 字节头（完整字段表）

打包（`packed`）小端。`xochitl` 打印顺序与偏移完全对应：

| 偏移 | 大小 | 字段（xochitl 打印名） | 实测值 | 说明 |
|---|---|---|---|---|
| 0x00 | u32 | `checksum` | `0x734762BC` | **CRC32**：`crc32(4×0x00 || file[4:file_length])`，实测通过 ✔ |
| 0x04 | u32 | `file_length` | 282610 | = 实际文件大小 ✔ |
| 0x08 | u32 | `serial_number` | 7802 | 生产序号 |
| 0x0C | u8 | `run_type` | 0x11 | 运行类型 |
| 0x0D | u8 | `fpl_platform` | 0x00 | FPL 平台码 |
| 0x0E | u16 | `fpl_lot` | **259 (0x0103)** | = 文件名里的 `R259` ✔ |
| 0x10 | u8 | `mode_version` | **0x19** | 决定模式命名表（见 §10） |
| 0x11 | u8 | `wf_version` | 0xAB | |
| 0x12 | u8 | `wf_subversion` | 0x21 | |
| 0x13 | u8 | `wf_type` | 0x51 | |
| 0x14 | u8 | `fpl_size` | 0x00 | 面板尺寸码 |
| 0x15 | u8 | `mfg_code` | 0xCE | 面板厂商码；xochitl 的 `tft_vid` 大概率由它/barcode 推出（见 §4） |
| 0x16 | u8 | `wfm_rev` | 0x00 | |
| 0x17 | u8 | `frame_rate_hex` / `frame_rate` | **0x85 = 133** | 帧率 133 Hz（xochitl 会同时按 hex/%d 打两遍） |
| 0x18 | u8 | （xochitl 未打印） | 0x55 | |
| 0x19 | u8 | `vcom_offset` | 0x00 | |
| 0x1A | u16 | （xochitl 未打印） | 0x0000 | |
| 0x1C | u24 | `xwia` | **0x40 (64)** | xwi 块偏移（指向长度字节） |
| 0x1F | u8 | `cs1` | 0xCB | 语义未知（不是简单求和，见 §11） |
| 0x20 | u24 | `wmta` | **0x5F (95)** | 模式表偏移 ✔ 与公式 `48+16+1+29+1` 一致 |
| 0x23 | u8 | `fvsn` | 0x01 | |
| 0x24 | u8 | `luts` | **0x04** | LUT 类型；0x04 = 5-bit / 32×32 / 4 码每字节 |
| 0x25 | u8 | `mc` | **7** | 模式数 − 1 |
| 0x26 | u8 | `trc` | **13** | 温区数 − 1 |
| 0x27 | u8 | `awv` | 0x03 | advanced waveform flags |
| 0x28 | u8 | `eb` | 0xFF | |
| 0x29 | u8 | `sb` | 0xFC | |
| 0x2A | 1 | （保留） | 0x01 | |
| 0x2B..0x2E | 4 | （保留） | 0 | |
| 0x2F | u8 | `cs2` | 0x77 | 语义未知 |

`luts` 取值→几何（与公开工具 `wbf_waveform_dump` 一致）：

| luts | 状态数 | 码/字节 | 单向 | 说明 |
|---|---|---|---|---|
| 0x00 | 16 | 4 | 否 | 4-bit LUT，16×16 |
| **0x04** | **32** | **4** | 否 | **5-bit LUT，32×32（本样本）** |
| 0x05 | 32 | 2 | 否 | 多电压 LUT |
| 0x15 | 32 | 2 | 否 | 多电压 LUT v2 |
| 0x1D | 32 | 2 | 是 | 单向 LUT（每相位只有 32 个目标） |

---

## 3. 温区表（0x30..0x3F）

`trc+1 = 14` 个下界 + 末界 + 1 B 校验和：

```
bounds  = 00 03 06 09 0c 0f 12 15 18 1b 1e 21 26 2b 30   (0,3,6,9,12,15,18,21,24,27,30,33,38,43)
末界    = 0x3E = 0x30 = 48
校验和  = 0x3F = 0x47 = (sum(bounds)+48)&0xFF ✔
```

14 个温区（半开区间）：

```
[0,3) [3,6) [6,9) [9,12) [12,15) [15,18) [18,21) [21,24)
[24,27) [27,30) [30,33) [33,38) [38,43) [43,48)
```

与 zplash 的 `tmin/tmax` 完全对应，只是 zplash 把两端钳到 `-64` 和 `+200`：
zplash `[-64,2] [3,5] ... [43,200]` ↔ wbf 的第 0 档 `[0,3)`、第 13 档 `[43,48)`，
外推规则 zplash 自己选的。**14 档一一对应，无歧义。**

---

## 4. xwi 块（额外波形信息）

`xwia=0x40`：`[u8 len][len B ASCII][u8 cs]`，`cs = (len + Σchars) & 0xFF`。

本样本：`len=0x1D(29)`，字符串 `320_R259_AFAB21_ED103TC2M1_TC`，`cs=0x88` ✔

这就是文件名本体，也解释了 xochitl 的 `xwi: %s` / `xwi_size: %d` / `xwi_cs: 0x%02X`
三个字段。`tft_vid: %s` 则是从 mfg_code/barcode 推出的面板 VID
（xochitl 还有 `U2/M1/U3/C5/C6` 等字符串和 `Unable to decode TFT_VID from barcode.`）。

---

## 5. 模式表与模式块

```
模式表 @ wmta(0x5F)：8 × {u24 表地址, u8 = (b0+b1+b2)&0xFF}
  mode0 -> 0x07F      mode4 -> 0x23F
  mode1 -> 0x0EF      mode5 -> 0x2AF
  mode2 -> 0x15F      mode6 -> 0x31F
  mode3 -> 0x1CF      mode7 -> 0x38F
```

每模式占 **0x70 B**，里面是**两个** 14 项的指针库：

```
+0x00  14 × {u24 波形表地址, u8 cs}    ← 主温区表（本文件全部有效）
+0x38  14 × {u24 地址, u8 cs}          ← 第二库（mode4/mode5 有内容，其余全 0）
```

第二库在 mode4/mode5 里指向文件尾部 0x44E90 / 0x44F00 起的
**8 B / 12 B 记录**（`01 00 01 02 <ptr> <cs>`、`02 00 02 04 <ptr><cs> <ptr><cs>`），
这些记录又指向 0x44FA8 之后的 **8×8、值域 0..7 的抖动矩阵**。
我把它原样报进 JSON 的 `modes[i].extra_bank`，**但不解释其语义**（见 §11）。
这很可能是 GL16 的“抖动/多灰阶混合”通道，值得以后单独挖。

---

## 6. 波形表：RLE + 相位几何

### 6.1 RLE（已用 79 张表的校验和全部验证）

```
rle = True
loop:
    c = *p++
    cs += c
    if c == 0xFC: rle = not rle          # 切换 原样/RLE
    elif c == 0xFF: break                # 表结束
    elif not rle: emit(c)                # 原样 1 字节
    else:  n = *p++; cs += n; emit(c × (n+1))   # RLE：重复 n+1 次
assert *p == cs & 0xFF                   # 尾随 1 B 校验和
```

注意是 `n+1`（长度 0 表示 1 次），且校验和**包含** `0xFC`/`n`/`0xFF` 本身。
本样本 79 张表全部通过（`wbf.py --check` 会打印）。

### 6.2 相位 = 32×32 网格

```
bytes_per_phase = states*states / codes_per_byte = 32*32/4 = 256
phases          = len(expanded) / 256
```

每字节装 4 个 2-bit 码；字节顺序：**外层 dst(0..31)，内层 src 每 4 个一组**。

`wbf.py` 里 `phase_grid()` 给出 `grid[src][dst]`，方便按 `(prev,cur)` 取波形。

### 6.3 2-bit 码只出现 0/1/2，从没出现过 3

实测分布（temp band 0）：

| 模式 | 相位码 0 | 1 | 2 | 3 |
|---|---|---|---|---|
| INIT | 5 120 | 73 728 | 73 728 | 0 |
| DU | 72 528 | 1 482 | 742 | 0 |
| GC16 | 97 240 | 17 712 | 19 192 | 0 |
| A2 | 35 636 | 102 | 102 | 0 |
| DU4 | 80 364 | 3 166 | 2 486 | 0 |

INIT 的 1/2 完全对称（73 728 / 73 728），像“正/负极性驱动”；0 = 不驱动。
1、2 哪个是“向黑/向白”**未确定**。

### 6.4 每张表尾部固定 16 B 记录

78/79 张表在“RLE 数据 + 0xFF + 校验和”之后紧跟着**完全相同**的 16 B：

```
01 00 | b0 04 | b0 04 | c0 08 | 40 06 | 00 00 | 00 00 | 00 77
       1200    1200    2240    1600
```

模式表区之后（0x3FF）也有一份同样的记录。看起来是波形/时序常数
（1200/2240/1600），**语义未确定**；解析时跳过即可。

---

## 7. 自检结果（脚本可直接复现）

`python3 wbf.py <file> --check`：

```
self-check OK: mode 0 (INIT) phases == zplash byte_count (INIT)
self-check OK: mode 2 (GC16) phases == zplash phase_count (GC16)
self-check OK: mode 3 (GL16) phases == zplash phase_count (GC16)
self-check OK: mode 4 (GL16) phases == zplash phase_count (GC16)
self-check OK: mode 5 (GL16) phases == zplash phase_count (GC16)
```

各模式相位阶梯（14 个温区，B0 最冷）：

| mode | 名字 | 相位阶梯 |
|---|---|---|
| 0 | INIT | 149,133,117,103,89,159,135,111,99,87,79,71,63,59 |
| 1 | DU | 73,65,57,50,43,37,31,25,22,19,17,15,13,12 |
| 2 | GC16 | **131,116,102,90,77,66,55,46,38,38,38,38,38,38** ← zplash phase_count |
| 3 | GL16 | 131,116,102,90,77,66,55,46,38,38,38,38,38,38 |
| 4 | GL16 | 131,116,102,90,77,66,55,46,38,38,38,38,38,38 |
| 5 | GL16 | 131,116,102,90,77,66,55,46,38,38,38,38,38,38 |
| 6 | A2 | 35,31,28,24,21,17,15,12,10,10,10,10,10,10 |
| 7 | DU4 | 84,75,65,58,49,42,35,29,24,24,24,24,24,24 |

注意 mode3/4/5 指向**同一批表地址**（0x20B7C…），即三个 GL16 模式共享同一套 LUT
——这正好对上模式名表里 GL16 连续出现三次。

另外三类校验也全部通过（`wbf.py` 的 `to_json()` 里有字段）：
* 全局 CRC32 = 头 `checksum` ✔
* 温区表校验和 ✔
* xwi 校验和 ✔
* 79 张波形表校验和 ✔（112 个引用去重后 79 张）

---

## 8. 模式是什么 / 与 GC16、DU、A2、GL16 的对应

依据：
1. `mode_version = 0x19`；公开工具
   [`wbf_waveform_dump`](https://cdn.jsdelivr.net/gh/zephray/glider@master/utils/wbf_waveform_dump/main.c)
   的模式名表里 `versions={0x19,0x43}` 对应
   `{"INIT","DU","GC16","GL16","GL16","GL16","A2","DU4"}`，**正好 8 项 = mc+1**；
   该条目的样例面板是 **ES103TC1**，与本文件 xwi 里的 **ED103TC2M1** 同一 10.3" 家族。
2. `mc+1 = 8` 与上述表长度吻合。
3. mode3/4/5 共享表 → GL16 三连。
4. 目标态（dst）使用集合完全印证语义：

| mode | dst 索引集合 | 语义 |
|---|---|---|
| 0 INIT | 0..31 全部 | 全刷初始化，任意前态 → 任意目标 |
| 1 DU | **{0, 30}** | 只驱动到黑/白 → 二值直刷 |
| 2 GC16 | **{0,2,4,…,30}** | 16 灰阶全量刷新 |
| 3/4/5 GL16 | {0,2,…,30} | 16 灰阶，波形更短（复用 GC16 表） |
| 6 A2 | **{0, 30}** + src∈{0,1,3,29,30,31} | 二值极速刷新 |
| 7 DU4 | **{0, 10, 20, 30}** | **4 灰阶**直刷 |

DU4 只用 4 个目标 `0/10/20/30`——正好是灰阶 `0/5/10/15` 的两倍，
这是 `dst_index = 2 × 灰阶` 的最硬证据：
* GC16 用全部 16 灰阶 → 偶数索引 `0,2,…,30` ✔
* DU4 用 4 灰阶 `{0,5,10,15}` → `{0,10,20,30}` ✔
* A2/DU 用 `{黑,白} = {0,15}` → `{0,30}` ✔

所以：**wbf 的目标轴有 32 个槽位，但面板只有 16 个灰阶，映射是 `2×g`**
（奇数槽位恒为 0，实测确认）。

---

## 9. 对我们的事情最重要的结论：wbf 有“前态维度”

* wbf 的每张波形表是 **二维 `[prev 0..31][cur 0..31]`**，
  每个相位 2 bit，256 B/相位。
* zplash 里编译进去的是 **每相位 16 字节、只有目标灰阶、没有前态**
  （`phase_tables`），以及一套 `byte_tables`。
* 但两者**相位计数逐值相同**（GC16 阶梯 / INIT 阶梯），
  说明 zplash 的表就是从这份官方 wbf 里“压扁”出来的：
  保留了相位长度，丢掉了 32×32 的前态维度，只留 16 个目标灰阶。

**含义**：要做真正的局部刷新 / 去残影，必须用 wbf 的二维表——因为
“同一个目标灰阶、不同前态”的波形是不一样的，zplash 的 1D 表做不到这件事。
另外 `DU`（mode 1）在 wbf 里只写 `{黑,白}` 两个目标，所以 rM2 的
“快速局部刷新”在波形层面本来就是二值的，中间灰靠 dither。

---

## 10. 关于 xochitl 的 16×16 CSV 导出（一个紧张点，已解释）

xochitl 的导出格式串 `0,%i,%i` / `15,%i,%i` / `%i,0,%i` / `%i,15,%i` / `%1,%2,%3`
确实像 **16×16** 网格。但文件结构强约束是 32×32：
若按 16×16（64 B/相位），GC16 会得到 524 相位，与 zplash 的 131 差 4 倍；
按 32×32（256 B/相位）则 14 个值**全部精确吻合**。

结论：**文件里是 32×32**；xochitl 导出的 16×16 是它内部转换后的表示
（很可能只导出 `dst=2×g` 的 16 个灰阶 × 16 个 prev 取样）。
这一条属于“合理推断”，不是直接证据。

---

## 11. 没能确定的部分（明确标注）

1. **字节内 2-bit 码的位序（LSB-first vs MSB-first）**
   参考实现（`wbf_waveform_dump`）用 LSB-first；但按 MSB-first 解，
   GC16 的“前态=目标态”对角元有 28/32 完全为 0，LSB-first 只有 16/32。
   两者都能通过所有校验和（校验和与位序无关）。**未确定**，
   默认按参考实现 LSB-first，`wbf.py` 里 `phase_grid(..., msb_first=)` 可切换。
2. **`src` 与 `dst` 谁在外层**
   参考实现是“外层 dst、内层 src 每 4 个一组”。因为配对方式与位序耦合，
   单靠文件无法独立证实。默认按参考实现。
3. **相位码 1 与 2 的极性**（哪个向黑、哪个向白）。只知 0=不驱动、3 从不出现。
4. **`cs1`、`cs2`、`eb`、`sb`、`awv`、`fvsn` 的确切含义**。
5. **每张表后固定 16 B 记录**（`01 00 b0 04 b0 04 c0 08 40 06 …77`）的含义；
   78/79 张表后都相同，疑似时序/电压常数。
6. **第二指针库 + 尾部描述符 + 8×8 抖动矩阵**的语义
   （mode4/mode5 有内容）。已报进 JSON 的 `extra_bank`，未解释。
7. **`dst_index = 2×灰阶` 的机制原因**（是“5-bit 控制器”还是“留 1 bit 给别的东西”）。
8. **`src` 轴为什么用 0,1,2,3,4 与偶数 6..28 再加 29,30,31**（20 个值），
   而不是纯偶数 16 个。可能是“未初始化/边缘态”的特殊编码。
9. 只有 1 个样本，因此**42 个文件的差异**（三种 TFT VID、不同 FPL lot、
   是否有 `luts=0x00/0x05/0x15/0x1D` 的变体）都是按代码路径推断，未实测。

---

## 12. `wbf.py` 用法

```bash
source ~/myenv/bin/activate
python3 wbf.py /tmp/320_R259_AFAB21_ED103TC2M1_TC.wbf -o /tmp/wbf.json --check
python3 wbf.py FILE.wbf --summary            # 人读摘要 + 全部表清单
python3 wbf.py FILE.wbf --grid -o out.json   # 每相位展开成 32×32 整数网格（大）
python3 wbf.py FILE.wbf --no-lut             # 只要头/表结构
python3 wbf.py dir/*.wbf                     # 批量，逐个出 .json
```

输出结构（与题目要求一致，另有扩展字段）：

```json
{
  "file": "...", "file_size": 282610,
  "header": { "checksum": ..., "mc": 7, "trc": 13, "luts": 4, ...,
              "_field_layout": [{"name":"checksum","offset":0,"size":4}, ...],
              "_declared_crc32_ok": true },
  "lut_geometry": {"states":32, "codes_per_byte":4, "unidirectional":false,
                   "transitions":1024, "bytes_per_phase":256, "note":"..."},
  "temperature_bounds": [0,3,6,...,43,48],
  "xwi": {"string": "320_R259_AFAB21_ED103TC2M1_TC", "checksum_ok": true, ...},
  "modes": [
    {"index":0, "name":"INIT", "temp_table_addr":127, "extra_bank":[],
     "temps":[
       {"index":0, "range":[0,2], "bound":0, "next_bound":3,
        "table_id":0, "table_addr":1039, "phases":149,
        "expanded_bytes":38144, "checksum_ok":true,
        "lut": ["55ff55...", ...]        // 每项 = 一个相位的 256 B（hex）
       }, ...]}, ...]
}
```

* 默认 `lut` 是**每相位 512 个 hex 字符**（无损、紧凑）：全文件约 3.3 MB。
* `--grid` 时 `lut[phase]` 变成 `grid[src][dst]` 的 32×32 整数数组（约 12 MB）。
* `phase_grid()` 也可以单独 import 用来做 `(prev,cur) -> 波形` 查表。
* 健壮性：截断文件/垃圾数据/全零文件都不会崩，只会返回非 0 或带 `warnings`。
  本样本无 warning、无 error。

---

## 14. 外部参考（用于交叉验证，非本仓库产物）

* `wbf_waveform_dump`（glider / NekoInk）— 头结构、RLE、相位几何的权威实现：
  <https://cdn.jsdelivr.net/gh/zephray/glider@master/utils/wbf_waveform_dump/main.c>
* `python-waveform-parser`（aceisace）— 同格式的 Python 参考：
  <https://raw.githubusercontent.com/aceisace/python-waveform-parser/main/wbf-parser.py>
* i.MX EPDC 内核驱动的 `waveform_data_header`（早期 48 B 版本的字段名）：
  <https://lore.kernel.org/linux-arm-kernel/20220206080016.796556-4-andreas@kemnade.info/raw>

本文件的**所有数值**都是我自己从样本里测出来的，外部资料只用来对照字段名与
解码算法；两者的每一个校验和都对上了。
