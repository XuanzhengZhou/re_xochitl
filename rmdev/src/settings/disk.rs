//! 磁盘占用：`statvfs(3)`。
//!
//! 官方文案是 `0.49 GB of 6.41 GB used`，即 1024³ 字节的 GB。
//! 设备上 `/home/root` 是单独分区；本机开发时没有这个挂载点，退回截图里的
//! 0.49 / 6.41，保证无头渲染和官方截图对得上。
//!
//! 以前这里是 fork 一个 `df -k` 再解析：主线程每 60 s 等一次子进程，
//! 开机冷缓存时一次能耗 947 ms（见 [`read`] 的注释）。

const GB: f64 = 1024.0 * 1024.0 * 1024.0;

/// `0.49 GB of 6.41 GB used`
pub fn format_used(used: f64, total: f64) -> String {
    format!("{used:.2} GB of {total:.2} GB used")
}

/// 磁盘水位（官方 `res_0686a15.qml` 的 `enoughFreeSpaceForCreating` /
/// `enoughFreeSpaceForDownload`，`res_06b7a13.qml` 的两句提示）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    /// 剩余 ≥ 5%：正常。
    Ok,
    /// 剩余 < 5% 但还没满：官方只提示 `Disk is almost full.`，仍然能写。
    AlmostFull,
    /// 一点剩余都没有：官方切只读模式，所有写操作被拦住。
    Full,
}

/// 剩余空间比例低于这个值算「将满」（官方阈值 5%）。
pub const ALMOST_FULL_FREE: f64 = 0.05;

/// 按 `(已用, 总量)` 判水位。
///
/// 官方阈值：剩余 < 5% = AlmostFull；剩余 0（或已用超过总量、总量读不到）= Full。
/// 读不到总量时按 Full 处理最安全——宁可不写，也不能写到一半没空间。
pub fn level(used: f64, total: f64) -> Level {
    // 总量 <= 0 / NaN、已用 NaN 都是「量不出来」：当成满盘。
    if !(total > 0.0) || !used.is_finite() {
        return Level::Full;
    }
    let free = total - used;
    if free <= 0.0 {
        return Level::Full;
    }
    // 用比例比较：5.0 / 100.0 和字面量 0.05 是同一个 double，
    // 正好剩 5% 时不会因为浮点误差掉进 AlmostFull。
    if free / total < ALMOST_FULL_FREE {
        Level::AlmostFull
    } else {
        Level::Ok
    }
}

/// 这个水位下还能不能写盘：只有 Full 会切官方只读模式。
pub fn to_savable(level: Level) -> bool {
    !matches!(level, Level::Full)
}

/// 已用比例，0..1。
pub fn ratio(used: f64, total: f64) -> f64 {
    if total <= 0.0 {
        0.0
    } else {
        (used / total).clamp(0.0, 1.0)
    }
}

/// `(used_gb, total_gb)`；读不到退回官方截图里的数值。
///
/// **不要改回 fork `df`**：`p4_check_disk` 每 60 s 调一次，而且就在主线程的
/// `tick()` 里同步等结果。开机那会儿 `df` 的二进制和页缓存全是冷的，实测一次
/// `df` 能把主循环按住 **947 ms**（日志 `[slow] tick 947ms`，紧跟在
/// `[webui]` 那行后面）——用户看到的就是"整个界面冻住一会儿，然后才动"。
/// `statvfs` 是一个 syscall，微秒级；口径和 `df -k` 的 Used 列一致
/// （`blocks - bfree`，含 root 保留块）。
pub fn read() -> (f32, f32) {
    match statvfs("/home/root") {
        Some((used, total)) => (used as f32, total as f32),
        // 本机开发时没有这个挂载点：退回官方截图里的数值，
        // 保证无头渲染和官方截图对得上。
        None => (0.49, 6.41),
    }
}

