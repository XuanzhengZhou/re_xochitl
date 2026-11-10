//! 页面文字的存储层：官方 `.rm` 的 `RootText` 文字块（读写）+ sidecar 后端。
//!
//! # 结论：官方 `.rm` 文字格式**现在能正确读写了**（A）
//!
//! 判定依据不是"猜得像"，而是四条硬证据：
//!
//! 1. 开源实现 `rmscene` 0.8.0 有完整的 RootText 读写代码（下面每条都给了 URL + 行号）；
//! 2. 它带的**真机样本**（`Normal_AB.rm`、`Bold_Heading_Bullet_Normal.rm`）能逐字节
//!    被本模块复现 —— 单测里就是拿这两份字节做的 `parse → serialize → 与原文相等`；
//! 3. 固件里那条独立的写路径（`FUN_0036ed08`）的每个 tag 与 rmscene 一一对上（见下表）；
//! 4. 本模块**从零写出**的文件，用真 rmscene 0.8.0 读回，段落/样式/位置全对：
//!    `RmText::plain("AB")` 写出的整份文件与真机 `Normal_AB.rm` **逐字节相同**；
//!    三段（标题/正文/列表，含中文）的文件 rmscene 读出
//!    `HEADING '标题' / PLAIN 'first line' / BULLET 'bullet item'`，
//!    样式键正是那两个换行字符的隐含 id `(1,18)`、`(1,29)`。
//!
//! 之前判 B 的原因（`M3-scene-model.md:545` 的 tag 表与固件对不上、没有样本、样式枚举未知）
//! 现在全部解开：`M3` 那张表是简化/转抄错的，rmscene 与固件完全一致；样式枚举在
//! `scene_items.py:148-161`；CRDT 顺序在 `crdt_sequence.py:99-156`。
//!
//! ## 逐条对照（rmscene 源码 ↔ 固件 `FUN_0036ed08_0036ed08.c`）
//!
//! | 位置 | rmscene 0.8.0 | 固件 | 结论 |
//! |---|---|---|---|
//! | 块类型 | `scene_stream.py:758` `BLOCK_TYPE = 0x07` | `FUN_00336c5c:1165` 块头 `0x7000000` | ✅ |
//! | 块 id | `:768` `read_id(1)` + `assert block_id == CrdtId(0,0)` | `:50` tag(1,ID) | ✅ 恒 (0,0) |
//! | 文本容器 | `:771` tag(2,LEN4) | `:75` `0x2c` | ✅ |
//! | 文本项外壳 | `:774-775` 两层 tag(1,LEN4) | `:83` + `:112` 两个 `0x1c` | ✅ 就是这两层 |
//! | 每项 | `:675` tag(0,LEN4) | `:133` `0x0c` | ✅ |
//! | item/left/right | `:676-678` tag(2..4,ID) | `:151/:221/:278` `0x2f/0x3f/0x4f` | ✅ |
//! | deleted_length | `:679` tag(5,int) | `:340` `0x54` | ✅ |
//! | 文本 | `:681-691` tag(6,LEN4) = 字符串或格式码 | `:345` `0x6c` + `:398` `0x24` | ✅ |
//! | 样式容器 | `:782` tag(2,LEN4) → `:783` tag(1,LEN4) | `:486` `0x2c` + `:494` `0x1c` | ✅ |
//! | 样式项 | `:716-737` 裸 crdt id + tag(1,ID) + tag(2,LEN4){0x11,code} | `:565` `0x1f` + 尾部 `0x11,code` | ✅ |
//! | 位置 | `:790-794` tag(3,LEN4) {f64 pos_x, f64 pos_y} | `:765` `0x3c` + 16 字节 | ✅ 16B = 2×f64 |
//! | 宽度 | `:797` tag(4,Byte4) f32 | `:792` `0x44` + f32 | ✅ |
//!
//! `M3-scene-model.md:545` 写的 `3=pos_x, 4=pos_y, 5=width` 是错的（没有 tag 5；
//! tag 3 是 16 字节的两枚 f64）。以本文件上表为准。
//!
//! 源码 URL（`main` 分支 / 0.8.0）：
//! - <https://github.com/ricklupton/rmscene/blob/main/src/rmscene/scene_stream.py>
//! - <https://github.com/ricklupton/rmscene/blob/main/src/rmscene/scene_items.py>
//! - <https://github.com/ricklupton/rmscene/blob/main/src/rmscene/text.py>
//! - <https://github.com/ricklupton/rmscene/blob/main/src/rmscene/crdt_sequence.py>
//! - 样本：<https://github.com/ricklupton/rmscene/tree/main/tests/data>
//!
//! ## 还没验证的（别当已验证）
//!
//! 1. **没在真机上打开过**本模块写出的文件（不许 ssh remarkable、不部署）。
//!    间接证据：两个真机样本可逐字节复现（`roundtrip_*_byte_exact`、
//!    `plain_ab_block_matches_device_bytes`）。
//! 2. 只重写 `RootText`(0x07) 与 `PageInfo`(0x0A) 的字符/行计数，不动
//!    SceneTree/TreeNode/items。官方读侧是否还要求别的（anchors、SceneInfo）未知。
//! 3. 排版（字号/行高/断行）不在文件里：文件只有整篇 `pos_x/pos_y/width` + 每段样式。
//!    所以 [`SceneText::size`] 是**渲染约定**，不落盘。
//! 4. 多作者并发合并只读不写：写出的都是单作者、单 item 的文档。
//!
//! ## 数据形状（照抄 rmscene）
//!
//! ```text
//! RootText 块（type 0x07, min=1, cur=1）
//!   tag(1, ID)               block_id，官方恒 (0,0)
//!   tag(2, LEN4) {
//!     tag(1, LEN4) { tag(1, LEN4) { varuint n; n × tag(0,LEN4) item } }
//!     tag(2, LEN4) { tag(1, LEN4) { varuint n; n × format } }
//!   }
//!   tag(3, LEN4) { f64 pos_x, f64 pos_y }
//!   tag(4, Byte4) f32 width
//! item   = { tag(2,ID) id, tag(3,ID) left, tag(4,ID) right, tag(5,Byte4) deleted,
//!            [tag(6,LEN4): varuint len + u8 1 + utf8，或再跟 tag(2,Byte4) 格式码] }
//! format = 裸 crdt id + tag(1,ID) 时间戳 + tag(2,LEN4) { u8 0x11, u8 样式码 }
//! ```

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::doc::rm::{Id, HEADER};
use crate::doc::library::TextItem as SidecarText;

/// 块类型：RootText / PageInfo。
const BT_ROOT_TEXT: u8 = 0x07;
const BT_PAGE_INFO: u8 = 0x0a;

const TAG_BYTE1: u8 = 0x1;
const TAG_BYTE4: u8 = 0x4;
const TAG_LEN4: u8 = 0xc;
const TAG_ID: u8 = 0xf;

/// 官方："第一段没有前置换行，样式键用 (0,0)"（`scene_items.py:163` END_MARKER）。
const END_MARKER: Id = Id(0, 0);

// ---------------------------------------------------------------- 样式

/// 段落样式。存的是磁盘上的原始字节（`scene_items.py:148-161`）：
/// `0=BASIC 1=PLAIN 2=HEADING 3=BOLD 4=BULLET 5=BULLET2 6=CHECKBOX 7=CHECKBOX_CHECKED`。
///
/// 用 newtype 而不是 enum：未知值要能原样传回去，不能一读就丢。
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct ParagraphStyle(pub u8);

