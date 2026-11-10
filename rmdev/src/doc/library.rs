//! 文档模型与库扫描（`.metadata` / `.content` / `.pagedata` / `.rm`）。
//!
//! 字段与官方逐项对齐，依据是设备上真实的四件套 +
//! `out/modules/M4-document-format.md`。
//! 我们只写**必要字段**，其余保持官方原值，这样官方 UI 打开不会翻车。

use std::cell::{Cell, RefCell};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::rm::{ParsedPages, ParsedStats, RmPage, DEFAULT_PARSED_PAGES};
use super::Format;

pub const DEFAULT_ROOT: &str = "/home/root/.local/share/remarkable/xochitl";

#[derive(Clone, Debug)]
pub struct Book {
    pub uuid: String,
    pub name: String,
    /// 父文件夹的 uuid，根目录是 ""
    pub parent: String,
    pub pinned: bool,
    pub deleted: bool,
    /// `DocumentType` 或 `CollectionType`（=文件夹）
    pub doc_type: String,
    /// 毫秒时间戳
    pub last_modified: u64,
    /// 上次看到第几页（0 基）
    pub last_opened_page: u32,
    pub page_count: u32,
    /// 文件格式（`fileType` 字段，空/`notebook` 就是 `.rm` 笔记）
    pub format: Format,
    /// 文件字节数（.content 的 sizeInBytes）
    pub size: u64,
    /// 上次打开时间（毫秒）
    pub last_opened: u64,
}

pub struct Library {
    pub root: PathBuf,
    /// 最近解析过的页：翻页/重绘不再重新 parse 整页 `.rm`。
    /// 键是 (uuid, 页 id)，文件长度+mtime 变了自动失效。
    pages: RefCell<ParsedPages>,
    /// 只读模式（官方 `res_0686a15.qml`：磁盘满了
    /// `Your device is in read-only mode.`）。开的时候**所有**写入口都
    /// 通过 [`Library::guard_write`] 拒绝，读不受影响。
    read_only: Cell<bool>,
}

/// 页面上的一段文字（我们自己的存储：`texts-<pageid>.json`）
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct TextItem {
    pub x: f32,
    pub y: f32,
    pub size: f32,
    pub text: String,
}

impl Library {
    pub fn new() -> Library {
        // 本机渲染（`--render`）可以指向一个假库；真机上不设这个变量。
        let root = std::env::var("RMDEV_LIB_ROOT").unwrap_or_else(|_| DEFAULT_ROOT.to_string());
        Library {
            root: PathBuf::from(root),
            pages: RefCell::new(ParsedPages::new(DEFAULT_PARSED_PAGES)),
            read_only: Cell::new(false),
        }
    }

    pub fn with_root<P: AsRef<Path>>(p: P) -> Library {
        Library {
            root: p.as_ref().to_path_buf(),
            pages: RefCell::new(ParsedPages::new(DEFAULT_PARSED_PAGES)),
            read_only: Cell::new(false),
        }
    }

    /// 解析缓存的命中/占用情况（日志和测试用）。
    pub fn page_cache_stats(&self) -> ParsedStats {
        self.pages.borrow().stats()
    }

    /// 只读模式开关。main 按磁盘水位（[`crate::settings::disk::level`]）设置：
    /// `to_savable` 为 false 时打开。
    pub fn set_read_only(&self, on: bool) {
        self.read_only.set(on);
    }

    /// 现在是只读模式吗（库界面画提示条用）。
    pub fn is_read_only(&self) -> bool {
        self.read_only.get()
    }

    /// 写入口的统一闸门：只读时返回 [`io::Error::other`]`("read-only mode")`。
    ///
    /// 放在每个写函数的最前面（**先于任何读改写**），这样磁盘满的时候
    /// `.metadata` / `.content` / `.rm` 一个字节都不会被动过。
    fn guard_write(&self) -> io::Result<()> {
        if self.read_only.get() {
            return Err(io::Error::other("read-only mode"));
        }
        Ok(())
    }

    fn meta_path(&self, uuid: &str) -> PathBuf {
        self.root.join(format!("{uuid}.metadata"))
    }
    fn content_path(&self, uuid: &str) -> PathBuf {
        self.root.join(format!("{uuid}.content"))
    }
    fn pagedata_path(&self, uuid: &str) -> PathBuf {
        self.root.join(format!("{uuid}.pagedata"))
    }
    fn page_path(&self, uuid: &str, page: &str) -> PathBuf {
        self.root.join(uuid).join(format!("{page}.rm"))
    }

    /// DEV 入口那个文档不进我们的列表（它是给官方 UI 用的开关）。
    fn entry_uuid() -> Option<String> {
        std::fs::read_to_string("/home/root/.config/rmdev/entry.uuid")
            .ok()
            .map(|s| s.trim().to_string())
    }

    /// 扫描库。只返回文档（`DocumentType`）且未删除、未进回收站的。
    pub fn books(&self) -> Vec<Book> {
        let mut v = self.entries("", false);
        v.retain(|b| b.doc_type == "DocumentType");
        v
    }

    /// 全库（含子文件夹）里未删除的条目，按 `lastModified` 倒序。
    ///
    /// [`Library::entries`] 只查一层，抽屉里那两个列表要的是"整库"，
    /// 所以这里按文件夹往下走一遍。用 `seen` 防 `.metadata` 里 parent 成环。
    fn all_entries(&self) -> Vec<Book> {
        let mut out = Vec::new();
        let mut stack: Vec<String> = vec![String::new()];
        let mut seen: std::collections::BTreeSet<String> = Default::default();
        while let Some(dir) = stack.pop() {
            for b in self.entries(&dir, false) {
                if b.doc_type == "CollectionType" && seen.insert(b.uuid.clone()) {
                    stack.push(b.uuid.clone());
                }
                out.push(b);
            }
        }
        out.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
        out
    }

    /// 最近改过的 `n` 篇文档（按 `.metadata.lastModified` 倒序，只算文档）。
    ///
    /// 官方抽屉的 Recent 用的是 `LibraryView.RecentlyOpenedDate`（`lastOpened`）；
    /// 本任务契约要的是 `lastModified`，按契约走（和 `Sort::Updated` 一致）。
    pub fn recent(&self, n: usize) -> Vec<Book> {
        let mut v = self.all_entries();
        v.retain(|b| b.doc_type == "DocumentType");
        v.truncate(n);
        v
    }

    /// 收藏列表：官方就是 `.metadata.pinned`（文档和文件夹都算，未删除的），
    /// 按 `lastModified` 倒序。整库扫，子文件夹里的收藏也算。
    pub fn pinned(&self) -> Vec<Book> {
        let mut v = self.all_entries();
        v.retain(|b| b.pinned);
        v
    }

