//! 状态栏通知队列。
//!
//! 官方行为（`res_06b6382.qml` 的 `current` / `queue` / `enqueue` / `checkQueue`，
//! `res_06b7a13.qml` 的 `StatusIndicatorError`）：
//!
//! * 一条通知有 `message` + `timeout`，默认 `Values.popupTimeout` = 4 s
//!   （`res_06ba1de.qml` 第 100 行）；
//! * 同一条（`id` 相同）不重复入队；
//! * 时间到了由主循环 `tick()` 清掉，队列空了通知条就消失。
//!
//! 官方是 FIFO + 定时自动前进；任务契约要求能手动翻看（`1/3` 和左右箭头），
//! 所以这里保留队列，但额外记住"当前在看第几条"（`cur`），`next()` / `prev()`
//! 循环移动游标。纯逻辑，不碰界面，全部可单测。

use std::time::{Duration, SystemTime};

/// 官方 `Values.popupTimeout`（`res_06ba1de.qml`）：一条通知显示 4 s。
pub const DEFAULT_TTL: Duration = Duration::from_millis(4000);

/// 队列上限（任务契约 §5）：超了就把最旧的一条挤掉。
pub const MAX_NOTICES: usize = 8;

/// 通知类别。官方错误指示器前面画 `icon_warning_circle`，普通通知没有图标。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// 普通提示（同步完成、配对成功……）。
    Info,
    /// 出错（同步失败、加页失败……），通知条前面带一个警告圈。
    Error,
}

/// 一条通知。
#[derive(Clone, Debug)]
pub struct Notice {
    pub text: String,
    pub kind: Kind,
    /// 去重键（官方 `messageobject.id`）。默认就是文案本身。
    pub key: String,
    /// 入队时刻。
    pub born: SystemTime,
    /// 存活时长。
    pub ttl: Duration,
    /// 队列内部序号：`tick()` 删掉别的条目后还能认出"当前这条"是哪条。
    seq: u64,
}

impl Notice {
    pub fn new(kind: Kind, text: impl Into<String>, born: SystemTime) -> Notice {
        let text = text.into();
        Notice {
            key: text.clone(),
            text,
            kind,
            born,
            ttl: DEFAULT_TTL,
            seq: 0,
        }
    }

    /// 指定去重键（官方 `enqueue({ id: ... })`）。
    pub fn with_key(mut self, key: impl Into<String>) -> Notice {
        self.key = key.into();
        self
    }

    /// 指定存活时长（官方 `enqueue({ timeout: ... })`）。
    pub fn with_ttl(mut self, ttl: Duration) -> Notice {
        self.ttl = ttl;
        self
    }

    /// 到点了吗。系统时间被往回拨（`now < born`）时不算过期。
    pub fn expired(&self, now: SystemTime) -> bool {
        now.duration_since(self.born)
            .map(|d| d >= self.ttl)
            .unwrap_or(false)
    }
}

/// 通知队列：新的在后面，游标可以前后翻。
#[derive(Default)]
pub struct Queue {
    items: Vec<Notice>,
    /// 当前显示第几条（`items` 下标）。
    cur: usize,
    /// 递增序号，只用来在 `tick()` 里追踪当前条目。
    next_seq: u64,
}

