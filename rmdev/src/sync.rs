//! 本地资料库 ⇄ 自建服务器的同步引擎。
//!
//! 同步单位是**文件**，不做语义合并——服务器实现最简单，也最不容易把用户数据
//! 揉坏（官方 `.rmdoc` 导入导出在这一步最容易丢笔画）。对齐 `docs/SERVER.md` §3。
//!
//! 判断用**三路比较**：本地 / 服务器 / 上次同步时记下的基线（`{path: sha256}`）。
//! 只有基线才能区分"服务器删了"和"我还没同步过"，两路比较做不到。
//!
//! 基线只在 commit 成功后才原子替换：中途断电最多让下次多跑一趟（blob 内容寻址，
//! 重传命中 204 不额外占空间），不会把没传上去的文件记成"已同步"。
//!
//! 单个文件失败不中断整轮——一个坏 PDF 不该让整库同步卡死。失败的路径攒到最后
//! 汇总成一句英文给 UI（UI 只显示一行）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::net;

/// 设备只有 1 GB 内存，而我们要把整个文件读进内存算 sha/上传。
/// 超过这个尺寸的跳过并报错，宁可不同步也不能把设备搞崩。
const MAX_FILE: u64 = 32 * 1024 * 1024;

const MB: u64 = 1024 * 1024;

/// 一次同步的结果。`up/down/conflicts` 只统计**成功**的。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// 上传的文件数
    pub up: usize,
    /// 下载的文件数
    pub down: usize,
    /// 冲突副本数
    pub conflicts: usize,
    /// 同步后的服务器 rev
    pub rev: u64,
}

/// 基线状态文件。放配置目录而不是库目录里——否则它自己也会被当成资料同步出去。
///
/// `RMDEV_SYNC_STATE` 给测试用（每个测试一个临时文件，互不干扰）。
pub fn state_path() -> PathBuf {
    if let Ok(p) = std::env::var("RMDEV_SYNC_STATE") {
        return PathBuf::from(p);
    }
    PathBuf::from("/home/root/.config/rmdev/sync-state.json")
}

/// 同步一次。`root` 是资料库目录。
pub fn sync_once(cfg: &net::Config, root: &Path) -> Result<Stats, String> {
    // 我们会在返回前把新 rev 写进 cfg 并保存，但调用方传的是引用，所以这里自己存一份。
    let mut cfg = cfg.clone();
    let mut fails: Vec<String> = Vec::new();
    let mut retried = false;
    loop {
        match sync_round(&mut cfg, root, &mut fails) {
            Ok(st) => return finish(st, &fails),
            Err(RoundFail::Fatal(msg)) => return Err(with_fails(msg, &fails)),
            Err(RoundFail::RevChanged) if retried => {
                return Err(with_fails(
                    "server rev changed twice in a row".to_string(),
                    &fails,
                ));
            }
            Err(RoundFail::RevChanged) => {
                // 409：另一台设备刚提交过。下一轮会重新拉清单对齐 rev。
                // 这一轮传过的 blob 服务器已经有（204），重来不浪费空间。
                retried = true;
                fails.clear(); // 上一轮的真实错误重试时会重新发现
            }
        }
    }
}

/// 一轮同步里的"整轮失败"，和单文件失败区分开。
enum RoundFail {
    /// commit 撞上 409，需要重新拉清单再试。
    RevChanged,
    Fatal(String),
}

