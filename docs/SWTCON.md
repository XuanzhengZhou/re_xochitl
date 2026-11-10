# rM2 到底怎么写屏：没有 EPDC，只有用户态 swtcon

> 2026-11-07 真机取证。目标：搞清楚「epdc 写屏」到底怎么工作，以及为什么看起来只有官方 xochitl 能写。
>
> 结论提前说：**这台机器上没有 EPDC，也没有任何内核显示驱动接口可用。
> 上屏是官方 xochitl 里一段纯用户态代码（`EPFramebufferSwtcon`，软件 TCON）**
> **把像素打包好 memcpy 进 `/dev/fb0`，再用 `FBIOPAN_DISPLAY` 翻页。**
> rm2fb 并没有重写这段，它是**劫持官方进程**去调它。所以「只有官方能写」是真的——
> 但代价不是「没办法」，而是「要么复用那个进程，要么把 swtcon 抄出来」。

---

## 1. 硬件事实（全部真机核实）

| 项 | 实测值 | 取证方式 |
|---|---|---|
| 设备 / SoC | reMarkable 2.0 / i.MX7D | `/proc/device-tree/model`、`/sys/devices/soc0/machine` |
| 内核 | `5.4.70-v1.2.6-rm11x` | `uname -a` |
| **EPDC** | **`status = disabled`** | `/proc/device-tree/soc/aips-bus@30400000/epdc@306f0000/status` |
| `/proc/fb` | **只有 `0 mxs-lcdif`**，没有 epdc | `cat /proc/fb` |
| 显示驱动 | `mxsfb`（LCDIF），`30730000.lcdif` | `.../driver -> .../drivers/mxsfb`，dmesg `mxsfb: initialized` |
| DT 标志位 | `prevent-frying-pan`、`lcd-supply`、`lcd2-supply` | lcdif 节点属性 |
| `display#1` timings | `hactive=0x14e(334)`、`vactive=0x57d(1405)`、`24bpp / bus-width 24`、40 MHz | timing0 各属性 |
| `/dev/fb0` | `260 x 1408 @32bpp`，`stride 1040`，`virtual_size 260,23936` | `/sys/class/graphics/fb0/*`、`dfbinfo` |
| 物理地址 | `0xa9d00000`，32 MB | `dfbinfo`、`/proc/<pid>/maps` |

关键算术（都对得上）：

```
一页   = 1040 B/行 × 1408 行            = 1,464,320 B
17 页  = 1,464,320 × 17                 = 24,893,440 B  =  sysfs virtual_size ✔
零页   = (23936-1408) × 1040            = 23,429,120   （驱动回退目标）
```

也就是说 `/dev/fb0` 是 **17 个 260×1408 RGB32 页**，不是一张 1404×1872 的图。
面板 1404×1872 的图像被**转置 + 压成 4bpp** 才塞得进一页。

---

## 2. 官方的上屏链路

```
Qt Quick 场景图
   ↓  EPRenderer（软件合成器，QPainter 逐层画）
QImage 1404×1872 RGB16          ← rm2fb 把这行劫持成 /dev/shm/swtfb.01
   ↓  EPFramebuffer::sendUpdate(QRect, WaveformMode, UpdateFlags)   [0x552f74]
EPFramebufferSwtcon
   ↓  FUN_005560b0  [0x5560b0]   ← 唯一引用 /dev/fb0 的函数
转置 + 16bpp→4bit 打包
   ↓  memcpy 进 /dev/fb0 的某一页（17 页轮转）
framegen / vsync-flip 两个线程（SCHED_FIFO 98 / 99）
   ↓  ioctl(FBIOPAN_DISPLAY, yoffset = 页号 × 1408)
LCDIF 把这一页以 85Hz 串出 → 面板 TCON → 电子墨水
```

`EPFramebuffer` 是**纯虚接口**，`Q_PROPERTY Backend`：

```cpp
enum Backend { Tcon = 0, Swtcon = 1, Desktop = 2 };
```

- **Tcon** = rM1 那套硬件 EPDC；
- **Swtcon** = rM2 走的这条，**波形由用户态算**；
- Desktop = 桌面模拟器。

