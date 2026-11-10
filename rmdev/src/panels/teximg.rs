//! 公式图（LaTeX → 灰度 PNG）的**进程级缓存**。C15 地基，错题屏 / 墨迹回复共用。
//!
//! 服务器 `GET /api/v1/tex?f=<公式>&scale=N` 出 8 位灰度 PNG。pdflatex 冷启动
//! 就要几百 ms，所以规矩是：
//!
//! * **主线程绝不发请求**。调用方只 `cached()` 查缓存 + `request()` 挂后台任务，
//!   没有图的时候先画占位（[`crate::panels::ai::Img`] 那条路子也一样）；
//! * 同一个 `(公式, scale)` 只取一次，`Arc` 共享；**失败也缓存**，
//!   否则一次断网会让每帧都去重试、把网络打爆；
//! * [`epoch`] 每落地一张图 +1，调用方拿它判断"要不要重画"，
//!   不用自己维护回调。
//!
//! 缓存不落盘：服务器那边已经有 `<state>/cache/tex/` 了，设备再存一份只是
//! 白占 eMMC。

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use crate::net;
use crate::panels::ai::png_to_gray;

/// 单次取图的上限。冷启动 pdflatex + 字体加载 1~2 s 是常态，
/// 服务器自己还有 20 s 的编译上限，这里留点余量。
const TIMEOUT: Duration = Duration::from_secs(30);
/// 一张公式图的体积上限，防止对端发疯把内存吃光。
const MAX_BYTES: u64 = 8 * 1024 * 1024;

/// 解码后的一张灰度图（255 = 白）。
#[derive(Debug)]
pub struct Img {
    pub w: usize,
    pub h: usize,
    pub gray: Vec<u8>,
}

impl Img {
    /// 取一行的引用（越界给空 slice）。
    pub fn row(&self, y: usize) -> &[u8] {
        if y >= self.h {
            return &[];
        }
        &self.gray[y * self.w..(y + 1) * self.w]
    }

    /// 裁掉四周的白边，只留内容包围盒。公式图默认带 `standalone` 的 padding，
    /// 直接贴到别处会多出一圈空白，行内排版尤其难看。
    pub fn cropped(&self) -> Img {
        let (mut x0, mut y0, mut x1, mut y1) = (self.w, self.h, 0usize, 0usize);
        for y in 0..self.h {
            let row = self.row(y);
            for (x, &v) in row.iter().enumerate() {
                if v < 250 {
                    x0 = x0.min(x);
                    x1 = x1.max(x);
                    y0 = y0.min(y);
                    y1 = y1.max(y);
                }
            }
        }
        if x0 > x1 || y0 > y1 {
            return Img {
                w: 0,
                h: 0,
                gray: Vec::new(),
            };
        }
        let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
        let mut gray = Vec::with_capacity(w * h);
        for y in y0..=y1 {
            gray.extend_from_slice(&self.row(y)[x0..=x1]);
        }
        Img { w, h, gray }
    }

    /// 最近邻缩放到高度 `th`（宽度按比例）。公式图存的是 2 倍宽，
    /// 排到纸上要缩回 1 倍左右，最近邻够了——16 级灰下双线性看不出差别。
    pub fn scaled_to_h(&self, th: usize) -> Img {
        if self.h == 0 || self.w == 0 || th == 0 {
            return Img {
                w: 0,
                h: 0,
                gray: Vec::new(),
            };
        }
        let tw = ((self.w as f32) * (th as f32) / (self.h as f32)).round() as usize;
        let tw = tw.max(1);
        let mut gray = vec![255u8; tw * th];
        for y in 0..th {
            let sy = (y * self.h / th).min(self.h - 1);
            for x in 0..tw {
                let sx = (x * self.w / tw).min(self.w - 1);
                gray[y * tw + x] = self.gray[sy * self.w + sx];
            }
        }
        Img {
            w: tw,
            h: th,
            gray,
        }
    }
}

/// `(公式, scale)` 一个槽：`None` = 取过但失败了。
type Slot = Option<Arc<Img>>;

struct Cache {
    map: HashMap<(String, u32), Slot>,
    inflight: HashSet<(String, u32)>,
    epoch: Arc<AtomicU64>,
}

