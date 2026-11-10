//! 页面渲染缓存（与渲染后端无关）。
//!
//! 这一层不认识 PDF、也不碰 FFI，只做四件事：
//!
//! 1. 按 **(页号, 渲染宽度)** 存最近 N 页的灰度像素，LRU 淘汰；
//! 2. 把「后端」抽成 [`PageRenderer`] trait，测试里换成假后端就能在
//!    macOS 主机上验证「同一页请求 3 次只真渲染 1 次」；
//! 3. [`CachedDoc::prefetch`] 预热相邻页 —— **不开线程**，什么时候调由调用方决定；
//! 4. 统计 hits / misses / renders，日志和测试都拿它说话。
//!
//! 内存：一页 1323×1872 灰度 = 2.48 MB，[`DEFAULT_PAGES`] = 4 页 ≈ 10 MB。
//! 设备 512 MB，别把 N 开大。

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

/// 默认缓存页数：4 页 ≈ 10 MB（1323×1872 灰度）。
pub const DEFAULT_PAGES: usize = 4;

/// 缓存计数。`hits/misses` 只统计调用方**主动要页**（[`PageCache::get_or_render`]），
/// `renders` 统计真正落到后端的渲染次数（含预热）——所以
/// `hit_rate = hits / (hits + misses)`，`renders` 少掉几次就是省掉的渲染。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    pub hits: u64,
    pub misses: u64,
    pub renders: u64,
    pub entries: usize,
    pub capacity: usize,
    pub bytes: usize,
}

impl CacheStats {
    /// 命中率 0..=1；一次都没要过页时返回 0。
    pub fn hit_rate(&self) -> f32 {
        let n = self.hits + self.misses;
        if n == 0 {
            0.0
        } else {
            self.hits as f32 / n as f32
        }
    }
}

