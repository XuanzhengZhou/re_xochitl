//! 面板底层排查工具：把「写 fb + 翻页」和「波形编码」分开验证。
//!
//! ```text
//! paneldbg raw <hexcode> <frames>   直接给所有内容 cell 灌码（1=推黑 2=推白）
//! paneldbg enc                      列 GC16/DU 从白到黑、从黑到白的逐相位码
//! paneldbg slot                     打印槽 0 里几个关键位置的字（验证写入落地）
//! paneldbg regs                     读 LCDIF 寄存器（CUR_BUF/NEXT_BUF/CTRL…）
//! ```

use rmdev::panel::{Mode, Panel};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(|s| s.as_str()).unwrap_or("enc");
    if cmd == "regs" {
        regs();
        return;
    }
    if cmd == "panraw" {
        panraw();
        return;
    }
    if cmd == "scanpos" {
        scanpos();
        return;
    }
    if cmd == "card" {
        card();
        return;
    }
    if cmd == "demo" {
        // demo 换成了斜向反弹版
        let speed: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(500.0);
        bounce(speed, 12.0, rmdev::fb::WAVEFORM_MODE_DU, false);
        return;
    }
    if cmd == "bounce" {
        let speed: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(500.0);
        let secs: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(12.0);
        // 第 6 个参数 "gray" = 保留真实灰阶（不量化成纯黑白）。
        let keep_gray = args.get(5).map(|s| s == "gray" || s == "raw").unwrap_or(false);
        let wf = match args.get(4).map(|s| s.as_str()) {
            Some("du") | None => rmdev::fb::WAVEFORM_MODE_DU,
            Some("a2") => rmdev::fb::WAVEFORM_MODE_A2,
            Some("du4") => rmdev::fb::WAVEFORM_MODE_DU4,
            Some("gc16") => rmdev::fb::WAVEFORM_MODE_GC16,
            Some("gl16") => rmdev::fb::WAVEFORM_MODE_GL16,
            Some(other) => {
                eprintln!("未知波形 {other}（du/du4/a2/gc16/gl16）");
                std::process::exit(2);
            }
        };
        bounce(speed, secs, wf, keep_gray);
        return;
    }
    if cmd == "move" {
        let speed: f64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(600.0);
        let secs: f64 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(6.0);
        move_box(speed, secs);
        return;
    }
    let mut p = Panel::open().expect("打不开面板");
    eprintln!("{}", p.describe());

    match cmd {
        "enc" => {
            for mode in [Mode::Gc16, Mode::Gl16, Mode::Du, Mode::A2] {
                for (tgt, prv, label) in [
                    (0u8, 15u8, "黑←白"),
                    (15, 0, "白←黑"),
                    (15, 15, "白←白"),
                    (8, 0, "中灰←黑"),
                    (8, 15, "中灰←白"),
                    (8, 8, "中灰←中灰"),
                    (0, 8, "黑←中灰"),
                    (8, 8, "中灰←中灰"),
                ] {
                    match p.debug_pair_codes(mode, tgt, prv) {
                        Ok(codes) => {
                            let s: String = codes.iter().map(|c| char::from(b'0' + c)).collect();
                            println!("{:?} {label}: {} 相位 [{}]", mode, codes.len(), s);
                        }
                        Err(e) => println!("{:?} {label}: {e}", mode),
                    }
                }
            }
        }
        "slot" => {
            for (label, row, cell) in [
                ("row0 cell0", 0usize, 0usize),
                ("row3 cell26", 3, 26),
                ("row4 cell26", 4, 26),
                ("row700 cell100", 700, 100),
            ] {
                println!(
                    "{label:16} word#{:7} = 0x{:08X}",
                    row * 260 + cell,
                    p.peek_slot(row * 260 + cell)
                );
            }
        }
        "raw" => {
            let code = u16::from_str_radix(
                args.get(2).map(|s| s.trim_start_matches("0x")).unwrap_or("1"),
                16,
            )
            .unwrap_or(1);
            let n: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(24);
            eprintln!("raw code=0x{code:04X} × {n} 帧");
            p.raw_frames(code, n).expect("raw_frames");
            println!("row4 cell26 = 0x{:08X}", p.peek_slot(4 * 260 + 26));
            // 不要让 Drop 再补一帧（否则看不到最后一帧的落地情况）
            std::mem::forget(p);
        }
        "img" => {
            // 左半黑、右半白，编 GC16 的第 22 相位（真正驱动的相位），播一帧。
            let (w, h) = (1404usize, 1872usize);
            let mut levels = vec![15u8; w * h];
            // **列主序**：levels[x*h + y]（和 `Panel::set_rect` 的约定一致）。
            // 左半 (x<702) 黑。
            for x in 0..702 {
                for y in 0..h {
                    levels[x * h + y] = 0;
                }
            }
            let g = p.mode_slot(Mode::Gc16).unwrap_or(0);
            let _ = p.set_rect(0, 0, w, h, &levels, g);
            p.prepare(0, 0, w, h);
            let n: usize = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(1);
            p.debug_set_phase(Mode::Gc16, 22);
            p.encode_phase(0, 0, w, h).unwrap();
            for _ in 0..n {
                p.show_pending(0, 0, w, h).unwrap();
            }
            println!("已播 {n} 帧（不补空白）");
            std::mem::forget(p);
        }
        "slotwatch" => {
            // 两槽写不同的码，翻到槽 0，然后连采样 LCDIF 的 CUR_BUF/NEXT_BUF。
            // 目的：确认"翻页之后面板到底在扫哪一槽"，以及内核有没有把
            // NEXT_BUF 复位回槽 1（防烧屏）。
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/mem")
                .expect("/dev/mem");
            use std::os::unix::io::AsRawFd;
            let len = 0x100usize;
            let m = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    f.as_raw_fd(),
                    0x3073_0000u64 as libc::off_t,
                )
            };
            assert_ne!(m, libc::MAP_FAILED, "mmap LCDIF");
            let base = m as *const u32;
            let rd = |off: usize| unsafe { std::ptr::read_volatile(base.add(off / 4)) };
            p.debug_write_code(0, 1); // 槽 0：全推黑
            p.debug_write_code(1, 2); // 槽 1：全推白
            println!(
                "槽0=推黑 槽1=推白 | line_length={} yres_virtual={} smem_start=0x{:08X}",
                p.fb_line_length(),
                p.fb_yres_virtual(),
                p.fb_smem_start()
            );
            // 交替翻：同一个 yoffset 连续翻第二次时驱动是空操作（它比较的是
            // 内核里存着的 yoffset），所以必须交替才能看出 NEXT_BUF 有没有动。
            let t0 = std::time::Instant::now();
            for k in 0..12 {
                println!(
                    "t={:5.1} ms  CUR_BUF=0x{:08X} NEXT_BUF=0x{:08X} CTRL=0x{:08X} CTRL1=0x{:08X}",
                    t0.elapsed().as_secs_f64() * 1e3,
                    rd(0x40),
                    rd(0x50),
                    rd(0x00),
                    rd(0x10)
                );
                p.debug_pan_to(k % 2).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(3));
            }
            std::mem::forget(p);
        }
        "flip" => {
            // 判决性实验（不需要人看屏）：翻到指定槽之后，用 200 µs 的粒度
            // 采样 LCDIF 的 CUR_BUF/NEXT_BUF 整整 `ms` 毫秒，把**每一次变化**
            // 都打出来。想回答的问题只有一个：`FBIOPAN_DISPLAY` 到底有没有
            // 让面板换槽？
            //   * 完全没动       → 单缓冲，翻页是空操作，只能写正在扫的那一槽；
            //   * 动了一帧又回去 → 双缓冲可用，但内核每帧把 NEXT_BUF 复位，
            //                      必须两槽镜像同一帧内容才不会闪。
            let slot: usize = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(0);
            let ms: u64 = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(60);
            let mut f = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/mem")
                .expect("/dev/mem");
            use std::os::unix::io::AsRawFd;
            let m = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    0x100usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    f.as_raw_fd(),
                    0x3073_0000u64 as libc::off_t,
                )
            };
            assert_ne!(m, libc::MAP_FAILED, "mmap LCDIF");
            let base = m as *const u32;
            let rd = |off: usize| unsafe { std::ptr::read_volatile(base.add(off / 4)) };
            // 两槽都灌"无驱动"：这次实验只看寄存器，别去改屏幕内容。
            p.debug_write_code(0, 0);
            p.debug_write_code(1, 0);
            println!(
                "smem_start=0x{:08X} line_length={} yres_virtual={}（半屏 = {} 行 = 0x{:X} B）",
                p.fb_smem_start(),
                p.fb_line_length(),
                p.fb_yres_virtual(),
                p.fb_yres_virtual() / 2,
                p.fb_line_length() as u64 * (p.fb_yres_virtual() as u64 / 2)
            );
            let watch = |tag: &str, ms: u64, rd: &dyn Fn(usize) -> u32| {
                let (mut lc, mut ln) = (rd(0x40), rd(0x50));
                println!("  [{tag}] t=  0.00 ms  CUR=0x{lc:08X} NEXT=0x{ln:08X}");
                let t = std::time::Instant::now();
                while t.elapsed().as_millis() < ms as u128 {
                    let (c, n) = (rd(0x40), rd(0x50));
                    if c != lc || n != ln {
                        println!(
                            "  [{tag}] t={:6.2} ms  CUR=0x{c:08X} NEXT=0x{n:08X}  <- 变化",
                            t.elapsed().as_secs_f64() * 1e3
                        );
                        lc = c;
                        ln = n;
                    }
                    std::thread::sleep(std::time::Duration::from_micros(200));
                }
            };
            for target in [slot, 1 - slot] {
                let t0 = std::time::Instant::now();
                let r = p.debug_pan_to(target);
                println!(
                    "翻到槽 {target}：{r:?}（pan 耗时 {:.1} ms）",
                    t0.elapsed().as_secs_f64() * 1e3
                );
                watch(&format!("slot{target}"), ms, &rd);
            }
            std::mem::forget(p);
        }
        "setbench" => {
            // 量 `set_rect` 本身：矩形越大越看得出分块转置的效果。
            for (w, h) in [(64, 64), (256, 256), (452, 443), (1274, 621), (1404, 1872)] {
                let levels = vec![7u8; w * h];
                let mut best = f64::MAX;
                let mut worst: f64 = 0.0;
                for _ in 0..5 {
                    let t = std::time::Instant::now();
                    let g = p.mode_slot(Mode::Gc16).unwrap_or(0);
            let _ = p.set_rect(0, 0, w, h, &levels, g);
                    let ms = t.elapsed().as_secs_f64() * 1e3;
                    best = best.min(ms);
                    worst = worst.max(ms);
                }
                let px = (w * h) as f64;
                println!(
                    "{w:4}×{h:<5} {:.1} Mpx  set_rect 最好 {best:7.2} ms 最差 {worst:7.2} ms  ({:.1} ns/px)",
                    px / 1e6,
                    best * 1e6 / px
                );
            }
            std::mem::forget(p);
        }
        "stripe" => {
            // 判决性实验：槽0 灌"推黑"、槽1 灌"推白"，然后一直翻槽 0。
            //   黑 = 面板只扫槽 0    白 = 只扫槽 1    灰 = 两槽交替
            let slot: usize = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(0);
            let secs: f64 = args.get(3).and_then(|v| v.parse().ok()).unwrap_or(3.0);
            p.debug_write_code(0, 1); // 槽0：推黑
            p.debug_write_code(1, 2); // 槽1：推白
            let n = (secs * 85.0) as usize;
            println!("翻槽 {slot} × {n} 帧：槽0=推黑 槽1=推白");
            for _ in 0..n {
                p.debug_pan_to(slot).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(11));
            }
            println!("结束");
            std::mem::forget(p);
        }
        "bench" => {
            // 编码微基准：整屏一格相位反复编码，看单相位耗时（≈ 帧预算的 3/4）。
            let (w, h) = (1404usize, 1872usize);
            for white in [true, false] {
                let levels = vec![if white { 15u8 } else { 0u8 }; w * h];
                let g = p.mode_slot(Mode::Gc16).unwrap_or(0);
            let _ = p.set_rect(0, 0, w, h, &levels, g);
                p.debug_reset_prv(15);
                p.prepare(0, 0, w, h);
                for k in 0..4 {
                    let t = std::time::Instant::now();
                    p.debug_set_phase(Mode::Gc16, 22);
            p.encode_phase(0, 0, w, h).unwrap();
                    println!(
                        "{} 整屏编码 #{k}: {:.1} ms",
                        if white { "全白" } else { "全黑" },
                        t.elapsed().as_secs_f64() * 1e3
                    );
                }
            }
            std::mem::forget(p);
        }
        "replay" => {
            // 把状态文件当成目标图，编 GC16 第 22 相位，播一帧（不补空白）。
            let path = std::env::var("RM_PANEL_STATE").unwrap_or_else(|_| "/tmp/rm2epd.state".into());
            let src = std::env::var("RM_REPLAY_SRC").unwrap_or(path);
            let data = std::fs::read(&src).expect("读状态文件");
            assert_eq!(data.len(), 1404 * 1872);
            p.debug_reset_prv(15); // 假设屏是白的，这样才能算出"要推黑"
            // 状态文件是行主序，`set_rect` 要列主序。
            let (w, h) = (1404usize, 1872usize);
            let mut cols = vec![0u8; w * h];
            for y in 0..h {
                for x in 0..w {
                    cols[x * h + y] = data[y * w + x];
                }
            }
            let g = p.mode_slot(Mode::Gc16).unwrap_or(0);
            let _ = p.set_rect(0, 0, 1404, 1872, &cols, g);
            p.prepare(0, 0, 1404, 1872);
            if std::env::var("RM_REPLAY_ALL").is_ok() {
                let n = p.phases(Mode::Gc16).unwrap();
                for ph in 0..n {
                    p.debug_set_phase(Mode::Du, ph);
                    p.encode_phase(0, 0, 1404, 1872).unwrap();
                    p.show_pending(0, 0, 1404, 1872).unwrap();
                }
                println!("已播完整 GC16（{n} 相位）");
            } else {
                p.debug_set_phase(Mode::Gc16, 22);
                p.encode_phase(0, 0, 1404, 1872).unwrap();
                p.show_pending(0, 0, 1404, 1872).unwrap();
                println!("已播状态文件的一帧");
            }
            std::mem::forget(p);
        }
        "par" => {
            // 并行波形基准：n 个区域同时驱，看帧周期还能不能压在一个 11.76 ms
            // 里。用法：`paneldbg par [w] [h] [du|gc16|mixed]`，内部扫 n=1/2/4/8。
            let w: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(300);
            let h: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(300);
            let m = args.get(4).map(|s| s.as_str()).unwrap_or("du").to_string();
            println!("区域 {w}×{h}  波形 {m}  （并集 = 驱动矩形，一帧一个 bbox）");
            for n in [1usize, 2, 4, 8] {
                par_bench(&mut p, n, w, h, &m);
            }
        }
        "pen" => {
            // 模拟一次笔迹更新：20×20 小块，DU 25 相位，逐帧计时。
            let (x, y, w, h) = (400usize, 600usize, 20usize, 20usize);
            let mut levels = vec![15u8; w * h];
            for i in 0..w * h {
                levels[i] = 0;
            }
            let d = p.mode_slot(Mode::Du).unwrap_or(0);
            let _ = p.set_rect(x, y, w, h, &levels, d);
            // 状态文件里这块可能已经是黑的，那样 (目标,上一帧) 就是"不变"、
            // 编码结果当然是 0——先假设整屏是白底，才能验证编码通路。
            p.debug_reset_prv(15);
            p.prepare(x, y, w, h);
            let n = p.phases(Mode::Du).unwrap();
            let mut worst = 0f64;
            // 检查编码有没有真的落进槽里：这笔的列 = COLS-1-x .. COLS-x，
            // 内容 cell 从 (4+pc)*260+26 开始。
            let pc = 1404 - 1 - x - w / 2; // 笔迹中间那一列（面板坐标）
            // cell s 覆盖 y ∈ [UH-8-8s, UH-1-8s]，也就是 y = UH-1-8s 那一行 ⇒
            // s = (UH-1-y)/8（截断）。取笔迹中间那一行。
            let cell_s = (1871 - (y + h / 2)) / 8;
            let probe = (4 + pc) * 260 + 26 + cell_s;
            println!(
                "探针 pc={pc} cell_s={cell_s} word={probe}（该 cell 覆盖 y={}..{}）",
                1872 - 8 - cell_s * 8,
                1871 - cell_s * 8
            );
            // 笔迹外面的一列，应该始终是 0。
            let far = (4 + 100) * 260 + 26 + cell_s;
            for ph in 0..n {
                let t = std::time::Instant::now();
                p.debug_set_phase(Mode::Du, ph);
                p.encode_phase(x, y, w, h).unwrap();
                let te = t.elapsed().as_secs_f64() * 1e3;
                let t2 = std::time::Instant::now();
                p.show_pending(x, y, w, h).unwrap();
                let ts = t2.elapsed().as_secs_f64() * 1e3;
                worst = worst.max(te + ts);
                if ph < 3 || ph + 1 == n {
                    println!(
                        "相位 {ph:2}: 编码 {te:5.1} ms + 显示 {ts:5.1} ms | 槽 pc{pc} = 0x{:08X} 远处 = 0x{:08X}",
                        p.peek_slot(probe),
                        p.peek_slot(far)
                    );
                }
            }
            println!("最慢一帧 {worst:.1} ms");
            std::mem::forget(p);
        }
        "pace" => {
            // 写 fb 和 pan 的关系：只翻页 / 整槽写 / 只写 12 列。
            for (kind, label) in [(0u8, "只 pan"), (1, "整槽写+pan"), (2, "12 列写+pan")] {
                let v = p.debug_pace(kind, 20);
                let body: Vec<f64> = v.iter().skip(4).copied().collect();
                let mut s = body.clone();
                s.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let med = s[s.len() / 2];
                println!(
                    "{label:12} 中位 {med:6.2} ms  最小 {:6.2}  最大 {:6.2}",
                    s[0],
                    s[s.len() - 1]
                );
            }
            std::mem::forget(p);
        }
        other => eprintln!("未知命令 {other}"),
    }
}

