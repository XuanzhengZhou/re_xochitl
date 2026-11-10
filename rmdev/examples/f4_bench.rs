//! 临时计时台（F4 屏幕共享提速）：跑完就删，不进 git。
//!
//! `cargo run --release --example f4_bench -- /tmp/f4_page.pgm`
//!
//! 量：PGM 读入、各种 PNG 编码参数、降采样、回环 put_blob、zune-png 解码。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use miniz_oxide::deflate::compress_to_vec_zlib;

fn crc32(data: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let mut body = kind.to_vec();
    body.extend_from_slice(data);
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_be_bytes());
}

/// filter: 0=None 1=Sub 2=Up 3=Average 4=Paeth；level 走 miniz。
fn encode(gray: &[u8], w: usize, h: usize, filter: u8, level: u8) -> Vec<u8> {
    let mut raw = Vec::with_capacity((w + 1) * h);
    for y in 0..h {
        let row = &gray[y * w..(y + 1) * w];
        raw.push(filter);
        match filter {
            0 => raw.extend_from_slice(row),
            1 => {
                raw.push(row[0]);
                for x in 1..w {
                    raw.push(row[x].wrapping_sub(row[x - 1]));
                }
            }
            2 => {
                if y == 0 {
                    raw.extend_from_slice(row);
                } else {
                    let up = &gray[(y - 1) * w..y * w];
                    for x in 0..w {
                        raw.push(row[x].wrapping_sub(up[x]));
                    }
                }
            }
            _ => unreachable!(),
        }
    }
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&(w as u32).to_be_bytes());
    ihdr.extend_from_slice(&(h as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &compress_to_vec_zlib(&raw, level));
    chunk(&mut out, b"IEND", &[]);
    out
}

fn down(gray: &[u8], w: usize, h: usize, k: usize, dark: bool) -> (Vec<u8>, usize, usize) {
    let (dw, dh) = (w / k, h / k);
    let mut out = vec![255u8; dw * dh];
    for y in 0..dh {
        for x in 0..dw {
            let mut sum = 0u32;
            let mut mn = 255u8;
            for yy in 0..k {
                for xx in 0..k {
                    let p = gray[(y * k + yy) * w + x * k + xx];
                    sum += p as u32;
                    mn = mn.min(p);
                }
            }
            out[y * dw + x] = if dark { mn } else { (sum / (k * k) as u32) as u8 };
        }
    }
    (out, dw, dh)
}

fn bench<T>(name: &str, n: u32, mut f: impl FnMut() -> T) -> (Duration, T) {
    let _ = f();
    let mut best = Duration::from_secs(99);
    let mut total = Duration::from_secs(0);
    let mut last = None;
    for _ in 0..n {
        let t = Instant::now();
        let v = f();
        let e = t.elapsed();
        best = best.min(e);
        total += e;
        last = Some(v);
    }
    let med = total / n;
    println!("{name:<44} best {:>8.2} ms  avg {:>8.2} ms", best.as_secs_f64() * 1e3, med.as_secs_f64() * 1e3);
    (best, last.unwrap())
}

fn gray_of_png(png: &[u8]) -> (usize, usize, Vec<u8>) {
    use zune_png::zune_core::bytestream::ZCursor;
    use zune_png::zune_core::options::DecoderOptions;
    let mut dec =
        zune_png::PngDecoder::new_with_options(ZCursor::new(png), DecoderOptions::default());
    dec.decode_headers().unwrap();
    let (w, h) = dec.dimensions().unwrap();
    let px = dec.decode_raw().unwrap();
    (w, h, px.to_vec())
}

/// 字迹指标：全分辨率里"有墨"的像素 → 降采样后的同一格还剩下多少墨。
fn ink_stats(full: &[u8], w: usize, h: usize, small: &[u8], dw: usize, k: usize) {
    let mut cells = 0u64;
    let mut dark_cells = 0u64;
    let mut sum = 0u64;
    let mut seen = std::collections::HashSet::new();
    for y in 0..h {
        for x in 0..w {
            if full[y * w + x] < 128 {
                let key = (y / k) * dw + x / k;
                if seen.insert(key) {
                    cells += 1;
                    let v = small[key];
                    sum += v as u64;
                    if v < 160 {
                        dark_cells += 1;
                    }
                }
            }
        }
    }
    println!(
        "  降采样 {k}×：含墨格 {cells}，其中仍算墨(<160) {dark_cells}（{:.1}%），平均灰度 {:.0}",
        dark_cells as f64 * 100.0 / cells as f64,
        sum as f64 / cells as f64
    );
}

