# 电源键 / 睡眠 / 锁屏

> 2026-11-08。**官方有、我们之前完全没有**的三块：电源键、睡眠、关机框。
> 全部按官方 QML 的语义实现，不是照截图猜的。

## 1. 官方怎么做的（证据）

素材还是从 `xochitl` 里解出来的 QML 树（QML 树未随仓库分发）。

| 文件 | 内容 |
|---|---|
| `res_06b4fb3.qml` | `PowerOffDialog`：电源图标 + `Do you want to turn off your reMarkable?` + `Turn off` / `Cancel` 两个方按钮 |
| `res_06548820.qml` | 主 Window 的 `Keys.onPressed / onReleased`（`Qt.Key_PowerOff`）与 `poweroffConfirmationTimer` |
| `res_06b2dac.qml` | `UnlockScreen`：`Enter passcode` + 4 个 ⌀60 圆环 + `Show` / `Hide` + 错误提示条 |
| `res_06b6225.qml` | `SleepScreen`：全屏 `/usr/share/remarkable/suspended.png`，加载失败才画 40 px 的 `reMarkable is sleeping` |
| `Values.qml`（`res_0705…`） | 版面常量：`navigatorSettingsItemSecondMargin` 200、`navigatorSidebarItemMargin` 22、`navigatorHeaderTitleMargin` 25、`iconSizeMedium` 40、`fontSizeH1/H2` 36/28 |

把这三段接起来就是官方的状态机：

```text
按下电源键 → poweroffConfirmationTimer.start()（interval: 3000）
  ├─ 3 s 内抬手 → timer.running == true → stop() → goToSleep(true) → BatteryManager.deepSleep()
  └─ 3 s 到点   → onTriggered → poweroffDialog.visible = true（手还按着，框已经出来了）
                 抬手时 timer 已停 → 什么都不做
```

`BatteryManager` 的字符串表（`xochitl` 里 `src/deviceinfo/src/batterymanager.cpp` 那一簇）
给出挂起方式：

```text
/sys/power/state              "mem" / "standby"
/sys/class/rtc/rtc0/wakealarm
Error while writing to /sys/power/state
Suspend when display not showing sleep? Should not happen
Failed to reset RTC wakealarm / Was not able to set RTC WakeAlarm value
Wake-up reason: RTC / Charger / Other        （靠 /proc/interrupts 的 rtc alarm / charger 计数差）
DisplayState: Normal / Standby / LightSleep / DeepSleep
```

也就是：**先把睡眠图刷上屏，再 `echo mem > /sys/power/state`**，醒来靠电源键
（`snvs-powerkey` 的 `power/wakeup` = enabled）或 RTC 闹钟。

## 2. 我们的实现

| 文件 | 职责 |
|---|---|
| `src/input/power.rs` | 电源键 evdev 线程。**按能力找设备**（谁的 `EVIOCGBIT(EV_KEY)` 有 `KEY_POWER` 116 就是谁），不写死 `event0`；`value=2`（自动重复）忽略 |
| `src/power.rs` | 手势状态机（纯逻辑，可单测）+ 挂起封装（`/sys/power/state`，`RM_SUSPEND_STATE` 可换路径） |
| `src/panels/sleepscreen.rs` | 睡眠图：`suspended.png` 拉伸铺满；读不到就走官方兜底文案 |
| `src/panels/poweroff.rs` | 关机框：版面全部由 `Values.qml` 的 anchor 关系算出来（`buttons(1404,1872) == (329,649,351,87) / (724,649,351,87)`） |
| `src/main.rs` | `on_power` / `sleep_now` / `wake` / `open_poweroff` / `tap_poweroff`，以及「睡着/关机框时吞掉笔和手指」 |
| `src/fb.rs` | `wait_idle()`：挂起前必须等面板线程把手上那一轮波形驱完 |

要点：

* **短按 3 s 内 = 睡**；睡着时短按 = 醒；**按住 3 s = 弹关机框**（边框弹出时机与官方
  一致：不等松手）。
* 睡着以后笔和手指全部丢弃（否则会在睡眠图上画画）；只有电源键还认。
* 挂起前 `flush_page_now()` 存盘 + `wait_idle(3 s)` 等波形走完 + 刷睡眠图，
  三步顺序不能换。
* 醒来：设了密码就进锁屏（官方 Security 页原话：*"You will be asked for your
  passcode when the device is turned on or awakened from sleep mode"*），
  没设密码就整页重画当前界面。
* **唤醒那一刻的按键要丢掉**：内核恢复后，把我们叫醒的那次按下/抬起还排在
  channel 里，不挡掉就会被判成"又按了一下"，屏幕闪一下就又睡了。
  实现是 `power_mute`（600 ms）。
* 深度睡眠返回 `Err`（内核不支持/被别的进程占着）就退化成"浅睡"：屏幕停在睡眠图，
  进程继续跑，按一下电源键就醒。**不会卡在半睡不醒的状态里**。