/// 测试卡：灰阶、渐变、细线、文字。走的是和 app 完全同一条通路
/// （`Fb` 画进平面 → `refresh` → 面板线程），所以看到的就是驱动的真实效果。
fn card() {
    use rmdev::fb::{Fb, UPDATE_MODE_FULL, WAVEFORM_MODE_GC16};
    let mut fb = Fb::open().expect("打开面板");
    let (w, h) = (fb.width, fb.height);
    paint_card(&mut fb);
    // 驱动到底把每一级灰算成了哪个 4-bit 电平 —— 直接说给用户听，
    // 免得「看起来差不多」和「真的只画了 8 级」分不清。
    //
    // 通路是：RGB565 → 绿通道 6 bit → `(g*15+31)/63` → 4 bit。
    let mut lv = Vec::new();
    let n = 16;
    let bw = (w - 80) / n;
    for i in 0..n {
        let tone = (i * 255 / (n - 1)) as u8;
        let g8 = fb.get(40 + i * bw + bw / 2, 150);
        let g = ((rmdev::fb::gray565(g8) >> 5) & 0x3f) as u32;
        let l4 = (g * 15 + 31) / 63;
        lv.push(l4);
        print!("{tone:3}→{l4:2}  ");
    }
    println!("\n测试卡 16 级灰阶（tone→4bit 电平）：{lv:?}");
    let t0 = std::time::Instant::now();
    fb.refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
    println!(
        "整屏 GC16 投递完成 {:.0} ms；屏上此刻应该是 16 条不同深浅的竖条 + 下面一条连续渐变",
        t0.elapsed().as_secs_f64() * 1e3
    );
    std::thread::sleep(std::time::Duration::from_millis(2000));
    drop(fb);
}

