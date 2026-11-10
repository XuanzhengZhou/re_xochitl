# 开机启动链（remux 拉起我们的界面）

2026-11-08 起，设备开机**只跑一个 UI：我们的 `rmdev`**，由官方启动器 `remux` 直接拉起，
不再跑 `xochitl`。

## 现状（**不再经过 remux**）

```text
systemd → rmdev.service (enabled)
            └─ ExecStart=/home/root/rmdev          ← 直接起，没有中间人
                 HOME=/home/root
                 RM_AUTO_SLEEP_SECS=0             ← 仅开发期
```

**为什么把 remux 摘出去**（真机踩到的三件事，全都会表现成"开机白屏/非常卡/灰屏"）：

1. `remux.service` 带 `Restart=on-failure` + `StartLimitInterval=30` + `StartLimitBurst=5`。
   rmdev 在开机阶段连挂 5 次，systemd 会把**整个 remux 单元**停掉——屏停在最后一帧，
   而且**再也不会自己起来**（只能手动重启）。换成 `rmdev.service` 后
   `StartLimitIntervalSec=0`，崩多少次都一直重试。
2. remux 是"多任务启动器"，它自己也碰 framebuffer/EPDC（默认套 `librm2fb_client`）。
   我们的面板驱动是自己写 EPDC 的，两个写者交替推波形 = 花屏/灰屏/"叠了两层"。
3. 多一层 `xochitl.draft` → shell → `exec` 的链子，就多一层 `LD_PRELOAD`/`HOME` 的坑
   （老脚本得手动 `unset LD_PRELOAD` 才不抢 ioctl）。

现在的开机链：

```text
systemd → rmdev.service                  ← enabled
          rmdev-wifi.service             ← 起 wpa_supplicant + udhcpc + 校园网门户
          tailscaled.service             ← userspace TUN + socks5 :1055
```

- `remux.service`：**已 disable**（文件还在，回滚用）。
- `xochitl.service` / `draft.service` / `rm2fb.service`：都是 disabled。
- `rmdev-autostart.service`：仍在但 disabled，是更老的"launcherctl start-app"方案。
- `/opt/etc/draft/xochitl.draft` 的 `call=` 现在**不会被任何人读**（remux 不跑），留着当回滚路径。

### 部署（不经过 remux 了）

```sh
cargo build --release --target armv7-unknown-linux-gnueabihf
scp target/armv7-unknown-linux-gnueabihf/release/rmdev rm2:/home/root/rmdev.new
ssh rm2 'systemctl stop rmdev; mv /home/root/rmdev.new /home/root/rmdev;
         chmod +x /home/root/rmdev; systemctl start rmdev'
```

> 覆盖正在跑的可执行文件是 ETXTBSY，所以必须"停 → mv → 起"。
> `tools/deploy.sh` 里走 remux 的那套（`systemctl restart remux`）已经过时，
> 下次改它的时候换掉。

### 验收

```sh
ssh rm2 'systemctl is-active rmdev; journalctl -u rmdev -b | grep -c "Started rmdev"'
# active / 1        ← 一个实例、没有重启循环
ssh rm2 'for p in /proc/[0-9]*; do tr "\0" " " < $p/cmdline; echo; done | grep -cE "rmdev|remux"'
# 1（只有 rmdev，没有 remux）
```

两次冷启动都是 40 s 内出文档库，屏上正常，没有白屏/灰屏。

## 联网

`xochitl` 一停，WiFi 就没人管了：`wpa_supplicant` 不起、休眠后 `brcmfmac` 会挂死
（`dmesg` 里 `failed backplane access over SDIO`）。所以补了两个系统服务：

| 服务 | 干什么 |
|---|---|
| `rmdev-wifi.service` → `/home/root/dev/rmdev-wifi.sh` | 起 `wpa_supplicant`、`udhcpc` 要地址、Campus-WLAN 自动跑 `campus_wlan_login.sh --auto`；连续 3 次关联不上且 `dmesg` 里有 SDIO 错误就 `rmmod/insmod brcmfmac`；用户用 `rfkill block` 关掉 WiFi 时安静待着 |
| `tailscaled.service`（改过） | 设备没有内核 TUN（`tun.ko` 不存在），所以是 `--tun=userspace-networking --socks5-server=127.0.0.1:1055`。原来写成 `After=network-online.target`，开机常起不来，**改成 `network.target` 才对** |

