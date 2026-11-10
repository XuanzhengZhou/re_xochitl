# 电子书格式支持矩阵

> 实测环境：reMarkable 2（armv7l / Cortex-A7 / 994 MB / 1404×1872 灰阶），
> 全部「真机」列都是实际部署后用 `tools/shot.sh` 截图确认过的，不是本地单测。

## 1. 支持情况

| 格式 | 状态 | 页数 | 真机 | 解析方式 | 已知限制 |
|---|---|---|---|---|---|
| `.rm` 笔记 | ✅ | — | ✅ | 自己的格式 | — |
| **PDF** | ✅ | 文档自报 | ✅ | 设备自带 `/usr/lib/libpdfium.so`（`dlopen`，不打包） | ✅ 文字层 + 大纲（`FPDFText_*` / `FPDFBookmark_*`，缺符号降级） |
| **EPUB** | ✅ | 排版后 | ✅ | 手写 zip 中央目录 + OPF + XHTML 标签流 | ✅ 图片（JPEG/PNG，灰度）；表格拍平；CSS 不做 |
| **TXT** | ✅ | 排版后 | ✅ | BOM → UTF-8 全量校验 → **内置 GBK 表** → Latin-1 | — |
| **Markdown** | ✅ | 排版后 | ✅ | 行级标记剥离 | `*`/`_` 全删（`a*b` 会掉字符） |
| **HTML** | ✅ | 排版后 | ✅ | 标签流扫描，不建树 | `<pre>`/表格不保结构 |
| **RTF** | ✅ | 排版后 | ✅ | 控制字扫描，组跳过 | 代理对会拆、表格/图片不处理 |
| **FB2** | ✅ | 排版后 | ✅ | XML 标签流 | 嵌套 section 的目录项同页；`<binary>` 图片未解 |
| **DOCX** | ✅ | 排版后 | ✅ | 自写 zip + `word/document.xml` | 列表编号丢；不读 styles.xml 继承 |
| **CBZ**（漫画） | ✅ | zip 条目数 | ✅ | `zune-jpeg` + `zune-png` + 16.16 定点面积平均缩放 | — |
| **MOBI** | ✅ | 排版后 | ✅ | PalmDB + PalmDOC header + PalmDOC LZ77 / HUFF-CDIC | 合订本只出 KF7 段 |
| **AZW3**（KF8） | ✅ | 排版后 | ⚠️ 本地验证 | 同上 + KF8 正文 / FDST flow 0 | 无 INDX 重组；KV8 内嵌图未解 |
| `.cbr`（RAR） | ❌ | — | — | 纯 Rust 无可用 RAR 实现 | 提示转 CBZ |

新增格式只要在 `src/doc/<fmt>.rs` 里造出一个 `reflow::Book`（`Vec<Chapter>` + `Vec<TocEntry>`），
再套 `reflow::ReflowDoc`，然后在 `src/doc/mod.rs` 的 `open()` 里加一行分派即可——
断行、分页、字号、页面缓存、目录、搜索全部继承。

## 2. 为什么不用现成的阅读器引擎

调研过（含实测体积）：

| 候选 | 否决理由 |
|---|---|
| KOReader | AGPL-3.0；官方 armhf 包 27.3 MB（解包 80.6 MB），是 Lua 应用不是库，还要 LuaJIT + SDL3 |
| crengine-ng | GPL-2+（传染）；全套 4.58 MiB，还要为 armv7 交叉编译 freetype/harfbuzz/fribidi/unibreak/utf8proc/zstd/z/png/jpeg 共 8 个 C 库 |
| MuPDF | AGPL-3.0（商用要买授权）；裁剪版 3.09 MiB，Alpine 全量 45.1 MiB |
| calibre | Python + Qt6，`ebook-convert` 就是它的 Python 管线，没有可嵌的 C 库 |
| fb2 / docx-rs / html2text | 拖 quick-xml/html5ever 等一大堆依赖，而我们要的逻辑只有几百行 |

**结论**：格式解析全自己写，只借纯 Rust 的叶子 crate。
唯一例外是 PDF——设备上已经有 pdfium，`dlopen` 用现成的，一个字节都不用打包。

代价与收益：新增依赖只有 `zune-jpeg` / `zune-png`（约 +210 KB，纯 Rust，零 C），
换来 EPUB/TXT/MD/HTML/RTF/FB2/DOCX/CBZ 全部支持；如果再引 muPDF 至少是 +3 MB 且整个项目变 AGPL。