/// 把测试卡画进平面（不刷新）。
fn paint_card(fb: &mut rmdev::fb::Fb) {
    use rmdev::fb::gray565;
    let (w, h) = (fb.width, fb.height);
    fb.fill_rect(0, 0, w, h, gray565(255));

    // 16 级灰阶：看驱动的灰度还原和相邻级能不能分开
    let n = 16;
    let bw = (w - 80) / n;
    for i in 0..n {
        let tone = (i * 255 / (n - 1)) as u8;
        fb.fill_rect(40 + i * bw, 50, bw - 4, 200, gray565(tone));
    }
    // 连续渐变：看有没有断带、阶梯
    for x in 0..(w - 80) {
        let tone = (x * 255 / (w - 81)) as u8;
        fb.fill_rect(40 + x, 280, 1, 120, gray565(tone));
    }
    // 1/2/3 px 竖线与横线：看对齐、锐度、有没有被 cell 边界吃掉
    for k in 0..14 {
        // 竖线：x 每条挪 30，宽度 1/2/3 循环
        let x = 60 + k * 30;
        fb.fill_rect(x, 430, 1 + (k % 3) as i32, 120, gray565(0));
        // 横线：y 每条挪 9（以前 y 写死 580，14 条全叠在一条上了）
        fb.fill_rect(60, 580 + k * 9, 200, 1 + (k % 3) as i32, gray565(0));
    }
    // 棋盘：看有没有漏驱/竖带
    for y in 0..8 {
        for x in 0..16 {
            if (x + y) % 2 == 0 {
                fb.fill_rect(760 + x * 32, 430 + y * 32, 32, 32, gray565(0));
            }
        }
    }
    // 文字：不同字号 + 中文
    if let Some(mut t) = rmdev::ui::text::Text::load() {
        let mut y = 760;
        for (size, s) in [
            (44.0f32, "reMarkable 2 test card"),
            (30.0, "The quick brown fox jumps over the lazy dog"),
            (22.0, "0123456789 !@#$%^&*() []{}<> ~ and some thin strokes"),
            (26.0, "中文测试：墨水屏局部刷新 / 灰阶 / 细线"),
        ] {
            t.draw(fb, 60, y, size, s, 0);
            y += (size as i32 * 2).max(44);
        }
    }
}

