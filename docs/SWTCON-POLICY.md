# SWTCON-POLICY — xochitl 的电子墨水刷新策略 / 波形表导出开关

> 目标样本：从设备取出的 `xochitl`（ELF32 ARM ET_EXEC，8,027,240 B）。
> 分析日期：本次会话。所有结论都给地址；没有证据的写「未确定」。
> 复跑脚本在 `tools/`，见文末 §6。

**地址约定**：文中所有地址都是 **vaddr**（链接地址）。
`va = file_off + 0x10000`（file_off < 0x7906a4），`va = file_off + 0x20000`（≥ 0x7906a4）。
例：字符串 `/usr/share/remarkable/speedy-gonzales.csv` 在 file_off `0x72d3bc` → vaddr **`0x73d3bc`**。

---

## 0. 结论速览（TL;DR）

| # | 问题 | 结论 | 置信度 |
|---|---|---|---|
| Q1 | CSV 导出开关 | **不存在。这个 build 里没有任何代码会写 `/usr/share/remarkable/speedy-gonzales.csv`。** 该路径只被**读**（`QFile::open(ReadOnly)`）。所有 "export" 字样指的是「把解析好的 CSV **灌进**内存波形表」，不是「把内存表写成 CSV」。想拿波形表只能改二进制去打 `0x55ef48`（stdout dumper） | **高** |
| Q2 | `WaveformMode` 枚举 | 有效值 `{0,1,2,3,6,8,10}`，`{4,5,7,9}` 直接判 invalid 并 return；`sendUpdate` 里有一张 `→ 内部 wbf mode` 的映射表 | 见 §2 |
| Q3 | INIT 表 | `clearScreen`/上电清场走独立 entry，`Could not fetch init waveform` 在 `FUN_00553f04` | 见 §3 |
| — | **附带发现（对替代实现有用）** | 那个 CSV 路径是个**波形表覆盖入口**：设备上放一个合法 CSV 到该路径，xochitl 启动时会解析它并置 `this+0x3a=1`（跳过 wbf） | 高，见 §1.4 |

---

## 1. Q1 — 波形表 CSV「导出」开关：不存在（但存在一个**导入**入口）

### 1.1 先纠正一个前提：字符串没被绝对地址引用，是因为代码是 PIC 锚点风格

在文件里搜 LE32(字符串 vaddr) 找不到任何命中，这是**正常的**，不是线索缺失。
这个二进制的所有 rodata 取址都是「literal 池存相对偏移 + `add rX, pc, rX`」：

```
 5532cc: e59f0358   ldr  r0, [pc, #0x358]     @ 0x55362c → 0x001ea0e0   (偏移，不是地址)
 5532d0: e3a01029   mov  r1, #0x29            (41 = strlen)
 5532d4: e08f0000   add  r0, pc, r0           → 0x5532dc + 0x1ea0e0 = 0x73d3bc
```

`0x73d3bc` = `"/usr/share/remarkable/speedy-gonzales.csv"`（41 字节，`0x72d3bc`+0x10000）。
我写了一个线性扫描器把这种 `ldr`+`add pc` 对全部还原成常量（`tools/scan_consts.py`，
`.text` 1,384,065 条指令 → 48,878 个常量），**全二进制只有一处**构造出这个字符串地址：

```
0x5532d4  -> 0x73d3bc   (/usr/share/remarkable/speedy-gonzales.csv)
```

它落在 `FUN_0055324c`（`EPFramebufferSwtcon` 的初始化，见 §1.2）。

### 1.2 唯一的调用链

```
FUN_0055324c  (init/ctor, 0x3f0 B)          ← 唯一调用者 0x5496bc
  └─ 0x553300: bl FUN_005563b0(char* path, int 8)
        FUN_005563b0 (0x49c B)              ← 唯一调用者 0x553300
          ├─ 0x556404: bl FUN_0055f7dc(&tbl, QString(path), 1)   ← CSV 解析器
          ├─ 0x5567d8: bl 0x55d044(tbl, 6)   ← 校验/统计（FUN_0055cd24 的热段）
          ├─ 0x5567e8: bl FUN_0055d3d4(tbl, 6)  ← "export to table"
          └─ 0x556810: printf("Unable to export waveform table.")
```

`0x5496bc` 是 `EPFramebufferSwtcon` 的构造点；这条链**在启动时无条件走一遍**
（设备 journal 里那行 `Unable to load waveform from file: /usr/share/remarkable/speedy-gonzales.csv`
就是它——因为文件不存在，解析失败，printf 走 `0x556464`）。

### 1.3 决定性证据：这里**没有**任何写文件的代码

**(a) 打开模式是 ReadOnly。** `FUN_0055f7dc` 里唯一的 `QFile::open` 调用：

```
 55f810: bl   0x3d6b4     ; QFile::QFile(QString const&)
 55f818: mov  r1, #1      ; ← QIODevice::ReadOnly
 55f81c: bl   0x3c730     ; QFile::open(QFlags<QIODevice::OpenModeFlag>)
```

`QIODevice::ReadOnly = 0x0001`。全函数没有第二个 `open`。

**(b) 三个「看起来在导出」的函数，输出目标全是 stdout。**
把 `bl` 目标按 PLT 名解析（`tools/calls.py` + `tools/plt.tsv`）：