    /// 统一查询：`parent` 传 "" 表示根目录；`deleted=true` 查回收站。
    /// 文件夹（`CollectionType`）也会被查出来，调用方自己按 `doc_type` 分。
    pub fn entries(&self, parent: &str, deleted: bool) -> Vec<Book> {
        let skip = Self::entry_uuid();
        let mut out = Vec::new();
        let Ok(dir) = fs::read_dir(&self.root) else {
            return out;
        };
        for e in dir.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("metadata") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&p) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
            if ty != "DocumentType" && ty != "CollectionType" {
                continue;
            }
            let is_deleted = v.get("deleted").and_then(|x| x.as_bool()).unwrap_or(false);
            if is_deleted != deleted {
                continue;
            }
            let par = v.get("parent").and_then(|x| x.as_str()).unwrap_or("");
            // 回收站里的东西保留原来的 parent，所以查删除项时不按目录过滤
            if !deleted && par != parent {
                continue;
            }
            let Some(uuid) = p.file_stem().and_then(|s| s.to_str()).map(String::from) else {
                continue;
            };
            if skip.as_deref() == Some(uuid.as_str()) {
                continue;
            }
            let name = v
                .get("visibleName")
                .and_then(|x| x.as_str())
                .unwrap_or("(未命名)")
                .to_string();
            let last = v
                .get("lastModified")
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let content = self.read_content(&uuid);
            let page_count = content
                .get("pageCount")
                .and_then(|x| x.as_u64())
                .unwrap_or(1) as u32;
            let ft = content.get("fileType").and_then(|x| x.as_str()).unwrap_or("");
            let format = match ft {
                // 官方 xochitl 扫库时会把不认识的 `.content` 改写成 notebook
                //（电子书于是变成"1 页空白笔记本"、缩略图全白）。被改写过的条目
                // 按同名伴生文件（`<uuid>.mobi` 之类）自愈，别把书当笔记打开。
                "" | "notebook" => self.sidecar_format(&uuid).unwrap_or(Format::Rm),
                other => Format::of_ext(other),
            };
            let size = content
                .get("sizeInBytes")
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let last_opened = v
                .get("lastOpened")
                .and_then(|x| x.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            let lop = v
                .get("lastOpenedPage")
                .and_then(|x| x.as_u64())
                .unwrap_or(0) as u32;
            out.push(Book {
                uuid,
                name,
                last_opened_page: lop,
                format,
                size,
                last_opened,
                parent: par.to_string(),
                pinned: v.get("pinned").and_then(|x| x.as_bool()).unwrap_or(false),
                deleted: is_deleted,
                doc_type: ty.to_string(),
                last_modified: last,
                page_count,
            });
        }
        out.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
        out
    }

    /// 收藏（官方 = `.metadata.pinned`）。
    pub fn set_pinned(&self, uuid: &str, on: bool) -> io::Result<()> {
        self.guard_write()?;
        let mut meta = self.read_meta(uuid);
        meta["pinned"] = json!(on);
        self.write_json(&self.meta_path(uuid), &meta)
    }

    /// 重命名。
    pub fn rename(&self, uuid: &str, name: &str) -> io::Result<()> {
        self.guard_write()?;
        let mut meta = self.read_meta(uuid);
        meta["visibleName"] = json!(name);
        meta["lastModified"] = json!(now_ms().to_string());
        meta["metadatamodified"] = json!(true);
        self.write_json(&self.meta_path(uuid), &meta)
    }

    /// 从回收站恢复：`deleted` 置 false、`parent` 还原到原文件夹。
    ///
    /// `parent` 本来就由 [`Library::delete_book`] 原样保留，所以这里不动它；
    /// 只有旧数据 / 以前写成 `"trash"` 的条目才放回根目录。读不到 `.metadata`
    /// 或 JSON 坏了直接报错，绝不凭空造一份新的元数据。
    pub fn restore(&self, uuid: &str) -> io::Result<()> {
        self.guard_write()?;
        let text = fs::read_to_string(self.meta_path(uuid))?;
        let mut meta: Value = serde_json::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        if meta
            .get("parent")
            .and_then(|x| x.as_str())
            .map(|p| p.is_empty() || p == "trash")
            .unwrap_or(true)
        {
            meta["parent"] = json!("");
        }
        meta["deleted"] = json!(false);
        self.write_json(&self.meta_path(uuid), &meta)
    }

    /// 复制一篇文档，返回新 uuid。
    ///
    /// 官方 `LibraryController.cloneEntries`：只复制到原文档所在的文件夹。
    /// 命名规则是"重名就加序号"：`a` → `a 2` → `a 3`（复制副本 `a 2` 时接着往后排）。
    /// `.metadata` 换新 uuid、`visibleName` 换掉，其余伴生文件
    /// （`.content` / `.pagedata` / `.pdf` / `.epub` / …）与页目录整个拷。
    pub fn duplicate(&self, uuid: &str) -> io::Result<String> {
        let text = fs::read_to_string(self.meta_path(uuid))?;
        let mut meta: Value = serde_json::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        // 文件夹不复制（官方多选里 Duplicate 只对 Document 出现）
        if meta.get("type").and_then(|x| x.as_str()) == Some("CollectionType") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "文件夹不能复制",
            ));
        }
        let parent = meta
            .get("parent")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let name = meta
            .get("visibleName")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let name = self.next_copy_name(&name, &parent);
        let new = new_uuid("doc");

        // 先拷目录和伴生文件、最后写 `.metadata`：中途失败不会留下一条
        // "看起来完整"的半成品；失败时把已经拷出来的东西清掉。
        let copied = self.copy_document_files(uuid, &new);
        if let Err(e) = copied {
            self.remove_entry_files(&new);
            return Err(e);
        }

        meta["visibleName"] = json!(name);
        meta["deleted"] = json!(false);
        meta["lastModified"] = json!(now_ms().to_string());
        meta["metadatamodified"] = json!(true);
        if let Err(e) = self.write_json(&self.meta_path(&new), &meta) {
            self.remove_entry_files(&new);
            return Err(e);
        }
        Ok(new)
    }

    /// 把条目移到另一个文件夹（`parent` 传 `""` = 根目录）。
    pub fn move_to(&self, uuid: &str, parent: &str) -> io::Result<()> {
        if uuid == parent {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "不能把文件夹移到自己里面",
            ));
        }
        let text = fs::read_to_string(self.meta_path(uuid))?;
        let mut meta: Value = serde_json::from_str(&text)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        meta["parent"] = json!(parent);
        meta["lastModified"] = json!(now_ms().to_string());
        meta["metadatamodified"] = json!(true);
        self.write_json(&self.meta_path(uuid), &meta)
    }

    /// 复制时下一个不撞名的名字：`a` → `a 2`，已有 `a 2` 就 `a 3`。
    fn next_copy_name(&self, name: &str, parent: &str) -> String {
        let base = copy_base(name);
        let mut taken: std::collections::BTreeSet<String> = self
            .entries(parent, false)
            .into_iter()
            .map(|b| b.name)
            .collect();
        // 回收站里的同名条目也算，免得恢复回来撞名
        for b in self.entries(parent, true) {
            if b.parent == parent {
                taken.insert(b.name);
            }
        }
        let mut n = 2u32;
        loop {
            let cand = format!("{base} {n}");
            if !taken.contains(&cand) {
                return cand;
            }
            n += 1;
        }
    }

    /// 拷 `<uuid>/` 页目录 + 根目录下所有 `<uuid>.*` 伴生文件（`.metadata` /
    /// `.content` 除外，后者由调用方按需拷）。
    fn copy_document_files(&self, uuid: &str, new: &str) -> io::Result<()> {
        let dir = self.root.join(uuid);
        if dir.is_dir() {
            copy_dir_all(&dir, &self.root.join(new))?;
        }
        let prefix = format!("{uuid}.");
        for e in fs::read_dir(&self.root)? {
            let p = e?.path();
            if !p.is_file() {
                continue;
            }
            let Some(fname) = p.file_name().and_then(|s| s.to_str()).map(String::from) else {
                continue;
            };
            let Some(rest) = fname.strip_prefix(&prefix) else {
                continue;
            };
            if rest == "metadata" || rest == "content" {
                continue;
            }
            fs::copy(&p, self.root.join(format!("{new}.{rest}")))?;
        }
        if self.content_path(uuid).is_file() {
            fs::copy(self.content_path(uuid), self.content_path(new))?;
        }
        Ok(())
    }

    /// 清掉复制失败留下的 `<uuid>` 目录和 `<uuid>.*` 伴生文件。
    fn remove_entry_files(&self, uuid: &str) {
        let _ = fs::remove_dir_all(self.root.join(uuid));
        let prefix = format!("{uuid}.");
        if let Ok(rd) = fs::read_dir(&self.root) {
            for e in rd.flatten() {
                if e.file_name().to_string_lossy().starts_with(&prefix) {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
    }

    /// 新建文件夹。
    pub fn create_folder(&self, name: &str, parent: &str) -> io::Result<String> {
        self.guard_write()?;
        let uuid = new_uuid("dir");
        let meta = json!({
            "deleted": false,
            "lastModified": now_ms().to_string(),
            "metadatamodified": true,
            "modified": true,
            "parent": parent,
            "pinned": false,
            "synced": false,
            "type": "CollectionType",
            "version": 0,
            "visibleName": name,
        });
        self.write_json(&self.meta_path(&uuid), &meta)?;
        self.write_json(&self.content_path(&uuid), &json!({}))?;
        Ok(uuid)
    }

    /// 这一本的所有页 id，按显示顺序。
    fn texts_path(&self, uuid: &str, idx: usize) -> Option<std::path::PathBuf> {
        let ids = self.page_ids(uuid);
        let id = ids.get(idx)?;
        Some(self.root.join(uuid).join(format!("texts-{id}.json")))
    }

    pub fn load_texts(&self, uuid: &str, idx: usize) -> Vec<TextItem> {
        let Some(p) = self.texts_path(uuid, idx) else {
            return Vec::new();
        };
        let Ok(s) = fs::read_to_string(&p) else {
            return Vec::new();
        };
        serde_json::from_str(&s).unwrap_or_default()
    }

    pub fn save_texts(&self, uuid: &str, idx: usize, v: &[TextItem]) -> io::Result<()> {
        self.guard_write()?;
        let Some(p) = self.texts_path(uuid, idx) else {
            return Ok(());
        };
        if v.is_empty() {
            let _ = fs::remove_file(&p);
            return Ok(());
        }
        fs::write(&p, serde_json::to_vec_pretty(v).unwrap_or_default())
    }

    pub fn page_ids(&self, uuid: &str) -> Vec<String> {
        let c = self.read_content(uuid);
        let mut out = Vec::new();
        if let Some(arr) = c.pointer("/cPages/pages").and_then(|x| x.as_array()) {
            for p in arr {
                if p.get("deleted").and_then(|d| d.as_bool()).unwrap_or(false) {
                    continue;
                }
                if let Some(id) = p.get("id").and_then(|x| x.as_str()) {
                    out.push(id.to_string());
                }
            }
        }
        if out.is_empty() {
            if let Some(id) = self.first_page_id(uuid) {
                out.push(id);
            }
        }
        out
    }

    /// 读一页并解析；命中 [`ParsedPages`] 就直接给副本（翻页/重绘不用再 parse）。
    ///
    /// 解析失败包成 `InvalidData`，让调用方能分开报"读失败"和"解析失败"；
    /// 解析失败的页**不进缓存**（别把空页当成这一页的内容）。
    fn read_page(&self, uuid: &str, page_id: &str) -> io::Result<RmPage> {
        let path = self.page_path(uuid, page_id);
        let key = format!("{uuid}/{page_id}");
        let meta = fs::metadata(&path).ok();
        if let Some(meta) = &meta {
            if let Some(p) = self
                .pages
                .borrow_mut()
                .get(&key, meta.len(), meta.modified().ok())
            {
                return Ok(p);
            }
        }
        let bytes = fs::read(&path)?;
        match RmPage::parse(&bytes) {
            Ok(p) => {
                if let Some(meta) = &meta {
                    self.pages
                        .borrow_mut()
                        .put(&key, meta.len(), meta.modified().ok(), p.clone());
                }
                Ok(p)
            }
            Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
        }
    }

    /// 第 `idx` 页（越界就是空白页，不 panic）。
    pub fn load_page_at(&self, uuid: &str, idx: usize) -> RmPage {
        let ids = self.page_ids(uuid);
        let Some(page) = ids.get(idx) else {
            return RmPage::default();
        };
        match self.read_page(uuid, page) {
            Ok(p) => p,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                eprintln!("[doc] 解析 {uuid} 第{idx}页 失败：{e}");
                RmPage::default()
            }
            Err(e) => {
                // 新建但没写过的页没有 .rm，这不算错误，当空白页
                if e.kind() != io::ErrorKind::NotFound {
                    eprintln!("[doc] 读 {uuid} 第{idx}页 失败：{e}");
                }
                let mut p = RmPage::default();
                p.author = uuid_bytes(uuid);
                p
            }
        }
    }


    /// 落盘第 `idx` 页。**不改 pageCount**，除非 `.content` 里没有。
    pub fn save_page_at(&self, uuid: &str, idx: usize, page: &RmPage) -> io::Result<()> {
        self.guard_write()?;
        let mut ids = self.page_ids(uuid);
        if ids.is_empty() {
            ids.push(new_uuid("page"));
        }
        let page_id = ids
            .get(idx)
            .cloned()
            .unwrap_or_else(|| new_uuid("page"));

        let page_dir = self.root.join(uuid);
        fs::create_dir_all(&page_dir)?;
        let bytes = page.serialize();
        fs::write(self.page_path(uuid, &page_id), &bytes)?;
        self.pages.borrow_mut().invalidate(&format!("{uuid}/{page_id}"));

        let mut content = self.read_content(uuid);
        if content.is_null() {
            content = default_content(uuid, &page_id);
        }
        // 页删光了（0 页，官方允许空文档）之后又落笔：把这一页重新登记回
        // `cPages.pages`，否则墨迹写进了 `<pageid>.rm`，`.content` 里却没有它，
        // 翻回来就没了。
        let empty = content
            .pointer("/cPages/pages")
            .and_then(|x| x.as_array())
            .map(|a| a.is_empty())
            .unwrap_or(true);
        if empty {
            content["cPages"] = json!({ "pages": [{
                "id": page_id,
                "idx": { "timestamp": "1:1", "value": "" },
                "template": { "timestamp": "1:1", "value": self.pagedata_template(uuid) },
            }]});
        }
        if empty || content.get("pageCount").and_then(|x| x.as_u64()).is_none() {
            content["pageCount"] = json!(ids.len());
        }
        content["sizeInBytes"] = json!(bytes.len().to_string());
        self.write_json(&self.content_path(uuid), &content)?;

        let mut meta = self.read_meta(uuid);
        meta["lastModified"] = json!(now_ms().to_string());
        meta["lastOpenedPage"] = json!(idx);
        self.write_json(&self.meta_path(uuid), &meta)?;
        Ok(())
    }

    /// 在第 `after` 页之后插入一张空白页，返回新页的序号。
    /// 复制第 `idx` 页（含墨迹和文字），返回新页序号
    pub fn duplicate_page(&self, uuid: &str, idx: usize) -> io::Result<usize> {
        self.guard_write()?;
        let mut content = self.read_content(uuid);
        let Some(pages) = content["cPages"]["pages"].as_array().cloned() else {
            return Ok(idx);
        };
        let Some(src) = pages.get(idx) else {
            return Ok(idx);
        };
        let src_id = src
            .get("id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let id = new_uuid("page");
        std::fs::create_dir_all(self.root.join(uuid))?;
        let from = self.page_path(uuid, &src_id);
        if from.exists() {
            fs::copy(&from, self.page_path(uuid, &id))?;
        }
        let tfrom = self.root.join(uuid).join(format!("texts-{src_id}.json"));
        if tfrom.exists() {
            let _ = fs::copy(&tfrom, self.root.join(uuid).join(format!("texts-{id}.json")));
        }
        let entry = json!({
            "id": id,
            "idx": { "timestamp": "1:1", "value": "" },
            "template": src
                .get("template")
                .cloned()
                .unwrap_or_else(|| json!({ "timestamp": "1:1", "value": "Blank" })),
        });
        let at = (idx + 1).min(pages.len());
        if content["cPages"]["pages"].is_array() {
            content["cPages"]["pages"]
                .as_array_mut()
                .unwrap()
                .insert(at, entry);
        }
        let n = content["cPages"]["pages"].as_array().unwrap().len();
        content["pageCount"] = json!(n);
        self.write_json(&self.content_path(uuid), &content)?;
        Ok(at)
    }

    /// 确保 `.content` 里至少有 `n` 个页条目，返回 `true` = 真的写了盘。
    ///
    /// 电子书（PDF/EPUB/TXT/MOBI…）的页数由文档自己决定，不能插入/删除；
    /// 但每一页要能写批注，就得在 `cPages.pages` 里有对应的 page id
    /// （官方对 PDF 批注也是这么存的，每页一个 `.rm`）。
    ///
    /// **只补不删**：重排格式的页数会随字号变，多出来的页可能已经写满批注，
    /// 删掉就丢了。
    pub fn ensure_pages(&self, uuid: &str, n: usize) -> io::Result<bool> {
        self.guard_write()?;
        let mut content = self.read_content(uuid);
        if content.is_null() || n == 0 {
            return Ok(false);
        }
        let have = content
            .pointer("/cPages/pages")
            .and_then(|x| x.as_array())
            .map(|a| a.len())
            .unwrap_or(0);
        if have >= n {
            return Ok(false);
        }
        let template = self.pagedata_template(uuid);
        let mut pages = content
            .pointer("/cPages/pages")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        for _ in have..n {
            pages.push(json!({
                "id": new_uuid("page"),
                "idx": { "timestamp": "1:1", "value": "" },
                "template": { "timestamp": "1:1", "value": template },
            }));
        }
        if !content["cPages"].is_object() {
            content["cPages"] = json!({});
        }
        content["cPages"]["pages"] = json!(pages);
        content["pageCount"] = json!(n);
        self.write_json(&self.content_path(uuid), &content)?;
        Ok(true)
    }

    pub fn add_page(&self, uuid: &str, after: usize) -> io::Result<usize> {
        self.guard_write()?;
        let mut content = self.read_content(uuid);
        if content.is_null() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "没有 .content"));
        }
        let id = new_uuid("page");
        let idx = (after + 1).min(content["cPages"]["pages"]
            .as_array()
            .map(|a| a.len())
            .unwrap_or(0));
        let entry = json!({
            "id": id,
            "idx": { "timestamp": "1:1", "value": "" },
            // 新页沿用 `.pagedata` 里的模板（Notebook settings 选的那张）；
            // 老文档没有 `.pagedata` 时还是 Blank。
            "template": { "timestamp": "1:1", "value": self.pagedata_template(uuid) },
        });
        if content["cPages"]["pages"].is_array() {
            content["cPages"]["pages"]
                .as_array_mut()
                .unwrap()
                .insert(idx, entry);
        } else {
            content["cPages"] = json!({ "pages": [entry] });
        }
        let n = content["cPages"]["pages"].as_array().unwrap().len();
        content["pageCount"] = json!(n);
        self.write_json(&self.content_path(uuid), &content)?;

        std::fs::create_dir_all(self.root.join(uuid))?;
        let mut page = RmPage::default();
        page.author = uuid_bytes(uuid);
        fs::write(self.page_path(uuid, &id), page.serialize())?;

        let mut meta = self.read_meta(uuid);
        meta["lastModified"] = json!(now_ms().to_string());
        self.write_json(&self.meta_path(uuid), &meta)?;
        Ok(idx)
    }

    // === P10 begin ===
    /// 在第 `after` 页之后**新插一页**，把 `items` 作为文字块写到新页上。
    ///
    /// P10（手写转文字）用：官方转换结果是"结果新增一页（原页保留）"。
    /// 这里只做"新增页 + 落文字"两件事——`add_page` 只写新页的 `.rm`，
    /// 文字进 `texts-<新页id>.json`（`save_texts`），所以原页的 `.rm` 和
    /// 原页的文字一个字节都不会被碰。返回新页序号。
    pub fn insert_text_page(
        &self,
        uuid: &str,
        after: usize,
        items: &[TextItem],
    ) -> io::Result<usize> {
        let idx = self.add_page(uuid, after)?;
        self.save_texts(uuid, idx, items)?;
        Ok(idx)
    }
    // === P10 end ===

    /// 删除某一页（多页时才允许）。
    pub fn delete_page(&self, uuid: &str, idx: usize) -> io::Result<()> {
        self.guard_write()?;
        let mut content = self.read_content(uuid);
        let Some(arr) = content["cPages"]["pages"].as_array_mut() else {
            return Ok(());
        };
        if arr.len() <= 1 || idx >= arr.len() {
            return Ok(());
        }
        let entry = arr.remove(idx);
        let n = arr.len();
        content["pageCount"] = json!(n);
        self.write_json(&self.content_path(uuid), &content)?;
        if let Some(id) = entry.get("id").and_then(|x| x.as_str()) {
            let _ = fs::remove_file(self.page_path(uuid, id));
            self.pages.borrow_mut().invalidate(&format!("{uuid}/{id}"));
        }
        Ok(())
    }

    /// 把第 `from` 页移到 `to`（0 基，`to` 是移动后的目标下标）。
    ///
    /// 只改 `.content` 里 `cPages.pages` 的顺序 + `pageCount`，**不动 `.rm` 文件**：
    /// `.rm` 是按 pageid 命名的，显示顺序只由 `cPages.pages` 决定。
    /// 越界返回 `InvalidInput`（拿不到 `cPages.pages` 是 `NotFound`）。
    pub fn move_page(&self, uuid: &str, from: usize, to: usize) -> io::Result<()> {
        self.guard_write()?;
        let mut content = self.read_content(uuid);
        {
            let Some(arr) = content["cPages"]["pages"].as_array_mut() else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "没有 cPages.pages",
                ));
            };
            let n = arr.len();
            if from >= n || to >= n {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("页号越界：{from} → {to}（共 {n} 页）"),
                ));
            }
            if from == to {
                return Ok(());
            }
            let entry = arr.remove(from);
            arr.insert(to.min(n - 1), entry);
            content["pageCount"] = json!(n);
        }
        self.write_json(&self.content_path(uuid), &content)
    }

    /// 删多页，返回真删掉几页。
    ///
    /// 下标按 `cPages.pages` 的原始下标处理，越界的跳过；内部**倒序**删，
    /// 这样前面的下标不会因为删掉后面的页而错位。允许删空（官方也允许空文档）。
    pub fn delete_pages(&self, uuid: &str, idx: &[usize]) -> io::Result<usize> {
        self.guard_write()?;
        let mut content = self.read_content(uuid);
        let mut gone: Vec<String> = Vec::new();
        let mut n = 0usize;
        {
            let Some(arr) = content["cPages"]["pages"].as_array_mut() else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    "没有 cPages.pages",
                ));
            };
            let mut idx: Vec<usize> = idx.iter().copied().filter(|&i| i < arr.len()).collect();
            idx.sort_unstable();
            idx.dedup();
            for &i in idx.iter().rev() {
                let entry = arr.remove(i);
                if let Some(id) = entry.get("id").and_then(|x| x.as_str()) {
                    gone.push(id.to_string());
                }
                n += 1;
            }
            content["pageCount"] = json!(arr.len());
        }
        self.write_json(&self.content_path(uuid), &content)?;
        for id in gone {
            let _ = fs::remove_file(self.page_path(uuid, &id));
            self.pages.borrow_mut().invalidate(&format!("{uuid}/{id}"));
        }
        let mut meta = self.read_meta(uuid);
        meta["lastModified"] = json!(now_ms().to_string());
        self.write_json(&self.meta_path(uuid), &meta)?;
        Ok(n)
    }

    /// 给多页打标签（`on=false` 是去掉），返回真正改动了几页。
    ///
    /// 落在 `.content.pageTags`（复用 [`Library::set_page_tags`]），
    /// 已经是要改的状态的页跳过、越界下标跳过。
    pub fn tag_pages(&self, uuid: &str, idx: &[usize], tag: &str, on: bool) -> io::Result<usize> {
        self.guard_write()?;
        let c = self.read_content(uuid);
        let Some(arr) = c.pointer("/cPages/pages").and_then(|x| x.as_array()) else {
            return Ok(0);
        };
        let ids: Vec<String> = idx
            .iter()
            .filter_map(|&i| {
                arr.get(i)?
                    .get("id")?
                    .as_str()
                    .map(String::from)
            })
            .collect();
        let current = self.page_tags(uuid);
        let mut n = 0;
        for id in ids {
            let mut tags = current
                .iter()
                .find(|(p, _)| p == &id)
                .map(|(_, t)| t.clone())
                .unwrap_or_default();
            if tags.iter().any(|t| t == tag) == on {
                continue;
            }
            if on {
                tags.push(tag.to_string());
            } else {
                tags.retain(|t| t != tag);
            }
            self.set_page_tags(uuid, &id, &tags)?;
            n += 1;
        }
        Ok(n)
    }

    /// 全部标签及出现次数（`.content.tags`）。
    /// 某个文档的标签（`.content.tags`）
    pub fn tags_of(&self, uuid: &str) -> Vec<String> {
        let c = self.read_content(uuid);
        c.get("tags")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 覆盖某个文档的标签
    pub fn set_tags(&self, uuid: &str, tags: &[String]) -> io::Result<()> {
        self.guard_write()?;
        let mut c = self.read_content(uuid);
        c["tags"] = json!(tags);
        self.write_json(&self.content_path(uuid), &c)
    }

    /// 页面级标签（`.content.pageTags`）：`(pageId, tags)`，保持文件里的顺序。
    ///
    /// 官方结构：`[{"pageId": "<uuid>", "tags": ["a", "b"]}]`。
    pub fn page_tags(&self, uuid: &str) -> Vec<(String, Vec<String>)> {
        let c = self.read_content(uuid);
        let Some(arr) = c.get("pageTags").and_then(|x| x.as_array()) else {
            return Vec::new();
        };
        arr.iter()
            .filter_map(|e| {
                let id = e.get("pageId").and_then(|x| x.as_str())?;
                let tags = e
                    .get("tags")
                    .and_then(|x| x.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|t| t.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                Some((id.to_string(), tags))
            })
            .collect()
    }

    /// 覆盖某一页的标签：这一页已经有记录就改掉，没有就追加。
    pub fn set_page_tags(&self, uuid: &str, page_id: &str, tags: &[String]) -> io::Result<()> {
        self.guard_write()?;
        let mut c = self.read_content(uuid);
        if !c.is_object() {
            c = json!({});
        }
        if !c.get("pageTags").map(|x| x.is_array()).unwrap_or(false) {
            c["pageTags"] = json!([]);
        }
        let arr = c["pageTags"].as_array_mut().expect("pageTags 刚补过数组");
        let mut found = false;
        for e in arr.iter_mut() {
            if e.get("pageId").and_then(|x| x.as_str()) == Some(page_id) {
                e["tags"] = json!(tags);
                found = true;
                break;
            }
        }
        if !found {
            arr.push(json!({ "pageId": page_id, "tags": tags }));
        }
        self.write_json(&self.content_path(uuid), &c)
    }

    /// 全库所有带标签的页面：`(文档 uuid, pageId, tags)`。
    pub fn all_page_tags(&self) -> Vec<(String, String, Vec<String>)> {
        let mut out = Vec::new();
        let Ok(dir) = fs::read_dir(&self.root) else {
            return out;
        };
        for e in dir.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("content") {
                continue;
            }
            let Some(uuid) = p.file_stem().and_then(|s| s.to_str()).map(String::from) else {
                continue;
            };
            for (page, tags) in self.page_tags(&uuid) {
                if !tags.is_empty() {
                    out.push((uuid.clone(), page, tags));
                }
            }
        }
        out
    }

    /// 全库用到的标签名（文档级 + 页面级），排序去重。官方 `Library.tagLabels()`。
    pub fn tag_labels(&self) -> Vec<String> {
        let mut set: std::collections::BTreeSet<String> = Default::default();
        let Ok(dir) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        for e in dir.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("content") {
                continue;
            }
            let Some(uuid) = p.file_stem().and_then(|s| s.to_str()).map(String::from) else {
                continue;
            };
            for t in self.tags_of(&uuid) {
                set.insert(t);
            }
            for (_, tags) in self.page_tags(&uuid) {
                for t in tags {
                    set.insert(t);
                }
            }
        }
        set.into_iter().collect()
    }

    /// 标签改名：文档级和页面级一起改，返回动过的文档数。
    pub fn rename_tag(&self, old: &str, new: &str) -> io::Result<usize> {
        self.guard_write()?;
        let mut n = 0;
        let Ok(dir) = fs::read_dir(&self.root) else {
            return Ok(0);
        };
        for e in dir.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("content") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&p) else {
                continue;
            };
            let Ok(mut v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let mut changed = false;
            if let Some(arr) = v.get_mut("tags").and_then(|x| x.as_array_mut()) {
                for t in arr.iter_mut() {
                    if t.as_str() == Some(old) {
                        *t = json!(new);
                        changed = true;
                    }
                }
            }
            if let Some(pages) = v.get_mut("pageTags").and_then(|x| x.as_array_mut()) {
                for page in pages.iter_mut() {
                    if let Some(arr) = page.get_mut("tags").and_then(|x| x.as_array_mut()) {
                        for t in arr.iter_mut() {
                            if t.as_str() == Some(old) {
                                *t = json!(new);
                                changed = true;
                            }
                        }
                    }
                }
            }
            if changed {
                if let Some(arr) = v.get_mut("tags").and_then(|x| x.as_array_mut()) {
                    dedup_tags(arr);
                }
                if let Some(pages) = v.get_mut("pageTags").and_then(|x| x.as_array_mut()) {
                    for page in pages.iter_mut() {
                        if let Some(arr) = page.get_mut("tags").and_then(|x| x.as_array_mut()) {
                            dedup_tags(arr);
                        }
                    }
                }
                self.write_json(&p, &v)?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// 带某个标签的所有文档
    /// 从所有文档里去掉某个标签（文档级 + 页面级），返回改动了几篇
    pub fn delete_tag(&self, tag: &str) -> io::Result<usize> {
        self.guard_write()?;
        let mut n = 0;
        let Ok(dir) = fs::read_dir(&self.root) else {
            return Ok(0);
        };
        for e in dir.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("content") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&p) else {
                continue;
            };
            let Ok(mut v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let mut changed = false;
            if let Some(arr) = v.get_mut("tags").and_then(|x| x.as_array_mut()) {
                let before = arr.len();
                arr.retain(|t| t.as_str() != Some(tag));
                changed |= arr.len() != before;
            }
            if let Some(pages) = v.get_mut("pageTags").and_then(|x| x.as_array_mut()) {
                for page in pages.iter_mut() {
                    if let Some(arr) = page.get_mut("tags").and_then(|x| x.as_array_mut()) {
                        let before = arr.len();
                        arr.retain(|t| t.as_str() != Some(tag));
                        changed |= arr.len() != before;
                    }
                }
            }
            if changed {
                self.write_json(&p, &v)?;
                n += 1;
            }
        }
        Ok(n)
    }

    pub fn tagged(&self, tag: &str) -> Vec<Book> {
        let mut out = Vec::new();
        for b in self.entries("", false) {
            if b.doc_type != "DocumentType" {
                continue;
            }
            if self.tags_of(&b.uuid).iter().any(|t| t == tag) {
                out.push(b);
            }
        }
        out
    }

    pub fn tags(&self) -> Vec<(String, usize)> {
        let mut map: std::collections::BTreeMap<String, usize> = Default::default();
        let Ok(dir) = fs::read_dir(&self.root) else {
            return Vec::new();
        };
        for e in dir.flatten() {
            let p = e.path();
            if p.extension().and_then(|s| s.to_str()) != Some("content") {
                continue;
            }
            let Ok(text) = fs::read_to_string(&p) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if let Some(arr) = v.get("tags").and_then(|x| x.as_array()) {
                for t in arr {
                    if let Some(s) = t.as_str() {
                        *map.entry(s.to_string()).or_insert(0) += 1;
                    }
                }
            }
        }
        map.into_iter().collect()
    }

    /// 按名字搜索（大小写不敏感）。空串返回全部。
    pub fn search(&self, q: &str) -> Vec<Book> {
        let q = q.to_lowercase();
        let mut v = self.entries("", false);
        if !q.is_empty() {
            v.retain(|b| b.name.to_lowercase().contains(&q));
        }
        v
    }


    /// 库根目录里 `<uuid>.<ext>` 这种伴生文件是什么格式。
    ///
    /// `.content` 被第三方改写过（`fileType` 变 `notebook`）时用它把格式认回来：
    /// 笔记本体在 `<uuid>/<page>.rm` 里，根目录下不会有 pdf/epub/mobi 这种伴生文件，
    /// 所以扫到就是电子书。
    fn sidecar_format(&self, uuid: &str) -> Option<Format> {
        let prefix = format!("{uuid}.");
        let rd = fs::read_dir(&self.root).ok()?;
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let Some(rest) = name.strip_prefix(&prefix) else {
                continue;
            };
            let f = Format::of_ext(rest);
            if f.is_known() {
                return Some(f);
            }
        }
        None
    }

    fn read_content(&self, uuid: &str) -> Value {
        fs::read_to_string(self.content_path(uuid))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .unwrap_or(Value::Null)
    }

    /// 第一页的 page id（官方存在 `.content` 的 `cPages.pages[0].id`）。
    fn first_page_id(&self, uuid: &str) -> Option<String> {
        let c = self.read_content(uuid);
        c.pointer("/cPages/pages/0/id")
            .and_then(|x| x.as_str())
            .map(String::from)
    }

    pub fn load_page(&self, uuid: &str) -> RmPage {
        let Some(page) = self.first_page_id(uuid) else {
            return RmPage::default();
        };
        match self.read_page(uuid, &page) {
            Ok(p) => p,
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                eprintln!("[doc] 解析 {} 失败：{e}", uuid);
                RmPage::default()
            }
            Err(e) => {
                eprintln!("[doc] 读 {} 失败：{e}", uuid);
                RmPage::default()
            }
        }
    }

    /// 落盘一页，并同步 `.content` 的 pageCount / sizeInBytes
    /// 与 `.metadata` 的 lastModified —— 官方靠这三个字段判断"要不要重载"。
    pub fn save_page(&self, uuid: &str, page: &RmPage) -> io::Result<()> {
        self.guard_write()?;
        let page_id = self
            .first_page_id(uuid)
            .unwrap_or_else(|| new_uuid("page"));

        let page_dir = self.root.join(uuid);
        fs::create_dir_all(&page_dir)?;
        let bytes = page.serialize();
        fs::write(self.page_path(uuid, &page_id), &bytes)?;
        self.pages.borrow_mut().invalidate(&format!("{uuid}/{page_id}"));

        let mut content = self.read_content(uuid);
        if content.is_null() {
            content = default_content(uuid, &page_id);
        }
        content["pageCount"] = json!(1);
        content["sizeInBytes"] = json!(bytes.len().to_string());
        self.write_json(&self.content_path(uuid), &content)?;

        let mut meta = self.read_meta(uuid);
        meta["lastModified"] = json!(now_ms().to_string());
        meta["modified"] = json!(true);
        self.write_json(&self.meta_path(uuid), &meta)?;
        Ok(())
    }

    fn read_meta(&self, uuid: &str) -> Value {
        fs::read_to_string(self.meta_path(uuid))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .unwrap_or(Value::Null)
    }

    fn write_json(&self, path: &Path, v: &Value) -> io::Result<()> {
        // 官方是 4 空格缩进；格式差异不影响解析，但保持接近便于人工比对。
        let s = serde_json::to_string_pretty(v)?;
        fs::write(path, s)
    }

    /// 新建一本空白笔记本。
    /// 按 uuid 找一条（不管它在哪个文件夹里）
    pub fn find(&self, uuid: &str) -> Option<Book> {
        let meta = self.read_meta(uuid);
        let parent = meta
            .get("parent")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let deleted = meta
            .get("deleted")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        self.entries(&parent, deleted)
            .into_iter()
            .find(|b| b.uuid == uuid)
    }

    pub fn create_book(&self, name: &str) -> io::Result<String> {
        self.create_book_in(name, "")
    }

    pub fn create_book_in(&self, name: &str, parent: &str) -> io::Result<String> {
        self.guard_write()?;
        let uuid = new_uuid("doc");
        let page_id = new_uuid("page");
        fs::create_dir_all(self.root.join(&uuid))?;

        let meta = json!({
            "deleted": false,
            "lastModified": now_ms().to_string(),
            "lastOpened": now_ms().to_string(),
            "lastOpenedPage": 0,
            "metadatamodified": true,
            "modified": true,
            "parent": parent,
            "pinned": false,
            "synced": false,
            "type": "DocumentType",
            "version": 0,
            "visibleName": name,
        });
        self.write_json(&self.meta_path(&uuid), &meta)?;
        self.write_json(&self.content_path(&uuid), &default_content(&uuid, &page_id))?;
        fs::write(self.pagedata_path(&uuid), "Blank\n")?;

        let mut page = RmPage::default();
        page.author = uuid_bytes(&uuid);
        fs::write(self.page_path(&uuid, &page_id), page.serialize())?;
        Ok(uuid)
    }

    /// 按官方 New notebook 对话框的选项建一本：`first_page` = 封面显示首页还是上次那页，
    /// `template` 是 `.pagedata` 里的模板名（Blank / Lines medium / ...）。
    pub fn create_book_tpl(
        &self,
        name: &str,
        parent: &str,
        first_page: bool,
        template: &str,
    ) -> io::Result<String> {
        self.guard_write()?;
        let uuid = self.create_book_in(name, parent)?;
        let mut c = self.read_content(&uuid);
        // coverPageNumber = -1 表示"显示上次打开的页"，0 表示"始终第 1 页"
        c["coverPageNumber"] = json!(if first_page { 0 } else { -1 });
        if let Some(pages) = c
            .get_mut("cPages")
            .and_then(|p| p.get_mut("pages"))
            .and_then(|p| p.as_array_mut())
        {
            for pg in pages.iter_mut() {
                pg["template"] = json!({ "timestamp": "1:1", "value": template });
            }
        }
        self.write_json(&self.content_path(&uuid), &c)?;
        fs::write(self.pagedata_path(&uuid), format!("{template}\n"))?;
        Ok(uuid)
    }

    /// `.pagedata` 里记的模板名（官方是文档级的模板文件名）。
    fn pagedata_template(&self, uuid: &str) -> String {
        fs::read_to_string(self.pagedata_path(uuid))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Blank".to_string())
    }

    /// 改某一页的模板（官方 `DocumentController.setTemplateForPage(entry, page, tpl)`）。
    ///
    /// 同时把模板名写回 `.pagedata`：官方的 `.pagedata` 是文档级模板，
    /// `New page` 新增的页会沿用（见 [`Library::add_page`]）。
    pub fn set_page_template(&self, uuid: &str, idx: usize, template: &str) -> io::Result<()> {
        self.guard_write()?;
        let mut c = self.read_content(uuid);
        let Some(page) = c
            .pointer_mut("/cPages/pages")
            .and_then(|v| v.as_array_mut())
            .and_then(|a| a.get_mut(idx))
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("第 {idx} 页不存在"),
            ));
        };
        page["template"] = json!({ "timestamp": "1:1", "value": template });
        self.write_json(&self.content_path(uuid), &c)?;
        fs::write(self.pagedata_path(uuid), format!("{template}\n"))
    }

    /// 封面显示哪一页（`.content.coverPageNumber`：-1 = 上次打开的页，0 = 第一页）。
    pub fn set_cover_page(&self, uuid: &str, first_page: bool) -> io::Result<()> {
        self.guard_write()?;
        let mut c = self.read_content(uuid);
        if c.is_null() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "没有 .content"));
        }
        c["coverPageNumber"] = json!(if first_page { 0 } else { -1 });
        self.write_json(&self.content_path(uuid), &c)
    }

    /// 文档方向（`.content.orientation` + `customZoomOrientation`）。
    pub fn set_orientation(&self, uuid: &str, portrait: bool) -> io::Result<()> {
        self.guard_write()?;
        let mut c = self.read_content(uuid);
        if c.is_null() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "没有 .content"));
        }
        let v = if portrait { "portrait" } else { "landscape" };
        c["orientation"] = json!(v);
        c["customZoomOrientation"] = json!(v);
        self.write_json(&self.content_path(uuid), &c)
    }

    /// 某一页的模板文件名（`.content.cPages.pages[idx].template.value`）。
    pub fn page_template(&self, uuid: &str, idx: usize) -> String {
        self.read_content(uuid)
            .pointer(&format!("/cPages/pages/{idx}/template/value"))
            .and_then(|v| v.as_str())
            .unwrap_or("Blank")
            .to_string()
    }

    /// 封面显示哪一页（`.content.coverPageNumber`）：-1 = 上次打开的页，0 = 第一页。
    pub fn cover_page(&self, uuid: &str) -> i32 {
        self.read_content(uuid)
            .get("coverPageNumber")
            .and_then(|v| v.as_i64())
            .map(|v| if v == 0 { 0 } else { -1 })
            .unwrap_or(-1)
    }

    /// 文档方向，缺省竖版。
    pub fn orientation(&self, uuid: &str) -> String {
        self.read_content(uuid)
            .get("orientation")
            .and_then(|v| v.as_str())
            .unwrap_or("portrait")
            .to_string()
    }

    /// 真的从磁盘上删掉（回收站里的 "Delete permanently"）。
    /// 官方会连 `.metadata`/`.content`/`.pagedata`/`.thumbnails`/页目录一起删。
    pub fn purge(&self, uuid: &str) -> io::Result<()> {
        // 只读模式下放行：官方提示原文就是"删掉一些文件就能恢复正常"
        // （`res_0686a15.qml:118`），拦住它等于让用户没法自救。
        for ext in ["metadata", "content", "pagedata", "local"] {
            let _ = fs::remove_file(self.root.join(format!("{uuid}.{ext}")));
        }
        let _ = fs::remove_dir_all(self.root.join(uuid));
        let _ = fs::remove_dir_all(self.root.join(format!("{uuid}.thumbnails")));
        Ok(())
    }

    pub fn delete_book(&self, uuid: &str) -> io::Result<()> {
        // 只读模式下放行，理由同 `purge`：这是"腾空间"的唯一入口。
        // 官方是"标记删除"，这里保持一致，别真的 rm。
        // parent 保留原值——回收站是按 deleted 标志查的，不是按目录；
        // 恢复时也靠它把条目放回原文件夹（见 `restore`）。
        let mut meta = self.read_meta(uuid);
        meta["deleted"] = json!(true);
        self.write_json(&self.meta_path(uuid), &meta)
    }
}