/// 测试卡留在屏上，黑方块**斜着**走、撞边反弹，跑 `secs` 秒。
///
/// 和真手写一样的投递方式：每 ~10 ms 把「旧位置恢复成卡片原样 + 新位置画黑」
/// 交一次局部刷新。背景从 `bg`（测试卡的整屏副本）恢复，所以卡片内容不会
/// 被方块擦掉。
///
/// 正确效果：**只有黑方块在动，测试卡其他内容不受影响**。方块身后如果有
/// 黑色拖尾，说明那些像素没有被驱到白（DU 白←黑 需要连续 24 帧）。
fn bounce(speed: f64, secs: f64, wf: u32, keep_gray: bool) {
    use rmdev::fb::{
        gray565, Fb, UPDATE_MODE_FULL, UPDATE_MODE_PARTIAL, WAVEFORM_MODE_DU, WAVEFORM_MODE_GC16,
    };
    let mut fb = Fb::open().expect("打开面板");
    let (w, h) = (fb.width, fb.height);
    paint_card(&mut fb);
    // 把整张卡量化成**纯黑白**（阈值 128）。
    //
    // 这不是为了好看，是因为 DU 波形的 LUT 只对纯黑/纯白有意义：
    // `Du 中灰←黑` 整轮全是 0（不驱动），`Du 中灰←中灰` 也是 0。所以压在
    // 中间调上的方块走过去以后，中间调物理上回不来 —— 灰拖尾、白方块。
    // 量化成黑白以后 DU 能精确还原，才能验证"只有方块在动"。
    // （真实笔记页就是黑墨白纸，量化正好对应它的最坏情况。）
    if !keep_gray {
        for yy in 0..h {
            if let Some(row) = fb.row_mut(yy, 0, w) {
                for d in row.iter_mut() {
                    let g = (*d >> 5) & 0x3f;
                    *d = gray565(if g < 32 { 0 } else { 255 });
                }
            }
        }
    }
    fb.refresh(0, 0, w, h, WAVEFORM_MODE_GC16, UPDATE_MODE_FULL);
    println!("测试卡上屏（{}），开始斜向反弹 {speed:.0} px/s，{secs:.0} s，波形 {wf}", if keep_gray { "保留灰阶" } else { "纯黑白" });
    std::thread::sleep(std::time::Duration::from_millis(800));
    // 整屏备份：方块移动时用它把背景原样恢复
    let mut bg = vec![0u16; (w * h) as usize];
    for yy in 0..h {
        let base = (yy * w) as usize;
        if let Some(src) = fb.row_mut(yy, 0, w) {
            bg[base..base + w as usize].copy_from_slice(src);
        }
    }
    const S: i32 = 56;
    let (x0, y0) = (20, 20);
    let (x1, y1) = (w - 20 - S, h - 20 - S);
    let mut x = (w / 2) as f64;
    let mut y = (h / 2) as f64;
    let mut dx = speed * 10.0 / 1000.0 / 1.4142;
    let mut dy = dx;
    let (mut lx, mut ly) = (x as i32, y as i32);
    let t0 = std::time::Instant::now();
    let mut n = 0u32;
    while t0.elapsed().as_secs_f64() < secs {
        x += dx;
        y += dy;
        if x < x0 as f64 {
            x = x0 as f64;
            dx = -dx;
        }
        if x > x1 as f64 {
            x = x1 as f64;
            dx = -dx;
        }
        if y < y0 as f64 {
            y = y0 as f64;
            dy = -dy;
        }
        if y > y1 as f64 {
            y = y1 as f64;
            dy = -dy;
        }
        let (cx, cy) = (x.round() as i32, y.round() as i32);
        // 旧位置：从备份原样恢复
        for yy in (ly - 2)..(ly + S + 2) {
            if yy < 0 || yy >= h {
                continue;
            }
            if let Some(dst) = fb.row_mut(yy, lx - 2, S + 4) {
                let base = (yy * w) as usize;
                let a = (lx - 2).clamp(0, w) as usize;
                let n2 = dst.len();
                dst.copy_from_slice(&bg[base + a..base + a + n2]);
            }
        }
        // 新位置：**反色方块**（底色偏暗画白、偏亮画黑）。
        // 这样压在灰阶/棋盘格上也看得见，而不是黑方块压黑块"消失"。
        for yy in cy..(cy + S) {
            if yy < 0 || yy >= h {
                continue;
            }
            let base = (yy * w) as usize;
            if let Some(dst) = fb.row_mut(yy, cx, S) {
                for (i, d) in dst.iter_mut().enumerate() {
                    let b = bg[base + cx as usize + i];
                    let g = (b >> 5) & 0x3f;
                    *d = if g < 32 { gray565(255) } else { gray565(0) };
                }
            }
        }
        // 刷新矩形 = 旧 ∪ 新
        let rx = (lx - 2).min(cx - 2).max(0);
        let ry = (ly - 2).min(cy - 2).max(0);
        let rx1 = (lx + S + 2).max(cx + S + 2).min(w);
        let ry1 = (ly + S + 2).max(cy + S + 2).min(h);
        fb.refresh(rx, ry, rx1 - rx, ry1 - ry, wf, UPDATE_MODE_PARTIAL);
        lx = cx;
        ly = cy;
        n += 1;
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    println!("斜向反弹结束：{n} 次刷新，{:.1} s", t0.elapsed().as_secs_f64());
    std::thread::sleep(std::time::Duration::from_millis(1500));
    drop(fb);
}

/// 一条命令放完：测试卡 → 停 4 s → 白底 → 黑方块在同一条带上换 4 档速度。/// 一块黑方块按 `speed` px/s 横着走：每 ~10 ms 重画一次，只刷新
/// 「旧位置 ∪ 新位置」那一小块（和 app 手写时的投递方式一样）。
///
/// `speed` 小 = 墨有充足时间吃满波形；`speed` 大 = 每帧都在换位置。
/// 两种速度下都不该出现竖带、整块闪、或者拖尾。
fn move_box(speed: f64, secs: f64) {
    use rmdev::fb::{gray565, Fb, UPDATE_MODE_PARTIAL, WAVEFORM_MODE_DU};
    let mut fb = Fb::open().expect("打开面板");
    let (w, h) = (fb.width, fb.height);
    fb.fill_rect(0, 0, w, h, gray565(255));
    fb.refresh(0, 0, w, h, WAVEFORM_MODE_DU, UPDATE_MODE_PARTIAL);
    const S: i32 = 48; // 方块边长
    let y = h / 2 - S / 2;
    let x0 = 60;
    let x1 = w - 60 - S;
    let step_ms = 10.0;
    let step_px = (speed * step_ms / 1000.0).max(1.0);
    let mut x = x0 as f64;
    let mut dir = 1.0f64;
    let t0 = std::time::Instant::now();
    let mut last = x0;
    let mut frames = 0u32;
    while t0.elapsed().as_secs_f64() < secs {
        x += step_px * dir;
        if x > x1 as f64 {
            x = x1 as f64;
            dir = -1.0;
        }
        if x < x0 as f64 {
            x = x0 as f64;
            dir = 1.0;
        }
        let cx = x.round() as i32;
        // 先擦旧位置（平面里恢复白），再画新位置
        fb.fill_rect(last - 1, y - 1, S + 2, S + 2, gray565(255));
        fb.fill_rect(cx, y, S, S, gray565(0));
        let rx = (last - 1).min(cx - 1).max(0);
        let rw = ((last - 1 + S + 2).max(cx + S + 1) - rx).min(w - rx);
        fb.refresh(rx, y - 1, rw, S + 2, WAVEFORM_MODE_DU, UPDATE_MODE_PARTIAL);
        last = cx;
        frames += 1;
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    println!("方块 {speed} px/s，{frames} 次刷新，{:.1} s", t0.elapsed().as_secs_f64());
    std::thread::sleep(std::time::Duration::from_millis(800));
    drop(fb);
}

/// 找「扫描位置指示器」：LCDIF 里哪个寄存器在 11.76 ms 的帧内单调变化。
///
/// 单槽直接写的时候，唯一能消除接缝的办法是**知道扫到哪一列了**：把新的
/// cell 码写在扫描线刚扫过之后，让每一列都恰好晚一帧生效（均匀延迟就看不见），
/// 而不是「前半屏新码、后半屏旧码」（那是一条竖带）。
///
/// eLCDIF 的 `TRANSFER_COUNT` 在传输过程中会自减，所以这里把 0x00–0x100
/// 每个寄存器都采 40 次，报「变化过」的那些和它们的取值范围。
fn scanpos() {
    use std::os::unix::io::AsRawFd;
    const LCDIF: u64 = 0x3073_0000;
    let fdm = std::fs::OpenOptions::new()
        .read(true).write(true).open("/dev/mem").expect("/dev/mem");
    let m = unsafe {
        libc::mmap(std::ptr::null_mut(), 0x200, libc::PROT_READ | libc::PROT_WRITE,
                   libc::MAP_SHARED, fdm.as_raw_fd(), LCDIF as libc::off_t)
    };
    assert_ne!(m, libc::MAP_FAILED, "mmap LCDIF");
    let base = m as *const u32;
    let rd = |off: usize| unsafe { std::ptr::read_volatile(base.add(off / 4)) };
    // 同时开一个 fb，确保面板是亮的（不然 LCDIF 可能根本没在扫）。
    let _f = std::fs::OpenOptions::new().read(true).write(true).open("/dev/fb0").expect("/dev/fb0");
    let names: [(usize, &str); 16] = [
        (0x00, "CTRL"), (0x10, "CTRL1"), (0x20, "CTRL2"), (0x30, "TRANSFER_COUNT"),
        (0x40, "CUR_BUF"), (0x50, "NEXT_BUF"), (0x60, "TIMING"), (0x70, "VDCTRL0"),
        (0x80, "VDCTRL1"), (0x90, "VDCTRL2"), (0xa0, "VDCTRL3"), (0xb0, "VDCTRL4"),
        (0xc0, "DVCOUNT"), (0xd0, "CDC_LOCK"), (0xe0, "CRC_STAT"), (0xf0, "STAT"),
    ];
    let mut last: Vec<u32> = Vec::new();
    let mut minv: Vec<u32> = Vec::new();
    let mut maxv: Vec<u32> = Vec::new();
    for (k, _) in names.iter().enumerate() {
        last.push(rd(names[k].0));
        minv.push(u32::MAX);
        maxv.push(0);
    }
    // 采样 40 次，每次 ~300 µs ≈ 一帧 12 ms 扫过全屏。
    let mut changed: Vec<usize> = Vec::new();
    let t0 = std::time::Instant::now();
    let mut seq: Vec<(f64, u32)> = Vec::new();
    for _ in 0..40 {
        for k in 0..names.len() {
            let v = rd(names[k].0);
            if v != last[k] {
                if !changed.contains(&k) {
                    changed.push(k);
                }
                last[k] = v;
            }
            minv[k] = minv[k].min(v);
            maxv[k] = maxv[k].max(v);
        }
        // 专门把 TRANSFER_COUNT 的时间序列打出来，看它是不是按帧递减
        seq.push((t0.elapsed().as_secs_f64() * 1e3, rd(0x30)));
        std::thread::sleep(std::time::Duration::from_micros(300));
    }
    for k in 0..names.len() {
        println!(
            "{:16} 0x{:03X} = 0x{:08X}  范围 [0x{:08X}, 0x{:08X}] {}",
            names[k].1, names[k].0, last[k], minv[k], maxv[k],
            if changed.contains(&k) { "← 变化" } else { "" }
        );
    }
    println!("TRANSFER_COUNT 时间序列（ms, 值）：");
    for (t, v) in seq.iter().step_by(2) {
        println!("  {t:7.3} 0x{v:08X}");
    }
}

/// 最小实验：完全不碰 `Panel`，只拿 `/dev/fb0` 试「到底怎么才能让面板换槽」。
///
/// 官方 swtcon 两条路径都用：
///   * `pan_to_frame(i)`   = 设 yoffset + `FBIOPAN_DISPLAY`（每帧切槽）；
///   * `pan_and_unblank(i)`= 设 yoffset + `FBIOPUT_VSCREENINFO` + `FBIOBLANK(UNBLANK)`。
///
/// 所以这里依次试四种组合，每步之后读 LCDIF 的 CUR_BUF 和内核的 yoffset，
/// 看哪一种真的把扫描地址换掉了、以及它是不是阻塞一个帧周期。
fn panraw() {
    use std::os::unix::io::AsRawFd;
    const LCDIF: u64 = 0x3073_0000;
    const FBIOPUT_VSCREENINFO: libc::c_ulong = 0x4601;
    const FBIOGET_VSCREENINFO: libc::c_ulong = 0x4600;
    const FBIOPAN_DISPLAY: libc::c_ulong = 0x4606;
    const FBIOBLANK: libc::c_ulong = 0x4611;
    let fdm = std::fs::OpenOptions::new()
        .read(true).write(true).open("/dev/mem").expect("/dev/mem");
    let m = unsafe {
        libc::mmap(std::ptr::null_mut(), 0x100, libc::PROT_READ | libc::PROT_WRITE,
                   libc::MAP_SHARED, fdm.as_raw_fd(), LCDIF as libc::off_t)
    };
    assert_ne!(m, libc::MAP_FAILED, "mmap LCDIF");
    let base = m as *const u32;
    let rd = |off: usize| unsafe { std::ptr::read_volatile(base.add(off / 4)) };
    let f = std::fs::OpenOptions::new().read(true).write(true).open("/dev/fb0").expect("/dev/fb0");
    let fd = f.as_raw_fd();
    // fb_var_screeninfo 的布局：xres,yres,xres_virtual,yres_virtual,xoffset,yoffset(6×u32)…
    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Var { info: [u32; 6], rest: [u32; 26], res: [u32; 2] }
    let mut v = Var::default();
    unsafe { libc::ioctl(fd, FBIOGET_VSCREENINFO, &mut v as *mut Var) };
    println!(
        "初始 xres={} yres={} xvirt={} yvirt={} xoff={} yoff={} CUR=0x{:08X}",
        v.info[0], v.info[1], v.info[2], v.info[3], v.info[4], v.info[5], rd(0x40)
    );
    let xres = v.info[0];
    let yres = v.info[1];
    let mut show = |tag: &str, v: &Var| {
        println!(
            "  {tag:28} yoff={:6} CUR=0x{:08X} NEXT=0x{:08X}",
            v.info[5], rd(0x40), rd(0x50)
        );
    };
    // 先扩到 17 槽（官方 frameCount=16 的布局），再看 pan 认不认。
    v.info[3] = yres * 17;
    v.info[5] = yres * 16;
    let t = std::time::Instant::now();
    let r1 = unsafe { libc::ioctl(fd, FBIOPUT_VSCREENINFO, &mut v as *mut Var) };
    println!("PUT yvirt={} yoff={} -> {r1}（{:.1} ms）", v.info[3], v.info[5],
             t.elapsed().as_secs_f64() * 1e3);
    show("after PUT yoff=16", &v);
    std::thread::sleep(std::time::Duration::from_millis(50));
    show("  +50 ms", &v);
    // 方案 A：FBIOPAN_DISPLAY 切到槽 0
    v.info[5] = 0;
    let t = std::time::Instant::now();
    let r2 = unsafe { libc::ioctl(fd, FBIOPAN_DISPLAY, &mut v as *mut Var) };
    println!("PAN yoff=0 -> {r2}（{:.1} ms）", t.elapsed().as_secs_f64() * 1e3);
    show("after PAN yoff=0", &v);
    std::thread::sleep(std::time::Duration::from_millis(50));
    show("  +50 ms", &v);
    // 方案 B：PUT + UNBLANK 切到槽 1
    v.info[5] = yres;
    let t = std::time::Instant::now();
    let r3 = unsafe { libc::ioctl(fd, FBIOPUT_VSCREENINFO, &mut v as *mut Var) };
    let r4 = unsafe { libc::ioctl(fd, FBIOBLANK, 0 as libc::c_int) };
    println!("PUT yoff={yres} -> {r3} + UNBLANK -> {r4}（{:.1} ms）",
             t.elapsed().as_secs_f64() * 1e3);
    show("after PUT+UNBLANK yoff=1", &v);
    std::thread::sleep(std::time::Duration::from_millis(50));
    show("  +50 ms", &v);
    // 方案 C：PAN 切到槽 2
    v.info[5] = yres * 2;
    let t = std::time::Instant::now();
    let r5 = unsafe { libc::ioctl(fd, FBIOPAN_DISPLAY, &mut v as *mut Var) };
    println!("PAN yoff=2*yres -> {r5}（{:.1} ms）", t.elapsed().as_secs_f64() * 1e3);
    show("after PAN yoff=2", &v);
    std::thread::sleep(std::time::Duration::from_millis(50));
    show("  +50 ms", &v);
    // 还原到 2 槽布局，别把机器留在怪状态
    v.info[3] = yres * 2;
    v.info[5] = yres;
    unsafe { libc::ioctl(fd, FBIOPUT_VSCREENINFO, &mut v as *mut Var) };
    unsafe { libc::ioctl(fd, FBIOPAN_DISPLAY, &mut v as *mut Var) };
    show("还原 yvirt=2 yoff=1", &v);
    let _ = xres;
}

/// LCDIF 寄存器（`0x30730000`）：直接看 CUR_BUF 有没有跟着 pan 走。
fn regs() {
    const LCDIF: u64 = 0x3073_0000;
    let Ok(f) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/mem") else {
        eprintln!("/dev/mem 打不开");
        return;
    };
    use std::os::unix::io::AsRawFd;
    let len = 0x100usize;
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            f.as_raw_fd(),
            LCDIF as libc::off_t,
        )
    };
    if p == libc::MAP_FAILED {
        eprintln!("/dev/mem mmap 失败");
        return;
    }
    let base = p as *const u32;
    for (name, off) in [
        ("CTRL", 0x00usize),
        ("CTRL1", 0x10),
        ("CTRL2", 0x20),
        ("TRANSFER_COUNT", 0x30),
        ("CUR_BUF", 0x40),
        ("NEXT_BUF", 0x50),
        ("VDCTRL1", 0x80),
        ("VDCTRL4", 0xb0),
    ] {
        let v = unsafe { std::ptr::read_volatile(base.add(off / 4)) };
        println!("{name:15} 0x{off:03X} = 0x{v:08X}");
    }
}