fn sync_round(
    cfg: &mut net::Config,
    root: &Path,
    fails: &mut Vec<String>,
) -> Result<Stats, RoundFail> {
    if !root.is_dir() {
        return Err(RoundFail::Fatal(format!(
            "library not found: {}",
            root.display()
        )));
    }
    let state = state_path();
    let old_base = read_state(&state).map_err(RoundFail::Fatal)?;

    let man = net::manifest(cfg).map_err(|e| RoundFail::Fatal(format!("manifest: {e}")))?;
    // 清单里的 rev 才是服务器此刻的版本。别的地方可能已经提交过（另一台设备），
    // 拿旧 rev 当 commit 的 base 只会吃 409，所以一拉到手就对齐。
    if let Some(rev) = man.get("rev").and_then(|v| v.as_u64()) {
        adopt_rev(cfg, rev, fails);
    }
    let remote = parse_manifest(&man);
    let local = collect_local(root, fails);

    // 三方任意一边出现过都要看；排序后走，行为可复现。
    let mut paths: Vec<&String> = Vec::new();
    paths.extend(local.keys());
    paths.extend(remote.keys());
    paths.extend(old_base.keys());
    paths.sort();
    paths.dedup();

    let mut puts: Vec<net::Put> = Vec::new();
    let mut dels: Vec<serde_json::Value> = Vec::new();
    let mut stats = Stats::default();
    // 只有**确认到位**的路径才进新基线；失败的原样留在旧基线里，下次再来。
    let mut final_base = old_base.clone();

    for p in paths {
        let l = local.get(p.as_str()).map(|s| s.as_str());
        let r = remote.get(p.as_str()).map(|e| e.sha.as_str());
        let b = old_base.get(p.as_str()).map(|s| s.as_str());

        match plan(l, r, b) {
            Act::Nothing => {
                // 两边都没了这个文件，基线也该清掉，否则下次会以为服务器删过一次。
                if l.is_none() && r.is_none() {
                    final_base.remove(p.as_str());
                }
            }
            Act::Download => {
                let Some(sha) = r else { continue };
                let size = remote.get(p.as_str()).map(|e| e.size).unwrap_or(0);
                if size > MAX_FILE {
                    fails.push(format!(
                        "{p}: too large on server ({} MB > {} MB limit), skipped",
                        to_mb(size),
                        MAX_FILE / MB
                    ));
                    continue;
                }
                match pull_file(cfg, root, p, sha) {
                    Ok(()) => {
                        stats.down += 1;
                        final_base.insert(p.clone(), sha.to_string());
                    }
                    Err(e) => fails.push(format!("{p}: {e}")),
                }
            }
            Act::Upload => match push_upload(cfg, root, p, &mut puts, &mut final_base) {
                Ok(()) => stats.up += 1,
                Err(e) => fails.push(format!("{p}: {e}")),
            },
            Act::DelLocal => match std::fs::remove_file(root.join(p)) {
                Ok(()) => {
                    final_base.remove(p.as_str());
                }
                // 已经不在了也算达成目的（可能刚被用户删掉）
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    final_base.remove(p.as_str());
                }
                Err(e) => fails.push(format!("{p}: delete failed: {e}")),
            },
            Act::DelRemote => {
                dels.push(serde_json::json!({"path": p, "mtime": now_ms()}));
                final_base.remove(p.as_str());
            }
            Act::Conflict => {
                let Some(sha) = r else { continue };
                // 先把服务器那份留档，再让服务器跟本地走：顺序反了就可能丢数据。
                match net::get_blob(cfg, sha) {
                    Ok(server_bytes) => {
                        let side = conflict_path(root, p);
                        match write_atomic(&side, &server_bytes) {
                            Ok(()) => {
                                stats.conflicts += 1;
                                match push_upload(cfg, root, p, &mut puts, &mut final_base) {
                                    Ok(()) => stats.up += 1,
                                    Err(e) => fails.push(format!("{p}: {e}")),
                                }
                            }
                            Err(e) => fails.push(format!("{p}: keep server copy: {e}")),
                        }
                    }
                    Err(e) => fails.push(format!("{p}: fetch server copy: {e}")),
                }
            }
        }
    }

    if !puts.is_empty() || !dels.is_empty() {
        match net::commit(cfg, cfg.last_rev, &puts, &dels) {
            Ok((rev, srv_conflicts)) => {
                adopt_rev(cfg, rev, fails);
                // 服务器按 mtime 拒绝覆盖的路径并没有同步成功，基线里剔掉，
                // 否则下次会被误判成"两边一致"而永远不再重试。
                for p in srv_conflicts {
                    final_base.remove(&p);
                    fails.push(format!("{p}: server kept its newer copy"));
                }
            }
            Err(e) if e.contains("rev_changed") => return Err(RoundFail::RevChanged),
            Err(e) => return Err(RoundFail::Fatal(format!("commit: {e}"))),
        }
    }

    if let Err(e) = write_state(&state, &final_base) {
        fails.push(format!("save sync state: {e}"));
    }
    stats.rev = cfg.last_rev;
    Ok(stats)
}

/// 记住服务器 rev。变了才写盘——同步是低频操作，可也别每次白写一遍配置。
fn adopt_rev(cfg: &mut net::Config, rev: u64, fails: &mut Vec<String>) {
    if rev == cfg.last_rev {
        return;
    }
    cfg.last_rev = rev;
    if let Err(e) = cfg.save() {
        fails.push(format!("save config: {e}"));
    }
}

// ---------------------------------------------------------------- 决策

/// 单个路径该干什么。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    Nothing,
    Download,
    Upload,
    /// 服务器删了，本地也要删
    DelLocal,
    /// 本地删了，告诉服务器删
    DelRemote,
    /// 两边都改过：留本地，服务器那份写旁路文件
    Conflict,
}

