//! `.rm` v6 读写。
//!
//! 布局是照着设备上真实文件**逐字节扫出来的**，不是猜的：
//!
//! ```text
//! 43 字节 ASCII 头 "reMarkable .lines file, version=6          "
//! 块 = u32 长度 | u8 0 | u8 minVer | u8 curVer | u8 块类型 | 负载
//! 负载里的字段 = varuint(index<<4 | 类型) + 值
//!   类型: 0x1 Byte1 / 0x4 Byte4 / 0x8 Byte8 / 0xC Length4 / 0xF ID
//!   ID   = u8 part1 + varuint part2
//! ```
//!
//! 这台设备（OS 3.0.4）所有块的 `minVer = curVer = 1`，
//! 因此点数组是 **24 字节 = 6 × float32**（v2 的 14 字节格式在这里不出现）。
//!
//! 磁盘上的 float 就是内存语义值：pressure 是 0..1、dir 是弧度、width 是场景单位。
//! （rmscene 读出来会乘 4 / 乘 255，那是它自己的换算，不是磁盘真值。）

use std::collections::VecDeque;
use std::io;
use std::time::SystemTime;

use crate::pen::{Color, Line, Point, Tool};

pub const HEADER: &[u8] = b"reMarkable .lines file, version=6          ";

const BT_MIGRATION: u8 = 0x00;
const BT_SCENE_TREE: u8 = 0x01;
const BT_TREE_NODE: u8 = 0x02;
const BT_GROUP_ITEM: u8 = 0x04;
const BT_LINE_ITEM: u8 = 0x05;
const BT_AUTHOR_IDS: u8 = 0x09;
const BT_PAGE_INFO: u8 = 0x0a;

const TAG_BYTE1: u8 = 0x1;
const TAG_BYTE4: u8 = 0x4;
const TAG_BYTE8: u8 = 0x8;
const TAG_LEN4: u8 = 0xc;
const TAG_ID: u8 = 0xf;

const ITEM_GROUP: u8 = 0x02;
const ITEM_LINE: u8 = 0x03;

/// 固定根节点 / 图层节点的 CRDT id，与官方一致。
const ROOT: Id = Id(0, 1);
const LAYER: Id = Id(0, 11);
const NIL: Id = Id(0, 0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Id(pub u8, pub u64);

// ---------------------------------------------------------------- reader

struct R<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> R<'a> {
    fn new(b: &'a [u8]) -> R<'a> {
        R { b, p: 0 }
    }
    fn ok(&self, n: usize) -> io::Result<()> {
        if self.p + n <= self.b.len() {
            Ok(())
        } else {
            Err(io::Error::new(io::ErrorKind::UnexpectedEof, "rm: 数据截断"))
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
        let mut shift = 0;
        let mut out = 0u64;
        loop {
            let b = self.u8()?;
            out |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(out);
            }
            shift += 7;
            if shift > 63 {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "rm: varuint 溢出"));
            }
        }
    }
    fn tag(&mut self) -> io::Result<(u64, u8)> {
        let x = self.varuint()?;
        Ok((x >> 4, (x & 0xf) as u8))
    }
    fn id(&mut self) -> io::Result<Id> {
        let a = self.u8()?;
        let b = self.varuint()?;
        Ok(Id(a, b))
    }
    fn at_end(&self) -> bool {
        self.p >= self.b.len()
    }
}

impl<'a> R<'a> {
    /// 读一个 `Length4` 子块，返回它的切片。
    fn sub(&mut self) -> io::Result<R<'a>> {
        let n = self.u32()? as usize;
        self.ok(n)?;
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(R::new(s))
    }
}

// ---------------------------------------------------------------- writer

#[derive(Default)]
struct W {
    b: Vec<u8>,
}