## 3. 两个坑（踩过，记下来）

### 3.1 设备上 `iconv` 用不了

`/usr/lib/gconv/` 被裁到只剩 `IBM437.so` 和 `gconv-modules`：

```
$ ls /usr/lib/gconv/
IBM437.so
gconv-modules
```

所以 `iconv_open("UTF-8", "GB18030")` 必然返回 `(iconv_t)-1`，任何走系统 iconv 的
GBK 路径在真机上都会静默失败、退回 Latin-1（满屏乱码）。

修法：`src/doc/gbk.rs` 内置 GBK 双字节表（126 lead × 190 trail = 23940 项，其中 21791 个
有效码位，`u16` 数组 47 KB）。解码顺序是 **BOM → UTF-8 全量校验 → 内置 GBK 表 → 系统 iconv → Latin-1**，
前两步覆盖 99% 的文件，表兜住真机。

### 3.2 `-C target-cpu=cortex-a7` 在这台机器上没收益

armv7 目标默认**不开任何 `target_feature`**（`rustc --print cfg` 输出 0 条），
所以显式开 NEON 是个合理的猜想。实测（设备上 `--bench-ui`）：

| 项 | 不开 NEON | 开 NEON |
|---|---|---|
| `open_at`（光栅缓存） | 49.48 ms | 49.34 ms |
| `redraw_page`（内容没变） | 43.48 ms | 43.64 ms |
| 6 张卡 `blit_gray` | 7.19 ms | **7.74 ms（+7%）** |
| `canvas paint_all` 整屏 | 30.83 ms | 30.91 ms |

热点全是内存带宽受限（memcpy 类），不是算力受限；Cortex-A7 的 NEON 是 64-bit 实现、
吞吐本来就不高，LLVM 的自动向量化反而生成了更差的代码。**结论：不加**，配置文件里留了注释说明。

真要受益得是 JPEG/PNG 解码那条路，但 `zune-jpeg` 的 NEON 只对 `aarch64` 开
（`idct/neon.rs` 首行 `#![cfg(target_arch = "aarch64")]`），armv7 本来就走标量。

## 4. 在电子书上做批注

官方对 PDF 批注的存法是「每页一个 `.rm`，页 id 记在 `.content` 的 `cPages.pages`」。
我们照抄这个结构：

- 打开文档时 `Library::ensure_pages(uuid, n)` 把 `cPages.pages` 补齐到文档页数
  （**只补不删**——重排格式的页数会随字号变，删掉多出来的页会把批注一起丢掉）；
- 每页一个 `.rm`，走的是和笔记本完全相同的墨迹编解码、工具、撤销栈；
- 翻页就是换 `cPages` 里对应的 page id。

真机验证：EPUB 第 1 页画一笔 → 翻到第 2 页 → 翻回第 1 页，墨迹还在；
`.content` 的 `cPages` 从 0 补到 3 页，目录下出现 2 个 `.rm` 文件。

改字号之后页内容会变，但**页号不动**（`set_reading_layout` 不走 `goto_page`——
它第一条判据就是「目标页 == 当前页直接返回」，改字号恰好是「页号不变、内容全变」）。
另外页面光栅缓存（`page_fingerprint`）原来只看墨迹，改字号后会命中旧图，
现在把底图版本号 `bg_gen` 也混进指纹。

## 6. 真机验证记录（7 种格式同屏）

`shots/lib8.png`：MOBI(5 pages) / 漫画 CBZ(10 pages) / FB2(2 pages) /
Markdown(1 page) / 中文 GBK 小说(1 page) / EPUB(3 pages) / PDF(Page 7 of 41)
全部渲染出正确封面与页数。

## 5. 阅读设置

字号 5 档（`reflow::FONT_SIZES`），入口在**文档设置面板**（左侧工具栏那个四格图标）
的 `Text size` 行，点一次换一档。改字号会重排 → 页数变化 → 重新渲染当前页，
页号保持不变（读到大半本调字号不会被弹回第一页）。

行距/边距/字号已接到阅读界面（文档设置 → `Text size` → `Text settings` sheet，
按文档持久化到 `~/.config/rmdev/readcfg.json`）。`Font` / `Justification` 仍灰显。