/// 合成一页"密排小字"：每行从一段长文里错位截 34 个字，
/// 这样行与行之间没法被 deflate 整行匹配（比"同一行重复 60 次"更接近书页）。
fn dense_page(w: usize, h: usize) -> Vec<u8> {
    let src = "屏幕共享提速的目标是让远端在写字停顿后尽快看到这一页。\
        设备只有单核，deflate 是这里最贵的一步，所以先把整页缩小一半再编码；\
        缩到 702×936 时 32 px 的手写字还剩 16 px 高，笔画宽度 3 到 4 个像素，\
        在手机上仍然一眼能读；再往下缩到 468 宽就开始糊了，所以停在 2×。\
        测试页要像真正的书页：行行不同、标点密集、还有数字 0123456789 混在里面，\
        这样 deflate 找不到整行重复，量出来的编码耗时和字节数才不会偏乐观。\
        这一段文字本身就是用来把每一行填满的，长度足够错位取窗；\
        如果某一行太短就补一句注释，保证每行都有三十多个字。".repeat(4);
    let mut fb = rmdev::fb::Fb::open_mem();
    fb.clear(rmdev::fb::WHITE);
    let mut t = rmdev::ui::text::Text::load().expect("字体");
    let chars: Vec<char> = src.chars().collect();
    let mut y = 40;
    let mut i = 0usize;
    while y + 30 < h as i32 {
        let start = (i * 7) % (chars.len() - 40);
        let line: String = chars[start..start + 34].iter().collect();
        let line = format!("{:>2} {}", i + 1, line);
        t.draw(&mut fb, 40, y, 30.0, &line, 20);
        y += 30;
        i += 1;
    }
    let mut gray = vec![255u8; w * h];
    for y in 0..h {
        for x in 0..w {
            gray[y * w + x] = fb.get(x as i32, y as i32);
        }
    }
    gray
}

fn suite(tag: &str, w: usize, h: usize, gray: &[u8], save: bool) {
    println!("\n######## {tag}：{w}×{h}，墨迹占比 {:.2}% ########", {
        let ink = gray.iter().filter(|&&p| p < 128).count();
        ink as f64 * 100.0 / gray.len() as f64
    });

    // 段 2：全分辨率 PNG 各参数
    println!("== 全分辨率 {w}×{h} ==");
    for (name, f, l) in [
        ("filter=None level=6（现状 png_gray）", 0u8, 6u8),
        ("filter=None level=1", 0, 1),
        ("filter=Sub  level=6", 1, 6),
        ("filter=Up   level=1", 2, 1),
        ("filter=Up   level=6", 2, 6),
        ("filter=Up   level=9", 2, 9),
    ] {
        let (_, png) = bench(name, 5, || encode(gray, w, h, f, l));
        println!("  → {} B", png.len());
        if save && l == 6 && f == 0 {
            std::fs::write("/tmp/f4_full.png", &png).unwrap();
        }
    }

    // 段 3：降采样
    println!("== 降采样 ==");
    for k in [2usize, 3] {
        for dark in [false, true] {
            let (small, dw, dh) = down(gray, w, h, k, dark);
            let tag = if dark { "min" } else { "box" };
            let (_, png) = bench(&format!("{k}× {tag} filter=Up level=1（{dw}×{dh}）"), 5, || {
                encode(&small, dw, dh, 2, 1)
            });
            println!("  → {} B", png.len());
            let (_, png6) = bench(&format!("{k}× {tag} filter=Up level=6"), 3, || {
                encode(&small, dw, dh, 2, 6)
            });
            println!("  → {} B", png6.len());
            let (_, png2) = bench(&format!("{k}× {tag} filter=None level=6"), 3, || {
                encode(&small, dw, dh, 0, 6)
            });
            println!("  → {} B", png2.len());
            ink_stats(gray, w, h, &small, dw, k);
            if save {
                std::fs::write(format!("/tmp/f4_ds{k}_{tag}.png"), &png).unwrap();
                std::fs::write(format!("/tmp/f4_ds{k}_{tag}_l6.png"), &png6).unwrap();
                std::fs::write(format!("/tmp/f4_ds{k}_{tag}_none6.png"), &png2).unwrap();
            }
        }
    }

    // 段 3b：2× 降采样下 level 扫描（找拐点）
    {
        let (small, dw, dh) = down(gray, w, h, 2, false);
        for l in [1u8, 3, 4, 5, 6, 7, 9] {
            let (_, png) = bench(&format!("2× box level={l}"), 3, || {
                encode(&small, dw, dh, 0, l)
            });
            println!("  → {} B", png.len());
        }
    }

    // 段 4：解码（zune-png；浏览器 libpng/ImageDecoder 一般再快 3~10×）
    println!("== 解码（zune-png） ==");
    {
        let png = encode(gray, w, h, 0, 6);
        let (_, (dw, dh, _)) = bench("全分辨率 None l6（现状）", 5, || gray_of_png(&png));
        println!("  {dw}×{dh}，{} B", png.len());
        let (small, dw, dh) = down(gray, w, h, 2, false);
        let png = encode(&small, dw, dh, 0, 6);
        let _ = bench("2× 降采样 None l6", 5, || gray_of_png(&png));
        println!("  {dw}×{dh}，{} B", png.len());
        let mut se = 0f64;
        for y in 0..h {
            for x in 0..w {
                let a = gray[y * w + x] as f64;
                let b = small[(y / 2) * dw + x / 2] as f64;
                se += (a - b) * (a - b);
            }
        }
        let mse = se / (w * h) as f64;
        println!("  2× box 还原 PSNR = {:.2} dB", 10.0 * (255.0f64 * 255.0 / mse).log10());
    }
}