fn cache() -> &'static Mutex<Cache> {
    static C: OnceLock<Mutex<Cache>> = OnceLock::new();
    C.get_or_init(|| {
        Mutex::new(Cache {
            map: HashMap::new(),
            inflight: HashSet::new(),
            epoch: Arc::new(AtomicU64::new(0)),
        })
    })
}

/// 只查缓存。`Some(None)` = 取过且失败（画占位），`None` = 还没开始取。
fn probe(f: &str, scale: u32) -> Option<Slot> {
    let g = cache().lock().unwrap_or_else(|e| e.into_inner());
    g.map.get(&(f.to_string(), scale)).cloned()
}

/// 缓存里有没有（成功的那张）。
pub fn cached(f: &str, scale: u32) -> Option<Arc<Img>> {
    probe(f, scale).flatten()
}

/// 记一次失败（幂等）。调用方在渲染占位时用它区分"还没好"和"永远不会好"。
pub fn failed(f: &str, scale: u32) -> bool {
    matches!(probe(f, scale), Some(None))
}

/// 某张图是否还在飞（用来决定要不要等/重画）。
pub fn inflight_of(f: &str, scale: u32) -> bool {
    let g = cache().lock().unwrap_or_else(|e| e.into_inner());
    g.inflight.contains(&(f.to_string(), scale))
}

/// 还有没有在飞的取图任务。
pub fn inflight() -> bool {
    let g = cache().lock().unwrap_or_else(|e| e.into_inner());
    !g.inflight.is_empty()
}

/// 图片落地的版本号。每成功插入一张 +1。
pub fn epoch() -> u64 {
    let g = cache().lock().unwrap_or_else(|e| e.into_inner());
    g.epoch.load(Ordering::Relaxed)
}

/// 直接塞一张已经解好的图（单测和"我已经有图了"的场景用）。
pub fn seed(f: &str, scale: u32, w: usize, h: usize, gray: Vec<u8>) {
    let mut g = cache().lock().unwrap_or_else(|e| e.into_inner());
    let arc = Arc::new(Img { w, h, gray });
    g.map.insert((f.to_string(), scale), Some(arc));
    g.inflight.remove(&(f.to_string(), scale));
    g.epoch.fetch_add(1, Ordering::Relaxed);
}

fn insert(f: &str, scale: u32, img: Slot) {
    let mut g = cache().lock().unwrap_or_else(|e| e.into_inner());
    let ok = img.is_some();
    g.map.insert((f.to_string(), scale), img);
    g.inflight.remove(&(f.to_string(), scale));
    if ok {
        g.epoch.fetch_add(1, Ordering::Relaxed);
    }
}

/// 缓存里没有就挂一个后台线程去取。同一 `(公式, scale)` 不会重复发。
///
/// 主线程调用**立即返回**，取到的图走 [`cached`] 拿。
pub fn request(f: &str, scale: u32) {
    if f.trim().is_empty() {
        return;
    }
    {
        let mut g = cache().lock().unwrap_or_else(|e| e.into_inner());
        if g.map.contains_key(&(f.to_string(), scale)) {
            return;
        }
        let key = (f.to_string(), scale);
        if !g.inflight.insert(key) {
            return;
        }
    }
    let f = f.to_string();
    let _ = std::thread::Builder::new()
        .name("tex-fetch".into())
        .spawn(move || {
            let _ = get_or_fetch(&f, scale);
        });
}

/// 一次挂一批（错题屏整页的公式都塞进来）。
pub fn request_many<'a, I: IntoIterator<Item = &'a str>>(list: I, scale: u32) {
    for f in list {
        request(f, scale);
    }
}

/// 查缓存，没有就**同步**去取，取到（或失败）都写进缓存。
///
/// **只给后台线程用**：主线程调它会把界面卡到网络超时。
pub fn get_or_fetch(f: &str, scale: u32) -> Option<Arc<Img>> {
    {
        let g = cache().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(slot) = g.map.get(&(f.to_string(), scale)) {
            return slot.clone();
        }
    }
    let got = download(f, scale).map(Arc::new);
    let mut g = cache().lock().unwrap_or_else(|e| e.into_inner());
    // 别人可能在这期间已经塞进去了（并行取同一张）：以先到的为准
    if let Some(existing) = g.map.get(&(f.to_string(), scale)).cloned() {
        g.inflight.remove(&(f.to_string(), scale));
        return existing;
    }
    let ok = got.is_some();
    g.map.insert((f.to_string(), scale), got.clone());
    g.inflight.remove(&(f.to_string(), scale));
    if ok {
        g.epoch.fetch_add(1, Ordering::Relaxed);
    }
    got
}