impl Default for ParagraphStyle {
    /// 缺省是 PLAIN(=1)：老 sidecar 没有 style 列，读出来不能变成 BASIC(0)。
    fn default() -> ParagraphStyle {
        ParagraphStyle::PLAIN
    }
}

impl ParagraphStyle {
    pub const BASIC: ParagraphStyle = ParagraphStyle(0);
    pub const PLAIN: ParagraphStyle = ParagraphStyle(1);
    pub const HEADING: ParagraphStyle = ParagraphStyle(2);
    pub const BOLD: ParagraphStyle = ParagraphStyle(3);
    pub const BULLET: ParagraphStyle = ParagraphStyle(4);
    pub const BULLET2: ParagraphStyle = ParagraphStyle(5);
    pub const CHECKBOX: ParagraphStyle = ParagraphStyle(6);
    pub const CHECKBOX_CHECKED: ParagraphStyle = ParagraphStyle(7);

    pub fn label(self) -> &'static str {
        match self.0 {
            0 => "basic",
            1 => "plain",
            2 => "heading",
            3 => "bold",
            4 => "bullet",
            5 => "bullet2",
            6 => "checkbox",
            7 => "checkbox-checked",
            _ => "unknown",
        }
    }

    /// 渲染字号：文件里**没有**字号，这是本 App 按官方 QML 的两个正文字号定的
    /// （`fontSizeBody2`=25、`fontSizeBody1`=35，见 `refs/qml_all/res_06ba1de.qml`）。
    /// 只把标题放大，其它（含 BOLD/BULLET）都用正文字号。
    pub fn default_size(self) -> f32 {
        if self == ParagraphStyle::HEADING || self == ParagraphStyle::BASIC {
            35.0
        } else {
            25.0
        }
    }
}

// ---------------------------------------------------------------- 文档模型

/// 一个文本项的值：一段文本，或一个**行内格式码**（1=粗体起 2=粗体止 3=斜体起
/// 4=斜体止，见 `text.py:121-132`；格式码自己不带字符）。
#[derive(Clone, Debug, PartialEq)]
pub enum TextValue {
    Str(String),
    Format(u8),
}

/// CRDT 序列里的一项（`scene_stream.py:674-695`）。
#[derive(Clone, Debug, PartialEq)]
pub struct TextItem {
    pub id: Id,
    pub left: Id,
    pub right: Id,
    /// >0 表示这一段被删了，此时 `value` 是空串，长度由它表示。
    pub deleted: u32,
    pub value: TextValue,
}

/// 段落样式项（`scene_stream.py:712-739`）：`key` 是该段首字符的 id（首段是 (0,0)），
/// `timestamp` 是 LWW 时间戳，`style` 是样式码。
#[derive(Clone, Debug, PartialEq)]
pub struct TextFormat {
    pub key: Id,
    pub timestamp: Id,
    pub style: u8,
}

/// 一页的文字层 = 一个 RootText 文档。
#[derive(Clone, Debug, PartialEq)]
pub struct RmText {
    pub block_id: Id,
    /// 整篇左上角（文件里是 f64）
    pub pos_x: f64,
    pub pos_y: f64,
    /// 整篇宽度（文件里是 f32）
    pub width: f32,
    pub items: Vec<TextItem>,
    pub formats: Vec<TextFormat>,
}

/// 排版后的段落（给 UI / sidecar 用）。
#[derive(Clone, Debug, PartialEq)]
pub struct Paragraph {
    pub style: ParagraphStyle,
    pub text: String,
}

impl Default for RmText {
    fn default() -> RmText {
        // 官方新建文本层的默认值（`scene_stream.py:985-993`）
        RmText {
            block_id: END_MARKER,
            pos_x: -468.0,
            pos_y: 234.0,
            width: 936.0,
            items: Vec::new(),
            formats: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------- 读写原语

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, format!("rm 文字块: {msg}"))
}

struct R<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> R<'a> {
    fn new(b: &'a [u8]) -> R<'a> {
        R { b, p: 0 }
    }
    fn eof(&self) -> bool {
        self.p >= self.b.len()
    }
    fn ok(&self, n: usize) -> io::Result<()> {
        if self.p + n <= self.b.len() {
            Ok(())
        } else {
            Err(bad("数据截断"))
        }
    }
    fn u8(&mut self) -> io::Result<u8> {
        self.ok(1)?;
        let v = self.b[self.p];
        self.p += 1;
        Ok(v)
    }
    fn u32(&mut self) -> io::Result<u32> {
        self.ok(4)?;
        let v = u32::from_le_bytes(self.b[self.p..self.p + 4].try_into().unwrap());
        self.p += 4;
        Ok(v)
    }
    fn f32(&mut self) -> io::Result<f32> {
        Ok(f32::from_bits(self.u32()?))
    }
    fn f64(&mut self) -> io::Result<f64> {
        self.ok(8)?;
        let v = f64::from_le_bytes(self.b[self.p..self.p + 8].try_into().unwrap());
        self.p += 8;
        Ok(v)
    }
    fn varuint(&mut self) -> io::Result<u64> {
        let mut out = 0u64;
        let mut shift = 0;
        loop {
            let c = self.u8()?;
            out |= ((c & 0x7f) as u64) << shift;
            if c & 0x80 == 0 {
                return Ok(out);
            }
            shift += 7;
            if shift > 63 {
                return Err(bad("varuint 溢出"));
            }
        }
    }
    fn tag(&mut self) -> io::Result<(u64, u8)> {
        let x = self.varuint()?;
        Ok((x >> 4, (x & 0xf) as u8))
    }
    fn id(&mut self) -> io::Result<Id> {
        let a = self.u8()?;
        Ok(Id(a, self.varuint()?))
    }
    fn sub(&mut self) -> io::Result<R<'a>> {
        let n = self.u32()? as usize;
        self.ok(n)?;
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(R::new(s))
    }
    /// `varuint len + u8 is_ascii + utf8`（`tagged_block_writer.py:174-182`）。
    fn string(&mut self) -> io::Result<String> {
        let n = self.varuint()? as usize;
        let _ascii = self.u8()?;
        self.ok(n)?;
        let s = String::from_utf8(self.b[self.p..self.p + n].to_vec())
            .map_err(|_| bad("字符串不是 UTF-8"))?;
        self.p += n;
        Ok(s)
    }
}

#[derive(Default)]
struct W {
    b: Vec<u8>,
}

impl W {
    fn u8(&mut self, v: u8) {
        self.b.push(v);
    }
    fn u32(&mut self, v: u32) {
        self.b.extend_from_slice(&v.to_le_bytes());
    }
    fn f32(&mut self, v: f32) {
        self.b.extend_from_slice(&v.to_bits().to_le_bytes());
    }
    fn f64(&mut self, v: f64) {
        self.b.extend_from_slice(&v.to_bits().to_le_bytes());
    }
    fn varuint(&mut self, mut v: u64) {
        loop {
            let mut byte = (v & 0x7f) as u8;
            v >>= 7;
            if v != 0 {
                byte |= 0x80;
            }
            self.b.push(byte);
            if v == 0 {
                return;
            }
        }
    }
    fn tag(&mut self, index: u64, ty: u8) {
        self.varuint((index << 4) | ty as u64);
    }
    fn id(&mut self, index: u64, id: Id) {
        self.tag(index, TAG_ID);
        self.raw_id(id);
    }
    fn raw_id(&mut self, id: Id) {
        self.u8(id.0);
        self.varuint(id.1);
    }
    fn sub(&mut self, index: u64, f: impl FnOnce(&mut W)) {
        let mut inner = W::default();
        f(&mut inner);
        self.tag(index, TAG_LEN4);
        self.u32(inner.b.len() as u32);
        self.b.extend_from_slice(&inner.b);
    }
    fn string(&mut self, index: u64, s: &str) {
        self.sub(index, |w| {
            w.varuint(s.len() as u64);
            w.u8(1); // is_ascii，官方恒写 1
            w.b.extend_from_slice(s.as_bytes());
        });
    }
}