impl W {
    fn u8(&mut self, v: u8) {
        self.b.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.b.extend_from_slice(&v.to_le_bytes());
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
                break;
            }
        }
    }
    fn tag(&mut self, index: u64, ty: u8) {
        self.varuint((index << 4) | ty as u64);
    }
    fn id(&mut self, index: u64, id: Id) {
        self.tag(index, TAG_ID);
        self.u8(id.0);
        self.varuint(id.1);
    }
    fn int(&mut self, index: u64, v: u32) {
        self.tag(index, TAG_BYTE4);
        self.u32(v);
    }
    fn float(&mut self, index: u64, v: f32) {
        self.tag(index, TAG_BYTE4);
        self.f32(v);
    }
    fn double(&mut self, index: u64, v: f64) {
        self.tag(index, TAG_BYTE8);
        self.f64(v);
    }
    fn boolean(&mut self, index: u64, v: bool) {
        self.tag(index, TAG_BYTE1);
        self.u8(v as u8);
    }
    fn sub(&mut self, index: u64, f: impl FnOnce(&mut W)) {
        let mut inner = W::default();
        f(&mut inner);
        self.tag(index, TAG_LEN4);
        self.u32(inner.b.len() as u32);
        self.b.extend_from_slice(&inner.b);
    }
    /// 顶层块：负载先写进临时缓冲，最后补长度头。
    fn block(&mut self, ty: u8, f: impl FnOnce(&mut W)) {
        let mut inner = W::default();
        f(&mut inner);
        self.u32(inner.b.len() as u32);
        self.u8(0);
        self.u8(1); // min_version
        self.u8(1); // current_version
        self.u8(ty);
        self.b.extend_from_slice(&inner.b);
    }
}

// ---------------------------------------------------------------- page

/// 一个图层。官方一页最多 5 层。
#[derive(Clone, Debug)]
pub struct Layer {
    pub name: String,
    pub visible: bool,
    pub lines: Vec<Line>,
}

impl Default for Layer {
    fn default() -> Layer {
        Layer {
            name: "Layer 1".to_string(),
            visible: true,
            lines: Vec::new(),
        }
    }
}

/// 一页的墨迹。官方还有文本块、锚点等，这里先只要笔迹 + 图层。
#[derive(Clone, Debug)]
pub struct RmPage {
    /// 自下而上：`layers[0]` 是最底层（官方面板里显示在最下面）
    pub layers: Vec<Layer>,
    /// 当前正在写的图层下标
    pub current: usize,
    /// 作者 UUID（`.content` 里的 `uuids[0].first`），写文件时要一致
    pub author: [u8; 16],
}

impl Default for RmPage {
    fn default() -> RmPage {
        RmPage {
            layers: vec![Layer::default()],
            current: 0,
            author: [0u8; 16],
        }
    }
}

impl RmPage {
    /// 当前图层的笔迹（可写）
    pub fn lines_mut(&mut self) -> &mut Vec<Line> {
        if self.layers.is_empty() {
            self.layers.push(Layer::default());
        }
        if self.current >= self.layers.len() {
            self.current = self.layers.len() - 1;
        }
        &mut self.layers[self.current].lines
    }

    /// 当前图层的笔迹（只读）
    pub fn lines(&self) -> &[Line] {
        self.layers
            .get(self.current.min(self.layers.len().saturating_sub(1)))
            .map(|l| l.lines.as_slice())
            .unwrap_or(&[])
    }

    /// 所有**可见**图层的笔迹，自下而上（画页面用这个）
    pub fn visible_lines(&self) -> impl Iterator<Item = &Line> {
        self.layers
            .iter()
            .filter(|l| l.visible)
            .flat_map(|l| l.lines.iter())
    }

    /// 所有图层的笔总数（日志/统计用）
    pub fn total_lines(&self) -> usize {
        self.layers.iter().map(|l| l.lines.len()).sum()
    }

    /// 加一层（最多 5 层），返回新层下标
    pub fn add_layer(&mut self) -> Option<usize> {
        if self.layers.len() >= 5 {
            return None;
        }
        let n = self.layers.len() + 1;
        self.layers.push(Layer {
            name: format!("Layer {n}"),
            visible: true,
            lines: Vec::new(),
        });
        self.current = self.layers.len() - 1;
        Some(self.current)
    }

    /// 删掉某一层；只剩一层时不动
    pub fn delete_layer(&mut self, i: usize) -> bool {
        if self.layers.len() <= 1 || i >= self.layers.len() {
            return false;
        }
        self.layers.remove(i);
        if self.current >= self.layers.len() {
            self.current = self.layers.len() - 1;
        }
        true
    }

    /// 向上挪一层（i → i+1，也就是更靠上）
    pub fn move_layer_up(&mut self, i: usize) -> bool {
        if i + 1 >= self.layers.len() {
            return false;
        }
        self.layers.swap(i, i + 1);
        self.current = i + 1;
        true
    }

    /// 向下挪一层
    pub fn move_layer_down(&mut self, i: usize) -> bool {
        if i == 0 || i >= self.layers.len() {
            return false;
        }
        self.layers.swap(i, i - 1);
        self.current = i - 1;
        true
    }

    /// 与下面一层合并（笔迹并进去，然后删掉这层）
    pub fn merge_down(&mut self, i: usize) -> bool {
        if i == 0 || i >= self.layers.len() {
            return false;
        }
        let upper = self.layers.remove(i);
        self.layers[i - 1].lines.extend(upper.lines);
        self.current = i - 1;
        true
    }
}