/// 真正去网络取一张（不查缓存、不写缓存）。
fn download(f: &str, scale: u32) -> Option<Img> {
    let path = format!("/api/v1/tex?f={}&scale={}", urlencode(f), scale);
    let rd = net::open_stream("GET", &path, &[], TIMEOUT).ok()?;
    let mut png = Vec::new();
    rd.take(MAX_BYTES).read_to_end(&mut png).ok()?;
    if png.is_empty() {
        return None;
    }
    let (w, h, gray) = png_to_gray(&png)?;
    if w == 0 || h == 0 || gray.len() < w * h {
        return None;
    }
    Some(Img { w, h, gray })
}

/// 查询串转义。公式里 `+ ^ { } \` 全是保留字符，必须一个不落地转。
pub fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 清空缓存（单测之间隔离用）。
#[cfg(test)]
pub fn reset() {
    let mut g = cache().lock().unwrap_or_else(|e| e.into_inner());
    g.map.clear();
    g.inflight.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 缓存是进程级的，cargo 默认并行跑单测，不加锁会互相 `reset` 掉对方。
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        static L: OnceLock<Mutex<()>> = OnceLock::new();
        let g = L.get_or_init(|| Mutex::new(())).lock().unwrap_or_else(|e| e.into_inner());
        reset();
        g
    }

    #[test]
    fn urlencode_escapes_tex_reserved() {
        assert_eq!(urlencode("x^2+y"), "x%5E2%2By");
        assert_eq!(urlencode(r"\frac{a}{b}"), "%5Cfrac%7Ba%7D%7Bb%7D");
        assert_eq!(urlencode("abc-_.~"), "abc-_.~");
        // 中文（多字节）按字节转，服务器按 UTF-8 解
        assert_eq!(urlencode("价"), "%E4%BB%B7");
    }

    fn gray_img(w: usize, h: usize) -> Vec<u8> {
        vec![255; w * h]
    }

    #[test]
    fn seed_then_cached_hits_and_bumps_epoch() {
        let _g = lock();
        let e0 = epoch();
        seed("x^2", 2, 4, 4, gray_img(4, 4));
        assert!(cached("x^2", 2).is_some());
        assert!(epoch() > e0);
        // scale 进 key：不同 scale 不命中
        assert!(cached("x^2", 1).is_none());
        assert!(cached("y^2", 2).is_none());
    }

    #[test]
    fn failed_is_distinct_from_absent() {
        let _g = lock();
        assert!(!failed("a", 2));
        assert!(cached("a", 2).is_none());
        insert("a", 2, None);
        assert!(failed("a", 2));
        assert!(cached("a", 2).is_none());
    }

    #[test]
    fn epoch_does_not_bump_on_failure() {
        let _g = lock();
        let e0 = epoch();
        insert("bad", 2, None);
        assert_eq!(epoch(), e0);
    }

    #[test]
    fn request_dedupes_inflight() {
        let _g = lock();
        // 指向一个必然连不上的地址：只要不 panic、不重复挂线程即可
        request("z", 2);
        // 第一次可能已经失败返回了，所以这里不断言 inflight；
        // 再调一次不能 panic，也不能把已缓存的槽冲掉。
        request("z", 2);
        assert!(probe("z", 2).is_some() || inflight_of("z", 2));
    }

    #[test]
    fn empty_request_is_ignored() {
        let _g = lock();
        request("   ", 2);
        assert!(probe("   ", 2).is_none());
    }

    #[test]
    fn crop_drops_uniform_white_border() {
        // 6x6 中间 2x2 是黑
        let mut g = gray_img(6, 6);
        for y in 2..4 {
            for x in 2..4 {
                g[y * 6 + x] = 0;
            }
        }
        let img = Img {
            w: 6,
            h: 6,
            gray: g,
        };
        let c = img.cropped();
        assert_eq!((c.w, c.h), (2, 2));
        assert!(c.gray.iter().all(|&v| v == 0));
    }

    #[test]
    fn crop_of_blank_is_empty() {
        let img = Img {
            w: 4,
            h: 4,
            gray: gray_img(4, 4),
        };
        assert_eq!(img.cropped().w, 0);
    }

    #[test]
    fn scaled_keeps_aspect_and_reaches_target_height() {
        let img = Img {
            w: 40,
            h: 20,
            gray: gray_img(40, 20),
        };
        let s = img.scaled_to_h(10);
        assert_eq!(s.h, 10);
        assert_eq!(s.w, 20);
        assert_eq!(s.gray.len(), 200);
    }

    #[test]
    fn scaled_never_zero() {
        let img = Img {
            w: 1000,
            h: 3,
            gray: gray_img(1000, 3),
        };
        let s = img.scaled_to_h(1);
        assert!(s.w >= 1);
        assert_eq!(s.h, 1);
    }
}