// ---------------------------------------------------------------- 解析

/// 解析一个文本项子块的内容（tag(0,LEN4) 的壳由调用方剥掉）。
fn parse_item_body(body: &[u8]) -> io::Result<TextItem> {
    let mut it = R::new(body);
    let mut id = END_MARKER;
    let (mut left, mut right) = (END_MARKER, END_MARKER);
    let mut deleted = 0;
    let mut value = TextValue::Str(String::new());
    while !it.eof() {
        let (idx, ty) = it.tag()?;
        match (idx, ty) {
            (2, TAG_ID) => id = it.id()?,
            (3, TAG_ID) => left = it.id()?,
            (4, TAG_ID) => right = it.id()?,
            (5, TAG_BYTE4) => deleted = it.u32()?,
            (6, TAG_LEN4) => {
                let mut sb = it.sub()?;
                let s = sb.string()?;
                if sb.eof() {
                    value = TextValue::Str(s);
                } else {
                    // 空串 + tag(2,Byte4) = 行内格式码（`scene_stream.py:681-691`）
                    let (i2, t2) = sb.tag()?;
                    if (i2, t2) != (2, TAG_BYTE4) {
                        return Err(bad("字符串子块里的格式码不是 tag(2,Byte4)"));
                    }
                    value = TextValue::Format(sb.u32()? as u8);
                }
            }
            _ => return Err(bad("文本项里有未知 tag")),
        }
    }
    Ok(TextItem {
        id,
        left,
        right,
        deleted,
        value,
    })
}

fn parse_format(r: &mut R) -> io::Result<TextFormat> {
    // 裸 crdt id（没有 tag）→ tag(1,ID) 时间戳 → tag(2,LEN4){0x11, code}
    let key = r.id()?;
    let (i, t) = r.tag()?;
    if (i, t) != (1, TAG_ID) {
        return Err(bad("样式项缺少 tag(1,ID)"));
    }
    let timestamp = r.id()?;
    let (i, t) = r.tag()?;
    if (i, t) != (2, TAG_LEN4) {
        return Err(bad("样式项缺少 tag(2,LEN4)"));
    }
    let mut sb = r.sub()?;
    if sb.u8()? != 0x11 {
        return Err(bad("样式项前缀不是 0x11"));
    }
    let style = sb.u8()?;
    Ok(TextFormat {
        key,
        timestamp,
        style,
    })
}

/// 解析 RootText 块体（形状见模块头）。
fn parse_root_text(body: &[u8]) -> io::Result<RmText> {
    let mut r = R::new(body);
    let (i, t) = r.tag()?;
    if (i, t) != (1, TAG_ID) {
        return Err(bad("RootText 缺少 tag(1,ID)"));
    }
    let block_id = r.id()?;

    let (i, t) = r.tag()?;
    if (i, t) != (2, TAG_LEN4) {
        return Err(bad("RootText 缺少文本容器 tag(2,LEN4)"));
    }
    let mut cont = r.sub()?;

    // 文本项：两层 tag(1,LEN4)
    let (i, t) = cont.tag()?;
    if (i, t) != (1, TAG_LEN4) {
        return Err(bad("缺少文本项外层 tag(1,LEN4)"));
    }
    let mut outer = cont.sub()?;
    let (i, t) = outer.tag()?;
    if (i, t) != (1, TAG_LEN4) {
        return Err(bad("缺少文本项内层 tag(1,LEN4)"));
    }
    let mut items_r = outer.sub()?;
    let n = items_r.varuint()? as usize;
    let mut items = Vec::with_capacity(n);
    for _ in 0..n {
        let (idx, ty) = items_r.tag()?;
        if (idx, ty) != (0, TAG_LEN4) {
            return Err(bad("文本项不是 tag(0,LEN4)"));
        }
        let sub = items_r.sub()?;
        items.push(parse_item_body(sub.b)?);
    }

    // 样式：tag(2,LEN4) → tag(1,LEN4) → count + 项
    let (i, t) = cont.tag()?;
    if (i, t) != (2, TAG_LEN4) {
        return Err(bad("缺少样式容器 tag(2,LEN4)"));
    }
    let mut styles_outer = cont.sub()?;
    let (i, t) = styles_outer.tag()?;
    if (i, t) != (1, TAG_LEN4) {
        return Err(bad("缺少样式内层 tag(1,LEN4)"));
    }
    let mut styles_r = styles_outer.sub()?;
    let n = styles_r.varuint()? as usize;
    let mut formats = Vec::with_capacity(n);
    for _ in 0..n {
        formats.push(parse_format(&mut styles_r)?);
    }

    // 位置（16 字节 = 2×f64）与宽度
    let (i, t) = r.tag()?;
    if (i, t) != (3, TAG_LEN4) {
        return Err(bad("缺少位置 tag(3,LEN4)"));
    }
    let mut pos = r.sub()?;
    let pos_x = pos.f64()?;
    let pos_y = pos.f64()?;
    let (i, t) = r.tag()?;
    if (i, t) != (4, TAG_BYTE4) {
        return Err(bad("缺少宽度 tag(4,Byte4)"));
    }
    let width = r.f32()?;

    Ok(RmText {
        block_id,
        pos_x,
        pos_y,
        width,
        items,
        formats,
    })
}

/// 从一个 `.rm` 里读文字层。没有 RootText 块 = `Ok(None)`（纯笔迹页）。
pub fn parse_text(bytes: &[u8]) -> io::Result<Option<RmText>> {
    if bytes.len() < HEADER.len() || &bytes[..HEADER.len()] != HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "不是 .rm v6 文件（头不匹配）",
        ));
    }
    let mut p = HEADER.len();
    while p + 8 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[p..p + 4].try_into().unwrap()) as usize;
        let ty = bytes[p + 7];
        let start = p + 8;
        if start + len > bytes.len() {
            return Err(bad("块长度越界"));
        }
        if ty == BT_ROOT_TEXT {
            return parse_root_text(&bytes[start..start + len]).map(Some);
        }
        p = start + len;
    }
    Ok(None)
}

// ---------------------------------------------------------------- 段落 / CRDT 排序

#[derive(Clone, Copy, PartialEq)]
enum CharKind {
    Ch(char),
    Fmt,
    Deleted,
}

struct CharItem {
    id: Id,
    left: Id,
    right: Id,
    kind: CharKind,
}

/// 把「一段字符串一个 id」展开成「一个字符一个 id」（`text.py:20-58`）：
/// item 的 id 是首字符，后面隐含 +1。
fn expand(items: &[TextItem]) -> Vec<CharItem> {
    let mut out = Vec::new();
    for it in items {
        match &it.value {
            TextValue::Format(_) => out.push(CharItem {
                id: it.id,
                left: it.left,
                right: it.right,
                kind: CharKind::Fmt,
            }),
            TextValue::Str(_) if it.deleted > 0 => {
                let n = it.deleted as usize;
                let (mut id, mut left) = (it.id, it.left);
                for k in 0..n {
                    let right = if k + 1 < n { Id(id.0, id.1 + 1) } else { it.right };
                    out.push(CharItem {
                        id,
                        left,
                        right,
                        kind: CharKind::Deleted,
                    });
                    left = id;
                    id = Id(id.0, id.1 + 1);
                }
            }
            TextValue::Str(s) => {
                let chars: Vec<char> = s.chars().collect();
                let (mut id, mut left) = (it.id, it.left);
                for (k, c) in chars.iter().enumerate() {
                    let right = if k + 1 < chars.len() {
                        Id(id.0, id.1 + 1)
                    } else {
                        it.right
                    };
                    out.push(CharItem {
                        id,
                        left,
                        right,
                        kind: CharKind::Ch(*c),
                    });
                    left = id;
                    id = Id(id.0, id.1 + 1);
                }
            }
        }
    }
    out
}