fn layer_id(i: usize) -> Id {
    Id(0, 11 + i as u64 * 2)
}

fn group_id(i: usize) -> Id {
    Id(0, 12 + i as u64 * 2)
}

impl RmPage {
    /// 解析一个 `.rm`。不认识/坏掉的块会被跳过而不是整体失败——
    /// 官方文件里夹着大量 tombstone，我们不需要它们。
    pub fn parse(bytes: &[u8]) -> io::Result<RmPage> {
        if bytes.len() < HEADER.len() || &bytes[..HEADER.len()] != HEADER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "不是 .rm v6 文件（头不匹配）",
            ));
        }
        let mut page = RmPage::default();
        page.layers.clear();
        // 图层节点（名 / 可见）与带父节点的笔迹，先收下来再组装
        let mut nodes: Vec<(Id, String, bool)> = Vec::new();
        let mut items: Vec<(Id, Line)> = Vec::new();
        let mut r = R::new(&bytes[HEADER.len()..]);
        while !r.at_end() {
            let len = r.u32()? as usize;
            let _unk = r.u8()?;
            let _min = r.u8()?;
            let cur = r.u8()?;
            let ty = r.u8()?;
            r.ok(len)?;
            let body = &r.b[r.p..r.p + len];
            r.p += len;

            match ty {
                BT_LINE_ITEM => {
                    if let Some((parent, line)) = parse_line_item(body, cur) {
                        items.push((parent, line));
                    }
                }
                BT_TREE_NODE => {
                    if let Some(node) = parse_tree_node(body) {
                        nodes.push(node);
                    }
                }
                BT_AUTHOR_IDS => {
                    if let Some(uuid) = parse_author(body) {
                        page.author = uuid;
                    }
                }
                _ => {}
            }
        }

        // 根节点不是图层
        let mut ids: Vec<Id> = Vec::new();
        for (id, name, visible) in nodes {
            if id == ROOT || id == NIL {
                continue;
            }
            ids.push(id);
            page.layers.push(Layer {
                name,
                visible,
                lines: Vec::new(),
            });
        }
        if page.layers.is_empty() {
            page.layers.push(Layer::default());
        }
        // 笔迹按父节点归层；不认识的父节点先放最底层
        // 名字里混进控制字符（老版本写坏的文件）就退回默认名
        for (i, l) in page.layers.iter_mut().enumerate() {
            if l.name.chars().any(|c| c.is_control()) || l.name.trim().is_empty() {
                l.name = format!("Layer {}", i + 1);
            }
        }
        let last = page.layers.len() - 1;
        for (parent, line) in items {
            let i = ids.iter().position(|&x| x == parent).unwrap_or(0).min(last);
            page.layers[i].lines.push(line);
        }
        page.current = page.layers.len() - 1;
        Ok(page)
    }

    /// 生成一个官方可读的 `.rm`。
    ///
    /// 块顺序照抄官方：AuthorIds → MigrationInfo → PageInfo → SceneTree →
    /// TreeNode(root) → TreeNode(layer) → SceneGroupItem → SceneLineItem × N。
    pub fn serialize(&self) -> Vec<u8> {
        let mut w = W::default();
        w.b.extend_from_slice(HEADER);

        // AuthorIdsBlock
        w.block(BT_AUTHOR_IDS, |w| {
            w.varuint(1);
            w.sub(0, |w| {
                w.varuint(16);
                w.b.extend_from_slice(&self.author);
                w.u16(1);
            });
        });

        // MigrationInfoBlock
        w.block(BT_MIGRATION, |w| {
            w.id(1, Id(1, 1));
            w.boolean(2, true);
        });

        // PageInfoBlock: loads / merges / text_chars / text_lines
        w.block(BT_PAGE_INFO, |w| {
            w.int(1, 0);
            w.int(2, 0);
            w.int(3, 0);
            w.int(4, 0);
        });

        // SceneTreeBlock × N: (图层节点, 根, is_update, 父 = 根)
        for i in 0..self.layers.len() {
            w.block(BT_SCENE_TREE, |w| {
                w.id(1, layer_id(i));
                w.id(2, NIL);
                w.boolean(3, true);
                w.sub(4, |w| w.id(1, ROOT));
            });
        }

        // TreeNodeBlock: 根节点（label 空、visible）
        w.block(BT_TREE_NODE, |w| {
            w.id(1, ROOT);
            w.sub(2, |w| {
                w.id(1, NIL);
                w.sub(2, |w| {
                    w.varuint(0);
                    w.u8(1);
                });
            });
            w.sub(3, |w| {
                w.id(1, NIL);
                w.boolean(2, true);
            });
        });

        // TreeNodeBlock × N: 每个图层的名字与可见性
        for (i, layer) in self.layers.iter().enumerate() {
            w.block(BT_TREE_NODE, |w| {
                w.id(1, layer_id(i));
                w.sub(2, |w| {
                    w.id(1, group_id(i));
                    w.sub(2, |w| {
                        let s = layer.name.as_bytes();
                        w.varuint(s.len() as u64);
                        w.u8(1);
                        w.b.extend_from_slice(s);
                    });
                });
                w.sub(3, |w| {
                    w.id(1, NIL);
                    w.boolean(2, layer.visible);
                });
            });
        }

        // SceneGroupItemBlock × N: 把每个图层挂到根下面（双向链表）
        for i in 0..self.layers.len() {
            let left = if i == 0 { NIL } else { group_id(i - 1) };
            let right = if i + 1 == self.layers.len() {
                NIL
            } else {
                group_id(i + 1)
            };
            w.block(BT_GROUP_ITEM, |w| {
                w.id(1, ROOT);
                w.id(2, group_id(i));
                w.id(3, left);
                w.id(4, right);
                w.int(5, 0);
                w.sub(6, |w| {
                    w.u8(ITEM_GROUP);
                    w.id(2, layer_id(i));
                });
            });
        }

        // SceneLineItemBlock × N：每个图层内部自成一条 CRDT 双向链表
        let mut seq: u64 = 0;
        for (li, layer) in self.layers.iter().enumerate() {
        for (i, line) in layer.lines.iter().enumerate() {
            let item = Id(1, 1000 + seq);
            seq += 1;
            let left = if i == 0 { NIL } else { Id(1, 1000 + seq - 2) };
            let right = if i + 1 == layer.lines.len() {
                NIL
            } else {
                Id(1, 1000 + seq)
            };
            w.block(BT_LINE_ITEM, |w| {
                w.id(1, layer_id(li));
                w.id(2, item);
                w.id(3, left);
                w.id(4, right);
                w.int(5, 0); // deleted_length = 0 → 有效项
                w.sub(6, |w| {
                    w.u8(ITEM_LINE);
                    w.int(1, line.tool as u32);
                    w.int(2, line.color as u32);
                    w.double(3, line.thickness);
                    w.float(4, 0.0); // starting_length
                    w.sub(5, |w| {
                        for p in &line.points {
                            w.f32(p.x);
                            w.f32(p.y);
                            w.f32(p.speed);
                            w.f32(p.dir);
                            w.f32(p.width);
                            w.f32(p.pressure);
                        }
                    });
                    w.id(6, Id(0, 1)); // 未使用的时间戳，官方也写死
                });
            });
        }
        }

        w.b
    }
}