// ---------------------------------------------------------------- 文本里的公式

/// 一到数学就开拨的字符：见到它们就认为后面是公式。
fn math_start(c: char) -> bool {
    matches!(
        c,
        '∫' | '∬'
            | '∭'
            | '∮'
            | '∑'
            | '∏'
            | '√'
            | '∂'
            | '∇'
            | '∞'
            | '≃'
            | '≈'
    )
}

/// 见到就认为公式结束（正文又开始了）。
fn math_stop(c: char) -> bool {
    // 中文字符 + 中文标点：真机上的题目原文是中文叙述夹一段式子
    if ('\u{4e00}'..='\u{9fff}').contains(&c) {
        return true;
    }
    matches!(c, '。' | '，' | '；' | '：' | '、' | '！' | '？' | '（' | '）' | '「' | '」')
}

/// Unicode 数学符号 → TeX 命令。
fn sym_tex(c: char) -> Option<&'static str> {
    Some(match c {
        '∫' => r"\int",
        '∬' => r"\iint",
        '∭' => r"\iiint",
        '∮' => r"\oint",
        '∑' => r"\sum",
        '∏' => r"\prod",
        '√' => r"\sqrt",
        '∂' => r"\partial",
        '∇' => r"\nabla",
        '∞' => r"\infty",
        '±' => r"\pm",
        '∓' => r"\mp",
        '×' => r"\times",
        '÷' => r"\div",
        '·' => r"\cdot",
        '≠' => r"\neq",
        '≤' => r"\leq",
        '≥' => r"\geq",
        '≈' => r"\approx",
        '≡' => r"\equiv",
        '≃' => r"\simeq",
        '→' => r"\to",
        '⇒' => r"\Rightarrow",
        '∈' => r"\in",
        '∉' => r"\notin",
        '⊂' => r"\subset",
        '⊆' => r"\subseteq",
        '∪' => r"\cup",
        '∩' => r"\cap",
        'π' => r"\pi",
        'α' => r"\alpha",
        'β' => r"\beta",
        'γ' => r"\gamma",
        'δ' => r"\delta",
        'ε' => r"\varepsilon",
        'θ' => r"\theta",
        'λ' => r"\lambda",
        'μ' => r"\mu",
        'σ' => r"\sigma",
        'φ' => r"\varphi",
        'ω' => r"\omega",
        'Δ' => r"\Delta",
        'Σ' => r"\Sigma",
        'Ω' => r"\Omega",
        '′' => "'",
        '″' => "''",
        '⁻' => "-",
        '²' => "^2",
        '³' => "^3",
        _ => return None,
    })
}

/// 常见函数名：`sin x` 排成直立罗马体才像公式。
const FUNCS: &[&str] = &[
    "sin", "cos", "tan", "cot", "sec", "csc", "arcsin", "arccos", "arctan", "sinh", "cosh",
    "tanh", "ln", "log", "lg", "exp", "lim", "max", "min", "sup", "inf", "det", "gcd",
];