/// 三路比较。参数是三个位置的 sha256，`None` = 该位置没有这个文件。
fn plan(local: Option<&str>, remote: Option<&str>, base: Option<&str>) -> Act {
    match (local, remote) {
        (None, None) => Act::Nothing,
        // ---- 只有一边有 ----
        (None, Some(r)) => match base {
            // 基线里有、服务器没动 → 是我本地删的，让服务器跟着删
            Some(b) if b == r => Act::DelRemote,
            // 服务器新文件，或者服务器上的改了（本地已删，只能取服务器的）
            _ => Act::Download,
        },
        (Some(l), None) => match base {
            // 服务器删了、本地没动过 → 本地也删
            Some(b) if b == l => Act::DelLocal,
            // 本地新文件，或者服务器删了但本地改过 → 本地优先，传回去
            _ => Act::Upload,
        },
        // ---- 两边都有 ----
        (Some(l), Some(r)) => {
            if l == r {
                return Act::Nothing;
            }
            match base {
                // 没有基线又两边都有：两个独立文件，谁都不该覆盖谁
                None => Act::Conflict,
                Some(b) if b == l => Act::Download, // 只有服务器改
                Some(b) if b == r => Act::Upload,   // 只有本地改
                Some(_) => Act::Conflict,           // 三边全不同
            }
        }
    }
}

// ---------------------------------------------------------------- 清单

struct RemoteEntry {
    sha: String,
    size: u64,
}

fn parse_manifest(v: &serde_json::Value) -> HashMap<String, RemoteEntry> {
    let mut out = HashMap::new();
    let Some(entries) = v.get("entries").and_then(|e| e.as_array()) else {
        return out;
    };
    for e in entries {
        let Some(path) = e.get("path").and_then(|x| x.as_str()) else {
            continue;
        };
        if !safe_rel(path) || skip(path) {
            continue;
        }
        // 没有 sha256 的条目 = 服务器标了 deleted:true 的墓碑。
        // 对同步来说"没有这个文件"就够了，不用区分墓碑和从未存在。
        let Some(sha) = e.get("sha256").and_then(|x| x.as_str()) else {
            continue;
        };
        if sha.is_empty() {
            continue;
        }
        out.insert(
            path.to_string(),
            RemoteEntry {
                sha: sha.to_string(),
                size: e.get("size").and_then(|x| x.as_u64()).unwrap_or(0),
            },
        );
    }
    out
}

/// 设备自己的本地缓存（`.local/`：索引、回收站状态等）不是用户资料，
/// 同步到服务器会让另一台设备行为变怪；我们自己原子写留下的 `.tmp` 半截同理。
fn skip(rel: &str) -> bool {
    rel.split('/').any(|c| c == ".local") || rel.ends_with(".tmp")
}

/// 服务器给的 path 要拼进库目录，必须挡住绝对路径和 `..`，否则一条畸形清单
/// 就能往库外面写文件。
fn safe_rel(rel: &str) -> bool {
    !rel.is_empty()
        && !rel.starts_with('/')
        && !rel
            .split('/')
            .any(|c| c.is_empty() || c == "." || c == "..")
}

// ---------------------------------------------------------------- 本地

/// 扫库，返回 `相对路径 → sha256`。
///
/// 只碰普通文件：符号链接既不算文件也不算目录，自然跳过（跟着走可能在两台设备间
/// 制造环）。`.thumbnails` 要同步——另一台设备得能显示缩略图。
fn collect_local(root: &Path, fails: &mut Vec<String>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        for ent in rd.flatten() {
            let abs = ent.path();
            // DirEntry::metadata 不跟随符号链接，链接的 is_file/is_dir 都是 false
            let Ok(md) = ent.metadata() else { continue };
            if md.is_dir() {
                stack.push(abs);
                continue;
            }
            if !md.is_file() {
                continue;
            }
            let rel = rel_path(root, &abs);
            if skip(&rel) {
                continue;
            }
            if md.len() > MAX_FILE {
                fails.push(format!(
                    "{rel}: too large ({} MB > {} MB limit), skipped",
                    to_mb(md.len()),
                    MAX_FILE / MB
                ));
                continue;
            }
            match std::fs::read(&abs) {
                Ok(bytes) => {
                    out.insert(rel, net::sha256_hex(&bytes));
                }
                Err(e) => fails.push(format!("{rel}: read failed: {e}")),
            }
        }
    }
    out
}

fn rel_path(root: &Path, abs: &Path) -> String {
    abs.strip_prefix(root)
        .unwrap_or(abs)
        .to_string_lossy()
        .replace('\\', "/")
}

// ---------------------------------------------------------------- 传、取

fn push_upload(
    cfg: &net::Config,
    root: &Path,
    rel: &str,
    puts: &mut Vec<net::Put>,
    final_base: &mut HashMap<String, String>,
) -> Result<(), String> {
    let abs = root.join(rel);
    // 上传期间文件可能被官方 UI 改过，所以用 put_blob 回传的 sha 记基线，
    // 保证"基线里的 sha"确实就是"服务器上那份"。
    let bytes = std::fs::read(&abs).map_err(|e| format!("read failed: {e}"))?;
    let sha = net::put_blob(cfg, &bytes)?;
    puts.push(net::Put {
        path: rel.to_string(),
        sha256: sha.clone(),
        mtime: mtime_ms(&abs),
    });
    final_base.insert(rel.to_string(), sha);
    Ok(())
}

