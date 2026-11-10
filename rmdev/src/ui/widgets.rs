//! 通用界面零件：命中区和各种图标。
//!
//! 每个界面绘制时把自己的可点区域 `push` 进 `Hits`，手指落点用 `take` 查。
//! 这样新界面不用再手写坐标判断，也不会出现"画了按钮但点不到"。
//!
//! 所有尺寸都来自官方截图逐像素测量（`tools/probe.py`）。

use crate::fb::{gray565, Fb};

// ---------------------------------------------------------------- 命中区

/// 命中区（屏幕像素）+ 一个调用方自定义的动作编号。
pub struct Hits(Vec<(i32, i32, i32, i32, u32)>);

impl Hits {
    pub fn new() -> Hits {
        Hits(Vec::new())
    }
    pub fn clear(&mut self) {
        self.0.clear();
    }
    pub fn push(&mut self, x: i32, y: i32, w: i32, h: i32, action: u32) {
        self.0.push((x, y, w, h, action));
    }
    /// 命中测试。后压的优先（后画的在上层）。
    pub fn take(&self, x: f32, y: f32) -> Option<u32> {
        let (x, y) = (x as i32, y as i32);
        self.0
            .iter()
            .rev()
            .find(|(hx, hy, hw, hh, _)| x >= *hx && y >= *hy && x < hx + hw && y < hy + hh)
            .map(|&(.., a)| a)
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Default for Hits {
    fn default() -> Self {
        Hits::new()
    }
}

// ---------------------------------------------------------------- 基础绘制

pub fn disc(fb: &mut Fb, cx: i32, cy: i32, r: i32, tone: u8) {
    let c = gray565(tone);
    let rr = (r * r) as f32;
    for dy in -r..=r {
        for dx in -r..=r {
            if (dx * dx + dy * dy) as f32 <= rr {
                fb.put(cx + dx, cy + dy, c);
            }
        }
    }
}

pub fn ring(fb: &mut Fb, cx: i32, cy: i32, r: i32, t: i32, tone: u8) {
    let c = gray565(tone);
    let rr = (r * r) as f32;
    let ri = ((r - t) * (r - t)) as f32;
    for dy in -r..=r {
        for dx in -r..=r {
            let d = (dx * dx + dy * dy) as f32;
            if d <= rr && d >= ri {
                fb.put(cx + dx, cy + dy, c);
            }
        }
    }
}

pub fn hline(fb: &mut Fb, x: i32, y: i32, w: i32, tone: u8) {
    fb.fill_rect(x, y, w, 1, gray565(tone));
}

pub fn outline_rect(fb: &mut Fb, x: i32, y: i32, w: i32, h: i32, t: i32, tone: u8) {
    let c = gray565(tone);
    for i in 0..w {
        fb.fill_rect(x + i, y, 1, t, c);
        fb.fill_rect(x + i, y + h - t, 1, t, c);
    }
    for i in 0..h {
        fb.fill_rect(x, y + i, t, 1, c);
        fb.fill_rect(x + w - t, y + i, t, 1, c);
    }
}

/// 多边形填充（奇偶规则）。
pub fn poly(fb: &mut Fb, pts: &[(i32, i32)], tone: u8) {
    let c = gray565(tone);
    if pts.len() < 3 {
        return;
    }
    let ymin = pts.iter().map(|p| p.1).min().unwrap();
    let ymax = pts.iter().map(|p| p.1).max().unwrap();
    let xmin = pts.iter().map(|p| p.0).min().unwrap();
    let xmax = pts.iter().map(|p| p.0).max().unwrap();
    for y in ymin..=ymax {
        let mut xs: Vec<f32> = Vec::new();
        for k in 0..pts.len() {
            let (x0, y0) = pts[k];
            let (x1, y1) = pts[(k + 1) % pts.len()];
            if (y0 <= y && y1 > y) || (y1 <= y && y0 > y) {
                let t = (y - y0) as f32 / (y1 - y0) as f32;
                xs.push(x0 as f32 + (x1 - x0) as f32 * t);
            }
        }
        xs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        for pair in xs.chunks(2) {
            if pair.len() == 2 {
                let a = pair[0].max(xmin as f32);
                let b = pair[1].min(xmax as f32);
                if b > a {
                    fb.fill_rect(a as i32, y, (b - a) as i32, 1, c);
                }
            }
        }
    }
}

/// 两点之间画一条粗线。
pub fn line(fb: &mut Fb, x0: i32, y0: i32, x1: i32, y1: i32, t: i32, tone: u8) {
    let c = gray565(tone);
    let n = ((x1 - x0).abs()).max((y1 - y0).abs()).max(1);
    for k in 0..=n {
        let f = k as f32 / n as f32;
        let px = x0 + ((x1 - x0) as f32 * f) as i32;
        let py = y0 + ((y1 - y0) as f32 * f) as i32;
        fb.fill_rect(px, py, t, t, c);
    }
}

// ---------------------------------------------------------------- 覆盖率光栅化

/// 4×4 超采样的形状填充：`inside` 判子像素中心是否落在图形里，再按覆盖度
/// 从底色 `bg` 混到 `tone`。坐标是相对 `(x, y)` 的局部坐标。
///
/// 为什么不用一串 `fill_rect` 拼：工具条图标大多是圆弧和斜边，逐点铺方块会在
/// 边上留一圈台阶（和官方 Qt 渲染差得很明显）。整数对齐的矩形这里覆盖率恰好
/// 是 0 或 1，边仍然是硬的，所以同一套图元既能画直边也能画曲线。
pub fn shade(
    fb: &mut Fb,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    tone: u8,
    bg: u8,
    inside: impl Fn(f32, f32) -> bool,
) {
    let cov = coverage(w, h, &inside);
    blit_cov(fb, x, y, w, h, tone, bg, &cov);
}

/// 图标光栅缓存：图标是**静态**的，每帧重算 4×4 超采样纯属浪费。
///
/// 一个 40×40 的图标 = 1600 像素 × 16 次采样 = 2.56 万次 `inside`，而工具条
/// 一次要画 12 个、笔菜单再加 9 个 —— 实测重绘一次工具条 50ms、开个菜单 110ms，
/// 这就是"菜单反应慢"的全部来源（ioctl 只要 0.8ms）。
/// 现在同一张图标只算一次，之后就是一次 1600 像素的贴图。
pub fn shade_at(
    id: u32,
    fb: &mut Fb,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    tone: u8,
    bg: u8,
    inside: impl Fn(f32, f32) -> bool,
) {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::rc::Rc;
    thread_local! {
        static CACHE: RefCell<HashMap<(u32, u8, u8), Rc<Vec<u8>>>> = RefCell::new(HashMap::new());
    }
    let key = (id, tone, bg);
    let cov = CACHE.with(|c| c.borrow().get(&key).cloned());
    let cov = match cov {
        Some(v) => v,
        None => {
            let v = Rc::new(coverage(w, h, &inside));
            CACHE.with(|c| c.borrow_mut().insert(key, v.clone()));
            v
        }
    };
    blit_cov(fb, x, y, w, h, tone, bg, &cov);
}

/// 字符串 → 稳定 id（FNV-1a）。给缓存键用，不用来加密。
pub fn sid(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// 4×4 超采样求覆盖率，返回每个像素的命中数（0..=16）。
fn coverage(w: i32, h: i32, inside: &impl Fn(f32, f32) -> bool) -> Vec<u8> {
    let w = w.max(0) as usize;
    let h = h.max(0) as usize;
    let mut cov = vec![0u8; w * h];
    const N: i32 = 4;
    let step = 1.0 / N as f32;
    // 先按 1 次采样求"有墨"的紧包围盒，再只在盒内做 4×4 超采样。
    //
    // 原来整格都要扫：图标实际只占格子里一小块，采样点又不会跨出本像素，
    // 所以"先把盒找出来"画出来的结果和整格扫**逐像素相同**，只是把 2.5 万次
    // 降到几千次。
    let (mut bx0, mut by0, mut bx1, mut by1) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for py in 0..h as i32 {
        for px in 0..w as i32 {
            if inside(px as f32 + 0.5, py as f32 + 0.5) {
                bx0 = bx0.min(px);
                by0 = by0.min(py);
                bx1 = bx1.max(px);
                by1 = by1.max(py);
            }
        }
    }
    if bx1 < bx0 {
        return cov;
    }
    // 外扩 1px：半覆盖像素自己不在盒里，但它的采样点可能落到盒内像素边上。
    let bx0 = (bx0 - 1).max(0);
    let by0 = (by0 - 1).max(0);
    let bx1 = (bx1 + 1).min(w as i32 - 1);
    let by1 = (by1 + 1).min(h as i32 - 1);
    for py in by0..=by1 {
        for px in bx0..=bx1 {
            let mut hit = 0u8;
            for sy in 0..N {
                for sx in 0..N {
                    if inside(px as f32 + (sx as f32 + 0.5) * step, py as f32 + (sy as f32 + 0.5) * step) {
                        hit += 1;
                    }
                }
            }
            cov[py as usize * w + px as usize] = hit;
        }
    }
    cov
}

/// 把覆盖率贴进帧缓冲。
fn blit_cov(fb: &mut Fb, x: i32, y: i32, w: i32, h: i32, tone: u8, bg: u8, cov: &[u8]) {
    let w = w.max(0) as usize;
    let h = h.max(0) as usize;
    let (lo, hi) = (bg as f32, tone as f32);
    for py in 0..h {
        for px in 0..w {
            let hit = cov[py * w + px];
            if hit == 0 {
                continue;
            }
            let g = lo + (hi - lo) * (hit as f32 / 16.0);
            fb.put(x + px as i32, y + py as i32, gray565(g.round() as u8));
        }
    }
}


/// 点到线段的距离。
pub fn dist_seg(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let l2 = dx * dx + dy * dy;
    let t = if l2 > 0.0 {
        (((px - ax) * dx + (py - ay) * dy) / l2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (qx, qy) = (ax + dx * t - px, ay + dy * t - py);
    (qx * qx + qy * qy).sqrt()
}

/// 圆头粗线（两端各半个圆），`t` 是线宽。
pub fn in_capsule(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32, t: f32) -> bool {
    dist_seg(px, py, ax, ay, bx, by) <= t * 0.5
}

/// 实心圆。
pub fn in_disc(px: f32, py: f32, cx: f32, cy: f32, r: f32) -> bool {
    let (dx, dy) = (px - cx, py - cy);
    dx * dx + dy * dy <= r * r
}

/// 圆环（`r` 是外半径，`t` 是环宽）。
pub fn in_ring(px: f32, py: f32, cx: f32, cy: f32, r: f32, t: f32) -> bool {
    let (dx, dy) = (px - cx, py - cy);
    let d = dx * dx + dy * dy;
    d <= r * r && d >= (r - t) * (r - t)
}

/// 轴对齐矩形。
pub fn in_rect(px: f32, py: f32, x: f32, y: f32, w: f32, h: f32) -> bool {
    px >= x && px < x + w && py >= y && py < y + h
}

/// 空心矩形（描边 `t`，边朝内长）。
pub fn in_frame(px: f32, py: f32, x: f32, y: f32, w: f32, h: f32, t: f32) -> bool {
    in_rect(px, py, x, y, w, h) && !in_rect(px, py, x + t, y + t, w - 2.0 * t, h - 2.0 * t)
}

/// 把折线加宽成多边形（斜接角，两端平头）。返回值可以直接喂给 [`in_poly`]。
///
/// 圆头粗线（[`in_capsule`]）在折点处会鼓出一个半圆，画菱形/箭头这种尖角
/// 图形时轮廓会明显比官方胖；这里按斜接算，角还是尖的。
pub fn thick(pts: &[(f32, f32)], w: f32) -> Vec<(f32, f32)> {
    let closed = pts.len() > 2 && pts[0] == pts[pts.len() - 1];
    let v: Vec<(f32, f32)> = if closed {
        pts[..pts.len() - 1].to_vec()
    } else {
        pts.to_vec()
    };
    let n = v.len();
    if n < 2 {
        return v;
    }
    let unit = |a: (f32, f32), b: (f32, f32)| {
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        let l = (dx * dx + dy * dy).sqrt().max(1e-6);
        (dx / l, dy / l)
    };
    let half = w * 0.5;
    let (mut left, mut right) = (Vec::with_capacity(n), Vec::with_capacity(n));
    for i in 0..n {
        let prev = if i > 0 {
            Some(unit(v[i - 1], v[i]))
        } else if closed {
            Some(unit(v[n - 1], v[0]))
        } else {
            None
        };
        let next = if i + 1 < n {
            Some(unit(v[i], v[i + 1]))
        } else if closed {
            Some(unit(v[0], v[1]))
        } else {
            None
        };
        let nrm = |d: (f32, f32)| (-d.1, d.0);
        let (np, nn) = (prev.map(nrm), next.map(nrm));
        let dir = match (np, nn) {
            (Some(a), Some(b)) => {
                let (sx, sy) = (a.0 + b.0, a.1 + b.1);
                let l = (sx * sx + sy * sy).sqrt().max(1e-6);
                (sx / l, sy / l)
            }
            (Some(a), None) => a,
            (None, Some(b)) => b,
            (None, None) => (0.0, 0.0),
        };
        // 1/cos(夹角/2)：斜接点要沿角平分线往外推，太尖就截断
        let k = match (np, nn) {
            (Some(a), Some(_)) => 1.0 / (dir.0 * a.0 + dir.1 * a.1).max(0.35),
            _ => 1.0,
        };
        let (ox, oy) = (dir.0 * half * k, dir.1 * half * k);
        left.push((v[i].0 + ox, v[i].1 + oy));
        right.push((v[i].0 - ox, v[i].1 - oy));
    }
    left.extend(right.into_iter().rev());
    left
}

/// 点是否在多边形内（奇偶规则）。
pub fn in_poly(px: f32, py: f32, pts: &[(f32, f32)]) -> bool {
    let n = pts.len();
    if n < 3 {
        return false;
    }
    let mut inside = false;
    let mut j = n - 1;
    for i in 0..n {
        let (xi, yi) = pts[i];
        let (xj, yj) = pts[j];
        if (yi > py) != (yj > py) && px < xi + (py - yi) / (yj - yi) * (xj - xi) {
            inside = !inside;
        }
        j = i;
    }
    inside
}

fn arc(fb: &mut Fb, cx: i32, cy: i32, r: i32, a0: f32, a1: f32, t: i32, tone: u8) {
    let c = gray565(tone);
    let n = (r * 6).max(24);
    for k in 0..=n {
        let a = a0 + (a1 - a0) * k as f32 / n as f32;
        let px = cx + (a.cos() * r as f32) as i32;
        let py = cy + (a.sin() * r as f32) as i32;
        fb.fill_rect(px, py, t, t, c);
    }
}

// ---------------------------------------------------------------- 顶栏

/// 返回箭头 `<`，尖端在 `(x, y)`。
pub fn back_arrow(fb: &mut Fb, x: i32, y: i32, arm: i32, tone: u8) {
    let c = gray565(tone);
    for i in 0..arm {
        for t in 0..3 {
            fb.put(x + i, y - i + t, c);
            fb.put(x + i, y + i - t, c);
        }
    }
}

/// 右尖括号 `>`，尖端在 `(x, y)`。
pub fn chevron_right(fb: &mut Fb, x: i32, y: i32, arm: i32, tone: u8) {
    let c = gray565(tone);
    for i in 0..arm {
        for t in 0..3 {
            fb.put(x - i, y - i + t, c);
            fb.put(x - i, y + i - t, c);
        }
    }
}

/// 汉堡图标。实测官方：宽 39，条高 4，行距 15（y=44/59/74）。
pub fn hamburger(fb: &mut Fb, x: i32, y: i32, w: i32, tone: u8) {
    let c = gray565(tone);
    for i in 0..3 {
        fb.fill_rect(x, y + i * 15, w, 4, c);
    }
}

/// 圆圈里一个 `+`。官方顶栏那三个就是这个，外径 40。
pub fn plus_circle(fb: &mut Fb, cx: i32, cy: i32, r: i32, tone: u8) {
    ring(fb, cx, cy, r, 3, tone);
    let c = gray565(tone);
    fb.fill_rect(cx - r / 2, cy - 1, r, 3, c);
    fb.fill_rect(cx - 1, cy - r / 2, 3, r, c);
}

/// 放大镜
/// 放大镜。官方那个把手的朝向是**左下**（不是右下）：圆心在右上，柄往左下伸。
pub fn magnifier(fb: &mut Fb, cx: i32, cy: i32, r: i32, tone: u8) {
    ring(fb, cx, cy, r, 4, tone);
    let c = gray565(tone);
    let len = r + 1;
    // 从圆环左下切线往外，45° 斜柄
    let (sx, sy) = (cx - r * 3 / 4, cy + r * 3 / 4);
    for i in 0..len {
        fb.fill_rect(sx - i, sy + i, 4, 4, c);
    }
}

// ---------------------------------------------------------------- 状态栏

/// 电池。官方实测：x 69..100（30 宽），y 1821..1845。
pub fn battery(fb: &mut Fb, x: i32, y: i32, pct: u8, tone: u8) {
    let c = gray565(tone);
    let (w, h, t) = (30, 24, 3);
    outline_rect(fb, x, y, w, h, t, tone);
    fb.fill_rect(x + w, y + h / 2 - 4, 3, 8, c);
    let innerw = w - 2 * t - 4;
    let fill = (innerw as f32 * (pct.min(100) as f32) / 100.0) as i32;
    fb.fill_rect(x + t + 2, y + t + 2, fill, h - 2 * t - 4, c);
}

/// Wi-Fi：三段开口朝下的弧 + 底部一个点。
pub fn wifi(fb: &mut Fb, cx: i32, bottom: i32, tone: u8) {
    wifi_bars(fb, cx, bottom, tone, 3);
}

/// Wi-Fi：按信号格数画 0..3 段弧（官方那个指示器也是"强弱少两段"）。
///
/// `bars` 来自 [`crate::settings::wifi::signal_bars`]（> -50 三格、> -60 两格、
/// > -70 一格、否则零格）。0 格时只剩底部那个点 —— 没连上时官方也是这个形态。
pub fn wifi_bars(fb: &mut Fb, cx: i32, bottom: i32, tone: u8, bars: u8) {
    for (i, r) in [30i32, 21, 12].iter().enumerate() {
        // 第 0 段是最外圈：bars=3 三圈全画，bars=1 只画最内圈。
        if (3 - i as u8) <= bars {
            arc(fb, cx, bottom, *r, -2.27, -0.87, 3, tone);
        }
    }
    disc(fb, cx, bottom - 3, 4, tone);
}

/// 闪电（充电中）。官方 x 39..56、y 1816..1848。
pub fn bolt(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let pts = [
        (11, 0),
        (0, 18),
        (7, 18),
        (4, 32),
        (18, 12),
        (10, 12),
        (14, 0),
    ];
    let pts: Vec<(i32, i32)> = pts.iter().map(|(px, py)| (x + px, y + py)).collect();
    poly(fb, &pts, tone);
}

/// 云朵（云端同步可用）。和 [`cloud_off`] 同一个外轮廓，只是不画叉。
pub fn cloud(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    // 云：三个半圆 + 底边，实心
    disc(fb, x + 8, y + 14, 7, tone);
    disc(fb, x + 17, y + 11, 9, tone);
    disc(fb, x + 24, y + 14, 7, tone);
    fb.fill_rect(x + 3, y + 12, 27, 9, c);
}

/// 云朵加一个叉（云端同步不可用）。官方 x 264..293、y 1818..1845。
pub fn cloud_off(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    // 云：三个半圆 + 底边，实心
    disc(fb, x + 8, y + 14, 7, tone);
    disc(fb, x + 17, y + 11, 9, tone);
    disc(fb, x + 24, y + 14, 7, tone);
    fb.fill_rect(x + 3, y + 12, 27, 9, c);
    // 右下角：先挖白，再画黑叉
    disc(fb, x + 23, y + 21, 10, 255);
    let (bx, by) = (x + 17, y + 15);
    for i in 0..11 {
        fb.fill_rect(bx + i, by + i, 3, 3, c);
        fb.fill_rect(bx + 10 - i, by + i, 3, 3, c);
    }
}

// ---------------------------------------------------------------- 侧栏图标（40×40）

/// My files：一张带折角的纸
pub fn icon_files(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    outline_rect(fb, x + 4, y + 4, 32, 32, 3, tone);
    for i in 0..10 {
        fb.fill_rect(x + 26 + i, y + 4, 10 - i, 1, c);
    }
}

/// Filters：漏斗（官方是**描边**漏斗：顶边一条 4px 横杠，两侧斜下收成管，管子到底部斜切）
pub fn icon_funnel(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    // 顶边
    fb.fill_rect(x, y, 40, 4, c);
    // 两侧斜边（5px 粗）
    for i in 0..20 {
        fb.fill_rect(x + 3 + i / 2, y + 4 + i, 5, 1, c);
        fb.fill_rect(x + 32 - i / 2, y + 4 + i, 5, 1, c);
    }
    // 两根竖管
    fb.fill_rect(x + 13, y + 20, 4, 14, c);
    fb.fill_rect(x + 23, y + 20, 4, 13, c);
    // 底部斜切
    for i in 0..14 {
        fb.fill_rect(x + 13 + i, y + 37 - i / 3, 4, 2, c);
    }
}

/// 把多边形画成**描边**：先用 `in_poly` 光栅化，再腐蚀一圈，剩下的就是边。
/// 比"沿着边线盖章"干净得多（后者拐角会毛、内侧会鼓）。
pub fn poly_band(fb: &mut Fb, outer: &[(f32, f32)], inner: &[(f32, f32)], tone: u8) {
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &(x, y) in outer {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    let (ix0, iy0) = (x0.floor() as i32 - 1, y0.floor() as i32 - 1);
    let (ix1, iy1) = (x1.ceil() as i32 + 1, y1.ceil() as i32 + 1);
    let c = gray565(tone);
    for y in iy0..iy1 {
        for x in ix0..ix1 {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            if in_poly(px, py, outer) && !in_poly(px, py, inner) {
                fb.put(x, y, c);
            }
        }
    }
}

/// 按**中心线**给多边形描边：到任一条边的距离 ≤ width/2 的像素涂黑。
/// 拐角是圆角（和官方图标字体一致），而且尖端会自然收细。
pub fn poly_stroke(fb: &mut Fb, pts: &[(f32, f32)], width: f32, tone: u8) {
    let r = width / 2.0;
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &(x, y) in pts {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    let (ix0, iy0) = ((x0 - r).floor() as i32, (y0 - r).floor() as i32);
    let (ix1, iy1) = ((x1 + r).ceil() as i32, (y1 + r).ceil() as i32);
    let c = gray565(tone);
    let n = pts.len();
    for y in iy0..=iy1 {
        for x in ix0..=ix1 {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let mut d = f32::MAX;
            for i in 0..n {
                let (ax, ay) = pts[i];
                let (bx, by) = pts[(i + 1) % n];
                d = d.min(dist_seg(px, py, ax, ay, bx, by));
            }
            if d <= r {
                fb.put(x, y, c);
            }
        }
    }
}

/// 把多边形按中心缩放到 `k` 倍（用来做描边的内轮廓）。
pub fn scale_about(pts: &[(f32, f32)], k: f32) -> Vec<(f32, f32)> {
    let n = pts.len() as f32;
    let (cx, cy) = (
        pts.iter().map(|p| p.0).sum::<f32>() / n,
        pts.iter().map(|p| p.1).sum::<f32>() / n,
    );
    pts.iter().map(|&(x, y)| (cx + (x - cx) * k, cy + (y - cy) * k)).collect()
}

pub fn poly_outline(fb: &mut Fb, pts: &[(f32, f32)], stroke: i32, tone: u8) {
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for &(x, y) in pts {
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x);
        y1 = y1.max(y);
    }
    let (ix0, iy0) = (x0.floor() as i32 - 1, y0.floor() as i32 - 1);
    let (ix1, iy1) = (x1.ceil() as i32 + 1, y1.ceil() as i32 + 1);
    let (w, h) = ((ix1 - ix0).max(1), (iy1 - iy0).max(1));
    let mut mask = vec![false; (w * h) as usize];
    for y in 0..h {
        for x in 0..w {
            mask[(y * w + x) as usize] = in_poly(ix0 as f32 + x as f32 + 0.5, iy0 as f32 + y as f32 + 0.5, pts);
        }
    }
    // 带宽 = r（"到边界的距离 ≤ r"就是边），所以要 stroke 像素粗就取 r = stroke
    let r = stroke.max(1);
    let c = gray565(tone);
    for y in 0..h {
        for x in 0..w {
            if !mask[(y * w + x) as usize] {
                continue;
            }
            // 到边界距离 > r 的像素算内部，不画
            let mut inner = true;
            'scan: for dy in -r..=r {
                for dx in -r..=r {
                    if dx * dx + dy * dy > r * r {
                        continue;
                    }
                    let (nx, ny) = (x + dx, y + dy);
                    if nx < 0 || ny < 0 || nx >= w || ny >= h || !mask[(ny * w + nx) as usize] {
                        inner = false;
                        break 'scan;
                    }
                }
            }
            if !inner {
                fb.put(ix0 + x, iy0 + y, c);
            }
        }
    }
}

/// Favorites：五角星（官方是描边星，外径 19.5、内径 7.4、笔画 5px）
pub fn icon_star(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    icon_star_s(fb, x as f32 + 20.0, y as f32 + 20.5, 19.0, tone)
}

/// 可缩放的星（空态用 90px 那颗，外接半径 ≈44）。
pub fn icon_star_s(fb: &mut Fb, cx: f32, cy: f32, ro: f32, tone: u8) {
    let ri = ro * 0.4;
    let mut pts: Vec<(f32, f32)> = Vec::with_capacity(10);
    for k in 0..10 {
        let a = -std::f32::consts::FRAC_PI_2 + k as f32 * std::f32::consts::PI / 5.0;
        let r = if k % 2 == 0 { ro } else { ri };
        pts.push((cx + a.cos() * r, cy + a.sin() * r));
    }
    poly_stroke(fb, &pts, (ro * 0.26).max(4.0), tone);
}

/// Tags：标签（官方是五边形：上边平、右上有直角、左下收成尖，右上角一个实心孔）
pub fn icon_tag(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    // 中心线五边形 + 5px 描边，右上角一个实心孔
    let (x, y) = (x as f32, y as f32);
    let pts = [
        (x + 18.5, y + 3.5),
        (x + 36.5, y + 3.5),
        (x + 36.5, y + 20.0),
        (x + 19.5, y + 37.0),
        (x + 3.5, y + 19.5),
    ];
    poly_stroke(fb, &pts, 5.0, tone);
    disc(fb, x as i32 + 26, y as i32 + 11, 3, tone);
}

/// 可缩放的标签图标（空态那颗 90px）。40px 版按尺寸等比放大，描边一起放大。
pub fn icon_tag_s(fb: &mut Fb, cx: f32, cy: f32, size: f32, tone: u8) {
    let k = size / 40.0;
    let p = |px: f32, py: f32| (cx + (px - 20.0) * k, cy + (py - 20.0) * k);
    let pts = [
        p(18.5, 3.5),
        p(36.5, 3.5),
        p(36.5, 20.0),
        p(19.5, 37.0),
        p(3.5, 19.5),
    ];
    poly_stroke(fb, &pts, 5.0 * k, tone);
    disc(fb, (cx + 6.0 * k) as i32, (cy - 9.0 * k) as i32, (3.0 * k) as i32, tone);
}

/// Trash：垃圾桶
pub fn icon_trash(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    fb.fill_rect(x + 5, y + 9, 30, 3, c);
    fb.fill_rect(x + 14, y + 5, 12, 3, c);
    outline_rect(fb, x + 10, y + 14, 20, 22, 3, tone);
    fb.fill_rect(x + 16, y + 19, 3, 13, c);
    fb.fill_rect(x + 22, y + 19, 3, 13, c);
}

/// Settings：齿轮
pub fn icon_gear(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    let (cx, cy) = (x + 20, y + 20);
    disc(fb, cx, cy, 16, tone);
    disc(fb, cx, cy, 8, 255);
    for k in 0..8 {
        let a = k as f32 * std::f32::consts::TAU / 8.0;
        let px = cx + (a.cos() * 17.0) as i32;
        let py = cy + (a.sin() * 17.0) as i32;
        fb.fill_rect(px - 3, py - 3, 7, 7, c);
    }
}

// ---------------------------------------------------------------- 设置页图标（40×40）

/// 文档/页面（General）
pub fn icon_doc(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    outline_rect(fb, x + 8, y + 4, 24, 32, 3, tone);
    let c = gray565(tone);
    fb.fill_rect(x + 14, y + 12, 12, 3, c);
    fb.fill_rect(x + 14, y + 20, 12, 3, c);
    fb.fill_rect(x + 14, y + 28, 8, 3, c);
}

/// 存储卡/磁盘（Storage）
pub fn icon_storage(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    fb.fill_rect(x + 5, y + 4, 30, 3, c);
    outline_rect(fb, x + 8, y + 4, 24, 32, 3, tone);
    fb.fill_rect(x + 13, y + 21, 14, 10, c);
}

/// 锁（Security）
pub fn icon_lock(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    // 锁梁
    arc(fb, x + 20, y + 17, 9, std::f32::consts::PI, std::f32::consts::TAU, 4, tone);
    fb.fill_rect(x + 11, y + 16, 4, 8, c);
    fb.fill_rect(x + 25, y + 16, 4, 8, c);
    // 锁体
    outline_rect(fb, x + 7, y + 23, 26, 15, 3, tone);
    fb.fill_rect(x + 19, y + 28, 3, 6, c);
}

/// 地球（Language）
pub fn icon_globe(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    let (cx, cy, r) = (x + 20, y + 20, 17);
    ring(fb, cx, cy, r, 3, tone);
    // 赤道
    fb.fill_rect(cx - r, cy - 1, 2 * r, 3, c);
    // 两条经线：椭圆描边
    for rx in [9.0f32, 15.0] {
        for yy in -r..=r {
            let t = yy as f32 / r as f32;
            let dx = (rx * (1.0 - t * t).max(0.0).sqrt()) as i32;
            fb.fill_rect(cx + dx - 1, cy + yy, 3, 1, c);
            fb.fill_rect(cx - dx - 1, cy + yy, 3, 1, c);
        }
    }
}

/// 字体大小（Accessibility）
pub fn icon_font(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    line(fb, x + 4, y + 32, x + 15, y + 8, 3, tone);
    line(fb, x + 26, y + 32, x + 15, y + 8, 3, tone);
    fb.fill_rect(x + 9, y + 22, 13, 3, c);
    line(fb, x + 26, y + 32, x + 33, y + 17, 3, tone);
    line(fb, x + 40, y + 32, x + 33, y + 17, 3, tone);
    fb.fill_rect(x + 30, y + 26, 7, 2, c);
}

/// 问号（Help）
pub fn icon_help(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    ring(fb, x + 20, y + 20, 17, 3, tone);
    arc(fb, x + 20, y + 15, 8, -2.6, 0.9, 3, tone);
    fb.fill_rect(x + 18, y + 18, 4, 8, c);
    fb.fill_rect(x + 18, y + 28, 4, 4, c);
}

/// 小箭头（Flight mode / 列表前缀）
pub fn icon_plane(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let pts = [
        (0, 18),
        (30, 4),
        (33, 9),
        (12, 20),
        (33, 31),
        (30, 36),
        (0, 22),
    ];
    let pts: Vec<(i32, i32)> = pts.iter().map(|(px, py)| (x + px, y + py)).collect();
    poly(fb, &pts, tone);
}

/// Wi-Fi 图标（40×40，用于设置侧栏）
pub fn wifi_icon(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    // 三段开口朝下的弧（屏幕上开口朝下 = 角度在 200°..340°）
    for r in [17i32, 12, 7] {
        arc(fb, x + 20, y + 32, r, -2.35, -0.79, 4, tone);
    }
    disc(fb, x + 20, y + 31, 3, tone);
}

/// 电池图标（40×40，用于设置侧栏）
pub fn icon_battery(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    outline_rect(fb, x + 6, y + 11, 24, 18, 3, tone);
    fb.fill_rect(x + 30, y + 17, 3, 6, c);
    fb.fill_rect(x + 10, y + 15, 16, 10, c);
}

/// 胶囊形描边（开关用）
pub fn capsule(fb: &mut Fb, x: i32, y: i32, w: i32, h: i32, t: i32, tone: u8) {
    let r = h / 2;
    for dy in -r..=r {
        for dx in -r..=r {
            let d = (dx * dx + dy * dy) as f32;
            let rr = (r * r) as f32;
            let ri = ((r - t) * (r - t)) as f32;
            if d <= rr && d >= ri {
                fb.put(x + r + dx, y + r + dy, gray565(tone));
                fb.put(x + w - r + dx, y + r + dy, gray565(tone));
            }
        }
    }
    let c = gray565(tone);
    fb.fill_rect(x + r, y, w - 2 * r, t, c);
    fb.fill_rect(x + r, y + h - t, w - 2 * r, t, c);
}

/// 重启图标：一个圆弧 + 箭头
pub fn icon_restart(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    let pi = std::f32::consts::PI;
    let mut k = 0;
    while k <= 80 {
        let a = -0.55 * pi + 1.75 * pi * k as f32 / 80.0;
        let px = x + (a.cos() * 12.0) as i32;
        let py = y + (a.sin() * 12.0) as i32;
        fb.fill_rect(px, py, 3, 3, c);
        k += 1;
    }
    // 箭头（在右上角，朝下）
    for i in 0..9 {
        fb.fill_rect(x + 8 + i, y - 13 + i, 3, 3, c);
        fb.fill_rect(x + 16 - i, y - 13 + i, 3, 3, c);
    }
}

/// 电源图标
pub fn icon_power(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    let pi = std::f32::consts::PI;
    let mut k = 0;
    while k <= 70 {
        let a = -0.28 * pi + 1.56 * pi * k as f32 / 70.0;
        let px = x + (a.cos() * 12.0) as i32;
        let py = y + (a.sin() * 12.0) as i32;
        fb.fill_rect(px, py, 3, 3, c);
        k += 1;
    }
    fb.fill_rect(x - 2, y - 15, 4, 13, c);
}

// ---------------------------------------------------------------- 图层面板用的小图标
// 官方 icon_eye / icon_eye_closed / icon_arrow_up / icon_arrow_down /
// icon_merge_layer / icon_template / icon_information_circle，都在 40×40 框里。

/// 眼睛（可见）
pub fn icon_eye(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    let (cx, cy) = (x + 20, y + 20);
    // 杏仁形：上下两条对称弧
    for i in -18..=18 {
        let t = i as f32 / 18.0;
        let dy = ((1.0 - t * t).max(0.0)).sqrt() * 11.0;
        let yy = dy as i32;
        fb.fill_rect(cx + i, cy - yy, 1, 3, c);
        fb.fill_rect(cx + i, cy + yy - 2, 1, 3, c);
    }
    disc(fb, cx, cy, 6, tone);
    disc(fb, cx, cy, 3, 255);
}

/// 眼睛（隐藏）：同形状 + 一道斜杠
pub fn icon_eye_closed(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    let (cx, cy) = (x + 20, y + 20);
    for i in -18..=18 {
        let t = i as f32 / 18.0;
        let dy = ((1.0 - t * t).max(0.0)).sqrt() * 11.0;
        let yy = dy as i32;
        fb.fill_rect(cx + i, cy - yy, 1, 2, c);
        fb.fill_rect(cx + i, cy + yy - 1, 1, 2, c);
    }
    for i in 0..36 {
        fb.fill_rect(cx - 18 + i, cy + 16 - i, 3, 3, c);
    }
}

/// 上箭头
pub fn icon_arrow_up(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    let (cx, cy) = (x + 20, y + 20);
    fb.fill_rect(cx - 1, cy - 8, 3, 18, c);
    for i in 0..9 {
        fb.fill_rect(cx - 1 - i, cy - 8 + i, 3, 3, c);
        fb.fill_rect(cx + 2 + i, cy - 8 + i, 3, 3, c);
    }
}

/// 下箭头
pub fn icon_arrow_down(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    let (cx, cy) = (x + 20, y + 20);
    fb.fill_rect(cx - 1, cy - 10, 3, 18, c);
    for i in 0..9 {
        fb.fill_rect(cx - 1 - i, cy + 7 - i, 3, 3, c);
        fb.fill_rect(cx + 2 + i, cy + 7 - i, 3, 3, c);
    }
}

/// 向下合并：上面一条线往中间收，下面一横
pub fn icon_merge_layer(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    let (cx, cy) = (x + 20, y + 20);
    fb.fill_rect(cx - 9, cy - 10, 3, 12, c);
    fb.fill_rect(cx + 7, cy - 10, 3, 12, c);
    for i in 0..10 {
        fb.fill_rect(cx - 9 + i, cy - 10 + i, 3, 3, c);
        fb.fill_rect(cx + 8 - i, cy - 10 + i, 3, 3, c);
    }
    fb.fill_rect(cx - 9, cy + 8, 19, 3, c);
}

/// 模板：小窗格里一个山形
pub fn icon_template(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    outline_rect(fb, x + 6, y + 8, 29, 24, 2, tone);
    let c = gray565(tone);
    for i in 0..10 {
        fb.fill_rect(x + 9 + i, y + 27 - i, 3, 3, c);
        fb.fill_rect(x + 19 + i, y + 18 + i, 3, 3, c);
    }
    disc(fb, x + 14, y + 15, 2, tone);
}

/// 信息圆（"到上限了"）
pub fn icon_info_circle(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    ring(fb, x + 20, y + 20, 15, 3, tone);
    let c = gray565(tone);
    fb.fill_rect(x + 18, y + 18, 4, 3, c);
    fb.fill_rect(x + 18, y + 24, 4, 9, c);
}

/// 屏幕共享：方框里一个播放三角
pub fn icon_screen(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    outline_rect(fb, x + 3, y + 8, 34, 24, 2, tone);
    let c = gray565(tone);
    fb.fill_rect(x + 12, y + 33, 16, 2, c);
    // 三角
    for i in 0..9 {
        fb.fill_rect(x + 17 + i / 2, y + 14 + i, 2, 1, c);
    }
    for i in 0..18 {
        let k = if i < 9 { i } else { 17 - i };
        fb.fill_rect(x + 17, y + 14 + i, 2 + k, 1, c);
    }
}

/// 发邮件：纸飞机
pub fn icon_send(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    for i in 0..26 {
        fb.fill_rect(x + 6 + i, y + 20 - i / 2, 2, 2, c);
        fb.fill_rect(x + 6 + i, y + 20 + i / 2, 2, 2, c);
    }
    for i in 0..12 {
        fb.fill_rect(x + 8 + i, y + 20 + i / 3, 2, 2, c);
    }
}

// ---------------------------------------------------------------- 选区悬浮条（40×40）

/// 复制：两张错开的纸。官方是 `Icons.icon_duplicate`
/// （`res_066a78f.qml:243`），字形拿不到，按同组图标的画法用描边拼。
///
/// 前面那张纸盖住后面那张的重叠处，所以要用白底挖一下——只用在悬浮条的
/// 白底按钮上（`SceneSelectionHandler.qml:239`）。
pub fn icon_duplicate(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    let c = gray565(tone);
    // 后面那张：只露左上两条边
    outline_rect(fb, x + 2, y + 3, 24, 28, 3, tone);
    // 前面那张：先压白，再描边，最后两条"字行"
    fb.fill_rect(x + 14, y + 12, 24, 28, gray565(255));
    outline_rect(fb, x + 14, y + 12, 24, 28, 3, tone);
    fb.fill_rect(x + 20, y + 21, 13, 3, c);
    fb.fill_rect(x + 20, y + 28, 9, 3, c);
}

/// 剪切：剪刀。官方是 `Icons.icon_cut`（`res_066a78f.qml:259`）。
///
/// 两个指环在下、两片刀刃在中间交叉、刀刃尖朝上。
pub fn icon_cut(fb: &mut Fb, x: i32, y: i32, tone: u8) {
    // 刀刃：左下环 → 右上尖，右下环 → 左上尖
    line(fb, x + 10, y + 26, x + 32, y + 2, 3, tone);
    line(fb, x + 30, y + 26, x + 8, y + 2, 3, tone);
    // 指环（描边 3px）
    ring(fb, x + 10, y + 32, 6, 3, tone);
    ring(fb, x + 30, y + 32, 6, 3, tone);
}