### 2.1 波形数据在哪

不是什么「藏在芯片里」的固件：

```
xochitl[4839]: Reading waveforms from /usr/share/remarkable/320_R355_AF0A11_ED103TC2U2_VB3300-KCD_TC.wbf
xochitl[4839]: Unable to load waveform from file: /usr/share/remarkable/speedy-gonzales.csv
```

- `/usr/share/remarkable/` 下有 **42 个 `.wbf`**（E Ink 波形库，每个约 280 KB，按面板料号命名）；
- 二进制里还留了一条 `speedy-gonzales.csv` 的旧路径，**本固件没有这个文件**，走的是回退分支；
- 本机实际加载的是 **`320_R355_AF0A11_ED103TC2U2_VB3300-KCD_TC.wbf`**。

这行日志是从**真正当 server 的那个 xochitl 进程**里出来的，所以它就是这台机器的面板波形。
DT 里的 `parade,panel_id_support` 说明料号是从 Parade TCON 读回来的。

### 2.2 像素格式（反编译 0x5560b0 的内联循环）

源：1404×1872、RGB565、行距 2808 B（= 1404×2）。
目标：先建一块 `malloc(0x281ac0)` = **2,628,288 B = 1404×1872 字节**（1 字节/像素）。

```c
// 每轮从 8 行、同一列取 8 个像素，转置写进 8 个连续字节
nib = (px_low_byte >> 1) & 0xF;      // 等价于 gray8 >> 4（灰度图 blue = g>>3）
out = (nib << 4) | nib;              // 半字节复制：高低 nibble 相同
```

即：

- **16 级灰**，`level = (rgb565 低字节 >> 1) & 0xF`；
- 每个字节只放 1 个像素的有效值，**高低半字节都填同一个**（这样硬件读哪半边都对）；
- **转置**：输出行 1872 B、共 1404 行，列↔行互换；
- 之后 framegen 再把这份 8bpp 转置缓冲压成 **4bpp 打包**写进 fb 页：
  `1872 px × 4 bit = 936 B ≤ 1040 B`，`1404 行 ≤ 1408 行`，多出来的就是 padding。

> 尚未钉死的一处：1040−936 = 104 B/行、1408−1404 = 4 行，
> 这些 padding 落在行尾还是交错，需要**抓一帧活的面板页**反推（见 §6）。

### 2.3 `prevent_frying_pan`：每次只显示一帧

`drivers/video/fbdev/mxsfb.c`（reMarkable 的 fork）：

```c
static int mxsfb_pan_display(struct fb_var_screeninfo *var, struct fb_info *fb_info)
{
    ...
    writel(fb_info->fix.smem_start + offset, host->base + host->devdata->next_buf);
    writel(CTRL1_CUR_FRAME_DONE_IRQ_EN, ...);
    ret = wait_for_completion_timeout(&host->flip_complete, HZ / 2);   // 等一帧
    ...
    if (host->prevent_frying_pan) {
        /* Next frame will be the zero frame (last frame in buffer) by default. */
        offset = fb_info->fix.line_length * (var->yres_virtual - var->yres);
        writel(fb_info->fix.smem_start + offset, host->base + host->devdata->next_buf);
    }
    return ret;      // ← 注意：返回的是"剩余 jiffies"，不是 0
}
```

三个必须知道的后果：

1. **一次 pan = 面板只收一帧，然后 LCDIF 立刻被指回最后一页**（防直流偏压把屏"煎糊"）；
   **所以每帧都得重新 pan**，不能指望「写一次内存就一直在」。
2. **`FBIOPAN_DISPLAY` 成功时返回 49 / 48（剩余 jiffies），不是 0。**
   凡是 `if (ioctl(...) != 0)` 判失败的代码都会误判。必须是 `< 0`。
3. 每次 pan 会**阻塞约一帧**（85 Hz ≈ 11.8 ms）。串行刷屏就是这个开销。

---

## 3. 「只有官方能写」的真相

`librm2fb_server.so` 不是驱动，是**寄生**。它就干三件事：