// ---------------------------------------------------------------- parsed page cache

impl RmPage {
    /// 这一页的内容大约占多少字节（按 len 算，不含 Vec 的预留容量）。
    /// 缓存做内存量级参考/日志用——是下界，不是精确分配量。
    pub fn heap_bytes(&self) -> usize {
        let mut n = std::mem::size_of::<RmPage>();
        for l in &self.layers {
            n += std::mem::size_of::<Layer>()
                + l.name.len()
                + l.lines.len() * std::mem::size_of::<Line>();
            for line in &l.lines {
                n += line.points.len() * std::mem::size_of::<Point>();
            }
        }
        n
    }
}

/// 解析缓存的统计（日志/测试用）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ParsedStats {
    pub hits: u64,
    pub misses: u64,
    pub entries: usize,
    pub capacity: usize,
    pub bytes: usize,
}

/// 默认缓存几页已解析的墨迹。一页实测量级约 0.6 MB 内存，4 页 ≈ 2.5 MB。
pub const DEFAULT_PARSED_PAGES: usize = 4;

struct ParsedEntry {
    key: String,
    len: u64,
    mtime: Option<SystemTime>,
    page: RmPage,
}

/// 最近解析过的页：翻页/重绘不用再 parse 一遍 `.rm`。
///
/// * 键 = `(文档 uuid, 页 id)` —— 页 id 在增删页后仍然稳定；
/// * 另存文件的 `(长度, mtime)` 做校验：文件被外部改过就自动失效，
///   不会把旧墨迹递给调用方；
/// * 超容量按 LRU 淘汰。
///
/// `get` 返回的是 `RmPage` 的**深拷贝**（实测比重新 parse 快 13 倍），
/// 调用方拿到的是独占所有权，和主循环里 `nb.page` 的用法兼容。
pub struct ParsedPages {
    cap: usize,
    /// 队尾 = 最近用到的。
    entries: VecDeque<ParsedEntry>,
    hits: u64,
    misses: u64,
}