| 函数 | 体积 | 全部被调用的外部符号 | 判定 |
|---|---|---|---|
| `FUN_0055ef48`（`0,%i,%i` / `15,%i,%i` / `%i,0,%i` / `%i,15,%i`） | 0x380 | `memset`, **`printf` ×11** | CSV 文本打到 stdout，没有 `fopen`/`QFile::open` |
| `FUN_0055a148`（`Printing mode %d, temp %d:` / `%x->%x: ` / ANSI 色） | 0x198 | `printf` ×6, `puts` | 同上 |
| `FUN_0055e464` | 0x6c4 | `printf` ×5, `puts`, `QString::operator=` | 同上 |

**(c) `"Unable to export to table: some waveforms are not valid"` 的语义是「导入」不是「导出」。**
它在 `FUN_0055d3d4`（0x8a4 B）里，另外两条同族字符串是
`"Exporting to table without appending zero!"`（`0x73d9f0`，ref `0x55dabc`）。
"export **to table**" = 把 CSV 条目写进内部波形表；"without appending zero" = 内部表要求末尾补 0 终止符。
调用方那句 `"Unable to export waveform table."`（ref `0x5534f8`，`QMessageLogger::warning`）
是**同一个失败**的对外措辞。两句话都是「CSV → 内部表」失败，不是「内部表 → 文件」失败。

**(d) 三个 dumper 没有任何静态交叉引用。** `FUN_0055ef48` / `FUN_0055a148` / `FUN_0055e464`：
- 没有任何 `bl`/`blx` 指向它们（130,260 个 `bl` 点全扫过）；
- 没有任何 `b`/`bcc` 指向它们（全 `.text` 条件/无条件分支扫过）；
- 文件里搜不到它们的 LE32 地址（.data.rel.ro / .got / .data 全搜过）；
- 该区域也没有 `add pc, pc, rX, lsl #2` 或 `ldr pc` 跳转表。

即：编译进去了，但调用点被条件编译掉了。**这意味着不存在「运行时开关」**，
命令行参数 / 环境变量 / Qt 日志分类 / 配置项都无解 —— 你没有任何东西可以打开它。

### 1.4 但是：这个 CSV 路径是一个活的**波形表覆盖入口**（对你们有用）

`FUN_0055324c` 在调 `FUN_005563b0` 前把 `this+0x3a = 0`，调用成功则置 1：

```
 553278: strb r1, [r4, #0x3a]     ; r1 = 0，先清标志
 ...
 553300: bl   FUN_005563b0        ; 解析 CSV（成功返回 0）
 553358: cmp  r6, #0
 55335c: moveq r3, #1
 553360: strbeq r3, [r4, #0x3a]   ; ← CSV 加载成功 → this[0x3a] = 1（覆盖标志）
 553364: bne  0x5534b4            ; 失败 → qWarning("Unable to export waveform table.") 然后 return
```

也就是说：**在 `/usr/share/remarkable/speedy-gonzales.csv` 放一份合法 CSV，
`this+0x3a` 会被置 1，官方就用 CSV 里的波形而不用 `.wbf`。**
CSV 的行格式可以从 `FUN_0055ef48` 的格式串反推（见 §1.5），
解析器在 `FUN_0055f7dc`（报错串 `Corrupt data at line` / `Corrupt current value at line` /
`Corrupt target value at line` / `Corrupt waveform value at line`）。

> 注意：`/usr/share` 在 rootfs 上，只读挂载；要用这个 hook 得先 `mount -o remount,rw /`。
> 这条**不需要改二进制**，是官方自带的后门级调试入口 —— 比逆向 wbf 省事得多。

### 1.5 CSV 格式（从格式串反推，尚未实机验证）

`FUN_0055ef48` 的格式串：

```
"0,%i,%i\n"     "15,%i,%i\n"     "%i,0,%i\n"     "%i,15,%i\n"     ","  "%4x" "%4d"
```

对应 E Ink 波形表的经典 16×16 布局：行 = current gray (0..15)，列 = target gray (0..15)，
每个格子是一串帧驱动码（4 bit/帧）。`0,` / `15,` / `,0` / `,15` 是四条边界行/列的特化写法
（省掉方括号/引号）。`^0+` / `0+$`（`0x73d9b0` / `0x73d9b4`，ref `0x55c30c` / `0x55c3f8`）
是 `QRegularExpression`，用来去前导/尾随 0。

> 未确定：一行里多帧码的确切分隔符（`%4x` 连写还是逗号分隔）需要抓一次真实 dump 才能定死。

### 1.6 已经排除的可能性

| 猜想 | 排除证据 |
|---|---|
| 命令行参数 | `.text` 里唯一构造 CSV 路径的点是 init（0x5532d4），没有任何 `argv` 比较链能到它 |
| 环境变量 | 同上；且 `0x5563b0` 只收 1 个 `char*` + 1 个 int 常量 8 |
| Qt 日志分类 | `rm.framebuffer.swtcon`（`FUN_00552ec0` 里）只用于 `qCDebug`，不影响控制流 |
| 配置项 | `0x5534b4` 前后没有读 `QSettings`/conf 的调用；`bne` 条件只看 CSV 解析返回值 |
| debug 构建才有 | **不是**。三个 dumper 的代码和字符串都在 release 二进制里，只是没人调用 |

**给你们的建议（Q1 的实用结论）**：
放弃「找导出开关」这条路。要么 (a) 直接把 `FUN_0055ef48` 当函数调用（patch 一个 `bl` 到 init 里，
或者用 `LD_PRELOAD` 不行——它在静态符号里，得改 ELF），要么 (b) 走 §1.4 的 CSV **导入**入口，
要么 (c) 直接写 `.wbf` 解析器。**理由：导出功能没有开关，只有不存在。**

---

## 6. 脚本