1. **覆盖 `__libc_start_main`** —— 进程一起来 `main` 就被换成 `server_main`，
   并把 `argv[0]` 改成 `"rm2fb-server"`、`prctl(PR_SET_NAME)` 也改掉。
   （所以 `ps` 里看到的是 `rm2fb-server`，而 `/proc/<pid>/exe` 仍指向 `/usr/bin/xochitl`。）
2. **hook `QImage::QImage(int,int,Format)`** —— 官方那个「framebuffer 镜像」QImage 一旦按
   1404×1872 申请，就把它的像素指针换成 `/dev/shm/swtfb.01`。
   日志 `REPLACING THE IMAGE with shared memory` 就是这里。
3. **靠 Qt 元对象反射调官方的方法**：

```cpp
// src/shared/swtfb.cpp（rm2fb 上游）
void initQT() {
    auto ptr = f_getInstance();            // ← 地址从配置文件里读
    instance = reinterpret_cast<QObject *>(ptr);
    img = (QImage *)(ptr + 8);             // 偏移 +8 就是它的 QImage
}
void SendUpdate(const QRect &rect, int waveform, int flags) const {
    QGenericArgument argWaveform("EPFramebuffer::WaveformMode", &waveform);
    QGenericArgument argUpdateMode("EPFramebuffer::UpdateFlags", &flags);
    QMetaObject::invokeMethod(instance, "sendUpdate", Qt::DirectConnection,
                              Q_ARG(QRect, rect), argWaveform, argUpdateMode);
}
```

`EPFramebuffer::sendUpdate` 的签名（从 moc 元数据里抠出来的，共 14 个方法）：

```
moc @ 0x7ae84c / static_metacall @ 0x547c88
 1  clearScreen()
 2  sendUpdate(QRect, WaveformMode, UpdateFlags)
 4  waitForLastUpdate()
 9  clearGhosting()                 // 内部走 sendUpdate(fullRect, 3)
10  addFastUpdateRect(QRect)
13  fastUpdateRegion()
```

**这就解释了所有现象**：

- 为什么把 `librm2fb_server.so` preload 进自己的程序会 SIGSEGV ——
  它要 `getInstance` 拿到 xochitl 里的那个单例，非 Qt、非 xochitl 的地址空间里啥也没有；
- 为什么「只有官方的 libepaper 能写屏」—— 真正碰面板的 `EPFramebufferSwtcon`
  和那一整条 framegen 流水线**只存在于 xochitl 二进制里**，而且波形也要它自己那份 `.wbf`；
- 你们现在看到的「屏幕卡死但客户端一切正常」，就是 `invokeMethod` 打到的那个进程状态不对
  （或者它压根没跑到 swtcon 提交）。

---

## 4. 现有代码里的三处错误假设

`rmdev/src/fb.rs` 顶部那些常量，在这台机器上**全是死代码**：

| 代码里的假设 | 实际 |
|---|---|
| `MXCFB_SEND_UPDATE = 0x4048462e` 用来上屏 | **本机没有 EPDC 驱动，没人处理它。** 只有 rm2fb 的 client 垫片会截走并转成一条 24 B 的 IPC 消息 |
| `WAVEFORM_MODE_DU/GC16/GL16`、`temp = 0x18`、`dither_mode` | 同上，全被垫片丢弃；swtcon 用的是自己的 `WaveformMode` 枚举 + `.wbf` |
| `/dev/fb0` 是 `1404×1872 @16bpp` | 裸 `/dev/fb0` 是 `260×1408 @32bpp`。只有**在 rm2fb client 垫片下**才看见 1404×1872——因为是垫片伪造的 |

`fb.rs` 里那句「裸 `/dev/fb0` 是 260×1408@32bpp 行缓冲，画上去什么也看不到」是**对的**，
但结论下错了：不是「必须走 rm2fb」，而是「必须按 swtcon 的格式往 17 页里写」。

另外 `RM_TEMP` / `RM_DITHER` / `RM_WAIT` 三个调参**从来没有生效过**，
之前所有基于它们做的性能/灰度对比结论都该作废重跑。

---

## 5. 三条路，按代价排

### A. 让官方 xochitl 继续当「隐形显示守护进程」（现状，0 风险）

