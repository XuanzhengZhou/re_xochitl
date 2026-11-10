# 「屏幕卡死但系统活着」的根因与修复

> **2026-11-07 更新：这条链路已经被彻底拆掉了。**
> `src/panel/` 里是纯 Rust 的面板驱动（自己解析 `.wbf`、编码波形、写 LCDIF），
> rmdev 现在不启动 xochitl、不加载 rm2fb client，直接写 `/dev/fb0`。
> 下面这份复盘保留下来，是因为它解释了**为什么当初必须那么小心**；
> 新的架构见 `PANEL.md`。

## 1. 症状

- 屏幕停在一帧，怎么点都不动；
- `ssh` 正常，`uptime`、`free` 都正常，负载 0.0x；
- `/tmp/rmdev.log` 停在一条输入事件上，之后什么都没有；
- `ps` 里 rmdev 状态 `S`（可中断睡眠），不是 `D`；
- `/dev/shm/swtfb.01` 的 mtime 停在某一刻再也不动。

**关键反直觉点**：我们的程序以为自己在正常刷屏。`refresh()` 一律返回成功，
没有一次 ioctl 报错，日志里连 `[fb] ioctl 后 r=-1` 都没有。

## 2. rm2fb 的架构

```
          ┌──────────────────────────────┐
client A ─┤  /dev/shm/swtfb.01（共享内存）├─ server ─→ EPDC（唯一真正写面板的）
client B ─┤  （1404×1872 16bpp 的假 fb）  │   ↑
          └──────────────────────────────┘   └─ 官方 xochitl + librm2fb_server.so.1
```

- 只有 **server** 能写 EPDC。rM2 的面板不是 16bpp RGB565，`rm2fb` 负责格式转换，
  所以**绕不开**（试过直接 `/dev/fb0`，几何不对）。
- 其它进程把帧写进共享内存，由 server 刷到面板。

## 3. 根因：一个错误的存活判据

Toltec 的 `/opt/etc/xochitl.env.d/rm2fb-preload.env`：

```bash
if [[ -f /dev/shm/swtfb.01 ]]; then
    export LD_PRELOAD="$LD_PRELOAD:/opt/lib/librm2fb_client.so.1"
else
    echo "rm2fb server is not running: starting without rm2fb client"
fi
```

**它用「文件存不存在」判断 server 在不在，可这个文件是持久的。**

server 一死，文件还在 → 之后**每一个**新起的进程都只加载 client 库 →
它们写共享内存永远"成功" → **但没有任何东西把帧刷到 EPDC**。

这就是「屏幕冻结而进程一切正常」的全部原因。

## 4. server 为什么会死

两条独立的路：

### 4.1 `rm2fb.service` 不会自愈

```
Nov 07 04:52:46  xochitl[193]: STARTING RM2FB
Nov 07 05:29:42  systemd: rm2fb.service: Succeeded.      ← 这里就没了
```

`Restart=on-failure`，而 xochitl 收到 SIGTERM 会**正常退出（exit 0）** →
systemd 判定 `Succeeded`，**不会重启** → server 永久消失。

### 4.2 部署脚本把它杀掉

`tools/deploy.sh` 原来自带一段"冲 EPDC 队列"的动作：

```sh
systemctl stop remux
start remux   # 它会拉起 app，而那个 app 可能正充当 server
stop remux    # ← 把 server 一起带走
```

journal 里对得上：

```
06:15:49  remux[2622]: Sourcing /opt/etc/xochitl.env.d/rm2fb-preload.env
06:15:50  remux[2622]: REPLACING THE IMAGE with shared memory    ← 它成了 server
06:15:50  remux[2622]: EPD platform plugin loaded!
06:15:52  systemd: remux.service: Main process exited, status=15 ← 2 秒后被杀
06:15:57  pt_i2c_adapter: PT_GET_HID_DESCRIPTOR_TIMEOUT=2500
...       屏幕从此 17 分钟没有刷新
```

那行 `PT_GET_HID_DESCRIPTOR_TIMEOUT` 是**结果**不是原因：触摸控制器被复位，
而屏幕已经不刷新了。

## 5. 修复

### 5.1 `tools/deploy.sh` 重写

只做三件事：**探测 server → 用 launcher 只关我们自己的 app → 重新拉起**。
非必要**绝不碰** `remux` / `xochitl` / `rm2fb.service`。

### 5.2 `tools/rm2fb-guard.sh`（新增）

按 `comm == rm2fb-server` 扫描 `/proc`，不信那个共享内存文件：

```sh
for p in /proc/[0-9]*; do
    case "$(cat "$p/comm" 2>/dev/null)" in
        rm2fb-server*) exit 0 ;;
    esac
done
exit 1
```

### 5.3 `tools/rmdev.sh`

启动前跑 guard，没有 server 就 `systemctl start rm2fb` 把它拉回来，
最多等 12 秒；`exit(3)`（首帧看门狗）时重试。

**不能自己当 server**：试过 `LD_PRELOAD=librm2fb_server.so.1 ./rmdev`，
**实测 SIGSEGV（rc=139）**——那个库是 Qt 的 QPA 平台插件，非 Qt 程序加载必崩。

### 5.4 xochitl 的 HOME 隔离（重要副作用）

为了救 server 去 `systemctl start rm2fb`，会让官方 xochitl 启动，而它会
**扫描并重写** `~/.local/share/remarkable/xochitl` 里它不认识的 `.content`：

```diff
- "fileType": "txt"      ← 我们写的
+ "fileType": "notebook" ← 被 xochitl 改的
```

结果所有电子书变成空白笔记本（缩略图全白、页数变 1）。修法：

- `/etc/systemd/system/rm2fb.service.d/isolate-home.conf`
  → `Environment="HOME=/home/root/xochitl-sandbox"`
- `/opt/etc/draft/xochitl.draft` 的 `call=` 指向 `/home/root/dev/xochitl-isolated.sh`
  （设空 HOME 再 exec）

我们的程序用**绝对路径** `DEFAULT_ROOT` 读库，不受 HOME 影响。

## 7. 复现与验证

```sh
# 制造故障（会掉屏幕，能靠下面这条恢复）
kill -TERM <rm2fb-server 的 pid>
/home/root/dev/rm2fb-guard.sh; echo $?     # → 1

# 恢复
setsid sh /home/root/dev/rmdev.sh &
tail -n 10 /home/root/dev/rmdev.log
# 应该看到：没有 framebuffer server，拉起 rm2fb.service
#           然后 [fb] ioctl 后 r=0
```