impl ParsedPages {
    pub fn new(cap: usize) -> ParsedPages {
        ParsedPages {
            cap,
            entries: VecDeque::new(),
            hits: 0,
            misses: 0,
        }
    }

    /// 命中返回副本，并把该页挪到队尾；未命中返回 `None`。
    /// 文件元数据对不上（长度或 mtime 变了）也算未命中。
    pub fn get(&mut self, key: &str, len: u64, mtime: Option<SystemTime>) -> Option<RmPage> {
        let i = self
            .entries
            .iter()
            .position(|e| e.key == key && e.len == len && e.mtime == mtime);
        let Some(i) = i else {
            self.misses += 1;
            return None;
        };
        self.hits += 1;
        if i + 1 != self.entries.len() {
            if let Some(e) = self.entries.remove(i) {
                self.entries.push_back(e);
            }
        }
        self.entries.back().map(|e| e.page.clone())
    }

    pub fn put(&mut self, key: &str, len: u64, mtime: Option<SystemTime>, page: RmPage) {
        if self.cap == 0 {
            return;
        }
        if let Some(i) = self.entries.iter().position(|e| e.key == key) {
            if let Some(mut e) = self.entries.remove(i) {
                e.len = len;
                e.mtime = mtime;
                e.page = page;
                self.entries.push_back(e);
            }
            return;
        }
        self.entries.push_back(ParsedEntry {
            key: key.to_string(),
            len,
            mtime,
            page,
        });
        while self.entries.len() > self.cap {
            self.entries.pop_front();
        }
    }

    /// 刚写过这一页：直接踢掉，别等 mtime 兜底。
    pub fn invalidate(&mut self, key: &str) {
        self.entries.retain(|e| e.key != key);
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn stats(&self) -> ParsedStats {
        ParsedStats {
            hits: self.hits,
            misses: self.misses,
            entries: self.entries.len(),
            capacity: self.cap,
            bytes: self.entries.iter().map(|e| e.page.heap_bytes()).sum(),
        }
    }
}

fn parse_author(body: &[u8]) -> Option<[u8; 16]> {
    let mut r = R::new(body);
    let n = r.varuint().ok()?;
    if n == 0 {
        return None;
    }
    // 第一个子块就是作者
    let (_, ty) = r.tag().ok()?;
    if ty != TAG_LEN4 {
        return None;
    }
    let mut s = r.sub().ok()?;
    let len = s.varuint().ok()? as usize;
    if len != 16 {
        return None;
    }
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&s.b[s.p..s.p + 16]);
    Some(uuid)
}