/// n 个区域同时驱时，一帧的真实代价。
///
/// 每个区域都按自己的波形盖相位戳（`set_rect` 传自己的 `mi`），所以 `mixed`
/// 那一档是 DU 和 GC16 在同一帧里并行推进 —— 这正是官方 swtcon「每个
/// WorkItem 自带 phase」的行为。驱动矩形取 n 个区域的并集（我们有意的设计：
/// 一个 bbox，一次 `emit_cells` 写下去）。
fn par_bench(p: &mut rmdev::panel::Panel, n: usize, w: usize, h: usize, kind: &str) {
    use rmdev::panel::Mode;
    p.debug_reset_img(15);
    let pick = |i: usize| -> Mode {
        match kind {
            "gc16" => Mode::Gc16,
            "gl16" => Mode::Gl16,
            "a2" => Mode::A2,
            "mixed" => {
                if i % 2 == 0 {
                    Mode::Du
                } else {
                    Mode::Gc16
                }
            }
            _ => Mode::Du,
        }
    };
    // 按可用宽度算列数，保证 n 个都放得下（以前写死 4 列，300 宽的
    // 第 4 个就溢出到屏幕外，n=4/8 直接被跳过）。
    let cols = ((1404usize.saturating_sub(140)) / (w + 24)).max(1);
    let mut rects: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut nph_max = 1usize;
    for i in 0..n {
        let cx = 140 + (i % cols) * (w + 24);
        let cy = 200 + (i / cols) * (h + 24);
        if cx + w > 1404 || cy + h > 1872 {
            println!("  n={n} 跳过：区域放不下");
            return;
        }
        let mode = pick(i);
        let mi = p.mode_slot(mode).unwrap_or(0);
        nph_max = nph_max.max(p.phases(mode).unwrap_or(1));
        let levels = vec![0u8; w * h];
        let _ = p.set_rect(cx, cy, w, h, &levels, mi);
        rects.push((cx, cy, w, h));
    }
    let u = rects
        .iter()
        .copied()
        .reduce(|a, b| {
            let x0 = a.0.min(b.0);
            let y0 = a.1.min(b.1);
            let x1 = (a.0 + a.2).max(b.0 + b.2);
            let y1 = (a.1 + a.3).max(b.1 + b.3);
            (x0, y0, x1 - x0, y1 - y0)
        })
        .unwrap();
    p.frame_stat_reset();
    let mut enc_ms = 0.0f64;
    let mut enc_max = 0.0f64;
    for _ in 0..nph_max {
        let t = std::time::Instant::now();
        p.encode_phase(u.0, u.1, u.2, u.3).unwrap();
        let e = t.elapsed().as_secs_f64() * 1e3;
        enc_ms += e;
        enc_max = enc_max.max(e);
        p.show_pending(u.0, u.1, u.2, u.3).unwrap();
    }
    let (fn_, favg, fmax, fslow) = p.frame_stat();
    let px = (u.2 * u.3) as f64;
    println!(
        "  n={n}  {nph_max} 帧  并集 {}×{} = {:.3} Mpx  编码 {:.2} 均 {:.2} 峰 ms（{:.1} ns/px）| 帧周期 {favg:.1} 均 {fmax:.1} 峰，超帧 {fslow}/{fn_}",
        u.2,
        u.3,
        px / 1e6,
        enc_ms / nph_max as f64,
        enc_max,
        enc_ms * 1e6 / (px * nph_max as f64)
    );
}