/// 递归拷目录（页目录里只有 `.rm` / 文字 sidecar，丢了就真丢了）。
fn copy_dir_all(src: &Path, dst: &Path) -> io::Result<()> {
    fs::create_dir_all(dst)?;
    for e in fs::read_dir(src)? {
        let e = e?;
        let to = dst.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_dir_all(&e.path(), &to)?;
        } else {
            fs::copy(e.path(), &to)?;
        }
    }
    Ok(())
}

/// 复制序号的基名：`a 2` / `a 3` 都还原成 `a`，普通名字原样返回。
/// 只认 ≥2 的纯数字后缀，`Chapter 2` 这种也会被当成副本序号（和官方
/// "重名加序号"的命名一致）。
fn copy_base(name: &str) -> &str {
    let name = name.trim_end();
    let Some(i) = name.rfind(' ') else {
        return name;
    };
    let tail = &name[i + 1..];
    if tail.is_empty() || tail.len() > 3 || !tail.bytes().all(|b| b.is_ascii_digit()) {
        return name;
    }
    match tail.parse::<u32>() {
        Ok(n) if n >= 2 && i > 0 => &name[..i],
        _ => name,
    }
}

fn default_content(uuid: &str, page_id: &str) -> Value {
    json!({
        "cPages": {
            "lastOpened": { "timestamp": "1:1", "value": page_id },
            "original": { "timestamp": "0:0", "value": -1 },
            "pages": [{
                "id": page_id,
                "idx": { "timestamp": "1:1", "value": "ba" },
                "template": { "timestamp": "1:1", "value": "Blank" },
            }],
            "uuids": [{ "first": format!("{:08x}-0000-4000-8000-000000000000", hash32(uuid)), "second": 1 }],
        },
        "coverPageNumber": -1,
        "customZoomCenterX": 0,
        "customZoomCenterY": 936,
        "customZoomOrientation": "portrait",
        "customZoomPageHeight": 1872,
        "customZoomPageWidth": 1404,
        "customZoomScale": 1,
        "documentMetadata": {},
        "dummyDocument": false,
        "extraMetadata": {
            "LastPen": "Finelinerv2",
            "LastFinelinerv2Color": "Black",
            "LastFinelinerv2Size": "2"
        },
        "fileType": "notebook",
        "fontName": "",
        "formatVersion": 2,
        "lineHeight": -1,
        "margins": 125,
        "orientation": "portrait",
        "pageCount": 1,
        "pageTags": [],
        "sizeInBytes": "0",
        "tags": [],
        "textAlignment": "justify",
        "textScale": 1,
        "transform": { "m11":1,"m12":0,"m13":0,"m21":0,"m22":1,"m23":0,"m31":0,"m32":0,"m33":1 },
        "zoomMode": "bestFit",
    })
}