fn parse_line_item(body: &[u8], version: u8) -> Option<(Id, Line)> {
    let mut r = R::new(body);
    // 1 父节点 / 2 自身 / 3 左 / 4 右
    let mut parent = NIL;
    for want in 1..=4u64 {
        let (idx, ty) = r.tag().ok()?;
        if idx != want || ty != TAG_ID {
            return None;
        }
        let id = r.id().ok()?;
        if want == 1 {
            parent = id;
        }
    }
    let (idx, ty) = r.tag().ok()?;
    if idx != 5 || ty != TAG_BYTE4 {
        return None;
    }
    let deleted = r.u32().ok()?;
    if deleted != 0 || r.at_end() {
        return None; // tombstone / 空项
    }
    let (idx, ty) = r.tag().ok()?;
    if idx != 6 || ty != TAG_LEN4 {
        return None;
    }
    let mut v = r.sub().ok()?;
    if v.u8().ok()? != ITEM_LINE {
        return None;
    }

    let mut tool = Tool::Undefined;
    let mut color = Color::Black;
    let mut thickness = 2.0f64;
    let mut points = Vec::new();
    while !v.at_end() {
        let (idx, ty) = v.tag().ok()?;
        match (idx, ty) {
            (1, TAG_BYTE4) => tool = Tool::from_u8(v.u32().ok()? as u8),
            (2, TAG_BYTE4) => color = Color::from_u8(v.u32().ok()? as u8),
            (3, TAG_BYTE8) => thickness = v.f64().ok()?,
            (4, TAG_BYTE4) => {
                v.f32().ok()?;
            }
            (5, TAG_LEN4) => {
                let s = v.sub().ok()?;
                let step = if version >= 2 { 14 } else { 24 };
                // 点数组是唯一的热循环（一页几千到几万点）。整块 `chunks_exact`
                // 解码 + 一次 reserve，省掉逐点 6 次 `Result` 读和重复扩容。
                let rest = &s.b[s.p..];
                points.reserve(rest.len() / step);
                if step == 24 {
                    for c in rest.chunks_exact(24) {
                        points.push(Point {
                            x: f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                            y: f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                            speed: f32::from_le_bytes([c[8], c[9], c[10], c[11]]),
                            dir: f32::from_le_bytes([c[12], c[13], c[14], c[15]]),
                            width: f32::from_le_bytes([c[16], c[17], c[18], c[19]]),
                            pressure: f32::from_le_bytes([c[20], c[21], c[22], c[23]]),
                        });
                    }
                } else {
                    for c in rest.chunks_exact(14) {
                        points.push(Point {
                            x: f32::from_le_bytes([c[0], c[1], c[2], c[3]]),
                            y: f32::from_le_bytes([c[4], c[5], c[6], c[7]]),
                            speed: u16::from_le_bytes([c[8], c[9]]) as f32,
                            width: u16::from_le_bytes([c[10], c[11]]) as f32,
                            dir: c[12] as f32,
                            pressure: c[13] as f32,
                        });
                    }
                }
            }
            (6, TAG_ID) => {
                v.id().ok()?;
            }
            (_, TAG_ID) => {
                v.id().ok()?;
            }
            (_, TAG_BYTE4) => {
                v.u32().ok()?;
            }
            (_, TAG_BYTE8) => {
                v.f64().ok()?;
            }
            (_, TAG_BYTE1) => {
                v.u8().ok()?;
            }
            (_, TAG_LEN4) => {
                v.sub().ok()?;
            }
            _ => return None,
        }
    }

    Some((
        parent,
        Line {
            tool,
            color,
            thickness,
            points,
            bounds: Default::default(),
        },
    ))
}

/// 解析 TreeNodeBlock：节点 id、名字、可见性。
fn parse_tree_node(body: &[u8]) -> Option<(Id, String, bool)> {
    let mut r = R::new(body);
    let mut id = NIL;
    let mut name = String::new();
    let mut visible = true;
    while !r.at_end() {
        let (idx, ty) = r.tag().ok()?;
        match (idx, ty) {
            (1, TAG_ID) => id = r.id().ok()?,
            (2, TAG_LEN4) => {
                let mut v = r.sub().ok()?;
                while !v.at_end() {
                    let (_, t2) = v.tag().ok()?;
                    match t2 {
                        TAG_ID => {
                            v.id().ok()?;
                        }
                        TAG_LEN4 => {
                            let mut s2 = v.sub().ok()?;
                            let n = s2.varuint().ok()? as usize;
                            let _ = s2.u8().ok()?;
                            if n <= s2.b.len() - s2.p {
                                name =
                                    String::from_utf8_lossy(&s2.b[s2.p..s2.p + n]).to_string();
                            }
                        }
                        _ => return None,
                    }
                }
            }
            (3, TAG_LEN4) => {
                let mut v = r.sub().ok()?;
                while !v.at_end() {
                    let (i2, t2) = v.tag().ok()?;
                    match (i2, t2) {
                        (2, TAG_BYTE1) => visible = v.u8().ok()? != 0,
                        (_, TAG_ID) => {
                            v.id().ok()?;
                        }
                        _ => return None,
                    }
                }
            }
            (_, TAG_ID) => {
                r.id().ok()?;
            }
            (_, TAG_BYTE1) => {
                r.u8().ok()?;
            }
            (_, TAG_BYTE4) => {
                r.u32().ok()?;
            }
            (_, TAG_BYTE8) => {
                r.f64().ok()?;
            }
            (_, TAG_LEN4) => {
                r.sub().ok()?;
            }
            _ => return None,
        }
    }
    Some((id, name, visible))
}