fn main() {
    let path = std::env::args().nth(1).unwrap();
    // 段 1：读 PGM（= cmd_render 里 fs::read + read_pgm 的等价开销）
    let bytes = std::fs::read(&path).unwrap();
    let (w, h, gray) = {
        let nl = bytes.iter().position(|&b| b == b'\n').unwrap() + 1;
        let nl2 = nl + bytes[nl..].iter().position(|&b| b == b'\n').unwrap() + 1;
        let nl3 = nl2 + bytes[nl2..].iter().position(|&b| b == b'\n').unwrap() + 1;
        (1404usize, 1872usize, bytes[nl3..].to_vec())
    };
    println!("页面 {w}×{h} = {} 灰度字节，PGM 文件 {} B", w * h, bytes.len());
    let _ = bench("① 读 PGM（fs::read + 去头）", 20, || {
        let b = std::fs::read(&path).unwrap();
        let nl = b.iter().position(|&x| x == b'\n').unwrap() + 1;
        let nl2 = nl + b[nl..].iter().position(|&x| x == b'\n').unwrap() + 1;
        let nl3 = nl2 + b[nl2..].iter().position(|&x| x == b'\n').unwrap() + 1;
        b.len() - nl3
    });
    // 段 1b：整页绘制（`d1_page_gray` 里 `paint_all` + 逐像素取灰）
    let _ = bench("①b paint_all + 取灰 2.6 MB", 5, || {
        let mut fb = rmdev::fb::Fb::open_mem();
        fb.clear(rmdev::fb::WHITE);
        let mut c = rmdev::canvas::Canvas::new(w, h);
        c.paint_all(&mut fb);
        let mut g = vec![255u8; w * h];
        for y in 0..h {
            for x in 0..w {
                g[y * w + x] = fb.get(x as i32, y as i32);
            }
        }
        g
    });

    suite("真实手写页 page1.rm", w, h, &gray, true);
    let dense = dense_page(w, h);
    suite("合成密排小字（同文重复，仅供对照）", w, h, &dense, false);
    // 真实 PDF 页：`Pdf::render` 就是设备上 `pdf_bg` 的来源，最接近"书页"的最坏情况
    if let Some(pdf_path) = std::env::args().nth(2) {
        let pdf = rmdev::doc::pdf::Pdf::open(&pdf_path).expect("开 PDF");
        let (_, g) = bench("①c Pdf::render 1404×1872（PDF 底图）", 3, || {
            pdf.render(0, w, h).expect("render")
        });
        if g.len() == w * h {
            suite("真实 PDF 页 0", w, h, &g, false);
        } else {
            println!("PDF 页尺寸 {:?} 不是 {w}×{h}，跳过", g.len());
        }
    }

    // 段 6：现在（F4 之后）的真实路径 = downscale + png_preview
    for (tag, g) in [("手写页", &gray), ("密排页", &dense)] {
        println!("\n== 优化后（{tag}）：downscale + png_preview ==");
        let (_, (ds, dw, dh)) = bench("downscale 2×", 5, || {
            rmdev::share::downscale(g, w, h, rmdev::share::PREVIEW_DIV)
        });
        println!("  → {dw}×{dh}，{} B 灰度", ds.len());
        let (_, png) = bench("png_preview（level=4）", 5, || {
            rmdev::share::png_preview(&ds, dw, dh)
        });
        println!("  → {} B", png.len());
        let (_, old) = bench("png_gray（旧：全分辨率 level=6）", 3, || {
            rmdev::share::png_gray(g, w, h)
        });
        println!("  → {} B", old.len());
        // 整条设备侧 CPU：降采样 + 编码
        let (_, png2) = bench("合计：降采样 + 编码", 5, || {
            let (s, a, b) = rmdev::share::downscale(g, w, h, rmdev::share::PREVIEW_DIV);
            rmdev::share::png_preview(&s, a, b)
        });
        println!("  → {} B", png2.len());
        std::fs::write(format!("/tmp/f4_after_{tag}.png"), &png2).unwrap();
        serve_and_time(&format!("{tag} 优化后 PNG"), &png2, 5);
    }

    // 段 5：回环 put_blob（真的走 net::put_blob，服务器是本地假 blob 端点）
    println!("\n== 回环 put_blob（本机 127.0.0.1，不含真实链路） ==");
    let png_full = encode(&gray, w, h, 0, 6);
    let (small, dw, dh) = down(&gray, w, h, 2, false);
    let png_2x = encode(&small, dw, dh, 2, 6);
    let (dense_small, ddw, ddh) = down(&dense, w, h, 2, false);
    let png_dense = encode(&dense, w, h, 0, 6);
    let png_dense2 = encode(&dense_small, ddw, ddh, 2, 6);
    serve_and_time("手写页 全分辨率现状", &png_full, 5);
    serve_and_time("手写页 2× 降采样", &png_2x, 5);
    serve_and_time("密排页 全分辨率现状", &png_dense, 3);
    serve_and_time("密排页 2× 降采样", &png_dense2, 3);
    serve_and_time("1 KB 小包（纯 HTTP 头开销）", &vec![0u8; 1024], 5);
}