/// 标签数组去重（保序）。改名把两个标签并成一个时用得上。
fn dedup_tags(arr: &mut Vec<Value>) {
    let mut seen: std::collections::BTreeSet<String> = Default::default();
    arr.retain(|t| match t.as_str() {
        Some(s) => seen.insert(s.to_string()),
        None => true,
    });
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn hash32(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// 文档 uuid 的前 16 字节当作者 uuid，这样同一本笔记的 `.rm` 作者稳定。
pub fn uuid_bytes(uuid: &str) -> [u8; 16] {
    let mut out = [0u8; 16];
    let hex: Vec<u8> = uuid
        .bytes()
        .filter(|b| b.is_ascii_hexdigit())
        .collect();
    for i in 0..16 {
        let hi = hex.get(i * 2).copied().unwrap_or(b'0');
        let lo = hex.get(i * 2 + 1).copied().unwrap_or(b'0');
        let v = |c: u8| -> u8 {
            match c {
                b'0'..=b'9' => c - b'0',
                b'a'..=b'f' => c - b'a' + 10,
                b'A'..=b'F' => c - b'A' + 10,
                _ => 0,
            }
        };
        out[i] = (v(hi) << 4) | v(lo);
    }
    out
}

/// 从 `/dev/urandom` 精确读 16 字节。
///
/// ⚠️ 绝对不能用 `fs::read("/dev/urandom")`：字符设备不会返回 EOF，
/// 它会一直读到进程被杀为止（实测把内存吃到几十 GB）。
fn random16() -> Option<[u8; 16]> {
    use std::io::Read;
    let mut f = fs::File::open("/dev/urandom").ok()?;
    let mut b = [0u8; 16];
    f.read_exact(&mut b).ok()?;
    Some(b)
}

/// 生成 uuid v4 风格的字符串。
pub fn new_uuid(kind: &str) -> String {
    let mut b = random16().unwrap_or([0u8; 16]);
    if b == [0u8; 16] {
        // 兜底：用时间打散
        let t = now_ms();
        for (i, slot) in b.iter_mut().enumerate() {
            *slot = ((t >> (i % 8 * 8)) & 0xff) as u8 ^ (i as u8 * 37);
        }
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let _ = kind;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_shape() {
        let u = new_uuid("doc");
        assert_eq!(u.len(), 36);
        assert_eq!(u.as_bytes()[14], b'4');
    }

    fn tmp_lib(tag: &str) -> Library {
        let dir = std::env::temp_dir().join(format!("rmdev-test-{tag}-{}", new_uuid("t")));
        std::fs::create_dir_all(&dir).unwrap();
        Library::with_root(&dir)
    }

    /// 被 xochitl 改写过的 `.content`（`fileType` 变 `notebook`）要能认回电子书：
    /// 否则书会变成"1 页空白笔记本"，整页缩略图空白。
    #[test]
    fn file_type_notebook_self_heals_from_sidecar() {
        let lib = tmp_lib("selffix");
        let id = lib.create_book("书").unwrap();
        // 造一本 MOBI：根目录放伴生文件，`.content` 里 fileType 被改成 notebook
        std::fs::write(lib.root.join(format!("{id}.mobi")), b"BOOKMOBI").unwrap();
        let p = lib.root.join(format!("{id}.content"));
        let mut v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        v["fileType"] = json!("notebook");
        std::fs::write(&p, v.to_string()).unwrap();

        let b = lib.entries("", false).into_iter().find(|b| b.uuid == id).unwrap();
        assert_eq!(b.format, Format::Mobi);
        assert_eq!(b.format.label(), "MOBI");

        // 没有伴生文件的真笔记本还是笔记本
        let nb = lib.create_book("笔记").unwrap();
        let b = lib.entries("", false).into_iter().find(|b| b.uuid == nb).unwrap();
        assert_eq!(b.format, Format::Rm);
    }

    #[test]
    fn folder_shows_up_as_collection() {
        let lib = tmp_lib("folder");
        let id = lib.create_folder("我的文件夹", "").unwrap();
        let v = lib.entries("", false);
        let f = v.iter().find(|b| b.uuid == id).expect("文件夹没出现在 entries 里");
        assert_eq!(f.doc_type, "CollectionType");
        assert_eq!(f.name, "我的文件夹");
        // books() 只返回文档，文件夹要被过滤掉
        assert!(lib.books().iter().all(|b| b.uuid != id));
    }

    /// 页面级标签：写进 `.content.pageTags`，同页覆盖不重复，读回顺序稳定。
    #[test]
    fn page_tags_roundtrip() {
        let lib = tmp_lib("pagetags");
        let id = lib.create_book("标签页").unwrap();
        let pages = lib.page_ids(&id);
        assert!(pages.len() >= 1);
        let p0 = pages[0].clone();

        lib.set_page_tags(&id, &p0, &["Ideas".into(), "Draft".into()]).unwrap();
        let got = lib.page_tags(&id);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, p0);
        assert_eq!(got[0].1, vec!["Ideas".to_string(), "Draft".to_string()]);

        // 同一页再写 = 覆盖，不是追加
        lib.set_page_tags(&id, &p0, &["Ideas".into()]).unwrap();
        let got = lib.page_tags(&id);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, vec!["Ideas".to_string()]);

        // 落盘结构就是官方的 `[{"pageId": .., "tags": [..]}]`
        let raw: Value =
            serde_json::from_str(&std::fs::read_to_string(lib.root.join(format!("{id}.content"))).unwrap())
                .unwrap();
        assert_eq!(raw["pageTags"][0]["pageId"], json!(p0));
        assert_eq!(raw["pageTags"][0]["tags"], json!(["Ideas"]));

        // 全库聚合和标签名
        assert_eq!(lib.all_page_tags().len(), 1);
        assert_eq!(lib.tag_labels(), vec!["Ideas".to_string()]);
    }

    /// 改名 / 删除要同时动到文档级和页面级标签。
    #[test]
    fn tag_rename_and_delete_cover_page_tags() {
        let lib = tmp_lib("tagrename");
        let a = lib.create_book("甲").unwrap();
        let b = lib.create_book("乙").unwrap();
        let pa = lib.page_ids(&a)[0].clone();
        let pb = lib.page_ids(&b)[0].clone();
        lib.set_tags(&a, &["旧名".into()]).unwrap();
        lib.set_page_tags(&a, &pa, &["旧名".into(), "留着".into()]).unwrap();
        lib.set_page_tags(&b, &pb, &["旧名".into()]).unwrap();

        assert_eq!(lib.rename_tag("旧名", "新名").unwrap(), 2);
        assert_eq!(lib.tags_of(&a), vec!["新名".to_string()]);
        assert_eq!(lib.page_tags(&a)[0].1, vec!["新名".to_string(), "留着".to_string()]);
        assert_eq!(lib.page_tags(&b)[0].1, vec!["新名".to_string()]);
        assert_eq!(lib.tag_labels(), vec!["新名".to_string(), "留着".to_string()]);

        assert_eq!(lib.delete_tag("新名").unwrap(), 2);
        assert!(lib.tags_of(&a).is_empty());
        assert_eq!(lib.page_tags(&b)[0].1, Vec::<String>::new());
        // 页面级里只剩「留着」那一页
        let tagged_pages = lib.all_page_tags();
        assert_eq!(tagged_pages.len(), 1);
        assert_eq!(tagged_pages[0].2, vec!["留着".to_string()]);
    }

    #[test]
    fn add_page_extends_and_saves() {
        let lib = tmp_lib("addpage");
        let id = lib.create_book("笔记本").unwrap();
        assert_eq!(lib.page_ids(&id).len(), 1);
        let at = lib.add_page(&id, 0).unwrap();
        assert_eq!(at, 1);
        assert_eq!(lib.page_ids(&id).len(), 2);
        // 新页是空页，但文件应该已经落盘
        let ids = lib.page_ids(&id);
        let p = lib.root.join(&id).join(format!("{}.rm", ids[1]));
        assert!(p.exists(), "新页的 .rm 没写出来");

        let mut page = lib.load_page_at(&id, 1);
        page.lines_mut().push(super::tests::line());
        lib.save_page_at(&id, 1, &page).unwrap();
        // 存第二页不能把页数改回 1
        let c = lib.read_content(&id);
        assert_eq!(c["pageCount"], 2);
        assert_eq!(lib.page_ids(&id).len(), 2);
    }

    // === P10 begin ===
    fn text(x: f32, y: f32, s: &str) -> TextItem {
        TextItem {
            x,
            y,
            size: 26.0,
            text: s.to_string(),
        }
    }

    /// P10：转文字的新页插在**原页之后**，原页的 `.rm` 与文字一个字节都不动。
    #[test]
    fn insert_text_page_keeps_the_original_page_untouched() {
        let lib = tmp_lib("ocrinsert");
        let id = lib.create_book("转文字").unwrap();
        // 原页：一笔墨 + 一段已有文字
        let mut p = lib.load_page_at(&id, 0);
        p.lines_mut().push(line());
        lib.save_page_at(&id, 0, &p).unwrap();
        lib.save_texts(&id, 0, &[text(10.0, 20.0, "原页")]).unwrap();
        let rm0 = lib.root.join(&id).join(format!("{}.rm", lib.page_ids(&id)[0]));
        let before = std::fs::read(&rm0).unwrap();

        let at = lib
            .insert_text_page(&id, 0, &[text(40.0, 120.0, "hello")])
            .unwrap();

        assert_eq!(at, 1, "新页要插在原页之后");
        assert_eq!(lib.page_ids(&id).len(), 2);
        assert_eq!(std::fs::read(&rm0).unwrap(), before, "原页 .rm 不许被改写");
        let old = lib.load_texts(&id, 0);
        assert_eq!(old.len(), 1);
        assert_eq!(old[0].text, "原页", "原页文字不许被覆盖");
        // 新页的墨迹是空的（不能把原页的墨复制过去）
        assert!(lib.load_page_at(&id, 1).visible_lines().next().is_none());
        let new = lib.load_texts(&id, 1);
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].text, "hello");
        assert_eq!(new[0].y, 120.0);
        // `.content` 的页数跟着涨
        assert_eq!(lib.read_content(&id)["pageCount"], 2);
    }

    /// P10：连续多段文字页要依次排在原页后面（第二段在 `after+2`）。
    #[test]
    fn insert_text_page_appends_consecutive_pages() {
        let lib = tmp_lib("ocrappend");
        let id = lib.create_book("多段").unwrap();
        let at1 = lib.insert_text_page(&id, 0, &[text(40.0, 120.0, "一")]).unwrap();
        // 第二段接在第一段后面（调用方这样算：原页 + 段序号）
        let at2 = lib.insert_text_page(&id, at1, &[text(40.0, 120.0, "二")]).unwrap();
        assert_eq!((at1, at2), (1, 2));
        assert_eq!(lib.page_ids(&id).len(), 3);
        assert_eq!(lib.load_texts(&id, 1)[0].text, "一");
        assert_eq!(lib.load_texts(&id, 2)[0].text, "二");
        // 空 items：页照插，但不会留 sidecar 文件
        let at3 = lib.insert_text_page(&id, at2, &[]).unwrap();
        assert_eq!(at3, 3);
        assert!(lib.load_texts(&id, 3).is_empty());
    }
    // === P10 end ===

    fn line() -> crate::pen::line::Line {
        crate::pen::line::Line {
            tool: crate::pen::line::Tool::Fineliner,
            color: crate::pen::line::Color::Black,
            thickness: 2.0,
            points: vec![crate::pen::line::Point {
                x: 10.0,
                y: 10.0,
                speed: 0.0,
                dir: 0.0,
                width: 2.0,
                pressure: 0.5,
            }],
            bounds: crate::pen::line::RectF {
                x: 9.0,
                y: 9.0,
                w: 2.0,
                h: 2.0,
            },
        }
    }

    #[test]
    fn rename_pin_and_restore() {
        let lib = tmp_lib("meta");
        let id = lib.create_book("旧名字").unwrap();
        lib.rename(&id, "新名字").unwrap();
        assert_eq!(lib.books()[0].name, "新名字");

        lib.set_pinned(&id, true).unwrap();
        assert!(lib.books()[0].pinned);

        lib.delete_book(&id).unwrap();
        assert!(lib.books().is_empty());
        assert_eq!(lib.entries("", true).len(), 1);

        lib.restore(&id).unwrap();
        assert_eq!(lib.books().len(), 1);
    }

    /// 复制文档：新 uuid、`visibleName` 按官方"重名加序号"排（`a` → `a 2` →
    /// `a 3`），`.content` / `.pagedata` / `.pdf` 伴生文件和页目录整个拷过来。
    #[test]
    fn duplicate_copies_files_and_numbers_names() {
        let lib = tmp_lib("dup");
        let id = lib.create_book("a").unwrap();
        let page_id = lib.page_ids(&id)[0].clone();
        std::fs::write(lib.root.join(format!("{id}.pdf")), b"%PDF-1.4").unwrap();
        std::fs::write(lib.root.join(format!("{id}.pagedata")), "Lines medium\n").unwrap();

        let d1 = lib.duplicate(&id).unwrap();
        assert_ne!(d1, id, "必须换新 uuid");
        assert_eq!(lib.find(&d1).unwrap().name, "a 2");
        assert_eq!(lib.find(&id).unwrap().name, "a", "原条目不能改名");
        assert_eq!(lib.find(&d1).unwrap().parent, "", "复制到同一文件夹");
        // 页目录整个拷：同一批 page id，`.rm` 字节一致
        assert_eq!(lib.page_ids(&d1), lib.page_ids(&id));
        let src = std::fs::read(lib.root.join(&id).join(format!("{page_id}.rm"))).unwrap();
        let dst = std::fs::read(lib.root.join(&d1).join(format!("{page_id}.rm"))).unwrap();
        assert_eq!(src, dst);
        // 伴生文件
        assert_eq!(
            std::fs::read(lib.root.join(format!("{d1}.pdf"))).unwrap(),
            b"%PDF-1.4"
        );
        assert_eq!(
            std::fs::read_to_string(lib.root.join(format!("{d1}.pagedata"))).unwrap(),
            "Lines medium\n"
        );
        assert!(lib.root.join(format!("{d1}.content")).exists());

        // 再从原条目复制 → `a 3`
        let d2 = lib.duplicate(&id).unwrap();
        assert_eq!(lib.find(&d2).unwrap().name, "a 3");
        // 复制副本 `a 2` → 接着往后排（a 2 / a 3 已占用）→ `a 4`
        let d3 = lib.duplicate(&d1).unwrap();
        assert_eq!(lib.find(&d3).unwrap().name, "a 4");

        // 副本有自己独立的 metadata
        lib.rename(&d1, "改过的副本").unwrap();
        assert_eq!(lib.find(&id).unwrap().name, "a");
    }

    /// 复制不存在的文档 → `NotFound`；文件夹不复制 → `InvalidInput`。
    #[test]
    fn duplicate_errors_on_missing_and_folder() {
        let lib = tmp_lib("duperr");
        let e = lib.duplicate("no-such-doc").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(!lib.root.join("no-such-doc.metadata").exists());

        let f = lib.create_folder("文件夹", "").unwrap();
        let e = lib.duplicate(&f).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);
        // 失败不能留下半成品
        assert_eq!(lib.entries("", false).len(), 1);
    }

    /// 从回收站恢复：`deleted` 清掉、`parent` 还回原文件夹（不是扔回根目录）。
    #[test]
    fn restore_returns_to_original_folder() {
        let lib = tmp_lib("restore2");
        let f = lib.create_folder("资料", "").unwrap();
        let id = lib.create_book_in("b", &f).unwrap();

        lib.delete_book(&id).unwrap();
        assert!(lib.books().is_empty());
        let trashed = lib
            .entries("", true)
            .into_iter()
            .find(|b| b.uuid == id)
            .expect("回收站里应该有它");
        assert_eq!(trashed.parent, f, "delete_book 不能把 parent 改成 trash");

        lib.restore(&id).unwrap();
        let b = lib.find(&id).unwrap();
        assert!(!b.deleted);
        assert_eq!(b.parent, f);
        assert!(lib.entries(&f, false).iter().any(|x| x.uuid == id));
        assert!(lib.entries("", false).iter().all(|x| x.uuid != id));
    }

    /// restore 的错误路径：没有 `.metadata` / JSON 坏了都报错，且不能凭空
    /// 写出一份只有 `deleted:false` 的假元数据；旧数据的 `parent:"trash"`
    /// 恢复到根目录。
    #[test]
    fn restore_error_paths_do_not_create_metadata() {
        let lib = tmp_lib("restoreerr");
        let e = lib.restore("no-such-doc").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::NotFound);
        assert!(
            !lib.root.join("no-such-doc.metadata").exists(),
            "失败不能造出 .metadata"
        );

        let id = lib.create_book("坏元数据").unwrap();
        std::fs::write(lib.root.join(format!("{id}.metadata")), "{ 不是 json").unwrap();
        let e = lib.restore(&id).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidData);

        // 旧数据（以前 delete_book 把 parent 写成 trash）：恢复到根目录
        let old = lib.create_book("旧条目").unwrap();
        std::fs::write(
            lib.root.join(format!("{old}.metadata")),
            json!({
                "type": "DocumentType",
                "visibleName": "旧条目",
                "parent": "trash",
                "deleted": true,
                "lastModified": "1",
            })
            .to_string(),
        )
        .unwrap();
        lib.restore(&old).unwrap();
        let b = lib.find(&old).unwrap();
        assert!(!b.deleted);
        assert_eq!(b.parent, "");
    }

    /// Move to folder：写 `.metadata.parent`；不能把文件夹移到自己里面。
    #[test]
    fn move_to_sets_parent_and_rejects_self() {
        let lib = tmp_lib("moveto");
        let f = lib.create_folder("目标", "").unwrap();
        let id = lib.create_book("要搬的").unwrap();
        lib.move_to(&id, &f).unwrap();
        assert_eq!(lib.find(&id).unwrap().parent, f);
        assert!(lib.entries(&f, false).iter().any(|b| b.uuid == id));

        let e = lib.move_to(&f, &f).unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::InvalidInput);

        lib.move_to(&id, "").unwrap();
        assert_eq!(lib.find(&id).unwrap().parent, "");
        assert_eq!(
            lib.move_to("no-such-doc", "").unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn search_is_case_insensitive() {
        let lib = tmp_lib("search");
        lib.create_book("Alpha 笔记").unwrap();
        lib.create_book("beta").unwrap();
        assert_eq!(lib.search("alpha").len(), 1);
        assert_eq!(lib.search("BETA").len(), 1);
        assert_eq!(lib.search("").len(), 2);
    }

    /// 搜索页的两条硬要求：空串列全部（回收站除外）、没命中就是空。
    #[test]
    fn search_lists_all_but_skips_trash() {
        let lib = tmp_lib("search2");
        lib.create_book("Quick sheets").unwrap();
        let id = lib.create_book("Draft").unwrap();
        lib.delete_book(&id).unwrap();
        assert_eq!(lib.search("").len(), 1);
        assert_eq!(lib.search("quick").len(), 1);
        assert!(lib.search("draft").is_empty(), "回收站里的不进搜索结果");
        assert!(lib.search("nothing here").is_empty());
    }

    #[test]
    fn uuid_bytes_roundtrip() {
        let b = uuid_bytes("1554f45c-b223-40d2-adf7-d436232fa56a");
        assert_eq!(b[0], 0x15);
        assert_eq!(b[1], 0x54);
        assert_eq!(b[15], 0x6a);
    }

    /// Notebook settings 落盘：当前页模板写 `.content` + `.pagedata`，
    /// 之后 `add_page` 出来的新页沿用；封面/方向写对应的 `.content` 字段。
    #[test]
    fn notebook_settings_persist() {
        let lib = tmp_lib("nbsettings");
        let id = lib.create_book("设置测试").unwrap();
        lib.set_page_template(&id, 0, "Lines medium").unwrap();
        let c = lib.read_content(&id);
        assert_eq!(
            c.pointer("/cPages/pages/0/template/value")
                .and_then(|v| v.as_str()),
            Some("Lines medium")
        );
        assert_eq!(
            std::fs::read_to_string(lib.root.join(format!("{id}.pagedata")))
                .unwrap()
                .trim(),
            "Lines medium"
        );

        // New page 继承 `.pagedata` 的模板（不是写死 Blank）
        let at = lib.add_page(&id, 0).unwrap();
        let c = lib.read_content(&id);
        assert_eq!(
            c.pointer(&format!("/cPages/pages/{at}/template/value"))
                .and_then(|v| v.as_str()),
            Some("Lines medium")
        );

        // 页码越界要报错，不能静默改第一页
        assert!(lib.set_page_template(&id, 9, "Grid medium").is_err());
        assert_eq!(
            lib.read_content(&id)
                .pointer("/cPages/pages/0/template/value")
                .and_then(|v| v.as_str()),
            Some("Lines medium")
        );

        // 封面：First page = 0，其余 = -1
        assert_eq!(lib.cover_page(&id), -1, "缺省是上次打开的页");
        lib.set_cover_page(&id, true).unwrap();
        assert_eq!(lib.read_content(&id)["coverPageNumber"], serde_json::json!(0));
        assert_eq!(lib.cover_page(&id), 0);
        lib.set_cover_page(&id, false).unwrap();
        assert_eq!(lib.read_content(&id)["coverPageNumber"], serde_json::json!(-1));
        assert_eq!(lib.cover_page(&id), -1);

        // 方向
        lib.set_orientation(&id, false).unwrap();
        let c = lib.read_content(&id);
        assert_eq!(c["orientation"], serde_json::json!("landscape"));
        assert_eq!(c["customZoomOrientation"], serde_json::json!("landscape"));
        assert_eq!(lib.orientation(&id), "landscape");
        lib.set_orientation(&id, true).unwrap();
        assert_eq!(
            lib.read_content(&id)["orientation"],
            serde_json::json!("portrait")
        );
        assert_eq!(lib.page_template(&id, 0), "Lines medium");
        assert_eq!(lib.page_template(&id, 99), "Blank");
    }

    /// 翻页热路径：同一页第 2/3 次读必须命中解析缓存；
    /// 自己保存或外部改文件后必须失效，不能拿到旧墨迹。
    #[test]
    fn page_parse_cache_hits_and_invalidates() {
        let lib = tmp_lib("pagecache");
        let id = lib.create_book("缓存测试").unwrap();
        let mut page = lib.load_page_at(&id, 0);
        page.lines_mut().push(line());
        lib.save_page_at(&id, 0, &page).unwrap();

        let base = lib.page_cache_stats();
        assert_eq!(lib.load_page_at(&id, 0).total_lines(), 1);
        let st = lib.page_cache_stats();
        assert_eq!(st.misses - base.misses, 1, "第一次要真解析");
        assert_eq!(st.hits - base.hits, 0);

        // 第 2/3 次同一页：都该命中
        let b = lib.load_page_at(&id, 0);
        let c = lib.load_page_at(&id, 0);
        assert_eq!(b.total_lines(), 1);
        assert_eq!(c.lines()[0].points[0].x, 10.0);
        let st = lib.page_cache_stats();
        assert_eq!(st.hits - base.hits, 2, "第 2/3 次必须命中缓存");
        assert_eq!(st.entries, 1);
        assert!(st.bytes > 0);

        // 走 save 写同一页 → 缓存失效
        let mut p2 = lib.load_page_at(&id, 0);
        p2.lines_mut().push(line());
        lib.save_page_at(&id, 0, &p2).unwrap();
        assert_eq!(
            lib.load_page_at(&id, 0).total_lines(),
            2,
            "保存后不能拿到旧页"
        );

        // 外部直接覆盖文件 → 长度/mtime 变化，元数据校验兜底
        let ids = lib.page_ids(&id);
        let path = lib.root.join(&id).join(format!("{}.rm", ids[0]));
        let mut ext = RmPage::default();
        for _ in 0..3 {
            ext.lines_mut().push(line());
        }
        std::fs::write(&path, ext.serialize()).unwrap();
        assert_eq!(
            lib.load_page_at(&id, 0).total_lines(),
            3,
            "外部改动要被元数据校验抓到"
        );
    }

    /// 攒一本 `n` 页的笔记，返回 `(库, uuid, 原始 page id 顺序)`。
    fn lib_with_pages(tag: &str, n: usize) -> (Library, String, Vec<String>) {
        let lib = tmp_lib(tag);
        let id = lib.create_book("多页").unwrap();
        for i in 0..n.saturating_sub(1) {
            lib.add_page(&id, i).unwrap();
        }
        let ids = lib.page_ids(&id);
        assert_eq!(ids.len(), n);
        (lib, id, ids)
    }

    /// `move_page` 只改 `cPages.pages` 顺序：`.rm` 文件一个都不许动。
    #[test]
    fn move_page_reorders_content_only() {
        let (lib, id, ids) = lib_with_pages("movepage", 5);

        lib.move_page(&id, 0, 4).unwrap();
        assert_eq!(
            lib.page_ids(&id),
            vec![ids[1].clone(), ids[2].clone(), ids[3].clone(), ids[4].clone(), ids[0].clone()]
        );
        assert_eq!(lib.read_content(&id)["pageCount"], json!(5));
        // 文件还在原来的 pageid 下，没有重命名/搬家
        for p in &ids {
            assert!(
                lib.root.join(&id).join(format!("{p}.rm")).exists(),
                "{p}.rm 被动了"
            );
        }

        // 从尾移到头
        lib.move_page(&id, 4, 0).unwrap();
        assert_eq!(lib.page_ids(&id), ids);

        // from == to 不变；越界报错且不动盘
        lib.move_page(&id, 2, 2).unwrap();
        assert_eq!(lib.page_ids(&id), ids);
        assert!(lib.move_page(&id, 0, 5).is_err());
        assert!(lib.move_page(&id, 5, 0).is_err());
        assert_eq!(lib.page_ids(&id), ids);
    }

    /// 删多页：倒序删不串页，`.rm` 跟着删，允许删空。
    #[test]
    fn delete_pages_removes_files_and_keeps_order() {
        let (lib, id, ids) = lib_with_pages("delpages", 5);
        assert_eq!(lib.delete_pages(&id, &[3, 1]).unwrap(), 2);
        assert_eq!(
            lib.page_ids(&id),
            vec![ids[0].clone(), ids[2].clone(), ids[4].clone()]
        );
        assert_eq!(lib.read_content(&id)["pageCount"], json!(3));
        for i in [1, 3] {
            assert!(
                !lib.root.join(&id).join(format!("{}.rm", ids[i])).exists(),
                "第 {} 页的 .rm 没删",
                i + 1
            );
        }
        for i in [0, 2, 4] {
            assert!(lib.root.join(&id).join(format!("{}.rm", ids[i])).exists());
        }

        // 越界下标跳过；重复下标只删一次
        assert_eq!(lib.delete_pages(&id, &[9, 0, 0]).unwrap(), 1);
        assert_eq!(lib.page_ids(&id).len(), 2);

        // 删空 → 0 页（官方允许空文档）
        let rest = lib.page_ids(&id);
        assert_eq!(lib.delete_pages(&id, &[0, 1]).unwrap(), rest.len());
        assert!(lib.page_ids(&id).is_empty());
        assert_eq!(lib.read_content(&id)["pageCount"], json!(0));
        assert_eq!(lib.delete_pages(&id, &[0]).unwrap(), 0);
    }

    /// 多页标签：打上 / 重复打（0） / 去掉；越界跳过。
    #[test]
    fn tag_pages_sets_and_clears() {
        let (lib, id, ids) = lib_with_pages("tagpages", 4);
        assert_eq!(lib.tag_pages(&id, &[0, 2], "gaps", true).unwrap(), 2);
        let got = lib.page_tags(&id);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, ids[0]);
        assert_eq!(got[0].1, vec!["gaps".to_string()]);
        assert_eq!(got[1].0, ids[2]);

        // 已经带这个标签 → 不改，返回 0
        assert_eq!(lib.tag_pages(&id, &[0, 2], "gaps", true).unwrap(), 0);
        // 只动选中的那页（第 3 页本来没有）
        assert_eq!(lib.tag_pages(&id, &[0, 1], "gaps", true).unwrap(), 1);
        assert_eq!(lib.page_tags(&id).len(), 3);

        // 去掉
        assert_eq!(lib.tag_pages(&id, &[0, 2], "gaps", false).unwrap(), 2);
        // 空数组的条目还留在 `.content.pageTags` 里（官方也是覆盖成 []）
        let got = lib.page_tags(&id);
        assert!(got.iter().filter(|(_, t)| !t.is_empty()).count() == 1);
        assert_eq!(
            got.iter().find(|(p, _)| p == &ids[1]).map(|(_, t)| t.clone()),
            Some(vec!["gaps".to_string()])
        );

        // 越界 / 空列表：不 panic
        assert_eq!(lib.tag_pages(&id, &[99], "gaps", true).unwrap(), 0);
        assert_eq!(lib.tag_pages(&id, &[], "gaps", true).unwrap(), 0);
        assert_eq!(lib.all_page_tags().len(), 1);
    }

    /// 抽屉的 Recent：按 `lastModified` 倒序、只算文档、上限 n，
    /// 子文件夹里的也算（`entries()` 只查一层，这里要整库）。
    #[test]
    fn recent_sorted_by_last_modified_desc() {
        let lib = tmp_lib("recent");
        let dir = lib.create_folder("子文件夹", "").unwrap();
        let a = lib.create_book("A").unwrap();
        let b = lib.create_book_in("B", &dir).unwrap();
        let c = lib.create_book("C").unwrap();
        let _ = lib.create_book("D").unwrap();
        // 手工把 lastModified 摆成 C(400) > B(300) > A(200) > D(100)
        for (id, ms) in [(&c, 400u64), (&b, 300), (&a, 200)] {
            let mut m = lib.read_meta(id);
            m["lastModified"] = json!(ms.to_string());
            lib.write_json(&lib.meta_path(id), &m).unwrap();
        }
        let d = lib
            .entries("", false)
            .into_iter()
            .find(|x| x.name == "D")
            .unwrap();
        let mut m = lib.read_meta(&d.uuid);
        m["lastModified"] = json!("100");
        lib.write_json(&lib.meta_path(&d.uuid), &m).unwrap();

        let got: Vec<String> = lib.recent(3).into_iter().map(|x| x.name).collect();
        assert_eq!(got, vec!["C", "B", "A"], "按 lastModified 倒序；子文件夹里的 B 要在");
        // n 是上限；文件夹不进 Recent
        assert_eq!(lib.recent(99).len(), 4);
        assert!(lib.recent(99).iter().all(|x| x.doc_type == "DocumentType"));
        assert_eq!(lib.recent(0).len(), 0);
    }

    /// 抽屉的 Favorites：只取 `.metadata.pinned`，整库扫，回收站里的不算。
    #[test]
    fn pinned_spans_folders_and_skips_trash() {
        let lib = tmp_lib("pinned");
        let dir = lib.create_folder("子文件夹", "").unwrap();
        let inside = lib.create_book_in("收藏的笔记", &dir).unwrap();
        let gone = lib.create_book("已删除").unwrap();
        let plain = lib.create_book("没收藏").unwrap();
        lib.set_pinned(&inside, true).unwrap();
        lib.set_pinned(&gone, true).unwrap();
        lib.delete_book(&gone).unwrap();

        let got = lib.pinned();
        assert_eq!(got.len(), 1, "子文件夹里的收藏要算，回收站里的不算");
        assert_eq!(got[0].uuid, inside);
        assert!(got.iter().all(|x| x.uuid != plain));
        // 文件夹自己也能被收藏（官方 Favorites 里就是混排）
        lib.set_pinned(&dir, true).unwrap();
        let got = lib.pinned();
        assert_eq!(got.len(), 2);
        assert!(got.iter().any(|x| x.doc_type == "CollectionType"));
        // 取消收藏回到 1 项
        lib.set_pinned(&inside, false).unwrap();
        assert_eq!(lib.pinned().len(), 1);
    }

    /// 页删光（0 页）之后再落笔：这一页要重新登记回 `.content`，
    /// 否则墨迹写进了 `<pageid>.rm` 却不在 `cPages.pages` 里，翻回来就没了。
    #[test]
    fn empty_document_gets_page_back_on_save() {
        let (lib, id, ids) = lib_with_pages("emptydoc", 2);
        assert_eq!(lib.delete_pages(&id, &[0, 1]).unwrap(), 2);
        assert!(lib.page_ids(&id).is_empty());
        let mut p = lib.load_page_at(&id, 0);
        p.author = uuid_bytes(&id);
        p.lines_mut().push(line());
        lib.save_page_at(&id, 0, &p).unwrap();
        assert_eq!(lib.page_ids(&id).len(), 1);
        assert_eq!(lib.read_content(&id)["pageCount"], json!(1));
        assert_eq!(lib.load_page_at(&id, 0).total_lines(), 1);
        assert!(!lib.root.join(&id).join(format!("{}.rm", ids[0])).exists());
    }

    // === P4 begin ===
    /// 只读模式下**每一个**写入口都要拒：官方 `Your device is in read-only mode.`
    /// 之后所有写操作被拦住（`res_0686a15.qml`），不能只拦一条路径。
    /// 断言错误文案是 `read-only mode`、类别是 `ErrorKind::Other`。
    fn assert_read_only<T: std::fmt::Debug>(what: &str, r: io::Result<T>) {
        match r {
            Ok(v) => panic!("{what}：只读模式下竟然写成功了（{v:?}）"),
            Err(e) => {
                assert_eq!(e.kind(), io::ErrorKind::Other, "{what}：{:?}", e.kind());
                assert_eq!(e.to_string(), "read-only mode", "{what}");
            }
        }
    }

    #[test]
    fn read_only_mode_blocks_every_write_entry() {
        let (lib, id, ids) = lib_with_pages("readonly", 2);
        // 关着的时候能写（先确认闸门默认是开的）
        lib.rename(&id, "改名前").unwrap();
        assert!(!lib.is_read_only());

        lib.set_read_only(true);
        assert!(lib.is_read_only());

        let mut page = lib.load_page_at(&id, 0);
        page.author = uuid_bytes(&id);
        let tag = vec!["x".to_string()];
        assert_read_only("save_page", lib.save_page(&id, &page));
        assert_read_only("save_page_at", lib.save_page_at(&id, 0, &page));
        assert_read_only(
            "save_texts",
            lib.save_texts(
                &id,
                0,
                &[TextItem {
                    x: 1.0,
                    y: 2.0,
                    size: 3.0,
                    text: "t".into(),
                }],
            ),
        );
        assert_read_only("set_pinned", lib.set_pinned(&id, true));
        assert_read_only("rename", lib.rename(&id, "新名字"));
        assert_read_only("restore", lib.restore(&id));
        assert_read_only("create_folder", lib.create_folder("夹", ""));
        assert_read_only("create_book", lib.create_book("书"));
        assert_read_only("create_book_in", lib.create_book_in("书", ""));
        assert_read_only("create_book_tpl", lib.create_book_tpl("书", "", true, "Lines medium"));
        // 只读模式下 `delete_book` / `purge` 是**放行**的：官方提示原文
        // "删掉一些文件就能恢复正常"，腾空间的入口不能被拦。
        lib.rename(&id, "腾空间前").ok();
        assert_read_only("add_page", lib.add_page(&id, 0));
        assert_read_only("delete_page", lib.delete_page(&id, 0));
        assert_read_only("delete_pages", lib.delete_pages(&id, &[0]));
        assert_read_only("move_page", lib.move_page(&id, 0, 1));
        assert_read_only("duplicate_page", lib.duplicate_page(&id, 0));
        assert_read_only("ensure_pages", lib.ensure_pages(&id, 5));
        assert_read_only("set_page_template", lib.set_page_template(&id, 0, "Blank"));
        assert_read_only("set_cover_page", lib.set_cover_page(&id, true));
        assert_read_only("set_orientation", lib.set_orientation(&id, true));
        assert_read_only("set_tags", lib.set_tags(&id, &tag));
        assert_read_only("set_page_tags", lib.set_page_tags(&id, &ids[0], &tag));
        assert_read_only("tag_pages", lib.tag_pages(&id, &[0], "x", true));
        assert_read_only("rename_tag", lib.rename_tag("a", "b"));
        assert_read_only("delete_tag", lib.delete_tag("a"));

        // 读路径不受影响：库照扫、页照读、收藏照样显示。
        assert!(lib.books().iter().any(|b| b.uuid == id));
        assert_eq!(lib.page_ids(&id).len(), 2);
        assert_eq!(lib.load_page_at(&id, 0).author, uuid_bytes(&id));
        assert!(!lib.find(&id).unwrap().pinned);

        // 盘上有空间了（水位回到 Ok）：写立刻恢复，不用重建 Library。
        lib.set_read_only(false);
        lib.rename(&id, "空间回来了").unwrap();
        assert_eq!(lib.find(&id).unwrap().name, "空间回来了");
    }

    /// 只读时写失败不能留下半截文件：`.metadata` / `.content` 要原样。
    #[test]
    fn read_only_does_not_touch_files() {
        let (lib, id, _ids) = lib_with_pages("readonly-disk", 1);
        let meta_before = std::fs::read_to_string(lib.root.join(format!("{id}.metadata"))).unwrap();
        let content_before = std::fs::read_to_string(lib.root.join(format!("{id}.content"))).unwrap();
        let dir_before = std::fs::read_dir(lib.root.join(&id)).unwrap().count();

        lib.set_read_only(true);
        assert_read_only("rename", lib.rename(&id, "不该落盘"));
        assert_read_only("set_pinned", lib.set_pinned(&id, true));

        assert_eq!(
            std::fs::read_to_string(lib.root.join(format!("{id}.metadata"))).unwrap(),
            meta_before
        );
        assert_eq!(
            std::fs::read_to_string(lib.root.join(format!("{id}.content"))).unwrap(),
            content_before
        );
        assert_eq!(std::fs::read_dir(lib.root.join(&id)).unwrap().count(), dir_before);
    }
    // === P4 end ===
}