impl RmPage {
    /// 只改某一层的名字，**不动**笔迹 / 顺序 / 可见性 / 当前层。
    ///
    /// 落盘仍走 [`RmPage::serialize`]（层名就是 `TreeNodeBlock` 里那段裸
    /// UTF-8），所以这里不碰解析器，也不另写 `.rm` 逻辑。
    ///
    /// 名字先 `trim`；空名字或含控制字符时不动并返回 `false`
    /// —— 那两种名字解析回来会被当成坏文件退回 `Layer N`，写进盘里等于改名失败。
    pub fn rename_layer(&mut self, i: usize, name: &str) -> bool {
        let name = name.trim();
        if name.is_empty() || name.chars().any(|c| c.is_control()) {
            return false;
        }
        let Some(layer) = self.layers.get_mut(i) else {
            return false;
        };
        layer.name = name.to_string();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(y: f32) -> Line {
        let mut l = Line::new(Tool::FinelinerV2, Color::Black, 2.0);
        for i in 0..10 {
            l.push(Point {
                x: 200.0 + i as f32 * 40.0,
                y,
                speed: 1.0,
                dir: 0.5,
                width: 4.0,
                pressure: 0.6,
            });
        }
        l
    }

    /// 手动导出样本给 rmscene 校验：RM_DUMP=1 cargo test dump_for_rmscene
    #[test]
    fn dump_for_rmscene() {
        if std::env::var("RM_DUMP").is_err() {
            return;
        }
        let mut page = RmPage::default();
        page.author = [
            0x56, 0xdc, 0xae, 0x4b, 0x90, 0x6b, 0xb0, 0x51, 0xa8, 0xf4, 0x89, 0xd7, 0x18, 0x85,
            0x0b, 0xa6,
        ];
        page.lines_mut().push(line(300.0));
        page.lines_mut().push(line(500.0));
        let bytes = page.serialize();
        std::fs::write("/tmp/ours.rm", &bytes).unwrap();
        eprintln!("wrote /tmp/ours.rm {} bytes", bytes.len());
    }

    #[test]
    fn layers_roundtrip() {
        let mut page = RmPage::default();
        page.author = [9u8; 16];
        page.lines_mut().push(line(100.0));
        page.add_layer().unwrap();
        page.layers[1].name = "Layer 2".into();
        page.lines_mut().push(line(200.0));
        page.layers[1].visible = false;

        let back = RmPage::parse(&page.serialize()).unwrap();
        assert_eq!(back.layers.len(), 2);
        assert_eq!(back.layers[0].name, "Layer 1");
        assert_eq!(back.layers[1].name, "Layer 2");
        assert!(!back.layers[1].visible);
        assert!(back.layers[0].visible);
        assert_eq!(back.layers[0].lines.len(), 1);
        assert_eq!(back.layers[1].lines.len(), 1);
        assert!((back.layers[1].lines[0].points[0].y - 200.0).abs() < 1e-3);
    }

    #[test]
    fn roundtrip() {
        let mut page = RmPage::default();
        page.author = [7u8; 16];
        page.lines_mut().push(line(300.0));
        page.lines_mut().push(line(500.0));

        let bytes = page.serialize();
        assert_eq!(&bytes[..HEADER.len()], HEADER);
        let back = RmPage::parse(&bytes).unwrap();
        assert_eq!(back.total_lines(), 2);
        assert_eq!(back.author, [7u8; 16]);
        let l = &back.layers[0].lines[0];
        assert_eq!(l.tool, Tool::FinelinerV2);
        assert_eq!(l.points.len(), 10);
        assert!((l.points[3].x - 320.0).abs() < 1e-3);
        assert!((l.points[3].width - 4.0).abs() < 1e-3);
        assert!((l.points[3].pressure - 0.6).abs() < 1e-6);
    }

    /// 和 MEASURED.md 里那页真实样本同量级：331 KB / 292 条线 / 约 1.37 万点。
    fn heavy_page() -> Vec<u8> {
        let mut page = RmPage::default();
        page.author = [3u8; 16];
        for i in 0..292 {
            let mut l = Line::new(Tool::FinelinerV2, Color::Black, 2.0);
            for k in 0..47 {
                let t = k as f32;
                l.points.push(Point {
                    x: 100.0 + t * 7.0,
                    y: 200.0 + i as f32,
                    speed: 1.5,
                    dir: 1.2,
                    width: 3.0,
                    pressure: 0.5,
                });
            }
            page.lines_mut().push(l);
        }
        page.serialize()
    }

    #[test]
    fn parsed_pages_hit_and_validate() {
        let mut c = ParsedPages::new(DEFAULT_PARSED_PAGES);
        let page = RmPage::parse(&heavy_page()).unwrap();
        let key = "uuid/page-id";

        assert!(c.get(key, 100, None).is_none(), "空缓存当然没有");
        c.put(key, 100, None, page.clone());

        let hit = c.get(key, 100, None).expect("同 key 同元数据必须命中");
        assert_eq!(hit.total_lines(), page.total_lines());
        assert_eq!(hit.layers[0].lines[0].points.len(), 47);

        assert!(c.get(key, 101, None).is_none(), "文件长度变了要失效");
        let mt = Some(SystemTime::UNIX_EPOCH);
        assert!(c.get(key, 100, mt).is_none(), "mtime 变了要失效");

        let st = c.stats();
        assert_eq!((st.hits, st.misses), (1, 3));
        assert_eq!(st.entries, 1);
        assert_eq!(st.bytes, page.heap_bytes(), "缓存里的副本和原页大小一致");

        c.invalidate(key);
        assert!(c.get(key, 100, None).is_none());
    }

    #[test]
    fn parsed_pages_evicts_lru() {
        let mut c = ParsedPages::new(2);
        for i in 0..2 {
            c.put(&format!("k{i}"), 1, None, RmPage::default());
        }
        // 摸一下 k0，让 k1 变成最久没用的
        assert!(c.get("k0", 1, None).is_some());
        c.put("k2", 1, None, RmPage::default());
        assert_eq!(c.stats().entries, 2);
        assert!(c.get("k0", 1, None).is_some(), "刚摸过的 k0 该留着");
        assert!(c.get("k1", 1, None).is_none(), "k1 该被淘汰");
    }

    /// 手动跑：`RM_BENCH=1 cargo test --lib doc::rm::tests::bench_parse -- --nocapture`
    /// 打印一页真实量级 `.rm` 的解析耗时；顺带量一下 `clone()`
    /// —— 用来判断"缓存已解析的页再深拷贝出去"到底值不值。
    #[test]
    fn bench_parse() {
        if std::env::var("RM_BENCH").is_err() {
            return;
        }
        let bytes = heavy_page();
        let len = bytes.len();
        let rounds = 200u32;

        let page = RmPage::parse(&bytes).unwrap();
        let lines = page.total_lines();
        let points: usize = page.visible_lines().map(|l| l.points.len()).sum();

        let t = std::time::Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(RmPage::parse(&bytes).unwrap());
        }
        let parse = t.elapsed() / rounds;

        let t = std::time::Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(page.clone());
        }
        let clone = t.elapsed() / rounds;