/// 按 left/right 偏序排出顺序（`crdt_sequence.py:99-156`）：Kahn 拓扑排序 + 堆，
/// 同时就绪时 key = `(1, -part1, part2)`（author id 大的排前面）。
fn toposort(chars: &[CharItem]) -> Option<Vec<usize>> {
    let n = chars.len();
    // id → 下标：排序后二分，避免给 Id 加 Hash
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&i| (chars[i].id.0, chars[i].id.1));
    let keys: Vec<(u8, u64)> = order.iter().map(|&i| (chars[i].id.0, chars[i].id.1)).collect();
    let find = |id: Id| -> Option<usize> {
        keys.binary_search(&(id.0, id.1)).ok().map(|j| order[j])
    };

    // 节点编号：0 = 起点、1 = 终点、2+i = 第 i 个字符项
    let nodes = n + 2;
    let mut indeg = vec![0usize; nodes];
    let mut dep: Vec<Vec<usize>> = vec![Vec::new(); nodes];
    for (i, c) in chars.iter().enumerate() {
        let l = find(c.left).map(|j| j + 2).unwrap_or(0);
        let r = find(c.right).map(|j| j + 2).unwrap_or(1);
        indeg[i + 2] += 1;
        dep[l].push(i + 2);
        indeg[r] += 1;
        dep[i + 2].push(r);
    }

    let key = |node: usize| -> (u8, i16, u64) {
        if node == 0 {
            (0, 0, 0)
        } else if node == 1 {
            (2, 0, 0)
        } else {
            let id = chars[node - 2].id;
            (1, -(id.0 as i16), id.1)
        }
    };
    let mut heap: BinaryHeap<Reverse<(u8, i16, u64, usize)>> = BinaryHeap::new();
    for node in 0..nodes {
        if indeg[node] == 0 {
            let k = key(node);
            heap.push(Reverse((k.0, k.1, k.2, node)));
        }
    }
    let mut out = Vec::with_capacity(n);
    while let Some(Reverse((_, _, _, node))) = heap.pop() {
        if node >= 2 {
            out.push(node - 2);
        }
        if node == 1 {
            break;
        }
        for &d in &dep[node] {
            indeg[d] -= 1;
            if indeg[d] == 0 {
                let k = key(d);
                heap.push(Reverse((k.0, k.1, k.2, d)));
            }
        }
    }
    if out.len() != n {
        return None; // 有环
    }
    Some(out)
}

impl RmText {
    /// 排好序的段落。样式键 = 该段前那个换行字符的 id（首段是 (0,0)），
    /// 与 `text.py:134-165` 一致。
    pub fn paragraphs(&self) -> io::Result<Vec<Paragraph>> {
        let chars = expand(&self.items);
        let order = toposort(&chars).ok_or_else(|| bad("文字 CRDT 链有环"))?;
        let mut out = Vec::new();
        let mut i = 0;
        while i < order.len() {
            let mut start = END_MARKER;
            if chars[order[i]].kind == CharKind::Ch('\n') {
                start = chars[order[i]].id;
                i += 1;
            }
            let mut text = String::new();
            while i < order.len() {
                match chars[order[i]].kind {
                    CharKind::Ch('\n') => break,
                    CharKind::Ch(c) => {
                        text.push(c);
                        i += 1;
                    }
                    CharKind::Deleted | CharKind::Fmt => i += 1,
                }
            }
            let style = self
                .formats
                .iter()
                .find(|f| f.key == start)
                .map(|f| ParagraphStyle(f.style))
                .unwrap_or_default();
            out.push(Paragraph { style, text });
        }
        Ok(out)
    }

    /// 新建一个单 item 的纯文本文档，id/时间戳/位置照抄官方默认
    /// （`scene_stream.py:985-993`）。文本里可以有 `\n`。
    pub fn plain(text: impl Into<String>) -> RmText {
        RmText {
            items: vec![TextItem {
                id: Id(1, 16),
                left: END_MARKER,
                right: END_MARKER,
                deleted: 0,
                value: TextValue::Str(text.into()),
            }],
            formats: vec![TextFormat {
                key: END_MARKER,
                timestamp: Id(1, 15),
                style: ParagraphStyle::PLAIN.0,
            }],
            ..RmText::default()
        }
    }

    /// 由段落生成文档：整篇放进**一个** text item，段间放 `\n`；
    /// 第 k 段（k>0）的样式键 = 它前面那个换行的隐含 id。
    pub fn from_paragraphs(paras: &[Paragraph]) -> RmText {
        if paras.is_empty() {
            return RmText::default();
        }
        let mut text = String::new();
        let mut formats = Vec::new();
        for (k, p) in paras.iter().enumerate() {
            if k > 0 {
                let key = Id(1, 16 + text.chars().count() as u64);
                formats.push(TextFormat {
                    key,
                    timestamp: Id(1, 15 + k as u64),
                    style: p.style.0,
                });
                text.push('\n');
            } else {
                formats.push(TextFormat {
                    key: END_MARKER,
                    timestamp: Id(1, 15),
                    style: p.style.0,
                });
            }
            text.push_str(&p.text);
        }
        RmText {
            items: vec![TextItem {
                id: Id(1, 16),
                left: END_MARKER,
                right: END_MARKER,
                deleted: 0,
                value: TextValue::Str(text),
            }],
            formats,
            ..RmText::default()
        }
    }

    /// 字符总数（含换行）+1，官方 PageInfo 的 text_chars_count（`scene_stream.py:958`）。
    fn char_count(&self) -> u32 {
        match self.paragraphs() {
            Ok(ps) if !ps.is_empty() => {
                let n: usize = ps.iter().map(|p| p.text.chars().count()).sum();
                (n + ps.len()) as u32 // n + (段数-1) 个换行 + 1
            }
            _ => 1,
        }
    }

    fn line_count(&self) -> u32 {
        self.paragraphs().map(|ps| ps.len() as u32).unwrap_or(0)
    }
}

// ---------------------------------------------------------------- 序列化

fn write_item(w: &mut W, it: &TextItem) {
    w.sub(0, |w| {
        w.id(2, it.id);
        w.id(3, it.left);
        w.id(4, it.right);
        w.tag(5, TAG_BYTE4);
        w.u32(it.deleted);
        match &it.value {
            // 删除段的值是空串：官方不写 tag(6)
            TextValue::Str(s) if s.is_empty() => {}
            TextValue::Str(s) => w.string(6, s),
            TextValue::Format(code) => w.sub(6, |w| {
                w.varuint(0);
                w.u8(1);
                w.tag(2, TAG_BYTE4);
                w.u32(*code as u32);
            }),
        }
    });
}

fn write_format(w: &mut W, f: &TextFormat) {
    w.raw_id(f.key);
    w.id(1, f.timestamp);
    w.sub(2, |w| {
        w.u8(0x11);
        w.u8(f.style);
    });
}