fn pull_file(cfg: &net::Config, root: &Path, rel: &str, sha: &str) -> Result<(), String> {
    let bytes = net::get_blob(cfg, sha)?;
    // 校验一次：服务器给错内容或传输截断时，宁可这一项失败，也不要污染本地库
    // （脏字节一旦进了基线，就再也不会被重新下载）。
    let got = net::sha256_hex(&bytes);
    if got != sha {
        return Err(format!("blob sha mismatch (want {sha}, got {got})"));
    }
    write_atomic(&root.join(rel), &bytes)
}

/// 先写 `.tmp` 再 rename：断电/被杀时库目录里要么是旧内容，要么没有，
/// 不会留下半截文件被官方 UI 当成坏文档。
fn write_atomic(dst: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(d) = dst.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("create dir {}: {e}", d.display()))?;
    }
    let name = dst
        .file_name()
        .ok_or_else(|| format!("bad path: {}", dst.display()))?;
    let mut tmp_name = name.to_os_string();
    tmp_name.push(".tmp");
    let tmp = dst.with_file_name(tmp_name);
    if let Err(e) = std::fs::write(&tmp, data) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("write: {e}"));
    }
    if let Err(e) = std::fs::rename(&tmp, dst) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("rename: {e}"));
    }
    Ok(())
}

// ---------------------------------------------------------------- 冲突文件名

/// `<path>.server-conflict-<yyyymmdd-HHMMSS>`，和原文件同目录。
fn conflict_path(root: &Path, rel: &str) -> PathBuf {
    root.join(format!("{rel}.server-conflict-{}", stamp(now_ms())))
}

