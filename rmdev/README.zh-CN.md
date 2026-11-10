# rmdev —— 按官方实现复刻的 reMarkable 2 笔记应用（开发中）

不是"另一个墨水屏笔记 app"，而是**照 xochitl 的逆向规格逐层重写**：
笔宽、压力、方向、落盘格式都对齐官方，砍掉订阅手写识别、遥测、EPUB、多语言、零售模式。

规格来自对官方 xochitl 的逆向（笔宽 / 压力 / 方向 / 落盘格式逐项对齐），
其中可公开的测量结论在 `../docs/MEASURED.md` 与 `../docs/FORMATS.md`。

> 说明：本文写于早期里程碑，进度表和目录树可能落后于代码；
> 最新功能清单与构建方式以顶层 `README.md` 为准。

---

## 当前进度

| 模块 | 状态 | 说明 |
|---|---|---|
| `pen/line.rs` | ✅ | `Point`(24 B) / `Line` / 23 个 `Tool` / 9 个 `Color`，数值与官方一致 |
| `pen/width.rs` | ✅ | 23 支笔的宽度公式，含 v2 压力重映射 / 平滑 / 阻尼 |
| `pen/stroke.rs` | ✅ | 落笔点不进 Line、speed=位移×缩放、压力=相邻均值+位移衰减、dir 两段平均 |
| `input/` | ✅ | evdev → 归一化坐标（轴交叉 + X 镜像）、轴量程走 `EVIOCGABS` |
| `fb.rs` | ✅ | rm2fb 帧缓冲 + mxcfb 局部刷新 |
| `canvas.rs` | ✅ | 逐点宽度胶囊光栅化 + 单层墨密度合成 |
| `chrome.rs` | ⚠️ | 开发用工具条（9 笔 + 3 档厚度 + 撤销），不是最终 UI |
| `.rm` 读写 | ❌ | 下一里程碑 |
| 页面 / 文档四件套 | ❌ | 下一里程碑 |
| 橡皮 / 选区 / 缩放 | ❌ | |
| AI / 同步 | ❌ | |

---

## 构建

```bash
# 交叉编译（armv7 + glibc，必须动态链接才能被 rm2fb 接管）
rustup target add armv7-unknown-linux-gnueabihf
cargo build --release --target armv7-unknown-linux-gnueabihf
# 链接器在 .cargo/config.toml 里指向 ~/bin/zigcc

# 单测（纯逻辑，本机跑）
cargo test
```

## 跑起来

```bash
scp target/armv7-unknown-linux-gnueabihf/release/rmdev remarkable:/home/root/
scp tools/inject_rmdev.py tools/devtest.sh remarkable:/home/root/
ssh remarkable 'sh /home/root/devtest.sh suite'   # 停 remux → 注入测试笔迹 → 抓帧缓冲 → 恢复
scp remarkable:/tmp/rmdev.fb /tmp/ && python3 tools/fb2png.py /tmp/rmdev.fb /tmp/dev.png
```

真机试笔：

```bash
ssh remarkable '/home/root/run.sh'     # 需要单独准备：停 remux + rm2fb-client 启动
```

* 退出：左下角连点 3 下
* 撤销：顶栏最左侧按钮
* 换笔/换厚度：点顶栏槽位

## 目录

```
src/
  pen/line.rs     Point / Line / Tool / Color —— 与 .rm 一一对应
  pen/width.rs    23 支笔的宽度公式（纯函数，最该单测）
  pen/stroke.rs   采样流 → Point 流
  input/evdev.rs  16 字节 input_event 读取 + EVIOCGABS
  input/decode.rs 一帧事件 → 归一化坐标（§4.1）
  fb.rs           rm2fb / mxcfb
  canvas.rs       光栅化 + 墨密度合成
  chrome.rs       开发工具条
tools/
  inject_rmdev.py 合成笔迹注入（注意内核会丢重复 ABS 值）
  devtest.sh      设备端一条龙测试
  fb2png.py       帧缓冲 dump → PNG
```

## 踩过的坑（都写进代码注释了）

1. **`input_event` 是 16 字节**，按 `(i32,u16,u16,i32)` 切片只有 12 字节 → 整个事件流错位，
   屏幕一动不动。现在按 16 字节逐个解。
2. **`bits_per_pixel` 不能当字节步长**，16bpp → bpp=2。用错只写 1/8 像素，屏幕毫无反应。
3. **落笔判定是 `BTN_TOUCH`，不是 `pressure>0`**——压力只影响宽度。
4. **`EVIOCGABS` 读出来 `p_max = 4095`**（不是 2047），注入脚本要按这个走。
5. 内核会**丢掉重复的 ABS 值**，注入时必须抖动。
6. `cargo test` 用的是 `src/lib.rs`，设备入口是 `src/main.rs`，两边共用同一份库。