/// 整个 RootText 块（含 8 字节块头）。
fn write_root_text_block(doc: &RmText) -> Vec<u8> {
    let mut w = W::default();
    w.id(1, doc.block_id);
    w.sub(2, |w| {
        w.sub(1, |w| {
            w.sub(1, |w| {
                w.varuint(doc.items.len() as u64);
                for it in &doc.items {
                    write_item(w, it);
                }
            });
        });
        w.sub(2, |w| {
            w.sub(1, |w| {
                w.varuint(doc.formats.len() as u64);
                for f in &doc.formats {
                    write_format(w, f);
                }
            });
        });
    });
    w.sub(3, |w| {
        w.f64(doc.pos_x);
        w.f64(doc.pos_y);
    });
    w.tag(4, TAG_BYTE4);
    w.f32(doc.width);

    let mut out = W::default();
    out.u32(w.b.len() as u32);
    out.u8(0); // unk
    out.u8(1); // min_version
    out.u8(1); // current_version
    out.u8(BT_ROOT_TEXT);
    out.b.extend_from_slice(&w.b);
    out.b
}

/// 一个块的原始字段。
struct Block {
    ty: u8,
    unk: u8,
    min: u8,
    cur: u8,
    body: Vec<u8>,
}

fn split_blocks(bytes: &[u8]) -> io::Result<(Vec<Block>, Vec<u8>)> {
    if bytes.len() < HEADER.len() || &bytes[..HEADER.len()] != HEADER {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "不是 .rm v6 文件（头不匹配）",
        ));
    }
    let mut out = Vec::new();
    let mut p = HEADER.len();
    while p + 8 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[p..p + 4].try_into().unwrap()) as usize;
        let start = p + 8;
        if start + len > bytes.len() {
            return Err(bad("块长度越界"));
        }
        out.push(Block {
            unk: bytes[p + 4],
            min: bytes[p + 5],
            cur: bytes[p + 6],
            ty: bytes[p + 7],
            body: bytes[start..start + len].to_vec(),
        });
        p = start + len;
    }
    Ok((out, bytes[p..].to_vec()))
}

fn emit(blocks: &[Block], tail: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(HEADER);
    for b in blocks {
        out.extend_from_slice(&(b.body.len() as u32).to_le_bytes());
        out.push(b.unk);
        out.push(b.min);
        out.push(b.cur);
        out.push(b.ty);
        out.extend_from_slice(&b.body);
    }
    out.extend_from_slice(tail);
    out
}

/// 改写 PageInfo 的 text_chars_count / text_lines_count（tag 3 / tag 4）。
/// 只认 Byte1/Byte4 形状的标签，遇到别的就原样不动（不猜）。
fn patch_page_info(body: &[u8], chars: u32, lines: u32) -> Option<Vec<u8>> {
    let mut r = R::new(body);
    let mut fields: Vec<(u64, u8, Vec<u8>)> = Vec::new();
    while !r.eof() {
        let (idx, ty) = r.tag().ok()?;
        match ty {
            TAG_BYTE4 => {
                r.ok(4).ok()?;
                let v = body[r.p..r.p + 4].to_vec();
                r.p += 4;
                if idx == 3 {
                    fields.push((idx, ty, chars.to_le_bytes().to_vec()));
                } else if idx == 4 {
                    fields.push((idx, ty, lines.to_le_bytes().to_vec()));
                } else {
                    fields.push((idx, ty, v));
                }
            }
            TAG_BYTE1 => {
                r.ok(1).ok()?;
                let v = body[r.p..r.p + 1].to_vec();
                r.p += 1;
                fields.push((idx, ty, v));
            }
            _ => return None,
        }
    }
    let mut w = W::default();
    for (idx, ty, v) in fields {
        w.tag(idx, ty);
        w.b.extend_from_slice(&v);
    }
    Some(w.b)
}

/// 把 `doc` 写进一个 `.rm`：有 RootText 块就**就地替换**，没有就插在 SceneTree(0x01)
/// 之后（两块官方样本的顺序都是 …0x01, 0x07, 0x02…），并顺手修正 PageInfo 的
/// 字符/行计数。`base` 就是原文件字节（`RmPage::serialize()` 的结果也行）。
pub fn serialize_text(base: &[u8], doc: &RmText) -> io::Result<Vec<u8>> {
    let (mut blocks, tail) = split_blocks(base)?;
    let new_block = write_root_text_block(doc);
    let (ty, body) = (new_block[7], new_block[8..].to_vec());

    let mut replaced = false;
    for b in blocks.iter_mut() {
        if b.ty == BT_ROOT_TEXT {
            b.ty = ty;
            b.unk = 0;
            b.min = 1;
            b.cur = 1;
            b.body = body.clone();
            replaced = true;
            break;
        }
    }
    if !replaced {
        let at = blocks
            .iter()
            .rposition(|b| b.ty == 0x01)
            .map(|i| i + 1)
            .or_else(|| blocks.iter().rposition(|b| b.ty == BT_PAGE_INFO).map(|i| i + 1))
            .unwrap_or(blocks.len());
        blocks.insert(
            at,
            Block {
                ty,
                unk: 0,
                min: 1,
                cur: 1,
                body,
            },
        );
    }

    let (chars, lines) = (doc.char_count(), doc.line_count());
    for b in blocks.iter_mut() {
        if b.ty == BT_PAGE_INFO {
            if let Some(new) = patch_page_info(&b.body, chars, lines) {
                b.body = new;
            }
        }
    }
    Ok(emit(&blocks, &tail))
}

/// 把一页的文字层整块拿掉（文字清空时用）。
pub fn strip_text(base: &[u8]) -> io::Result<Vec<u8>> {
    let (mut blocks, tail) = split_blocks(base)?;
    blocks.retain(|b| b.ty != BT_ROOT_TEXT);
    Ok(emit(&blocks, &tail))
}

// ---------------------------------------------------------------- 与 App / sidecar 的桥

/// 页面上的一段文字（给 UI 与 sidecar 用）。
///
/// `x/y/size` 是**渲染**参数：官方文件里只有整篇 `pos_x/pos_y/width` 和每段样式，
/// 没有逐段坐标与字号。`style` 会原样写进/读出官方格式。
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SceneText {
    pub x: f32,
    pub y: f32,
    pub text: String,
    pub size: f32,
    /// 官方 `color`；sidecar 与官方 RootText 都没有逐段颜色，缺省 0。
    #[serde(default)]
    pub color: u8,
    /// 段落样式（sidecar 里存成数字；老文件没这列时按 PLAIN）。
    #[serde(default)]
    pub style: ParagraphStyle,
}

impl SceneText {
    pub fn new(x: f32, y: f32, size: f32, text: impl Into<String>) -> SceneText {
        SceneText {
            x,
            y,
            size,
            text: text.into(),
            color: 0,
            style: ParagraphStyle::PLAIN,
        }
    }

    pub fn to_item(&self) -> SidecarText {
        SidecarText {
            x: self.x,
            y: self.y,
            size: self.size,
            text: self.text.clone(),
        }
    }
}

impl From<&SidecarText> for SceneText {
    fn from(t: &SidecarText) -> SceneText {
        SceneText::new(t.x, t.y, t.size, t.text.clone())
    }
}