// === P11 begin ===
//  打字文本落盘切到官方 `.rm` 的 `RootText` 文字层（`doc/rmtext.rs` 早就能读写它了，
//  只是 `DEFAULT_KIND` 一直没切）。读写都走这个后端：
//
//  * 读：官方 `.rm` 优先（`rmtext::parse_texts()`），这一页没有文字层才看 sidecar；
//  * 写：`rmtext::serialize_texts()` 造出整份新字节，**临时文件 + rename** 原子替换；
//    任何一步失败（这一页没有 `.rm`、不是 v6、磁盘只读）就退回 sidecar 并 `eprintln!`。
//
//  sidecar 在官方写成功时**也留一份**：`doc/rm::RmPage::serialize()` 不写 `RootText`
//  块，整页写回（落笔自动存盘 / 改层名 / 睡前存盘）会把文字层冲掉；有这份副本，
//  `main.rs` 那边整页写完能照着补回去，文字不会丢。

use crate::doc::rmtext::{self, SceneText, TextStore};

/// 官方文件里只有**整篇**一个 `pos_x/pos_y`，逐段位置得自己排——否则所有段落
/// 都会叠在同一点。行距取官方 QML 的 `line-height: 1.45`
/// （`refs/qml_all/frag_0058_00694ff7.qml:99`）。
fn p11_layout(v: &mut [SceneText]) {
    let Some(first) = v.first() else {
        return;
    };
    let (x0, mut y) = (first.x, first.y);
    for t in v.iter_mut() {
        t.x = x0;
        t.y = y;
        y += t.size * 1.45;
    }
}