就是 rm2fb 现在的做法，你们已经跑通了。它的价值被低估了：
那个进程**不显示自己的 UI**（`main` 被换掉了），只是个 25 MB 的显示服务。

代价：依赖官方二进制 + 每次 OTA 后要重验 + `HOME` 隔离那个坑（见 `RM2FB.md` §5.4）。

**它是可以接受的产品形态。** 唯一硬伤是「官方更新会动它」和启动顺序脆弱。

### B. 自己实现 swtcon（真正的解绑，估计 2~4 人周）

需要抄的东西已经很清楚了：

| 要抄的 | 位置 | 状态 |
|---|---|---|
| 转置 + 4bit 打包 | `0x5560b0` 内联循环 | 已反编译，公式见 §2.2 |
| 波形表加载 | `FUN_0055aee4`（`fread` 循环） | 已反编译；数据就是那个 `.wbf` |
| 提交/驱动序列生成 | `FUN_00555084`（3960 B，**函数体内没有任何 ioctl / 文件操作**） | 已反编译成 621 行伪 C，**读明白它才是真正的门槛** |
| fb 打开 + pan | 藏在 `FUN_00554d18` / `FUN_00554f58` 一带 | 未逐个钉死 |
| 波形映射 | `0x552f74`：`0→2, 1→0, 2→1, 3→2, 6→3, 8→1, 10→(flag?8:0)`；4/5/7/9 报 invalid | 已还原 |

好消息：`.wbf` 是标准 E Ink 波形容器（本机 `320_R355_AF0A11_ED103TC2U2_VB3300-KCD_TC.wbf`），
不用自己合成波形。坏消息：framegen 那条路径是按实时优先级 98 跑的，
时序错了会 LCDIF underflow（花屏）。

### C. 折中：只接管「写页 + 翻页」，波形仍借用一次（推荐先试）

如果 `FUN_00555084` 的产出其实是**每帧一份可复用的驱动序列**，那么：
让官方进程跑一次拿到序列 → 之后自己 memcpy + pan。
这条路能不能走通取决于 `FUN_00555084` 的输出去向，**需要再一轮反编译确认**，
是投入最小、收益最大的一步。

---

## 7. 复现命令

```sh
# 1. 确认没有 EPDC
ssh remarkable 'cat /proc/fb; \
  cat /proc/device-tree/soc/aips-bus@30400000/epdc@306f0000/status; echo'

# 2. fb 真几何
ssh remarkable 'for f in name stride bits_per_pixel virtual_size modes pan; do \
  printf "%-16s %s\n" "$f" "$(cat /sys/class/graphics/fb0/$f)"; done'

# 3. 谁真的握着 /dev/fb0
ssh remarkable 'for p in $(pidof xochitl); do echo "== $p"; \
  tr "\0" " " < /proc/$p/cmdline; echo; ls -l /proc/$p/fd | grep -E "fb0|shm"; done'

# 4. 看它在翻页（不是写像素）
ssh remarkable 'sh -c "strace -f -p $(pidof xochitl | cut -d\  -f1) \
  -e trace=ioctl -s 120 -o /tmp/t.tr & sleep 4; kill %1; head -n 20 /tmp/t.tr"'

# 5. 面板波形文件
ssh remarkable 'journalctl -a | grep -iE "wbf|waveform" | tail -n 5'
```

---

## 8. 一句话给写代码的人

> 别再找 `MXCFB_SEND_UPDATE` 了，这台机器上没有它。
> 上屏 = 把 1404×1872 灰度图**转置压成 4bpp**、memcpy 进 `/dev/fb0` 的 17 页之一、
> 然后 `FBIOPAN_DISPLAY(yoffset = 页号×1408)`；驱动会在一帧后自动把 LCDIF 指回零页，
> 所以**每帧都要翻**，而且它成功时返回 49 不是 0。
> 真正难的那部分（波形→驱动序列）在官方 xochitl 的 `EPFramebufferSwtcon` 里，
> 波形数据是 `/usr/share/remarkable/320_R355_AF0A11_ED103TC2U2_VB3300-KCD_TC.wbf`。