impl RmText {
    /// 段落 → App 条目。`x/y` 用文档的 `pos_x/pos_y`（整篇只有一个位置），
    /// `size` 由样式推（文件里没有字号）。
    pub fn to_scene_texts(&self) -> Vec<SceneText> {
        match self.paragraphs() {
            Ok(ps) => ps
                .into_iter()
                .map(|p| SceneText {
                    x: self.pos_x as f32,
                    y: self.pos_y as f32,
                    size: p.style.default_size(),
                    text: p.text,
                    color: 0,
                    style: p.style,
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }

    /// App 条目 → 文档。位置取第一条；宽度用官方默认 936（`scene_stream.py:986`）。
    pub fn from_scene_texts(texts: &[SceneText]) -> RmText {
        let paras: Vec<Paragraph> = texts
            .iter()
            .map(|t| Paragraph {
                style: t.style,
                text: t.text.clone(),
            })
            .collect();
        let mut doc = RmText::from_paragraphs(&paras);
        if let Some(first) = texts.first() {
            doc.pos_x = first.x as f64;
            doc.pos_y = first.y as f64;
        }
        doc
    }
}

/// 便捷：读一页的所有段落（每段一条 `SceneText`）。
pub fn parse_texts(bytes: &[u8]) -> io::Result<Vec<SceneText>> {
    Ok(parse_text(bytes)?.map(|t| t.to_scene_texts()).unwrap_or_default())
}

/// 便捷：把 `SceneText` 列表写进 `.rm`。
pub fn serialize_texts(base: &[u8], texts: &[SceneText]) -> io::Result<Vec<u8>> {
    if texts.is_empty() {
        return strip_text(base);
    }
    serialize_text(base, &RmText::from_scene_texts(texts))
}

// ---------------------------------------------------------------- 存储后端

/// 后端种类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreKind {
    Sidecar,
    Official,
}

impl StoreKind {
    pub fn label(self) -> &'static str {
        match self {
            StoreKind::Sidecar => "sidecar-json",
            StoreKind::Official => "rm-root-text",
        }
    }

    /// 能不能落盘。两种现在都能落，但官方后端**没在真机验证过**。
    pub fn implemented(self) -> bool {
        true
    }

    /// 写出的文件是否在真机上打开验证过。sidecar 是 App 自己的格式，算已验证。
    pub fn verified_on_device(self) -> bool {
        matches!(self, StoreKind::Sidecar)
    }
}

/// 默认后端：保持现状（官方后端等真机验证过再切）。
pub const DEFAULT_KIND: StoreKind = StoreKind::Sidecar;

/// 文字存储后端。key 是**页 id**（`texts-<pageid>.json` / `<pageid>.rm` 用的那个）。
pub trait TextStore {
    fn kind(&self) -> StoreKind;

    /// 读一页的文字。没有存储过 = 空（不报错）；文件坏了 = Err（不静默丢字）。
    fn load(&self, page_id: &str) -> io::Result<Vec<SceneText>>;

    /// 写一页的文字。空列表 = 清掉这份存储。
    fn save(&self, page_id: &str, texts: &[SceneText]) -> io::Result<()>;
}

/// sidecar 后端：`<dir>/texts-<pageid>.json`，一个 JSON 数组。
///
/// 与 `library.rs:235/245` 的读写**字节兼容**：它写的文件我们能读（没有 `color`/`style`
/// 列，走 serde default），我们写的它能读（`serde_json` 默认忽略多余字段）。
pub struct SidecarStore {
    dir: PathBuf,
}

impl SidecarStore {
    pub fn new(dir: impl Into<PathBuf>) -> SidecarStore {
        SidecarStore { dir: dir.into() }
    }

    /// 按库根 + 文档 uuid 建（= `library.rs:232` 的 `root/uuid`）。
    pub fn for_doc(root: &Path, uuid: &str) -> SidecarStore {
        SidecarStore::new(root.join(uuid))
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path_for(&self, page_id: &str) -> PathBuf {
        self.dir.join(format!("texts-{page_id}.json"))
    }
}

impl TextStore for SidecarStore {
    fn kind(&self) -> StoreKind {
        StoreKind::Sidecar
    }

    fn load(&self, page_id: &str) -> io::Result<Vec<SceneText>> {
        let p = self.path_for(page_id);
        let s = match fs::read_to_string(&p) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        serde_json::from_str(&s).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: 文字 sidecar 不是合法 JSON（{e}）", p.display()),
            )
        })
    }

    fn save(&self, page_id: &str, texts: &[SceneText]) -> io::Result<()> {
        let p = self.path_for(page_id);
        if texts.is_empty() {
            match fs::remove_file(&p) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
                Err(e) => return Err(e),
            }
        }
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }
        let body = serde_json::to_vec_pretty(texts)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        fs::write(&p, body)
    }
}

/// 官方后端：直接读写 `<dir>/<pageid>.rm` 里的 RootText 块。
///
/// ⚠ 没在真机上验证过（见模块头「还没验证的」）。证据强度：两个真机样本可逐字节复现。
pub struct OfficialStore {
    dir: PathBuf,
}

impl OfficialStore {
    pub fn new(dir: impl Into<PathBuf>) -> OfficialStore {
        OfficialStore { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn rm_path(&self, page_id: &str) -> PathBuf {
        self.dir.join(format!("{page_id}.rm"))
    }
}

impl Default for OfficialStore {
    fn default() -> OfficialStore {
        OfficialStore::new(".")
    }
}

impl TextStore for OfficialStore {
    fn kind(&self) -> StoreKind {
        StoreKind::Official
    }

    fn load(&self, page_id: &str) -> io::Result<Vec<SceneText>> {
        let p = self.rm_path(page_id);
        let bytes = match fs::read(&p) {
            Ok(b) => b,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        Ok(parse_text(&bytes)?.map(|t| t.to_scene_texts()).unwrap_or_default())
    }

    fn save(&self, page_id: &str, texts: &[SceneText]) -> io::Result<()> {
        let p = self.rm_path(page_id);
        let base = fs::read(&p).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("{}: 没有这一页的 .rm，不能凭空造页（{e}）", p.display()),
            )
        })?;
        fs::write(&p, serialize_texts(&base, texts)?)
    }
}

/// 按后端种类开一个 store。`dir` 是文档目录（`root/uuid`）。
pub fn open(kind: StoreKind, dir: &Path) -> Box<dyn TextStore> {
    match kind {
        StoreKind::Sidecar => Box::new(SidecarStore::new(dir)),
        StoreKind::Official => Box::new(OfficialStore::new(dir)),
    }
}