/// 同目录临时文件 + `rename`：写到一半断电也不会留下半份 `.rm`。
fn p11_write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "page.rm".to_string());
    let tmp = dir.join(format!(".{name}.p11.tmp"));
    let r = (|| -> io::Result<()> {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&tmp, path)
    })();
    if r.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    r
}

impl Library {
    /// 第 `idx` 页的页 id（`<pageid>.rm` / `texts-<pageid>.json` 用的那个）。
    pub fn page_id_at(&self, uuid: &str, idx: usize) -> Option<String> {
        self.page_ids(uuid).get(idx).cloned()
    }

    /// 读一页文字：官方 `.rm` 优先，没有文字层再退回 sidecar。
    ///
    /// 两边的字号都由段落样式推（`ParagraphStyle::default_size()`，文件里没有字号）。
    pub fn load_scene_texts(&self, uuid: &str, idx: usize) -> Vec<SceneText> {
        let Some(pid) = self.page_id_at(uuid, idx) else {
            return Vec::new();
        };
        let rm = self.root.join(uuid).join(format!("{pid}.rm"));
        match fs::read(&rm) {
            Ok(bytes) => match rmtext::parse_texts(&bytes) {
                Ok(mut v) if !v.is_empty() => {
                    for t in v.iter_mut() {
                        t.size = t.style.default_size();
                    }
                    p11_layout(&mut v);
                    return v;
                }
                // 这一页没有文字层：可能是我们上一版只写了 sidecar，往下看
                Ok(_) => {}
                Err(e) => eprintln!("[text] {} 读不出文字层（{e}），退回 sidecar", rm.display()),
            },
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("[text] 读 {} 失败（{e}），退回 sidecar", rm.display()),
        }
        match rmtext::SidecarStore::for_doc(&self.root, uuid).load(&pid) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("[text] 文字 sidecar 读不出来（{e}）");
                Vec::new()
            }
        }
    }

    /// 写一页文字：官方 `.rm` 优先（临时文件 + rename），失败退回 sidecar。
    ///
    /// 返回真正落地的那一侧（官方成功时 sidecar 只作保险副本，见本段开头说明）。
    pub fn save_scene_texts(
        &self,
        uuid: &str,
        idx: usize,
        texts: &[SceneText],
    ) -> io::Result<rmtext::StoreKind> {
        let Some(pid) = self.page_id_at(uuid, idx) else {
            return Ok(rmtext::StoreKind::Sidecar);
        };
        let rm = self.root.join(uuid).join(format!("{pid}.rm"));
        let official = (|| -> io::Result<()> {
            // 没有这一页的 `.rm` 就造不出页（官方格式的页头/场景树我们不凭空写）
            let base = fs::read(&rm)?;
            // 本来就没有文字层、这次也是空的：不用整份重写（关页/翻页都会走到这里）
            if texts.is_empty() && rmtext::parse_texts(&base)?.is_empty() {
                return Ok(());
            }
            let out = rmtext::serialize_texts(&base, texts)?;
            p11_write_atomic(&rm, &out)
        })();
        let side = rmtext::SidecarStore::for_doc(&self.root, uuid);
        match official {
            Ok(()) => {
                if let Err(e) = side.save(&pid, texts) {
                    eprintln!("[text] 官方 .rm 已写，sidecar 副本没写下来（{e}）");
                }
                Ok(rmtext::StoreKind::Official)
            }
            Err(e) => {
                eprintln!("[text] 官方 .rm 落盘失败（{e}），退回 sidecar");
                side.save(&pid, texts)?;
                Ok(rmtext::StoreKind::Sidecar)
            }
        }
    }
}