impl Queue {
    pub fn new() -> Queue {
        Queue {
            items: Vec::new(),
            cur: 0,
            next_seq: 1,
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 入队。同 key 已经在队列里 → 不重复塞，返回 `false`。
    ///
    /// 官方 `enqueue` 对 `isUnique` 的消息就是这么做的（`queueContains(id)`）；
    /// 我们统一按 key 去重，免得"同步失败"连续点云图标时堆一屏一样的字。
    pub fn push(&mut self, kind: Kind, text: impl Into<String>, now: SystemTime) -> bool {
        self.push_notice(Notice::new(kind, text, now))
    }

    /// 带自定义 key / ttl 的入队（官方 `enqueue({ id, timeout })`）。
    pub fn push_notice(&mut self, notice: Notice) -> bool {
        let mut n = notice;
        // 先把过期的清掉：否则一条"已经过期但还没 tick"的旧通知会挡住同 key 的新通知。
        self.tick(n.born);
        if self.items.iter().any(|i| i.key == n.key) {
            return false;
        }
        n.seq = self.next_seq;
        self.next_seq += 1;
        if self.items.len() >= MAX_NOTICES {
            // 旧的挤掉；当前游标跟着往前挪一格，指向的还是原来那条（如果还在）。
            self.items.remove(0);
            self.cur = self.cur.saturating_sub(1);
        }
        self.items.push(n);
        true
    }

    /// 当前这条。空队列 / 当前这条已经过期（还没 `tick`）→ `None`。
    pub fn current(&self, now: SystemTime) -> Option<&Notice> {
        let n = self.items.get(self.cur)?;
        if n.expired(now) {
            None
        } else {
            Some(n)
        }
    }

    /// 当前是第几条（1 起）+ 总条数，给通知条的 `1/3` 用。
    pub fn position(&self, now: SystemTime) -> Option<(usize, usize)> {
        self.current(now)?;
        Some((self.cur + 1, self.items.len()))
    }

    /// 下一条（到底了绕回第一条）。
    pub fn next(&mut self, now: SystemTime) -> Option<&Notice> {
        self.tick(now);
        if self.items.is_empty() {
            return None;
        }
        self.cur = (self.cur + 1) % self.items.len();
        self.current(now)
    }

    /// 上一条（第一条再往前是最后一条）。
    pub fn prev(&mut self, now: SystemTime) -> Option<&Notice> {
        self.tick(now);
        if self.items.is_empty() {
            return None;
        }
        self.cur = if self.cur == 0 {
            self.items.len() - 1
        } else {
            self.cur - 1
        };
        self.current(now)
    }

    /// 关掉当前这条，返回是否真的关了一条。
    ///
    /// 关掉后游标停在原下标，后面的条目往前顶 —— 也就是"接着显示下一条"，
    /// 和官方 `checkQueue()` 里 `queue.shift()` 的效果一致。
    pub fn dismiss(&mut self, now: SystemTime) -> bool {
        self.tick(now);
        if self.items.is_empty() {
            return false;
        }
        self.cur = self.cur.min(self.items.len() - 1);
        self.items.remove(self.cur);
        if self.cur >= self.items.len() {
            self.cur = self.items.len().saturating_sub(1);
        }
        true
    }

    /// 清掉过期的，返回清掉几条。
    pub fn tick(&mut self, now: SystemTime) -> usize {
        if self.items.is_empty() {
            self.cur = 0;
            return 0;
        }
        let cur_seq = self.items.get(self.cur).map(|n| n.seq);
        let before = self.items.len();
        self.items.retain(|n| !n.expired(now));
        let removed = before - self.items.len();
        if removed == 0 {
            return 0;
        }
        if self.items.is_empty() {
            self.cur = 0;
            return removed;
        }
        // 当前这条没被删就跟着它走；它被删了就让后一条顶上来（下标不动）。
        self.cur = cur_seq
            .and_then(|seq| self.items.iter().position(|n| n.seq == seq))
            .unwrap_or_else(|| self.cur.min(self.items.len() - 1));
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_000 + secs)
    }

    #[test]
    fn empty_queue_has_no_current() {
        let q = Queue::new();
        assert_eq!(q.len(), 0);
        assert!(q.is_empty());
        assert!(q.current(t(0)).is_none());
        assert!(q.position(t(0)).is_none());
    }

    #[test]
    fn push_shows_the_first_notice() {
        let mut q = Queue::new();
        assert!(q.push(Kind::Info, "Sync completed", t(0)));
        assert_eq!(q.len(), 1);
        let n = q.current(t(0)).expect("刚入队应该能取到");
        assert_eq!(n.text, "Sync completed");
        assert_eq!(n.kind, Kind::Info);
        assert_eq!(n.born, t(0));
        assert_eq!(n.ttl, DEFAULT_TTL);
    }

    #[test]
    fn same_key_is_not_queued_twice() {
        let mut q = Queue::new();
        assert!(q.push(Kind::Error, "Sync failed.", t(0)));
        assert!(!q.push(Kind::Error, "Sync failed.", t(1)));
        assert_eq!(q.len(), 1);
        // 不同文案照常入队
        assert!(q.push(Kind::Info, "Sync completed", t(1)));
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn custom_key_dedupes_different_text() {
        let mut q = Queue::new();
        let a = Notice::new(Kind::Info, "Syncing.. 1%", t(0)).with_key("sync-progress");
        let b = Notice::new(Kind::Info, "Syncing.. 2%", t(1)).with_key("sync-progress");
        assert!(q.push_notice(a));
        // 同一个 key 的进度型通知只留一条（官方按 id 去重）
        assert!(!q.push_notice(b));
        assert_eq!(q.len(), 1);
        assert_eq!(q.current(t(1)).unwrap().text, "Syncing.. 1%");
    }

    #[test]
    fn max_eight_evicts_the_oldest() {
        let mut q = Queue::new();
        // 同一时刻连来 8 条（时间一样，不会被 TTL 提前清掉）
        for i in 0..MAX_NOTICES {
            assert!(q.push(Kind::Info, format!("notice {i}"), t(0)));
        }
        assert_eq!(q.len(), MAX_NOTICES);
        assert!(q.push(Kind::Info, "notice 8", t(0)));
        assert_eq!(q.len(), MAX_NOTICES, "上限 8 条");
        // 从当前条开始往后翻一圈：最旧的那条应该被挤掉，新的要在。
        let mut seen = vec![q.current(t(0)).unwrap().text.clone()];
        for _ in 1..q.len() {
            seen.push(q.next(t(0)).unwrap().text.clone());
        }
        assert_eq!(seen.len(), MAX_NOTICES);
        assert!(!seen.iter().any(|s| s == "notice 0"), "最旧的应该被挤掉");
        assert!(seen.iter().any(|s| s == "notice 8"), "新的一条要在");
    }

    #[test]
    fn ttl_expiry_hides_current_and_tick_drops_it() {
        let mut q = Queue::new();
        let ttl = Duration::from_millis(4000);
        assert!(q.push(Kind::Info, "Disk is almost full.", t(0)));
        // 没到点：正常显示
        assert!(q.current(t(3)).is_some());
        // 到点：当前条不再显示，但队列还没清（等主循环 tick）
        assert!(q.current(t(4)).is_none());
        assert_eq!(q.len(), 1);
        assert_eq!(q.tick(t(4)), 1);
        assert_eq!(q.len(), 0);
        assert!(q.current(t(4)).is_none());
    }

    #[test]
    fn tick_keeps_unexpired_notices_and_tracks_current() {
        let mut q = Queue::new();
        assert!(q.push(Kind::Info, "old", t(0)));
        assert!(q.push(Kind::Info, "new", t(3)));
        // 游标移到 "new"
        assert_eq!(q.next(t(3)).unwrap().text, "new");
        // "old" 过期，"new" 还活着：当前条不能丢
        assert_eq!(q.tick(t(5)), 1);
        assert_eq!(q.len(), 1);
        assert_eq!(q.current(t(5)).unwrap().text, "new");
        assert_eq!(q.position(t(5)), Some((1, 1)));
    }

    #[test]
    fn tick_on_empty_queue_is_a_noop() {
        let mut q = Queue::new();
        assert_eq!(q.tick(t(99)), 0);
        assert!(q.is_empty());
    }

    #[test]
    fn next_wraps_around() {
        let mut q = Queue::new();
        for (i, s) in ["a", "b", "c"].iter().enumerate() {
            assert!(q.push(Kind::Info, *s, t(i as u64)));
        }
        assert_eq!(q.position(t(0)).unwrap(), (1, 3));
        assert_eq!(q.next(t(0)).unwrap().text, "b");
        assert_eq!(q.position(t(0)).unwrap(), (2, 3));
        assert_eq!(q.next(t(0)).unwrap().text, "c");
        assert_eq!(q.next(t(0)).unwrap().text, "a", "到底了绕回第一条");
        assert_eq!(q.position(t(0)).unwrap(), (1, 3));
    }

    #[test]
    fn prev_wraps_around() {
        let mut q = Queue::new();
        for (i, s) in ["a", "b", "c"].iter().enumerate() {
            assert!(q.push(Kind::Info, *s, t(i as u64)));
        }
        assert_eq!(q.prev(t(0)).unwrap().text, "c", "第一条往前是最后一条");
        assert_eq!(q.prev(t(0)).unwrap().text, "b");
        assert_eq!(q.prev(t(0)).unwrap().text, "a");
    }

    #[test]
    fn dismiss_drops_current_and_shows_the_next() {
        let mut q = Queue::new();
        for (i, s) in ["a", "b", "c"].iter().enumerate() {
            assert!(q.push(Kind::Info, *s, t(i as u64)));
        }
        assert!(q.dismiss(t(0)));
        assert_eq!(q.len(), 2);
        assert_eq!(q.current(t(0)).unwrap().text, "b", "关掉 a 之后显示 b");
    }

    #[test]
    fn dismiss_last_falls_back_to_previous() {
        let mut q = Queue::new();
        assert!(q.push(Kind::Info, "a", t(0)));
        assert!(q.push(Kind::Info, "b", t(0)));
        assert_eq!(q.next(t(0)).unwrap().text, "b");
        assert!(q.dismiss(t(0)));
        assert_eq!(q.len(), 1);
        assert_eq!(q.current(t(0)).unwrap().text, "a", "关掉最后一条后停在剩下那条");
        assert!(q.dismiss(t(0)));
        assert!(q.is_empty());
        assert!(!q.dismiss(t(0)), "空队列关不出东西来");
    }

    #[test]
    fn expired_duplicate_does_not_block_a_fresh_notice() {
        let mut q = Queue::new();
        assert!(q.push(Kind::Error, "Sync failed.", t(0)));
        // 4 s 之后同一条错误又来了：旧的那条已经过期，应该算新的一条
        assert!(q.push(Kind::Error, "Sync failed.", t(5)));
        assert_eq!(q.len(), 1);
        assert_eq!(q.current(t(5)).unwrap().born, t(5));
    }

    #[test]
    fn custom_ttl_expires_earlier() {
        let mut q = Queue::new();
        let n = Notice::new(Kind::Info, "Connected to Campus-WLAN", t(0))
            .with_ttl(Duration::from_secs(1));
        assert!(q.push_notice(n));
        assert!(q.current(t(0)).is_some());
        assert!(q.current(t(1)).is_none(), "自定义 ttl 到点就不过期显示了");
        assert_eq!(q.tick(t(1)), 1);
    }

    #[test]
    fn kind_is_preserved_for_the_warning_icon() {
        let mut q = Queue::new();
        assert!(q.push(Kind::Error, "Sync failed.", t(0)));
        assert_eq!(q.current(t(0)).unwrap().kind, Kind::Error);
    }
}