/// 一个能用的 store（当前 = sidecar）。
pub fn open_default(dir: &Path) -> Box<dyn TextStore> {
    open(DEFAULT_KIND, dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::rm::RmPage;
    use std::fs;

    // 两份真实设备文件（rmscene tests/data，见模块头 URL）。
    const NORMAL_AB_HEX: &str = "\
72654d61726b61626c65202e6c696e6573206669\
6c652c2076657273696f6e3d36202020202020202020201900000000010109010c13000000109fa55b4943c95c2bb4553682f6948906010005000000\
000101001f01012101140000000000010a140100000024000000003403000000440100000010000000000101011f000b2f000031014c030000001f\
000160000000000101071f00002c3e0000001c220000001c1d000000010c170000002f01103f00004f000054000000006c04000000020141422c\
120000001c0d0000000100001f010f2c0200000011013c100000000000000000407dc00000000000406d404400006a441c000000000101021f00\
012c0a0000001f00002c0200000000013c050000001f0000210123000000000101021f000b2c110000001f000c2c0900000007014c6179657220\
313c050000001f000021011a000000000101041f00012f000d3f00004f000054000000006c04000000022f000b";

    const BOLD_HEADING_BULLET_HEX: &str = "\
72654d61726b61626c65202e6c696e6573206669\
6c652c2076657273696f6e3d36202020202020202020203100000000010109020c13000000109fa55b4943c95c2bb4553682f694890602000c1300\
0000100996d61dd9bc5ea5a184a3a329bf9956010005000000000101001f01012100140000000000010a140f0000002401000000342b0000004404\
00000010000000000101011f000b2f000031014c030000001f00015b010000000101071f00002c390100001cf90000001cf4000000080c16000000\
2f01113f00004f000054000000006c030000000101410c1e0000002f023c3f01114f011254000000006c0b00000009010a6e6577206c696e650c17\
0000002f01123f01114f000054000000006c0400000002010a420c190000002f01163f01134f011454000000006c060000000401206973200c0e00\
00002f011a3f01194f0114540a0000000c160000002f023b3f01234f012454000000006c030000000101610c2c0000002f01243f01234f01145400\
0000006c190000001701206c6574746572206f662074686520616c7068616265740c170000002f01143f01134f000054000000006c040000000201\
0a432c360000001c310000000401121f01162c020000001104023c1f02452c02000000110201141f02162c02000000110100001f023b2c02000000\
11033c100000000000000000407dc00000000000406d404400006a441c000000000101021f00012c0a0000001f00002c0200000000013c05000000\
1f0000210123000000000101021f000b2c110000001f000c2c0900000007014c6179657220313c050000001f000021011a000000000101041f0001\
2f000d3f00004f000054000000006c04000000022f000b";

    fn unhex(s: &str) -> Vec<u8> {
        let b = s.as_bytes();
        (0..b.len() / 2)
            .map(|i| {
                let hi = (b[2 * i] as char).to_digit(16).unwrap() as u8;
                let lo = (b[2 * i + 1] as char).to_digit(16).unwrap() as u8;
                hi << 4 | lo
            })
            .collect()
    }

    fn normal_ab() -> Vec<u8> {
        unhex(NORMAL_AB_HEX)
    }
    fn bold_heading_bullet() -> Vec<u8> {
        unhex(BOLD_HEADING_BULLET_HEX)
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("rmtext-{}-{}", std::process::id(), tag));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }

    // ------------------------------------------------ 官方 RootText

    #[test]
    fn parse_normal_ab() {
        let doc = parse_text(&normal_ab()).unwrap().unwrap();
        assert_eq!(doc.block_id, END_MARKER);
        assert_eq!(doc.pos_x, -468.0);
        assert_eq!(doc.pos_y, 234.0);
        assert_eq!(doc.width, 936.0);
        assert_eq!(doc.items.len(), 1);
        assert_eq!(doc.items[0].id, Id(1, 16));
        assert_eq!(doc.formats[0].key, END_MARKER);
        assert_eq!(doc.formats[0].style, 1);
        assert_eq!(
            doc.paragraphs().unwrap(),
            vec![Paragraph {
                style: ParagraphStyle::PLAIN,
                text: "AB".into()
            }]
        );
    }

    #[test]
    fn parse_bold_heading_bullet_styles_and_text() {
        // 期望值来自 rmscene 自己的测试 tests/test_text_files.py:52-59
        let doc = parse_text(&bold_heading_bullet()).unwrap().unwrap();
        assert_eq!(
            doc.paragraphs().unwrap(),
            vec![
                Paragraph { style: ParagraphStyle::BOLD, text: "A".into() },
                Paragraph { style: ParagraphStyle::HEADING, text: "new line".into() },
                Paragraph {
                    style: ParagraphStyle::BULLET,
                    text: "B is a letter of the alphabet".into()
                },
                Paragraph { style: ParagraphStyle::PLAIN, text: "C".into() },
            ]
        );
    }

    #[test]
    fn roundtrip_normal_ab_byte_exact() {
        let base = normal_ab();
        let doc = parse_text(&base).unwrap().unwrap();
        assert_eq!(serialize_text(&base, &doc).unwrap(), base);
    }

    #[test]
    fn roundtrip_bold_heading_bullet_byte_exact() {
        let base = bold_heading_bullet();
        let doc = parse_text(&base).unwrap().unwrap();
        assert_eq!(
            serialize_text(&base, &doc).unwrap(),
            base,
            "含 4 段/3 种样式/1 个 tombstone 的文件也要逐字节复现"
        );
    }

    #[test]
    fn plain_ab_block_matches_device_bytes() {
        // 官方默认参数写出来的整块，应该与 Normal_AB.rm 里那一块一模一样
        let sample = normal_ab();
        let block = write_root_text_block(&RmText::plain("AB"));
        let at = sample.windows(block.len()).position(|w| w == block);
        assert!(at.is_some(), "生成的 RootText 块没能在真机文件里逐字节对上");
        let lead = at.unwrap();
        assert_eq!(sample[lead - 1], 0x01, "RootText 前一个块应是 SceneTree");
        assert_eq!(&sample[lead..lead + 8 + 96], &block[..], "整块相等");
    }

    #[test]
    fn from_paragraphs_then_parse_keeps_text_and_styles() {
        let paras = vec![
            Paragraph { style: ParagraphStyle::HEADING, text: "标题".into() },
            Paragraph { style: ParagraphStyle::PLAIN, text: "正文 line".into() },
            Paragraph { style: ParagraphStyle::BULLET, text: "项".into() },
        ];
        let doc = RmText::from_paragraphs(&paras);
        let out = serialize_text(&normal_ab(), &doc).unwrap();
        let back = parse_text(&out).unwrap().unwrap();
        assert_eq!(back.paragraphs().unwrap(), paras);
        assert_eq!(back.pos_x, -468.0);
    }

    #[test]
    fn unicode_text_uses_one_id_per_char() {
        let out = serialize_text(&normal_ab(), &RmText::plain("中文")).unwrap();
        let back = parse_text(&out).unwrap().unwrap();
        assert_eq!(back.paragraphs().unwrap()[0].text, "中文");
        assert_eq!(back.items[0].id, Id(1, 16));
    }

    #[test]
    fn page_info_counts_are_updated() {
        // 先用现有 RmPage 造纯笔迹页（PageInfo 计数是 0），再写文字进去
        let base = RmPage::default().serialize();
        let texts = vec![
            SceneText::new(-468.0, 234.0, 25.0, "AB"),
            SceneText::new(-468.0, 234.0, 25.0, "C"),
        ];
        let out = serialize_texts(&base, &texts).unwrap();
        let (blocks, _) = split_blocks(&out).unwrap();
        let pi = blocks.iter().find(|b| b.ty == BT_PAGE_INFO).unwrap();
        let mut r = R::new(&pi.body);
        let mut seen = Vec::new();
        while !r.eof() {
            let (idx, ty) = r.tag().unwrap();
            assert_eq!(ty, TAG_BYTE4);
            let v = r.u32().unwrap();
            if idx == 3 || idx == 4 {
                seen.push((idx, v));
            }
        }
        assert_eq!(seen, vec![(3, 5), (4, 2)]);
    }

    #[test]
    fn rm_page_parse_accepts_text_files() {
        // 官方文件里有 0x07 块，现有 RmPage::parse 必须照旧接受
        let page = RmPage::parse(&bold_heading_bullet()).unwrap();
        assert_eq!(page.layers.len(), 1);
        assert_eq!(page.layers[0].name, "Layer 1");
        assert_eq!(page.total_lines(), 0);

        // 我们自己写出来的文件也要能被 RmPage::parse 接受
        let base = RmPage::default().serialize();
        let out = serialize_texts(&base, &[SceneText::new(0.0, 0.0, 25.0, "hello")]).unwrap();
        assert_eq!(RmPage::parse(&out).unwrap().layers.len(), 1);
        assert_eq!(parse_texts(&out).unwrap()[0].text, "hello");
    }

    #[test]
    fn ink_only_page_has_no_text() {
        let base = RmPage::default().serialize();
        assert!(parse_text(&base).unwrap().is_none());
        assert!(parse_texts(&base).unwrap().is_empty());
        assert_eq!(strip_text(&base).unwrap(), base);
    }

    #[test]
    fn strip_then_write_puts_block_after_scene_tree() {
        let base = RmPage::default().serialize();
        let out = serialize_texts(&base, &[SceneText::new(1.0, 2.0, 25.0, "x")]).unwrap();
        let (blocks, _) = split_blocks(&out).unwrap();
        let text_at = blocks.iter().position(|b| b.ty == BT_ROOT_TEXT).unwrap();
        let tree_at = blocks.iter().rposition(|b| b.ty == 0x01).unwrap();
        assert_eq!(text_at, tree_at + 1, "RootText 应紧跟 SceneTree");
        // 清空文字后又回到纯笔迹
        let back = serialize_texts(&out, &[]).unwrap();
        assert_eq!(RmPage::parse(&back).unwrap().layers.len(), 1);
        assert!(parse_text(&back).unwrap().is_none());
    }

    #[test]
    fn malformed_text_block_is_an_error_not_silence() {
        // 位置子块的 tag 改坏
        let mut b = normal_ab();
        let at = b.windows(2).position(|w| w == [0x3c, 0x10]).unwrap();
        b[at] = 0x7c;
        assert!(parse_text(&b).is_err());
        // 块长度越界
        let mut c = normal_ab();
        c[141..145].copy_from_slice(&0xffffu32.to_le_bytes());
        assert!(parse_text(&c).is_err());
    }

    // ------------------------------------------------ sidecar 后端

    #[test]
    fn sidecar_roundtrip_keeps_every_field() {
        let d = tmpdir("roundtrip");
        let s = SidecarStore::new(&d);
        let v = vec![
            SceneText::new(100.0, 200.0, 25.0, "第一段"),
            SceneText::new(100.0, 240.0, 17.0, "second\nline"),
        ];
        s.save("page-a", &v).unwrap();
        assert_eq!(s.load("page-a").unwrap(), v);
        assert!(s.path_for("page-a").ends_with("texts-page-a.json"));
    }

    #[test]
    fn sidecar_missing_file_is_empty_not_error() {
        let d = tmpdir("missing");
        let s = SidecarStore::new(&d);
        assert_eq!(s.load("nope").unwrap(), Vec::new());
        let s2 = SidecarStore::new(d.join("no-such-doc"));
        assert_eq!(s2.load("nope").unwrap(), Vec::new());
    }

    #[test]
    fn sidecar_empty_save_removes_file() {
        let d = tmpdir("empty");
        let s = SidecarStore::new(&d);
        s.save("p", &[SceneText::new(0.0, 0.0, 25.0, "x")]).unwrap();
        assert!(s.path_for("p").exists());
        s.save("p", &[]).unwrap();
        assert!(!s.path_for("p").exists());
        assert_eq!(s.load("p").unwrap(), Vec::new());
    }

    #[test]
    fn sidecar_rejects_corrupt_json_instead_of_silently_dropping_text() {
        let d = tmpdir("corrupt");
        let s = SidecarStore::new(&d);
        fs::write(s.path_for("bad"), b"{not json").unwrap();
        let e = s.load("bad").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);
        assert!(e.to_string().contains("texts-bad.json"));
    }

    #[test]
    fn sidecar_reads_legacy_json_without_color_or_style() {
        // 正是 library.rs:253 写出来的形状（TextItem 没有 color/style 列）
        let d = tmpdir("legacy");
        let s = SidecarStore::new(&d);
        let legacy = r#"[{"x":10.0,"y":20.0,"size":25.0,"text":"旧数据"}]"#;
        fs::write(s.path_for("old"), legacy).unwrap();
        let v = s.load("old").unwrap();
        assert_eq!(v[0].text, "旧数据");
        assert_eq!(v[0].color, 0);
        assert_eq!(v[0].style, ParagraphStyle::PLAIN);
    }

    #[test]
    fn sidecar_output_is_readable_by_existing_text_item() {
        let d = tmpdir("compat");
        let s = SidecarStore::new(&d);
        s.save("c", &[SceneText::new(1.0, 2.0, 33.0, "兼容")]).unwrap();
        let raw = fs::read_to_string(s.path_for("c")).unwrap();
        let back: Vec<SidecarText> = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].text, "兼容");
        assert_eq!(back[0].size, 33.0);
    }

    #[test]
    fn text_item_conversion_roundtrips() {
        let it = SidecarText { x: 3.0, y: 4.0, size: 25.0, text: "转换".to_string() };
        let st = SceneText::from(&it);
        assert_eq!(st.x, 3.0);
        assert_eq!(st.color, 0);
        assert_eq!(st.style, ParagraphStyle::PLAIN);
        assert_eq!(st.to_item().text, "转换");
    }

    // ------------------------------------------------ 官方后端

    #[test]
    fn official_store_writes_into_existing_rm() {
        let d = tmpdir("official");
        let id = "page1";
        fs::write(d.join(format!("{id}.rm")), RmPage::default().serialize()).unwrap();
        let s = OfficialStore::new(&d);
        let texts = vec![
            SceneText::new(-468.0, 234.0, 25.0, "第一段"),
            SceneText::new(-468.0, 234.0, 25.0, "第二段"),
        ];
        s.save(id, &texts).unwrap();
        let back = s.load(id).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].text, "第一段");
        assert_eq!(back[1].text, "第二段");
        // 笔迹页结构没被破坏
        assert_eq!(RmPage::parse(&fs::read(s.rm_path(id)).unwrap()).unwrap().layers.len(), 1);
        // 清空 = 拿走文本块
        s.save(id, &[]).unwrap();
        assert!(parse_text(&fs::read(s.rm_path(id)).unwrap()).unwrap().is_none());
    }

    #[test]
    fn official_store_refuses_to_invent_a_page() {
        let d = tmpdir("official-missing");
        let s = OfficialStore::new(&d);
        let e = s.save("nope", &[SceneText::new(0.0, 0.0, 25.0, "x")]).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(e.to_string().contains("不能凭空造页"));
        assert_eq!(s.load("nope").unwrap(), Vec::new());
    }

    #[test]
    fn store_kind_reports_verification_status() {
        assert_eq!(DEFAULT_KIND, StoreKind::Sidecar);
        assert!(StoreKind::Sidecar.implemented());
        assert!(StoreKind::Official.implemented());
        assert!(StoreKind::Sidecar.verified_on_device());
        assert!(!StoreKind::Official.verified_on_device());
        assert_eq!(StoreKind::Official.label(), "rm-root-text");
    }

    #[test]
    fn open_dispatches_by_kind() {
        let d = tmpdir("open");
        let def = open_default(&d);
        assert_eq!(def.kind(), StoreKind::Sidecar);
        def.save("p", &[SceneText::new(5.0, 6.0, 25.0, "经由 trait")]).unwrap();
        assert_eq!(def.load("p").unwrap()[0].text, "经由 trait");

        fs::write(d.join("r.rm"), RmPage::default().serialize()).unwrap();
        let off = open(StoreKind::Official, &d);
        assert_eq!(off.kind(), StoreKind::Official);
        off.save("r", &[SceneText::new(0.0, 0.0, 25.0, "官方")]).unwrap();
        assert_eq!(off.load("r").unwrap()[0].text, "官方");
    }
}