/// 起一个只认 `POST /api/v1/blob` 的假服务器（读请求、回 204），用真 `net::put_blob` 打。
fn serve_and_time(name: &str, data: &[u8], n: u32) {
    let lis = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = lis.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let mut total = 0usize;
        for _ in 0..n + 1 {
            let (mut s, _) = lis.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 16384];
            loop {
                let k = s.read(&mut tmp).unwrap();
                if k == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..k]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                    let len: usize = head
                        .split("\r\n")
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .map(|v| v.trim().parse().unwrap())
                        .unwrap();
                    if buf.len() >= pos + 4 + len {
                        total += pos + 4 + len;
                        break;
                    }
                }
            }
            let _ = s.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n");
        }
        total
    });
    let dir = std::env::temp_dir().join(format!("f4cfg{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg_path = dir.join("cloud.json");
    std::fs::write(
        &cfg_path,
        format!(
            "{{\"url\":\"http://{}\",\"token\":\"t\",\"device_id\":\"d\",\"last_rev\":0}}",
            addr
        ),
    )
    .unwrap();
    let cfg = rmdev::net::Config::load_at(&cfg_path);
    let (el, _) = bench(name, n, || {
        rmdev::net::put_blob(&cfg, data).unwrap()
    });
    let _ = std::fs::remove_dir_all(&dir);
    let total = handle.join().unwrap();
    println!(
        "  → 上行 {} B（{} 次共 {} B），单次 {:.2} ms，约 {:.0} µs/KB",
        data.len(),
        n,
        total,
        el.as_secs_f64() * 1e3,
        el.as_secs_f64() * 1e6 / (data.len() as f64 / 1024.0)
    );
}

#[test]
fn f4_extra_bench() {
    let _ = bench("Text::load（App::new 每次都要解析字体）", 5, || {
        rmdev::ui::text::Text::load().is_some()
    });
    // 需要一页真机 .rm 才跑得动：RM_SAMPLE_RM=/path/to/page.rm cargo test --example f4_bench
    let Ok(path) = std::env::var("RM_SAMPLE_RM") else { return };
    let Ok(bytes) = std::fs::read(&path) else { return };
    let _ = bench("RmPage::parse（读一页 .rm）", 20, || {
        rmdev::doc::rm::RmPage::parse(&bytes).unwrap().total_lines()
    });
    let gray = vec![255u8; 1404 * 1872];
    let _ = bench("写 + 读回 2.6 MB PGM（cmd_render 的 /tmp 往返）", 10, || {
        let mut s = b"P5\n1404 1872\n255\n".to_vec();
        s.extend_from_slice(&gray);
        std::fs::write("/tmp/f4_pgm_rt.pgm", &s).unwrap();
        let back = std::fs::read("/tmp/f4_pgm_rt.pgm").unwrap();
        back.len()
    });
    let _ = bench("sha256_hex 656 KB（预览去重哈希）", 20, || {
        rmdev::net::sha256_hex(&vec![7u8; 702 * 936])
    });
}