/// `statvfs(3)` → `(已用 GB, 总量 GB)`，单位同 [`GB`]。
fn statvfs(path: &str) -> Option<(f64, f64)> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return None;
    }
    // `f_frsize` 才是"块大小"，`f_bsize` 只是最优 IO 大小；老内核可能给 0。
    let frsize = if st.f_frsize != 0 {
        st.f_frsize
    } else {
        st.f_bsize
    } as u64;
    let total = st.f_blocks as u64 * frsize;
    if total == 0 {
        return None;
    }
    let free = st.f_bfree as u64 * frsize;
    let used = total.saturating_sub(free);
    Some((used as f64 / GB, total as f64 / GB))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 挂载点能量出来，而且 `已用 <= 总量`；本机没有 `/home/root`
    /// 时 `read()` 必须退回官方常量（无头渲染靠它对齐截图）。
    #[test]
    fn statvfs_reads_a_real_mount() {
        let (used, total) = statvfs("/").expect("/ 一定要能量出来");
        assert!(total > 0.0, "总量 {total}");
        assert!(used >= 0.0 && used <= total, "已用 {used} / 总量 {total}");
        if !std::path::Path::new("/home/root").exists() {
            assert_eq!(read(), (0.49, 6.41));
        }
    }

    #[test]
    fn formats_like_official() {
        assert_eq!(format_used(0.49, 6.41), "0.49 GB of 6.41 GB used");
        assert!((ratio(3.0, 6.0) - 0.5).abs() < f64::EPSILON);
        assert_eq!(ratio(1.0, 0.0), 0.0);
    }

    /// 官方阈值就是 5%：正好剩 5.0% 还算正常，4.99% 就 `Disk is almost full.`，
    /// 一点不剩（或已用超过总量）才是 Full / 只读。
    #[test]
    fn level_boundaries_at_five_percent() {
        assert_eq!(level(0.0, 100.0), Level::Ok);
        assert_eq!(level(95.0, 100.0), Level::Ok, "正好剩 5% 不该报");
        assert_eq!(level(95.01, 100.0), Level::AlmostFull, "剩 4.99% 要报");
        assert_eq!(level(99.99, 100.0), Level::AlmostFull, "剩 0.01% 仍可写");
        assert_eq!(level(100.0, 100.0), Level::Full, "一点不剩 = 满");
        assert_eq!(level(105.0, 100.0), Level::Full, "已用超过总量也算满");
        // 真机上的量级（6.41 GB 分区）也要落在同一档：
        // 6.41 × 95% = 6.09，已用 6.08 还剩 5.1%，已用 6.10 只剩 4.8%。
        assert_eq!(level(6.08, 6.41), Level::Ok, "剩 5.1%");
        assert_eq!(level(6.10, 6.41), Level::AlmostFull, "剩 4.8%");
    }

    /// 负数 / 零总量 / NaN：绝不 panic，也不许误判成 Ok 之后继续写。
    #[test]
    fn level_degenerate_inputs() {
        assert_eq!(level(-1.0, 100.0), Level::Ok, "已用 -1 就是空盘");
        assert_eq!(level(0.0, 0.0), Level::Full, "总量 0 当满盘");
        assert_eq!(level(1.0, 0.0), Level::Full);
        assert_eq!(level(0.0, -5.0), Level::Full);
        assert_eq!(level(f64::NAN, 100.0), Level::Full);
        assert_eq!(level(0.0, f64::NAN), Level::Full);
        assert_eq!(level(f64::INFINITY, 100.0), Level::Full);
        assert_eq!(level(0.0, f64::INFINITY), Level::Ok);
    }

    /// 只有 Full 切只读；将满只提示，官方此时仍然允许写字。
    #[test]
    fn only_full_is_read_only() {
        assert!(to_savable(Level::Ok));
        assert!(to_savable(Level::AlmostFull));
        assert!(!to_savable(Level::Full));
        // level → to_savable 连起来：4.99% 还能写，满盘不能写。
        assert!(to_savable(level(95.01, 100.0)));
        assert!(!to_savable(level(100.0, 100.0)));
    }
}