设备出网路径：

```
rmdev ──http://100.64.0.1:8787──▶ socks5://127.0.0.1:1055 ──▶ tailscaled(userspace) ──▶ home-server
```

`server.json` 里必须有 `"proxy": "socks5://127.0.0.1:1055"`（`net.rs` 的 `open_stream`/`request`/
`put_blob` 都吃这个字段）。没它就只能直连——在受限网络里连不到自建服务器。

**验收**（拔掉 USB 也能测）：`systemctl is-active rmdev-wifi tailscaled` 都是 active；
`iw dev wlan0 link` 有 SSID；`/opt/bin/tailscale status` 里设备不是 offline。

## 两个坑（踩过，别再来一次）

1. **`LD_PRELOAD`**：`remux.service.d/rm2fb-preload.conf` 给整个服务树设了
   `LD_PRELOAD=/opt/lib/librm2fb_client.so.1`。我们的面板驱动自己玩 EPDC，
   套上这层 shim 会在 journal 里刷 `UNHANDLED IOCTL 17926`（= `0x4606`），
   还可能把 `/dev/fb0` 的 ioctl 抢走。启动脚本里 `unset LD_PRELOAD` 即可。
2. **`HOME`**：remux 继承的环境里 `HOME` 不保证是 `/home/root`，而
   `~/.config/rmdev`（设置、锁屏密码、配对令牌）都在那儿。启动脚本里显式 `export HOME=/home/root`。

## remux 的手势（为我们的页滑块让路）

`remux` 默认会在**右边缘**（zone `1263 0 1404 1872`）和两个下角装
`direction=up` 的 swipe 手势，用来拉起它自己的 app launcher。文档页滑块也吃右边缘，
会互相抢，所以写了：

```ini
# /home/root/.config/remux/remux.conf
launch_gesture=
back_gesture=
```

journal 里从 `ADDED SWIPE GESTURE` 变成 `RESETTING MT GESTURES` + `SETTING UP GESTURES`
就说明生效了。想恢复官方手势：把这两行删掉再 `systemctl restart remux`。

## 回滚（三步，任何一步都独立可用）

```sh
# 1) 回到 systemd 直起
ssh remarkable 'systemctl disable --now remux; systemctl enable --now rmdev-autostart'

# 2) 回到 remux→xochitl（沙箱 HOME，别让它扫我们的库）
ssh remarkable 'cp /home/root/xochitl.draft.orig /opt/etc/draft/xochitl.draft'

# 3) 恢复 remux 默认手势
ssh remarkable 'rm /home/root/.config/remux/remux.conf; systemctl restart remux'
```

备份文件：`/home/root/xochitl.draft.orig`、`/home/root/xochitl.service.orig`。

## 为什么不让 xochitl 在后台跑

以前 remux 拉的是一个 `xochitl-isolated.sh`（沙箱 `HOME`），
它唯一作用是"别让 remux 觉得没 app"。代价：
- 白吃内存/CPU；
- 它照样打开 `/dev/input/event0/1/2`（按钮/笔/触摸）和 telemetry 落盘；
- 早期它扫我们库时**重写**过不认识的 `.content`（把 `fileType` 从 txt/epub 改成 notebook），
  这就是库里两条坏条目的来源。

现在它彻底不启动，库不会再被第三方改写。

## 睡眠/唤醒的远程测试注意事项

电源键短按会真的把整机挂起（`/sys/power/state=mem`），**USB 网卡也跟着挂起**，
此时 ssh 必然断，RTC 闹钟不保证能叫醒（`/sys/class/rtc/rtc0/device/power/wakeup` 是 enabled，
但实测 300 s 闹钟没把 USB 拉起来）。所以：

- 远程测睡眠 = 只能请人按一下电源键；或者
- 短闹钟 `echo +20 > /sys/class/rtc/rtc0/wakealarm` 后立刻注入，唤醒后**重新插拔 USB 线**
  再连（gadget 可能不重新枚举）。

## 白屏是怎么回事 + 看门狗