* `Turn off` 走 `poweroff` → `systemctl poweroff` 两条路（官方是 `Qt.quit()`，
  它一退系统就断电；我们是 systemd 里的普通服务，退出不会断电）。

锁屏版面（`draw_lock`，照 `res_06b2dac.qml` 量）：

```text
passCodeInputItem  top = 450、height = 190
  "Enter passcode"  top + 28，38 px 粗体
  圆圈行            label 底 + 36
    72 空白 | ⌀60 圆环 ×4（间距 25）| 72 宽的 Show / Hide（下划线）
  Show 时圆环里显示数字，否则填 ⌀56 实心点
通知条（Wrong passcode. Try again.）490×84，top = 640，带挂锁图标
数字键盘           贴底（y = 1872 - 40 - 4×110 - 3×24 = 1320）
```

## 3. 上机验收（真机，`rmdev-autostart`）

| 项 | 结果 |
|---|---|
| 电源键被认出来 | ✅ `[input] 电源键 /dev/input/event0` |
| 按住 3 s | ✅ `[power] 按住 3 s → 关机框`，屏上逐字对得上（见 `main.rs` 的 `poweroff` 预览） |
| 点 Cancel | ✅ 回库界面，整页重画 |
| 短按 | ✅ `[power] 短按 152ms → 睡` |
| 睡眠图 | ✅ `[sleep] 睡眠图 suspended.png`，官方那张衬线字 1:1 |
| 真挂起 | ✅ `[sleep] 挂起到内存（/sys/power/state ← mem）…`，dmesg `PM: suspend entry (deep)` |
| 唤醒 | ✅ `[sleep] 醒了（挂了 1.7 s）`，醒来整页重画 |
| 有密码时唤醒 | ✅ 直接进锁屏（"Enter passcode" + 4 个空圆环） |
| 输 1234 解锁 | ✅ `[lock] 解锁` |
| 删掉密码文件 | ✅ `[lock] 密码文件已删除，解锁`（恢复后路还在） |

## 4. 两个已知问题（都记着，没糊过去）

### 4.1 插着 USB 时睡不住

挂着 USB 的时候，`PM: suspend exit` 在 **0.9~4.3 s** 后就发生（dmesg 里紧跟着
`brcmfmac: brcmf_sdio_bus_rxctl: resumed on timeout`）——USB 主机/充电器那一路
是唤醒源，一有活动就把设备叫醒。**拔掉 USB 才会真睡**，这一点需要在真机上确认
（我这边所有操作都靠 USB 网口，一拔就失联）。

顺带：会话里我设过 `rtc wakealarm` 防失联（`echo +240 > /sys/class/rtc/rtc0/wakealarm`），
它同时也是个唤醒源；官方只用它做定时同步。**别忘了 `echo 0 >` 清掉**。

### 4.2 整屏刷新慢（内容相关）

同一块 1404×1872 GL16：

| 内容 | 耗时 | 编码 | 帧周期 | 超帧 |
|---|---|---|---|---|
| 关机框（大片纯白） | **548 ms** | 331 ms | 11.8 ms | 0/45 |
| 睡眠图（细衬线字） | **1100 ms** | 751 ms | 23.8 ms | 45/45 |

`src/cpu.rs` 会在大块刷新时把 `scaling_min_freq` 顶到最高（`提频 true` 能确认生效），
但**对睡眠图这种内容没用**（16.4 ms/相位 vs 提频前的 17 ms）——瓶颈在内存带宽
（每相位读写 img/prv/编码表约 10 MB），不在主频。关机框快是因为大片同色命中了
「8 lane 同码」快路径。

要再快得改成"按 cell 只编变化的列"或上 NEON，跟 `PANEL.md` §「还没做」里的
两段式/流式那条是同一类活。**先记着**，不影响功能正确性。

## 5. 调试开关

| 变量 | 作用 |
|---|---|
| `RM_AUTO_SLEEP_SECS` | Auto sleep 的空闲阈值（默认 1200 = 官方 20 分钟；0 = 关） |
| `RM_SUSPEND_STATE` | 换 `/sys/power/state` 的路径（本机测试用） |
| `RM_SLEEP_IMG` | 换睡眠图（本机渲染预览用） |
| `kill -USR1 <pid>` | 把当前 UI 平面 dump 成 `/tmp/rmdev.shot.pgm`（`tools/shot.sh` 用它截图） |

## 6. 自动睡眠

官方 Battery settings 的 `Auto sleep`（`Settings.idleSuspendDelay`）= 20 分钟无输入就睡。
我们照做，但**默认关**（`prefs.auto_sleep` 默认 false，和设置页那个开关一致）：
现在挂起还没在"拔掉 USB"的条件下验证过，默认开着的话，用户放着不管二十分钟
设备就睡过去、而唤醒路径只在 USB 上验过——太冒险。确认没问题后把
`settings/prefs.rs` 里 `auto_sleep` 的默认值改成 `true` 就是官方行为。