struct Frame {
    page: u32,
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

/// 最近 N 页的灰度位图。键 = (页号, 渲染宽度)，高度只用来校验
/// —— 同一页同一宽度但高度变了（调用方换了适配算法）就重新渲染，
/// 不占用新的槽位。
pub struct PageCache {
    capacity: usize,
    /// 队首 = 最久没用到，队尾 = 最近用到。
    frames: VecDeque<Frame>,
    hits: u64,
    misses: u64,
    renders: u64,
}

impl PageCache {
    pub fn new(capacity: usize) -> PageCache {
        PageCache {
            capacity,
            frames: VecDeque::new(),
            hits: 0,
            misses: 0,
            renders: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.frames.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// 当前占用的字节数（纯像素数据，不含 Vec 头）。
    pub fn bytes(&self) -> usize {
        self.frames.iter().map(|f| f.pixels.len()).sum()
    }

    pub fn stats(&self) -> CacheStats {
        CacheStats {
            hits: self.hits,
            misses: self.misses,
            renders: self.renders,
            entries: self.frames.len(),
            capacity: self.capacity,
            bytes: self.bytes(),
        }
    }

    pub fn clear(&mut self) {
        self.frames.clear();
    }

    /// 该页该尺寸是否已在缓存里（不改变 LRU 顺序、不计命中）。
    pub fn holds(&self, page: u32, width: u32, height: u32) -> bool {
        self.frames
            .iter()
            .any(|f| f.page == page && f.width == width && f.height == height)
    }

    /// 查缓存，命中就把它挪到队尾（MRU）并返回像素。
    /// 这是裸查询，不计数——计数在 [`PageCache::get_or_render`] / 预热里做。
    pub fn find(&mut self, page: u32, width: u32, height: u32) -> Option<&[u8]> {
        let i = self
            .frames
            .iter()
            .position(|f| f.page == page && f.width == width && f.height == height)?;
        if i + 1 != self.frames.len() {
            let f = self.frames.remove(i)?;
            self.frames.push_back(f);
        }
        self.frames.back().map(|f| f.pixels.as_slice())
    }

    /// 放一页进缓存。同 (页号, 宽度) 的旧帧原地替换（高度不同也算同一帧，
    /// 因为键里只有宽度，见模块注释）。
    pub fn put(&mut self, page: u32, width: u32, height: u32, pixels: Vec<u8>) {
        if self.capacity == 0 {
            return;
        }
        if let Some(i) = self
            .frames
            .iter()
            .position(|f| f.page == page && f.width == width)
        {
            if let Some(mut f) = self.frames.remove(i) {
                f.height = height;
                f.pixels = pixels;
                self.frames.push_back(f);
            }
            return;
        }
        self.frames.push_back(Frame {
            page,
            width,
            height,
            pixels,
        });
        while self.frames.len() > self.capacity {
            self.frames.pop_front();
        }
    }

    /// 要一页：命中直接给副本，未命中调 `render` 渲染、存缓存、把渲染结果给调用方。
    ///
    /// 返回的是调用方独占的 `Vec`（命中时是一次 memcpy）：2.5 MB 的拷贝
    /// 比重新渲一遍便宜两个数量级，也省得调用方和缓存的生命周期纠缠。
    pub fn get_or_render<F>(
        &mut self,
        page: u32,
        width: u32,
        height: u32,
        render: F,
    ) -> Result<Vec<u8>, String>
    where
        F: FnOnce() -> Result<Vec<u8>, String>,
    {
        let hit = self.find(page, width, height).map(<[u8]>::to_vec);
        if let Some(px) = hit {
            self.hits += 1;
            return Ok(px);
        }
        self.misses += 1;
        let px = render()?;
        self.renders += 1;
        // 长度不对说明后端算错了尺寸，这种结果不进缓存（也别让它污染后面的命中）。
        if px.len() == width as usize * height as usize {
            self.put(page, width, height, px.clone());
        }
        Ok(px)
    }

    /// 预热：没有就渲染一份塞进缓存，**不返回副本**（省掉一次 2.5 MB 拷贝）。
    /// 返回 `true` 表示真的渲染了，`false` 表示已经在缓存里（或后端报错）。
    pub fn warm<F>(&mut self, page: u32, width: u32, height: u32, render: F) -> Result<bool, String>
    where
        F: FnOnce() -> Result<Vec<u8>, String>,
    {
        if self.find(page, width, height).is_some() {
            return Ok(false);
        }
        let px = render()?;
        self.renders += 1;
        if px.len() == width as usize * height as usize {
            self.put(page, width, height, px);
        }
        Ok(true)
    }
}

/// 渲染后端。生产实现是 pdfium（见 `pdf.rs`），测试实现是假的。
pub trait PageRenderer {
    fn page_count(&self) -> usize;
    /// 页面的高宽比（h/w），取不到给个 A4 的 1.414。
    fn aspect(&self, page: u32) -> f32;
    /// 渲染成 `width×height` 灰度（255 = 白），长度必须是 `width*height`。
    fn render(&self, page: u32, width: u32, height: u32) -> Result<Vec<u8>, String>;
}

/// 「后端 + 页缓存 + 页面尺寸缓存」的合体。
///
/// 挂在 `Rc` 后面给 [`crate::doc::pdf::Pdf`] 用：主循环每翻一页都会重新
/// `Pdf::open(同一个路径)`，真正贵的 `dlopen + FPDF_LoadDocument` 只做一次，
/// 页缓存也顺着同一个句柄留下来。
pub struct CachedDoc<R> {
    backend: R,
    cache: RefCell<PageCache>,
    /// 页号 → 高宽比。`FPDF_LoadPage` 每次都要解析页对象，翻回去时能省则省；
    /// 条目数上限就是这份文档的页数（每条约 32 字节）。
    aspects: RefCell<HashMap<u32, f32>>,
}

impl<R: PageRenderer> CachedDoc<R> {
    pub fn new(backend: R, pages: usize) -> CachedDoc<R> {
        CachedDoc {
            backend,
            cache: RefCell::new(PageCache::new(pages)),
            aspects: RefCell::new(HashMap::new()),
        }
    }

    pub fn page_count(&self) -> usize {
        self.backend.page_count()
    }

    pub fn aspect(&self, page: u32) -> f32 {
        if let Some(&v) = self.aspects.borrow().get(&page) {
            return v;
        }
        let v = self.backend.aspect(page);
        self.aspects.borrow_mut().insert(page, v);
        v
    }

    /// 要一页（走缓存）。参数单位是像素，和 `Pdf::render` 一一对应。
    pub fn render(&self, page: u32, width: u32, height: u32) -> Result<Vec<u8>, String> {
        let mut cache = self.cache.borrow_mut();
        cache.get_or_render(page, width, height, || {
            self.backend.render(page, width, height)
        })
    }

    /// contain 适配：把页面装进 `screen_w × screen_h`，四周留白。
    ///
    /// 公式和 `main.rs` 里算 `ww/hh` 的那 8 行**逐字一致**——
    /// 预热出来的键必须和真正翻页时请求的键完全一样，否则白干。
    fn fit(&self, page: u32, screen_w: u32, screen_h: u32) -> (u32, u32) {
        let asp = self.aspect(page).max(0.1);
        let mut hh = screen_h;
        let mut ww = (screen_h as f32 / asp).round() as u32;
        if ww > screen_w {
            ww = screen_w;
            hh = (screen_w as f32 * asp).round() as u32;
        }
        (ww.max(1), hh.max(1))
    }

    /// 预热一页。返回 `true` = 真的渲染并进了缓存；`false` = 已在缓存 / 越界 / 后端报错。
    ///
    /// **不开线程**：这是个同步调用，什么时候调（翻页动画之后？后台空闲？）
    /// 由调用方决定。失败静默——预热永远不该影响前台。
    pub fn prefetch(&self, page: u32, screen_w: u32, screen_h: u32) -> bool {
        if screen_w == 0 || screen_h == 0 || page as usize >= self.page_count() {
            return false;
        }
        let (w, h) = self.fit(page, screen_w, screen_h);
        let mut cache = self.cache.borrow_mut();
        cache
            .warm(page, w, h, || self.backend.render(page, w, h))
            .unwrap_or(false)
    }

    pub fn stats(&self) -> CacheStats {
        self.cache.borrow().stats()
    }

    pub fn clear_cache(&self) {
        self.cache.borrow_mut().clear();
    }
}

/// 单槽句柄缓存：同一个 key 的重复请求直接复用，不重新构造。
///
/// 用途：主循环每翻一页都 `Pdf::open(同一个路径)`，但真正贵的是
/// `dlopen + FPDF_LoadDocument`（把整份 PDF 的 xref/对象树解析一遍）。
/// 这里只留**最近打开的一份**，换书时才重开——内存上界 = 一份文档。
pub struct SlotCache<T> {
    slot: Option<(String, Rc<T>)>,
}

impl<T> SlotCache<T> {
    pub const fn new() -> SlotCache<T> {
        SlotCache { slot: None }
    }

    pub fn current_key(&self) -> Option<&str> {
        self.slot.as_ref().map(|(k, _)| k.as_str())
    }

    /// 命中同 key 就返回同一个 `Rc`；否则调 `make` 造一个新的放进槽里。
    /// `make` 失败时**不动**旧槽（旧文档还能继续用）。
    pub fn get_or_try_insert<E>(
        &mut self,
        key: &str,
        make: impl FnOnce() -> Result<Rc<T>, E>,
    ) -> Result<Rc<T>, E> {
        if let Some((k, v)) = &self.slot {
            if k == key {
                return Ok(Rc::clone(v));
            }
        }
        let v = make()?;
        self.slot = Some((key.to_string(), Rc::clone(&v)));
        Ok(v)
    }

    pub fn clear(&mut self) {
        self.slot = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// 假后端：数一数到底渲染了几次，不碰任何设备库。
    struct Fake {
        renders: Cell<u32>,
        aspects: Cell<u32>,
        pages: usize,
        /// 返回错误长度，验证"坏结果不进缓存"。
        bad_len: bool,
    }

    impl Fake {
        fn new(pages: usize) -> Fake {
            Fake {
                renders: Cell::new(0),
                aspects: Cell::new(0),
                pages,
                bad_len: false,
            }
        }
        fn renders(&self) -> u32 {
            self.renders.get()
        }
    }

    impl PageRenderer for Fake {
        fn page_count(&self) -> usize {
            self.pages
        }
        fn aspect(&self, _page: u32) -> f32 {
            self.aspects.set(self.aspects.get() + 1);
            1.414
        }
        fn render(&self, page: u32, width: u32, height: u32) -> Result<Vec<u8>, String> {
            self.renders.set(self.renders.get() + 1);
            let n = width as usize * height as usize;
            if self.bad_len {
                return Ok(vec![0u8; n / 2]);
            }
            Ok(vec![page as u8 + 1; n])
        }
    }

    /// 用户要的场景：**同一页请求 3 次，第 2/3 次不能再渲染**。
    #[test]
    fn same_page_three_times_renders_once() {
        let mut cache = PageCache::new(DEFAULT_PAGES);
        let calls = Cell::new(0);
        let render = |page: u32, w: u32, h: u32| {
            calls.set(calls.get() + 1);
            Ok(vec![page as u8 + 7; (w * h) as usize])
        };

        let a = cache
            .get_or_render(0, 100, 50, || render(0, 100, 50))
            .unwrap();
        let b = cache
            .get_or_render(0, 100, 50, || render(0, 100, 50))
            .unwrap();
        let c = cache
            .get_or_render(0, 100, 50, || render(0, 100, 50))
            .unwrap();

        assert_eq!(calls.get(), 1, "只有第一次该真渲染");
        assert_eq!(cache.stats().renders, 1);
        assert_eq!(cache.stats().hits, 2);
        assert_eq!(cache.stats().misses, 1);
        assert_eq!(cache.stats().hit_rate(), 2.0 / 3.0);
        assert_eq!(a, b);
        assert_eq!(b, c);
        assert_eq!(cache.stats().bytes, 100 * 50);
        eprintln!(
            "[test] 同一页请求 3 次：renders={} hits={} misses={} hit_rate={:.2}",
            cache.stats().renders,
            cache.stats().hits,
            cache.stats().misses,
            cache.stats().hit_rate()
        );
    }

    #[test]
    fn width_is_part_of_the_key() {
        let mut cache = PageCache::new(DEFAULT_PAGES);
        let calls = Cell::new(0);
        let mut get = |w: u32| {
            cache
                .get_or_render(0, w, 50, || {
                    calls.set(calls.get() + 1);
                    Ok(vec![1u8; (w * 50) as usize])
                })
                .unwrap()
        };
        get(100);
        get(100);
        get(200); // 换宽度 = 新键，必须重渲
        assert_eq!(calls.get(), 2);
        assert_eq!(cache.len(), 2);
    }

    #[test]
    fn same_width_new_height_replaces_the_slot() {
        let mut cache = PageCache::new(DEFAULT_PAGES);
        cache.put(0, 100, 50, vec![1; 100 * 50]);
        cache.put(0, 100, 60, vec![2; 100 * 60]); // 键只有宽度，高度变了原地换
        assert_eq!(cache.len(), 1);
        assert!(cache.holds(0, 100, 60));
        assert!(!cache.holds(0, 100, 50));
        assert_eq!(cache.find(0, 100, 60).unwrap()[0], 2);
    }

    /// N=4：翻 5 页后最老的那页被淘汰，最近 4 页还都在。
    #[test]
    fn lru_keeps_capacity() {
        let mut cache = PageCache::new(4);
        for page in 0..5u32 {
            cache.put(page, 100, 100, vec![page as u8; 100 * 100]);
        }
        assert_eq!(cache.len(), 4);
        assert!(!cache.holds(0, 100, 100), "最老的 0 页该被淘汰");
        for page in 1..5u32 {
            assert!(cache.holds(page, 100, 100), "第 {page} 页该留着");
        }
        assert_eq!(cache.bytes(), 4 * 100 * 100);

        // 命中的页会被"摸"到 MRU：访问 1 页后再进一页，淘汰的应该是 2 页
        cache.find(1, 100, 100).unwrap();
        cache.put(5, 100, 100, vec![5; 100 * 100]);
        assert!(cache.holds(1, 100, 100));
        assert!(!cache.holds(2, 100, 100));
    }

    #[test]
    fn wrong_length_render_is_not_cached() {
        let mut cache = PageCache::new(4);
        let fake = Fake {
            bad_len: true,
            ..Fake::new(1)
        };
        cache
            .get_or_render(0, 10, 10, || fake.render(0, 10, 10))
            .unwrap();
        cache
            .get_or_render(0, 10, 10, || fake.render(0, 10, 10))
            .unwrap();
        assert_eq!(cache.len(), 0);
        assert_eq!(fake.renders(), 2, "坏结果不该被当成命中");
    }

    /// 整条生产路径（CachedDoc = 后端 + 缓存 + 尺寸缓存）的验证。
    #[test]
    fn cached_doc_prefetch_then_render_is_a_hit() {
        let doc = CachedDoc::new(Fake::new(10), DEFAULT_PAGES);

        // 翻到第 0 页：真渲一次
        let px = doc.render(0, 426, 568).unwrap();
        assert_eq!(px.len(), 426 * 568);
        assert_eq!(doc.stats().renders, 1);

        // 同一页再要两次：命中
        doc.render(0, 426, 568).unwrap();
        doc.render(0, 426, 568).unwrap();
        assert_eq!(doc.stats().renders, 1);
        assert_eq!(doc.stats().hits, 2);

        // 预热第 1 页：fit(1, 426, 568) 里 asp=1.414 → w=round(568/1.414)=402, h=568
        assert_eq!(doc.fit(1, 426, 568), (402, 568));
        assert!(doc.prefetch(1, 426, 568));
        assert_eq!(doc.stats().renders, 2);
        // 再预热一次不重复渲染
        assert!(!doc.prefetch(1, 426, 568));
        assert_eq!(doc.stats().renders, 2);

        // 真正翻到第 1 页：就是刚才预热的那一帧，后端一次都不该再调
        let px = doc.render(1, 402, 568).unwrap();
        assert_eq!(px.len(), 402 * 568);
        assert_eq!(doc.stats().renders, 2);
        assert_eq!(doc.stats().hits, 3);

        // 越界预热是无害的 false
        assert!(!doc.prefetch(99, 426, 568));

        let st = doc.stats();
        eprintln!(
            "[test] 渲染第0页×3 + 预热第1页 + 翻到第1页：renders={} hits={} misses={} entries={} bytes={}",
            st.renders, st.hits, st.misses, st.entries, st.bytes
        );
    }

    #[test]
    fn cached_doc_remembers_aspect() {
        let fake = Fake::new(3);
        let doc = CachedDoc::new(fake, DEFAULT_PAGES);
        doc.aspect(2);
        doc.aspect(2);
        doc.aspect(2);
        assert_eq!(doc.backend.aspects.get(), 1, "同一页只该问一次后端");
        doc.aspect(1);
        assert_eq!(doc.backend.aspects.get(), 2);
    }

    #[test]
    fn slot_cache_reuses_handle() {
        let made = Cell::new(0);
        let mut slot: SlotCache<String> = SlotCache::new();

        let a = slot
            .get_or_try_insert("book.pdf", || {
                made.set(made.get() + 1);
                Ok::<Rc<String>, &str>(Rc::new("A".to_string()))
            })
            .unwrap();
        let b = slot
            .get_or_try_insert("book.pdf", || {
                made.set(made.get() + 1);
                Ok::<Rc<String>, &str>(Rc::new("B".to_string()))
            })
            .unwrap();
        assert_eq!(made.get(), 1, "同路径必须复用，不能重新构造");
        assert!(Rc::ptr_eq(&a, &b));

        let c = slot
            .get_or_try_insert("other.pdf", || {
                made.set(made.get() + 1);
                Ok::<Rc<String>, &str>(Rc::new("C".to_string()))
            })
            .unwrap();
        assert_eq!(made.get(), 2, "换路径才重开");
        assert_eq!(*c, "C");
        assert_eq!(slot.current_key(), Some("other.pdf"));
    }

    #[test]
    fn slot_cache_failure_keeps_previous_document() {
        let mut slot: SlotCache<String> = SlotCache::new();
        slot.get_or_try_insert("good.pdf", || {
            Ok::<Rc<String>, &str>(Rc::new("good".to_string()))
        })
        .unwrap();
        let r: Result<Rc<String>, &str> = slot.get_or_try_insert("bad.pdf", || Err("打不开"));
        assert!(r.is_err());
        assert_eq!(
            slot.current_key(),
            Some("good.pdf"),
            "失败的打开不该踢掉旧文档"
        );
    }

    /// 手动跑：`RM_BENCH=1 cargo test --lib doc::pagecache::tests::bench_hit_copy -- --nocapture`
    /// 命中路径的成本 = 把缓存里的灰度拷一份给调用方（1323×1872 ≈ 2.48 MB）。
    #[test]
    fn bench_hit_copy() {
        if std::env::var("RM_BENCH").is_err() {
            return;
        }
        let (w, h) = (1323u32, 1872u32);
        let mut cache = PageCache::new(DEFAULT_PAGES);
        cache.put(0, w, h, vec![7u8; (w * h) as usize]);
        let rounds = 200u32;
        let t = std::time::Instant::now();
        for _ in 0..rounds {
            std::hint::black_box(cache.find(0, w, h).unwrap().to_vec());
        }
        let dt = t.elapsed() / rounds;
        eprintln!(
            "[bench] 缓存命中拷贝 {w}×{h} = {:.2} MB：{dt:?}/次（{:.0} MB/s）",
            (w * h) as f64 / 1e6,
            (w * h) as f64 / dt.as_secs_f64() / 1e6
        );
    }

    #[test]
    fn zero_capacity_is_passthrough() {
        let mut cache = PageCache::new(0);
        let calls = Cell::new(0);
        cache
            .get_or_render(0, 4, 4, || {
                calls.set(calls.get() + 1);
                Ok(vec![0; 16])
            })
            .unwrap();
        cache
            .get_or_render(0, 4, 4, || {
                calls.set(calls.get() + 1);
                Ok(vec![0; 16])
            })
            .unwrap();
        assert_eq!(calls.get(), 2);
        assert!(cache.is_empty());
    }
}