        eprintln!(
            "[bench] .rm {len} B / {lines} 线 / {points} 点：parse {parse:?}，clone {clone:?}，{:.1} MB/s",
            len as f64 / parse.as_secs_f64() / 1e6
        );
    }

    /// 6a：层名往返。改名只动名字，笔迹 / 顺序 / 可见性 / 当前层全都不变；
    /// 空名字和带控制字符的名字拒绝（写进盘里解析会退回 `Layer N`）。
    #[test]
    fn rename_layer_only_touches_the_name() {
        let mut page = RmPage::default();
        page.author = [5u8; 16];
        page.lines_mut().push(line(120.0));
        page.add_layer().unwrap();
        page.layers[1].name = "Layer 2".into();
        page.lines_mut().push(line(420.0));
        page.layers[1].visible = false;
        page.current = 1;

        assert!(page.rename_layer(1, "  Sketch  "));
        assert_eq!(page.layers[1].name, "Sketch", "两头空白要 trim");
        assert_eq!(page.layers[0].name, "Layer 1", "别的层不许动");
        assert_eq!(page.layers[0].lines.len(), 1);
        assert_eq!(page.layers[1].lines.len(), 1);
        assert!(!page.layers[1].visible, "可见性不许动");
        assert_eq!(page.current, 1, "当前层不许动");

        // 序列化 → 解析：名字保住，其它字段也还是原样
        let back = RmPage::parse(&page.serialize()).unwrap();
        assert_eq!(back.layers[0].name, "Layer 1");
        assert_eq!(back.layers[1].name, "Sketch");
        assert!(!back.layers[1].visible);
        assert_eq!(back.layers[0].lines.len(), 1);
        assert_eq!(back.layers[1].lines.len(), 1);
        assert!((back.layers[1].lines[0].points[0].y - 420.0).abs() < 1e-3);
        assert_eq!(back.author, [5u8; 16]);

        // 多字节名字：写进盘里的是 UTF-8 字节长度，往返不能截断
        assert!(page.rename_layer(0, "手写笔记"));
        assert_eq!(
            RmPage::parse(&page.serialize()).unwrap().layers[0].name,
            "手写笔记"
        );

        // 非法输入 / 越界：不动、返回 false
        assert!(!page.rename_layer(0, "   "));
        assert!(!page.rename_layer(0, "bad\u{7}name"));
        assert!(!page.rename_layer(9, "nope"));
        assert_eq!(page.layers[0].name, "手写笔记", "非法输入一个字都不许改");
    }
}