/// UTC 的 `yyyymmdd-HHMMSS`。只为了排序好看，不值得为它引 chrono。
fn stamp(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!(
        "{y:04}{mo:02}{d:02}-{:02}{:02}{:02}",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// 1970-01-01 起的日数 → (年, 月, 日)。Howard Hinnant 的 civil_from_days。
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

// ---------------------------------------------------------------- 基线

fn read_state(p: &Path) -> Result<HashMap<String, String>, String> {
    match std::fs::read_to_string(p) {
        // 没有状态文件 = 第一次同步
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HashMap::new()),
        Err(e) => Err(format!("read sync state {}: {e}", p.display())),
        Ok(s) if s.trim().is_empty() => Ok(HashMap::new()),
        // 基线坏了不能当空表：那会把所有服务器文件都判成"两边都改过"，
        // 一次生成满库的冲突副本。宁可直接失败让人删掉它。
        Ok(s) => {
            serde_json::from_str(&s).map_err(|e| format!("bad sync state {}: {e}", p.display()))
        }
    }
}

fn write_state(p: &Path, base: &HashMap<String, String>) -> Result<(), String> {
    let mut keys: Vec<&String> = base.keys().collect();
    keys.sort(); // 内容稳定，方便人肉 diff
    let mut obj = serde_json::Map::new();
    for k in keys {
        obj.insert(
            k.clone(),
            serde_json::Value::String(base.get(k).cloned().unwrap_or_default()),
        );
    }
    let s = serde_json::to_string_pretty(&serde_json::Value::Object(obj))
        .map_err(|e| format!("encode sync state: {e}"))?;
    write_atomic(p, s.as_bytes())
}

// ---------------------------------------------------------------- 时间、汇总

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn mtime_ms(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or_else(now_ms)
}

fn to_mb(bytes: u64) -> u64 {
    // 向上取整："32 MB + 1 字节"要显示成 33 MB，不然用户看不懂为什么被跳过
    bytes.div_ceil(MB)
}

/// 全部成功才返回 `Ok`；有单个文件失败就把失败路径塞进错误摘要。
fn finish(st: Stats, fails: &[String]) -> Result<Stats, String> {
    if fails.is_empty() {
        Ok(st)
    } else {
        Err(summary(fails))
    }
}

fn with_fails(msg: String, fails: &[String]) -> String {
    if fails.is_empty() {
        msg
    } else {
        format!("{msg}; also {}", summary(fails))
    }
}

/// UI 只显示一句英文，所以得把路径塞进去；太多就截断，别把 toast 撑爆。
fn summary(fails: &[String]) -> String {
    const SHOW: usize = 4;
    let mut s = fails
        .iter()
        .take(SHOW)
        .cloned()
        .collect::<Vec<_>>()
        .join("; ");
    if fails.len() > SHOW {
        s.push_str(&format!("; +{} more", fails.len() - SHOW));
    }
    format!("{} file(s) failed: {s}", fails.len())
}

// ---------------------------------------------------------------- 测试

#[cfg(test)]
mod tests {
    //! 自带一个假服务器（`TcpListener` + 线程，`docs/SERVER.md` §3.3 的三个接口），
    //! 这样同步逻辑是在真实 HTTP 字节流上验证的，而不是打桩出来的幻觉。
    //!
    //! 状态文件路径走环境变量，是进程级的：所以每个测试都拿同一把锁串行跑，
    //! 否则两个测试会互相把 `RMDEV_SYNC_STATE` 改成对方的目录。

    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex, MutexGuard};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // ------------------------------------------------ 假服务器

    #[derive(Default)]
    struct Fake {
        rev: u64,
        /// path → (sha256, mtime)
        files: HashMap<String, (String, u64)>,
        /// path → mtime，服务器写下的墓碑
        tomb: HashMap<String, u64>,
        /// sha256 → 字节
        blobs: HashMap<String, Vec<u8>>,
        /// 下一次 commit 先回 409（模拟另一台设备抢在前面提交）
        reject_once: bool,
    }

    struct Server {
        port: u16,
        state: Arc<Mutex<Fake>>,
    }

    impl Server {
        fn start() -> Server {
            let l = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let port = l.local_addr().unwrap().port();
            let state = Arc::new(Mutex::new(Fake::default()));
            let owned = Arc::clone(&state);
            std::thread::spawn(move || serve(l, owned));
            Server { port, state }
        }

        fn url(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }

        /// 模拟"另一台设备已经同步过的文件"。
        fn put(&self, rel: &str, data: &[u8]) {
            let mut f = self.state.lock().unwrap();
            let sha = net::sha256_hex(data);
            f.blobs.insert(sha.clone(), data.to_vec());
            f.files.insert(rel.to_string(), (sha, now_ms()));
            f.tomb.remove(rel);
        }

        /// 清单里有、blob 没有：模拟服务器半坏。
        fn put_manifest_only(&self, rel: &str, data: &[u8]) {
            let mut f = self.state.lock().unwrap();
            f.files
                .insert(rel.to_string(), (net::sha256_hex(data), now_ms()));
        }

        /// 模拟"另一台设备删了这个路径"。
        fn del(&self, rel: &str) {
            let mut f = self.state.lock().unwrap();
            f.files.remove(rel);
            f.tomb.insert(rel.to_string(), now_ms());
        }

        fn sha(&self, rel: &str) -> Option<String> {
            self.state
                .lock()
                .unwrap()
                .files
                .get(rel)
                .map(|(s, _)| s.clone())
        }

        fn deleted(&self, rel: &str) -> bool {
            self.state.lock().unwrap().tomb.contains_key(rel)
        }

        fn rev(&self) -> u64 {
            self.state.lock().unwrap().rev
        }

        fn reject_next_commit(&self) {
            self.state.lock().unwrap().reject_once = true;
        }
    }

    fn serve(listener: TcpListener, state: Arc<Mutex<Fake>>) {
        for s in listener.incoming() {
            let Ok(mut s) = s else { continue };
            let req = read_request(&mut s);
            let (code, body) = route(&state, &req);
            let head = format!(
                "HTTP/1.1 {code} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let mut out = head.into_bytes();
            out.extend_from_slice(&body);
            let _ = s.write_all(&out);
            // 走出作用域 = 关连接，客户端靠 EOF 收完 body
        }
    }

    struct Req {
        method: String,
        path: String,
        body: Vec<u8>,
    }

    fn read_request(s: &mut TcpStream) -> Req {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 65536];
        let head_end = loop {
            if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break i;
            }
            let n = s.read(&mut tmp).unwrap_or(0);
            if n == 0 {
                return Req {
                    method: String::new(),
                    path: String::new(),
                    body: Vec::new(),
                };
            }
            buf.extend_from_slice(&tmp[..n]);
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).into_owned();
        let mut lines = head.split("\r\n");
        let mut first = lines.next().unwrap_or("").split_whitespace();
        let method = first.next().unwrap_or("").to_string();
        let path = first.next().unwrap_or("").to_string();
        let want: usize = lines
            .find_map(|l| {
                let (k, v) = l.split_once(':')?;
                k.eq_ignore_ascii_case("content-length")
                    .then(|| v.trim().parse().ok())?
            })
            .unwrap_or(0);
        let mut body = buf[head_end + 4..].to_vec();
        while body.len() < want {
            let n = s.read(&mut tmp).unwrap_or(0);
            if n == 0 {
                break;
            }
            body.extend_from_slice(&tmp[..n]);
        }
        body.truncate(want);
        Req { method, path, body }
    }

    fn route(state: &Arc<Mutex<Fake>>, r: &Req) -> (u16, Vec<u8>) {
        use serde_json::json;
        let mut f = state.lock().unwrap();
        let path = r.path.split('?').next().unwrap_or("");
        match (r.method.as_str(), path) {
            ("GET", "/api/v1/manifest") => {
                let mut entries = Vec::new();
                for (p, (sha, mtime)) in &f.files {
                    entries.push(json!({
                        "path": p,
                        "sha256": sha,
                        "size": f.blobs.get(sha).map(|b| b.len()).unwrap_or(0),
                        "mtime": mtime,
                    }));
                }
                for (p, mtime) in &f.tomb {
                    entries.push(json!({"path": p, "deleted": true, "mtime": mtime}));
                }
                let j = json!({"rev": f.rev, "entries": entries}).to_string();
                (200, j.into_bytes())
            }
            // X-Sha256 头这里不解析：按 body 重算等价
            ("POST", "/api/v1/blob") => {
                let sha = net::sha256_hex(&r.body);
                f.blobs.entry(sha).or_insert_with(|| r.body.clone());
                (204, Vec::new())
            }
            ("GET", p) if p.starts_with("/api/v1/blob/") => {
                let sha = p.trim_start_matches("/api/v1/blob/");
                match f.blobs.get(sha) {
                    Some(b) => (200, b.clone()),
                    None => (404, br#"{"error":"not_found"}"#.to_vec()),
                }
            }
            ("POST", "/api/v1/commit") => {
                if f.reject_once {
                    // `docs/SERVER.md` §3.3：rev 对不上就回 409，设备得重拉清单再来
                    f.reject_once = false;
                    let body = json!({"error": "rev_changed", "rev": f.rev}).to_string();
                    return (409, body.into_bytes());
                }
                let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap_or(json!({}));
                if let Some(puts) = v.get("puts").and_then(|p| p.as_array()) {
                    for p in puts {
                        let path = p["path"].as_str().unwrap_or("");
                        let sha = p["sha256"].as_str().unwrap_or("");
                        if path.is_empty() || sha.is_empty() {
                            continue;
                        }
                        let mtime = p["mtime"].as_u64().unwrap_or(0);
                        f.files.insert(path.to_string(), (sha.to_string(), mtime));
                        f.tomb.remove(path);
                    }
                }
                if let Some(dels) = v.get("dels").and_then(|d| d.as_array()) {
                    for d in dels {
                        let path = d["path"].as_str().unwrap_or("");
                        if path.is_empty() {
                            continue;
                        }
                        let mtime = d["mtime"].as_u64().unwrap_or(0);
                        f.files.remove(path);
                        f.tomb.insert(path.to_string(), mtime);
                    }
                }
                f.rev += 1;
                let j = json!({"rev": f.rev, "conflicts": []}).to_string();
                (200, j.into_bytes())
            }
            _ => (404, br#"{"error":"no_route"}"#.to_vec()),
        }
    }

    // ------------------------------------------------ 测试脚手架

    struct Ctx {
        dir: PathBuf,
        root: PathBuf,
        srv: Server,
        cfg: net::Config,
        /// 守住进程级的环境变量
        _guard: MutexGuard<'static, ()>,
    }

    impl Drop for Ctx {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn ctx(tag: &str) -> Ctx {
        let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("rmdev-sync-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("lib");
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("RMDEV_SYNC_STATE", dir.join("state.json"));
        std::env::set_var("RMDEV_SERVER_CONFIG", dir.join("server.json"));
        let srv = Server::start();
        let cfg = net::Config {
            url: srv.url(),
            token: "tok-test".into(),
            device_id: "dev-test".into(),
            last_rev: 0,
            mail_to: String::new(),
            ..Default::default()
        };
        Ctx {
            dir,
            root,
            srv,
            cfg,
            _guard: guard,
        }
    }

    fn put(root: &Path, rel: &str, data: &[u8]) {
        let abs = root.join(rel);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(abs, data).unwrap();
    }

    fn get(root: &Path, rel: &str) -> Option<Vec<u8>> {
        std::fs::read(root.join(rel)).ok()
    }

    fn baseline() -> HashMap<String, String> {
        let p = state_path();
        serde_json::from_str(&std::fs::read_to_string(p).unwrap_or_default()).unwrap_or_default()
    }

    fn files_in(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect()
    }

    // ------------------------------------------------ 规则表

    #[test]
    fn plan_covers_three_way_rules() {
        use Act::*;
        let (n, l, r, b) = (None, Some("l"), Some("r"), Some("b"));
        assert_eq!(plan(n, n, n), Nothing);
        // 一边有、没基线
        assert_eq!(plan(n, r, n), Download);
        assert_eq!(plan(l, n, n), Upload);
        // 两边一样，基线怎么样都无事
        assert_eq!(plan(b, b, b), Nothing);
        assert_eq!(plan(l, l, None), Nothing);
        // 只有一边改了
        assert_eq!(plan(b, r, b), Download);
        assert_eq!(plan(l, b, b), Upload);
        // 三边全不同
        assert_eq!(plan(l, r, b), Conflict);
        // 删除：两边都删了 → 清基线
        assert_eq!(plan(n, n, b), Nothing);
        // 服务器删了、本地没动 → 本地也删
        assert_eq!(plan(b, n, b), DelLocal);
        // 本地删了、服务器没动 → 让服务器删
        assert_eq!(plan(n, b, b), DelRemote);
        // 服务器删了但本地改过 → 本地留着并传上去
        assert_eq!(plan(l, n, b), Upload);
        // 本地删了但服务器改过 → 取回服务器的
        assert_eq!(plan(n, r, b), Download);
        // 没基线又两边都有 → 谁都不覆盖谁
        assert_eq!(plan(l, r, None), Conflict);
    }

    #[test]
    fn stamp_formats_utc() {
        // 2024-02-29T12:34:56Z
        assert_eq!(stamp(1_709_210_096_000), "20240229-123456");
        assert_eq!(stamp(0), "19700101-000000");
    }

    // ------------------------------------------------ 端到端

    #[test]
    fn empty_library_downloads_everything() {
        let cx = ctx("down");
        cx.srv.put("a.metadata", b"meta-a");
        cx.srv.put("a/p1.rm", b"page-1");
        cx.srv.put("a.thumbnails/p1.png", b"png");
        let st = sync_once(&cx.cfg, &cx.root).unwrap();
        assert_eq!((st.up, st.down, st.conflicts), (0, 3, 0));
        assert_eq!(get(&cx.root, "a.metadata").as_deref(), Some(&b"meta-a"[..]));
        // 页目录和缩略图目录都得自动建出来
        assert_eq!(get(&cx.root, "a/p1.rm").as_deref(), Some(&b"page-1"[..]));
        assert_eq!(
            get(&cx.root, "a.thumbnails/p1.png").as_deref(),
            Some(&b"png"[..])
        );
        let base = baseline();
        assert_eq!(base.len(), 3);
        assert_eq!(base["a.metadata"], net::sha256_hex(b"meta-a"));
        assert_eq!(base["a/p1.rm"], net::sha256_hex(b"page-1"));
    }

    #[test]
    fn new_local_file_uploads() {
        let cx = ctx("upl");
        put(&cx.root, "new.metadata", b"hello server");
        put(&cx.root, "new/pa.rm", b"ink");
        let st = sync_once(&cx.cfg, &cx.root).unwrap();
        assert_eq!(st.up, 2);
        assert_eq!(
            cx.srv.sha("new.metadata").unwrap(),
            net::sha256_hex(b"hello server")
        );
        assert_eq!(cx.srv.sha("new/pa.rm").unwrap(), net::sha256_hex(b"ink"));
        assert_eq!(st.rev, cx.srv.rev());
        assert_eq!(baseline()["new.metadata"], net::sha256_hex(b"hello server"));
    }

    #[test]
    fn both_sides_changed_makes_conflict_copy() {
        let cx = ctx("conflict");
        cx.srv.put("doc.metadata", b"v1");
        sync_once(&cx.cfg, &cx.root).unwrap();
        assert_eq!(get(&cx.root, "doc.metadata").as_deref(), Some(&b"v1"[..]));

        // 本地改了，服务器（另一台设备）也改了
        put(&cx.root, "doc.metadata", b"v2-local");
        cx.srv.put("doc.metadata", b"v2-server");

        let st = sync_once(&cx.cfg, &cx.root).unwrap();
        assert_eq!(st.conflicts, 1);
        // 本地内容不动
        assert_eq!(
            get(&cx.root, "doc.metadata").as_deref(),
            Some(&b"v2-local"[..])
        );
        // 服务器那份落在同目录的旁路文件里
        let side: Vec<String> = files_in(&cx.root)
            .into_iter()
            .filter(|n| n.starts_with("doc.metadata.server-conflict-"))
            .collect();
        assert_eq!(side.len(), 1, "{side:?}");
        assert_eq!(get(&cx.root, &side[0]).as_deref(), Some(&b"v2-server"[..]));
        // 服务器跟着本地走
        assert_eq!(
            cx.srv.sha("doc.metadata").unwrap(),
            net::sha256_hex(b"v2-local")
        );

        // 冲突不重复发生（旁路文件本身会被同步上去，那是正常的）
        let again = sync_once(&cx.cfg, &cx.root).unwrap();
        assert_eq!(again.conflicts, 0);
        assert_eq!(again.down, 0);
    }

    #[test]
    fn remote_delete_removes_only_that_file() {
        let cx = ctx("del");
        cx.srv.put("nb/p1.rm", b"page1");
        cx.srv.put("nb/p2.rm", b"page2");
        sync_once(&cx.cfg, &cx.root).unwrap();
        assert!(get(&cx.root, "nb/p1.rm").is_some());

        cx.srv.del("nb/p1.rm");
        let st = sync_once(&cx.cfg, &cx.root).unwrap();
        assert!(get(&cx.root, "nb/p1.rm").is_none());
        // 只删这一个文件，目录树不能整个端掉
        assert_eq!(get(&cx.root, "nb/p2.rm").as_deref(), Some(&b"page2"[..]));
        assert_eq!((st.up, st.down), (0, 0));
    }

    #[test]
    fn local_delete_propagates_as_tombstone() {
        let cx = ctx("rmlocal");
        cx.srv.put("bye.metadata", b"bye");
        sync_once(&cx.cfg, &cx.root).unwrap();
        std::fs::remove_file(cx.root.join("bye.metadata")).unwrap();

        let st = sync_once(&cx.cfg, &cx.root).unwrap();
        assert!(cx.srv.deleted("bye.metadata"));
        assert!(cx.srv.sha("bye.metadata").is_none());
        assert_eq!(st.up, 0);
        assert!(st.rev > 0);
        assert!(!baseline().contains_key("bye.metadata"));
    }

    #[test]
    fn second_sync_is_a_noop() {
        let cx = ctx("idem");
        cx.srv.put("a.metadata", b"A");
        cx.srv.put("a.content", b"B");
        put(&cx.root, "mine.metadata", b"M");
        let first = sync_once(&cx.cfg, &cx.root).unwrap();
        assert_eq!((first.down, first.up), (2, 1));

        let second = sync_once(&cx.cfg, &cx.root).unwrap();
        assert_eq!((second.up, second.down, second.conflicts), (0, 0, 0));
        assert_eq!(second.rev, first.rev);
        // 服务器也不能被再写一遍
        assert_eq!(cx.srv.rev(), first.rev);
    }

    #[test]
    fn failed_download_leaves_no_partial_file() {
        let cx = ctx("atomic");
        // 清单里有这个文件、blob 却没有：模拟服务器半坏 / 传输中断
        cx.srv.put_manifest_only("ghost.metadata", b"never arrives");
        let err = sync_once(&cx.cfg, &cx.root).unwrap_err();
        assert!(err.contains("ghost.metadata"), "{err}");
        assert!(err.contains("1 file(s) failed"), "{err}");
        // 半截文件不能留在库里被官方 UI 当成坏文档
        assert!(!cx.root.join("ghost.metadata").exists());
        assert!(
            files_in(&cx.root).iter().all(|n| !n.ends_with(".tmp")),
            "{:?}",
            files_in(&cx.root)
        );
        // 失败的东西也不能记进基线（否则再也不会重试）
        assert!(baseline().is_empty());
    }

    #[test]
    fn oversized_file_is_skipped() {
        let cx = ctx("big");
        let abs = cx.root.join("huge.pdf");
        let f = std::fs::File::create(&abs).unwrap();
        // 稀疏文件：不真占盘，看大小就能判
        f.set_len(MAX_FILE + 1).unwrap();
        drop(f);

        let err = sync_once(&cx.cfg, &cx.root).unwrap_err();
        assert!(err.contains("huge.pdf"), "{err}");
        assert!(err.contains("33 MB > 32 MB limit"), "{err}");
        // 跳过的文件不能被理解成"本地删了"发给服务器
        assert_eq!(cx.srv.rev(), 0);
        assert!(cx.srv.sha("huge.pdf").is_none());
    }

    #[test]
    fn rev_conflict_retries_and_converges() {
        let cx = ctx("retry");
        cx.srv.put("a.metadata", b"A");
        put(&cx.root, "mine.metadata", b"M");
        // 第一次 commit 吃 409：另一台设备刚提交过
        cx.srv.reject_next_commit();

        let st = sync_once(&cx.cfg, &cx.root).unwrap();
        assert_eq!(st.up, 1);
        assert_eq!(cx.srv.sha("mine.metadata").unwrap(), net::sha256_hex(b"M"));
        // 重试那轮不该把已下载的文件当成冲突
        assert_eq!(st.conflicts, 0);
        assert_eq!(st.rev, cx.srv.rev());
    }

    #[test]
    fn local_cache_is_not_synced() {
        let cx = ctx("localdir");
        cx.srv.put("a.metadata", b"A");
        put(&cx.root, ".local/index", b"device cache");
        sync_once(&cx.cfg, &cx.root).unwrap();
        assert!(cx.srv.sha(".local/index").is_none());
        assert_eq!(baseline().len(), 1);
    }
}