#[cfg(test)]
mod p11_tests {
    use super::*;
    use crate::doc::rmtext::ParagraphStyle;

    /// 真机页面：`~/re_xochitl/fixture` 下三本笔记的 `.rm`（可用 `RMDEV_FIXTURE` 换路径）。
    fn fixture_rms() -> Vec<PathBuf> {
        let root = std::env::var("RMDEV_FIXTURE")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                PathBuf::from(std::env::var("HOME").unwrap_or_default()).join("rm2_re/fixture")
            });
        let mut out = Vec::new();
        let Ok(dirs) = fs::read_dir(&root) else {
            return out;
        };
        for d in dirs.flatten() {
            if !d.path().is_dir() {
                continue;
            }
            let Ok(files) = fs::read_dir(d.path()) else {
                continue;
            };
            for f in files.flatten() {
                let p = f.path();
                if p.extension().map(|e| e == "rm").unwrap_or(false) {
                    out.push(p);
                }
            }
        }
        out.sort();
        out
    }

    /// 切块：(块类型, 整块原始字节)。头是 `HEADER` 那句 43 字节。
    fn rm_blocks(b: &[u8]) -> Vec<(u8, Vec<u8>)> {
        let mut out = Vec::new();
        let mut p = crate::doc::rm::HEADER.len();
        while p + 8 <= b.len() {
            let len = u32::from_le_bytes(b[p..p + 4].try_into().unwrap()) as usize;
            let start = p + 8;
            if start + len > b.len() {
                break;
            }
            out.push((b[p + 7], b[p..start + len].to_vec()));
            p = start + len;
        }
        out
    }

    /// 笔迹/场景部分 = 除 `RootText`(0x07) 与 `PageInfo`(0x0A) 之外的块。
    fn strokes(blocks: &[(u8, Vec<u8>)]) -> Vec<(u8, Vec<u8>)> {
        blocks
            .iter()
            .filter(|(t, _)| *t != 0x07 && *t != 0x0a)
            .cloned()
            .collect()
    }

    fn mk(text: &str, style: ParagraphStyle) -> SceneText {
        SceneText {
            x: 40.0,
            y: 120.0,
            size: style.default_size(),
            text: text.to_string(),
            color: 0,
            style,
        }
    }

    /// 造一个只有一页、页 id 指定、`.rm` 用真机页面拷进去的文档目录。
    fn p11_doc(tag: &str, src: Option<&Path>) -> (Library, String, String) {
        let dir = std::env::temp_dir().join(format!("rmdev-p11-{tag}-{}", new_uuid("t")));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let lib = Library::with_root(&dir);
        let uuid = "p11-doc";
        let pid = src
            .map(|p| p.file_stem().unwrap().to_string_lossy().to_string())
            .unwrap_or_else(|| "no-rm-page".to_string());
        fs::create_dir_all(dir.join(uuid)).unwrap();
        if let Some(src) = src {
            fs::copy(src, dir.join(uuid).join(format!("{pid}.rm"))).unwrap();
        }
        let content = serde_json::json!({
            "fileType": "notebook",
            "pageCount": 1,
            "cPages": { "pages": [ { "id": pid, "idx": { "value": 0 } } ] },
            "pages": [ pid ],
        });
        fs::write(dir.join(format!("{uuid}.content")), content.to_string()).unwrap();
        (lib, uuid.to_string(), pid)
    }

    fn biggest_fixture() -> Option<PathBuf> {
        fixture_rms()
            .into_iter()
            .max_by_key(|p| fs::metadata(p).map(|m| m.len()).unwrap_or(0))
    }

    /// 真机页面走 `serialize_texts(parse_texts(x))` 往返：**笔迹一个字节都不许变**。
    #[test]
    fn official_roundtrip_keeps_strokes_on_device_pages() {
        let rms = fixture_rms();
        assert!(
            !rms.is_empty(),
            "没找到真机 `.rm`：把 RMDEV_FIXTURE 指向 ~/re_xochitl/fixture"
        );
        for p in &rms {
            let base = fs::read(p).unwrap();
            let texts = rmtext::parse_texts(&base).unwrap();
            let out = rmtext::serialize_texts(&base, &texts).unwrap();
            assert_eq!(
                strokes(&rm_blocks(&base)),
                strokes(&rm_blocks(&out)),
                "{}：往返改了笔迹/场景块",
                p.display()
            );
        }
    }

    /// 往真机页面里写 3 段（标题/正文/待办）：笔迹不变、文字能读回、页码信息跟着改。
    #[test]
    fn writing_text_into_device_page_keeps_strokes() {
        let Some(src) = biggest_fixture() else {
            eprintln!("跳过：没有真机 `.rm` fixture");
            return;
        };
        let base = fs::read(&src).unwrap();
        let texts = vec![
            mk("P11 heading", ParagraphStyle::HEADING),
            mk("body text 123", ParagraphStyle::PLAIN),
            mk("todo", ParagraphStyle::CHECKBOX),
        ];
        let out = rmtext::serialize_texts(&base, &texts).unwrap();
        assert_eq!(
            strokes(&rm_blocks(&base)),
            strokes(&rm_blocks(&out)),
            "写文字不能动笔迹"
        );
        let back = rmtext::parse_texts(&out).unwrap();
        assert_eq!(back.len(), 3);
        assert_eq!(back[0].style, ParagraphStyle::HEADING);
        assert_eq!(back[1].text, "body text 123");
        assert_eq!(back[2].style, ParagraphStyle::CHECKBOX);
        // PageInfo 的字符/行计数要跟着更新（官方首屏按它排版）
        let pi = |b: &[u8]| {
            rm_blocks(b)
                .into_iter()
                .find(|(t, _)| *t == 0x0a)
                .map(|(_, raw)| raw)
                .unwrap()
        };
        let (a, b) = (pi(&base), pi(&out));
        assert_ne!(a, b, "PageInfo 没跟着改");
    }

    /// 写 → 官方 `.rm` 里真有文字层，读回来段落/样式/顺序都对，sidecar 副本也在。
    #[test]
    fn save_then_load_uses_official_layer() {
        let Some(src) = biggest_fixture() else {
            eprintln!("跳过：没有真机 `.rm` fixture");
            return;
        };
        let (lib, uuid, pid) = p11_doc("official", Some(&src));
        let base = fs::read(&src).unwrap();
        let texts = vec![
            mk("Heading", ParagraphStyle::HEADING),
            mk("body", ParagraphStyle::PLAIN),
            mk("bullet", ParagraphStyle::BULLET),
        ];
        let kind = lib.save_scene_texts(&uuid, 0, &texts).unwrap();
        assert_eq!(kind, rmtext::StoreKind::Official, "有 .rm 就该走官方后端");

        let rm = lib.root.join(&uuid).join(format!("{pid}.rm"));
        let out = fs::read(&rm).unwrap();
        assert_eq!(
            strokes(&rm_blocks(&base)),
            strokes(&rm_blocks(&out)),
            "落盘后笔迹不许变"
        );
        assert!(
            rm_blocks(&out).iter().any(|(t, _)| *t == 0x07),
            "整份文件里应该有 RootText 块"
        );
        // 临时文件不许留在目录里
        let leftovers: Vec<_> = fs::read_dir(lib.root.join(&uuid))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("p11.tmp"))
            .collect();
        assert!(leftovers.is_empty(), "临时文件没清掉：{leftovers:?}");

        let got = lib.load_scene_texts(&uuid, 0);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].text, "Heading");
        assert_eq!(got[0].style, ParagraphStyle::HEADING);
        assert_eq!(got[1].text, "body");
        assert_eq!(got[2].style, ParagraphStyle::BULLET);
        // 官方文件只有一个整篇位置：读回来要按行距排开，不能叠在一起
        assert!(
            got[0].y < got[1].y && got[1].y < got[2].y,
            "段落没排开：{:?}",
            got.iter().map(|t| t.y).collect::<Vec<_>>()
        );
        // 字号按样式推
        assert_eq!(got[0].size, ParagraphStyle::HEADING.default_size());
        assert_eq!(got[1].size, ParagraphStyle::PLAIN.default_size());
    }

    /// 这一页压根没有 `.rm`：造不出页，退回 sidecar，读回来还是那些字。
    #[test]
    fn save_falls_back_to_sidecar_without_rm() {
        let (lib, uuid, pid) = p11_doc("sidecar", None);
        let texts = vec![mk("only sidecar", ParagraphStyle::BULLET)];
        let kind = lib.save_scene_texts(&uuid, 0, &texts).unwrap();
        assert_eq!(kind, rmtext::StoreKind::Sidecar);
        assert!(lib
            .root
            .join(&uuid)
            .join(format!("texts-{pid}.json"))
            .exists());
        let got = lib.load_scene_texts(&uuid, 0);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "only sidecar");
        assert_eq!(got[0].style, ParagraphStyle::BULLET);
    }

    /// 读的时候官方优先：sidecar 里是过期的旧文字，也要按 `.rm` 显示。
    /// （老 sidecar 没有 `style`/`color` 两列，照样读得出来 = PLAIN。）
    #[test]
    fn official_layer_wins_over_stale_sidecar() {
        let Some(src) = biggest_fixture() else {
            eprintln!("跳过：没有真机 `.rm` fixture");
            return;
        };
        let (lib, uuid, pid) = p11_doc("precedence", Some(&src));
        lib.save_scene_texts(&uuid, 0, &[mk("device", ParagraphStyle::PLAIN)])
            .unwrap();
        // 手动塞一份过期 sidecar（老格式：只有 x/y/size/text）
        let stale = serde_json::to_vec(&vec![TextItem {
            x: 10.0,
            y: 20.0,
            size: 34.0,
            text: "stale".into(),
        }])
        .unwrap();
        fs::write(
            lib.root.join(&uuid).join(format!("texts-{pid}.json")),
            stale,
        )
        .unwrap();
        let got = lib.load_scene_texts(&uuid, 0);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "device");
        assert_eq!(got[0].style, ParagraphStyle::PLAIN, "老 sidecar 缺 style 列");
    }

    /// 文字清空：官方那份 `RootText` 拿掉，sidecar 副本也一并清掉（别"复活"旧字）。
    #[test]
    fn clearing_text_strips_layer_and_sidecar() {
        let Some(src) = biggest_fixture() else {
            eprintln!("跳过：没有真机 `.rm` fixture");
            return;
        };
        let (lib, uuid, pid) = p11_doc("clear", Some(&src));
        lib.save_scene_texts(&uuid, 0, &[mk("temp", ParagraphStyle::PLAIN)])
            .unwrap();
        let kind = lib.save_scene_texts(&uuid, 0, &[]).unwrap();
        assert_eq!(kind, rmtext::StoreKind::Official);
        let rm = fs::read(lib.root.join(&uuid).join(format!("{pid}.rm"))).unwrap();
        assert!(
            !rm_blocks(&rm).iter().any(|(t, _)| *t == 0x07),
            "清空后不该再有 RootText 块"
        );
        assert!(
            !lib.root
                .join(&uuid)
                .join(format!("texts-{pid}.json"))
                .exists(),
            "sidecar 副本没清掉"
        );
        assert!(lib.load_scene_texts(&uuid, 0).is_empty());
    }

    /// 本来就没有文字层、这次也是空的：不许整份重写（关页/翻页每次都会调）。
    #[test]
    fn saving_empty_on_textless_page_does_not_rewrite() {
        let Some(src) = biggest_fixture() else {
            eprintln!("跳过：没有真机 `.rm` fixture");
            return;
        };
        let (lib, uuid, pid) = p11_doc("noop", Some(&src));
        let rm = lib.root.join(&uuid).join(format!("{pid}.rm"));
        let before = fs::read(&rm).unwrap();
        assert_eq!(
            lib.save_scene_texts(&uuid, 0, &[]).unwrap(),
            rmtext::StoreKind::Official
        );
        assert_eq!(fs::read(&rm).unwrap(), before, "空文字不该重写整份 `.rm`");
    }

    /// 整页写回（`save_page_at` → `RmPage::serialize()`）会把 `RootText` 冲掉：
    /// 这是 P11 每次整页写完都要补一次文字层的原因，也说明 sidecar 副本不能丢。
    #[test]
    fn page_serialize_drops_text_layer_and_repair_restores_it() {
        let Some(src) = biggest_fixture() else {
            eprintln!("跳过：没有真机 `.rm` fixture");
            return;
        };
        let (lib, uuid, pid) = p11_doc("repair", Some(&src));
        let keep = [mk("keep me", ParagraphStyle::BULLET)];
        lib.save_scene_texts(&uuid, 0, &keep).unwrap();
        assert_eq!(lib.load_scene_texts(&uuid, 0)[0].text, "keep me");

        // 模拟落笔存盘：读整页 → 原样写回（`save_page_at` 不碰文字层）
        let page = lib.load_page_at(&uuid, 0);
        lib.save_page_at(&uuid, 0, &page).unwrap();
        let rm = lib.root.join(&uuid).join(format!("{pid}.rm"));
        assert!(
            rmtext::parse_texts(&fs::read(&rm).unwrap())
                .unwrap()
                .is_empty(),
            "整页写回本来就丢 RootText（已知行为，P11 靠补写兜住）"
        );
        // 但 sidecar 副本还在：读侧照样拿得到（`main.rs` 的补写也靠它）
        assert_eq!(lib.load_scene_texts(&uuid, 0)[0].text, "keep me");

        // 补写（= `App::p11_repair_text_layer`）之后官方文件里又有了
        lib.save_scene_texts(&uuid, 0, &keep).unwrap();
        let bytes = fs::read(&rm).unwrap();
        assert!(!rmtext::parse_texts(&bytes).unwrap().is_empty(), "补写没生效");
        let back = lib.load_scene_texts(&uuid, 0);
        assert_eq!(back[0].text, "keep me");
        assert_eq!(back[0].style, ParagraphStyle::BULLET);
    }

    /// 页号越界 / 没有这一页：什么都不做，不 panic。
    #[test]
    fn save_out_of_range_page_is_noop() {
        let (lib, uuid, _) = p11_doc("oob", None);
        assert_eq!(
            lib.save_scene_texts(&uuid, 9, &[mk("x", ParagraphStyle::PLAIN)])
                .unwrap(),
            rmtext::StoreKind::Sidecar
        );
        assert!(lib.load_scene_texts(&uuid, 9).is_empty());
    }
}
// === P11 end ===