我们的程序收到 `SIGTERM` 会**清屏一帧再退出**（P6 加的行为，避免面板停在半张波形）。
而 `remux` 只在启动时拉起一次 App，**不会**自动重启。所以任何把 rmdev 杀掉的操作
（`systemctl restart remux`、`killall rmdev`、崩溃）只要没紧接着重新拉起，屏幕就停在白屏。

2026-11-08 加了兜底：

```
rmdev-guard.timer  (enabled, 每 30 s)
  └─ rmdev-guard.service (oneshot)
       sleep 5；pidof rmdev 有就退出，没有就 systemctl restart remux
```

少了 5 s 是为了避开 `deploy.sh` 的 `killall → mv → restart remux` 窗口，别在拷贝途中抢先启动旧二进制。
手动恢复也很简单：`ssh remarkable 'systemctl restart remux'`。

## 面板 INIT 不能跳

冷启动时 `Panel::open()` 找不到面板状态就会跑一遍 111 帧的 INIT 序列。**这一段不能省**：

| 做法 | 结果 |
|---|---|
| 跑 INIT | 后续帧正常显示（库界面） |
| 跳过 INIT（`RM_PANEL_INIT=0`） | 相位算出来了、`pan#1..6` 也发出去了，但**屏上一个像素都不变**，停在官方的 `reMarkable is starting` |

所以 INIT 不只是"建立基线"，它是后面驱动可见的前提（大概是它把 EPDC 的波形/极性
状态初始化成我们编码器假设的那一套）。默认必须开。

### 由此带出的两个已知问题

1. **状态文件在 `/tmp`**（`panel::state_path()`，默认 `/tmp/rm2epd.state`）。
   `/tmp` 是内存盘，**每次重启都清空** → 每次冷启动 `fresh = true` → 每次都跑 INIT。
   跑完面板落在哪个灰度取决于重启前屏上是什么，于是"有时正常、有时库界面淡灰像残影"。
   想挪到 `/home/root` 之前必须先搞清上面那条（否则 `fresh = false` → 不跑 INIT → 全黑屏）。
2. **第二次整屏刷新不出帧**：加了开机进度条（`boot_progress`）之后实测，第一次
   全屏刷新会出 `pan#1..6`，紧接着的第二次全屏刷新**一条 `pan#` 都没有**。
   启动进度条就是被这个卡住的（屏上停在白/条上）。函数先留着，标了 `dead_code`。

### 恢复手法

面板被留在坏状态（白屏 / 淡灰 / 官方 splash）时：

```sh
ssh rm2 'rm -f /tmp/rm2epd.state; systemctl restart rmdev'
```

删掉状态文件 → `fresh = true` → 重跑 INIT → 画面回来。

## 间歇性白屏的真凶：开机时槽位判错

```
[panel] LCDIF 正在扫槽 0（CUR_BUF-SLOT 判出来的）   ← 开机那一刻判错了
[panel] 槽位纠正：0 → 1（pan 之后按 CUR_BUF 重探）   ← pan 之后真相是 1
```

LCDIF 是**双缓冲**（槽 0 / 槽 1）。我们按 `CUR_BUF` 寄存器判断"面板正在扫哪一槽"，
但**开机那一刻 LCDIF 还没被程序化**，`CUR_BUF` 读出来是 0 或垃圾值，
`probe_slot` 就退回槽 0 —— 如果面板其实在扫槽 1，我们所有帧都喷到了没人看的那块
画布上：

* 整屏帧"发出去了但屏上一个像素都不变" → **白屏**（停在官方 splash）；
* 局部刷新只是把旧画面"洗"成灰 → **淡灰/残影**；
* 判对的时候一切正常 → **间歇性**。

### 修法（`panel::emit` / `raw::reprobe`）

1. **整屏刷新写两个槽**（`write_slot_at(0/1)`）：多 1.4 MB DRAM 流量（约 5 ms），
   换来"无论面板扫哪一槽都看得见"，开机第一帧必定落地。
2. **pan 之后等一帧再重探 `CUR_BUF`**：这时候内核一定已经把 `CUR_BUF` 指到新槽，
   读到的才是真的；读出来不一样就纠正 `slot`，之后所有局部刷新都写对槽。
   （刚 `pan` 完立刻读会读到旧值，反而把槽改错——所以顺序必须是 pan → 等一帧 → 重探。）