fn texify_body(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        let c = chars[i];
        // 函数名：前后都不是字母才算（`asin` 里的 `sin` 不能换）
        if c.is_ascii_alphabetic() {
            let mut j = i;
            while j < chars.len() && chars[j].is_ascii_alphabetic() {
                j += 1;
            }
            let word: String = chars[i..j].iter().collect();
            let prev_alpha = i > 0 && chars[i - 1].is_ascii_alphabetic();
            if !prev_alpha && FUNCS.contains(&word.as_str()) {
                out.push('\\');
                out.push_str(&word);
                // 原文后面已经有空格就不要再补一个（`\sin  x` 排出来像两个间隔）
                if chars.get(j) != Some(&' ') {
                    out.push(' ');
                }
            } else {
                out.push_str(&word);
            }
            i = j;
            continue;
        }
        // 上标/下标的 Unicode 字符（`e⁻ˣ` 这种）先按普通字符处理
        match sym_tex(c) {
            Some(t) => out.push_str(t),
            None => out.push(c),
        }
        i += 1;
    }
    out
}

/// 把一段"人话里的公式"变成带 `$…$` 的 TeX。
///
/// 服务器早期把题目原文存成了 Unicode 伪公式（`∫_{0}^{+∞} x e^{-x} dx`），
/// 直接显示就是一坨反斜杠和花括号。这里做的只有两件事：
///
/// * **已经带 `$` 的原样返回**（新数据就是 TeX，不用动）；
/// * 否则从第一个数学符号开始、到中文/中文标点为止，把这一段当公式，
///   做符号 → TeX 命令的替换，再套上 `$…$`。
///
/// 没找到数学符号就原样返回——宁可不动，也不要把叙述性文字包成公式。
pub fn latexify(s: &str) -> String {
    if s.contains('$') {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let Some(start) = chars.iter().position(|&c| math_start(c)) else {
        return s.to_string();
    };
    let mut end = chars.len();
    for (i, &c) in chars.iter().enumerate().skip(start) {
        if math_stop(c) {
            end = i;
            break;
        }
    }
    let head: String = chars[..start].iter().collect();
    let body: String = chars[start..end].iter().collect();
    let tail: String = chars[end..].iter().collect();
    if body.trim().is_empty() {
        return s.to_string();
    }
    format!("{head}${body}${tail}", body = texify_body(body.trim()))
}

#[cfg(test)]
mod latexify_tests {
    use super::latexify;

    #[test]
    fn already_tex_is_untouched() {
        assert_eq!(latexify(r"$\int_0^1 x dx$"), r"$\int_0^1 x dx$");
    }

    #[test]
    fn plain_prose_without_math_is_untouched() {
        let s = "2. 计算这个式子的值";
        assert_eq!(latexify(s), s);
    }

    #[test]
    fn url_is_untouched() {
        let s = "见 https://example.com/a?b=1";
        assert_eq!(latexify(s), s);
    }

    #[test]
    fn unicode_integral_becomes_tex() {
        // 服务器现在存的就是这种形状
        assert_eq!(
            latexify("1. 计算定积分：∫_{0}^{1} (3x^2 - 2x + 1) dx"),
            r"1. 计算定积分：$\int_{0}^{1} (3x^2 - 2x + 1) dx$"
        );
    }

    #[test]
    fn infinity_and_mixed_greek() {
        assert_eq!(
            latexify("∫_{0}^{+∞} x e^{-x} dx"),
            r"$\int_{0}^{+\infty} x e^{-x} dx$"
        );
        assert_eq!(latexify("∫_{0}^{π/2} sin x"), r"$\int_{0}^{\pi/2} \sin x$");
    }

    #[test]
    fn stops_at_cjk() {
        assert_eq!(
            latexify("∫_{0}^{1} x dx 是多少"),
            r"$\int_{0}^{1} x dx$是多少"
        );
    }

    #[test]
    fn function_names_get_backslash_but_not_inside_words() {
        assert_eq!(latexify("∫ sin x + cos x"), r"$\int \sin x + \cos x$");
        // `asin` 里的 `sin` 不能被替换成 `a\sin`
        assert_eq!(latexify("∫ asin x"), r"$\int asin x$");
    }

    #[test]
    fn sqrt_and_relations() {
        assert_eq!(latexify("√2 ≤ 3"), r"$\sqrt2 \leq 3$");
    }
}
