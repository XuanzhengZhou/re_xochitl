# Power Key / Sleep / Lock Screen

> English translation of [`../POWER.md`](../POWER.md) (Chinese original).

> Three pieces the official build has and the rewrite previously lacked entirely: the power
> key, sleep, and the poweroff dialog. All three follow the semantics of the official QML,
> not guesses made from screenshots.

## 1. How the official build does it (evidence)

The material is again the QML tree extracted from `xochitl` (the QML tree is not
distributed with this repository).

| File | Content |
|---|---|
| `res_06b4fb3.qml` | `PowerOffDialog`: power icon + `Do you want to turn off your reMarkable?` + `Turn off` / `Cancel` square buttons |
| `res_06548820.qml` | the main Window's `Keys.onPressed / onReleased` (`Qt.Key_PowerOff`) and `poweroffConfirmationTimer` |
| `res_06b2dac.qml` | `UnlockScreen`: `Enter passcode` + 4 × ⌀60 rings + `Show` / `Hide` + error bar |
| `res_06b6225.qml` | `SleepScreen`: full-screen `/usr/share/remarkable/suspended.png`; only if loading fails does it draw the 40 px `reMarkable is sleeping` |
| `Values.qml` (`res_0705…`) | layout constants: `navigatorSettingsItemSecondMargin` 200, `navigatorSidebarItemMargin` 22, `navigatorHeaderTitleMargin` 25, `iconSizeMedium` 40, `fontSizeH1/H2` 36/28 |

Wire these three together and you get the official state machine:

```text
power key pressed → poweroffConfirmationTimer.start() (interval: 3000)
  ├─ released within 3 s → timer.running == true → stop() → goToSleep(true) → BatteryManager.deepSleep()
  └─ 3 s elapsed         → onTriggered → poweroffDialog.visible = true (finger still down, the dialog is already up)
                  release with the timer already stopped → nothing happens
```

`BatteryManager`'s string table (the cluster under `src/deviceinfo/src/batterymanager.cpp`
in `xochitl`) gives the suspend mechanism:

```text
/sys/power/state              "mem" / "standby"
/sys/class/rtc/rtc0/wakealarm
Error while writing to /sys/power/state
Suspend when display not showing sleep? Should not happen
Failed to reset RTC wakealarm / Was not able to set RTC WakeAlarm value
Wake-up reason: RTC / Charger / Other        (via the rtc alarm / charger count delta in /proc/interrupts)
DisplayState: Normal / Standby / LightSleep / DeepSleep
```

In other words: **push the sleep image to the panel first, then `echo mem > /sys/power/state`**;
wake-up comes from the power key (`snvs-powerkey`'s `power/wakeup` = enabled) or the RTC alarm.

## 2. Implementation

| File | Responsibility |
|---|---|
| `src/input/power.rs` | power-key evdev thread. **Finds the device by capability** (whichever node has `KEY_POWER` 116 in its `EVIOCGBIT(EV_KEY)`), never a hardcoded `event0`; `value=2` (autorepeat) is ignored |
| `src/power.rs` | gesture state machine (pure logic, unit-testable) + suspend wrapper (`/sys/power/state`, path overridable via `RM_SUSPEND_STATE`) |
| `src/panels/sleepscreen.rs` | sleep image: `suspended.png` stretched to fill; if it cannot be read, falls back to the official fallback text |
| `src/panels/poweroff.rs` | poweroff dialog: the entire layout is computed from the anchor relations in `Values.qml` (`buttons(1404,1872) == (329,649,351,87) / (724,649,351,87)`) |
| `src/main.rs` | `on_power` / `sleep_now` / `wake` / `open_poweroff` / `tap_poweroff`, plus "swallow pen and finger while asleep or while the poweroff dialog is up" |
| `src/fb.rs` | `wait_idle()`: before suspending, must wait for the panel thread to finish the waveform batch in flight |

Key points:

* **Short press within 3 s = sleep**; short press while asleep = wake; **holding for 3 s =
  the poweroff dialog appears** (at the same moment as the official build: it does not wait
  for release).
* Once asleep, every pen and finger event is discarded (otherwise you would draw on the
  sleep image); only the power key is still recognised.
* Before suspend, `flush_page_now()` to persist + `wait_idle(3 s)` to let the waveform
  finish + push the sleep image. The order of these three steps must not change.
* On wake: with a passcode set, go to the lock screen (the official Security page says
  verbatim: *"You will be asked for your passcode when the device is turned on or awakened
  from sleep mode"*); without one, redraw the current screen in full.
* **The key event at the moment of wake must be dropped**: after the kernel resumes, the
  press/release that woke us is still queued in the channel, and if it is not blocked it is
  judged as "pressed again" — the screen flashes and the device goes back to sleep.
  Implemented as `power_mute` (600 ms).
* If deep sleep returns `Err` (not supported by the kernel / held by another process), fall
  back to "light sleep": the screen stays on the sleep image, the process keeps running, and
  one press of the power key wakes it. **It never gets stuck half-asleep.**
* `Turn off` takes both routes, `poweroff` → `systemctl poweroff` (the official build calls
  `Qt.quit()`, and the system powers off as soon as it exits; the rewrite is an ordinary
  systemd service, so exiting does not cut power).

Lock-screen layout (`draw_lock`, measured against `res_06b2dac.qml`):

```text
passCodeInputItem  top = 450, height = 190
  "Enter passcode"  top + 28, 38 px bold
  ring row          label bottom + 36
    72 blank | ⌀60 rings ×4 (spacing 25) | Show / Hide, 72 wide (underlined)
  digits shown inside the rings when Show is on, otherwise filled ⌀56 dots
notification bar (Wrong passcode. Try again.) 490×84, top = 640, with lock icon
numeric keypad     bottom-aligned (y = 1872 - 40 - 4×110 - 3×24 = 1320)
```

## 3. On-device acceptance (real hardware, `rmdev-autostart`)

| Item | Result |
|---|---|
| Power key recognised | ✅ `[input] power key /dev/input/event0` |
| Hold 3 s | ✅ `[power] held 3 s → poweroff dialog`; the on-screen text matches character for character (see the `poweroff` preview in `main.rs`) |
| Tap Cancel | ✅ back to the library view, full-page redraw |
| Short press | ✅ `[power] short press 152ms → sleep` |
| Sleep image | ✅ `[sleep] sleep image suspended.png`, the official serif artwork 1:1 |
| Real suspend | ✅ `[sleep] suspending to memory (/sys/power/state ← mem)…`, dmesg `PM: suspend entry (deep)` |
| Wake | ✅ `[sleep] awake (was suspended 1.7 s)`, full-page redraw on wake |
| Wake with a passcode set | ✅ goes straight to the lock screen ("Enter passcode" + 4 empty rings) |
| Unlock with 1234 | ✅ `[lock] unlocked` |
| Delete the passcode file | ✅ `[lock] passcode file deleted, unlocked` (the recovery path still works) |

Log lines in this document are quoted in translation; `rmdev` prints these messages in Chinese.

## 4. Two known issues (both tracked, not papered over)

### 4.1 It will not stay asleep with USB plugged in

With USB attached, `PM: suspend exit` happens after **0.9–4.3 s** (in dmesg immediately
followed by `brcmfmac: brcmf_sdio_bus_rxctl: resumed on timeout`) — the USB host/charger
path is a wake source, and any activity wakes the device. **It only really sleeps once USB
is unplugged**, and that still needs confirming on the device (the working setup reaches the
device only over the USB network interface, so unplugging it also drops the link).

Aside: an `rtc wakealarm` set as a safety net against losing the link
(`echo +240 > /sys/class/rtc/rtc0/wakealarm`) is itself a wake source as well; the official
build uses it only for scheduled sync. **Do not forget to clear it with `echo 0 >`.**

### 4.2 Full-screen refresh is slow (content-dependent)

Same 1404×1872 GL16 panel:

| Content | Time | Encode | Frame period | Overframes |
|---|---|---|---|---|
| Poweroff dialog (large areas of pure white) | **548 ms** | 331 ms | 11.8 ms | 0/45 |
| Sleep image (thin serif text) | **1100 ms** | 751 ms | 23.8 ms | 45/45 |

`src/cpu.rs` pins `scaling_min_freq` to the maximum for large refreshes (`boost true`
confirms it took effect), but **it does nothing for content like the sleep image**
(16.4 ms/phase vs 17 ms before the boost) — the bottleneck is memory bandwidth (each phase
reads/writes img/prv/encode tables, about 10 MB), not clock frequency. The poweroff dialog
is fast because large areas of one colour hit the "8 lanes, same code" fast path.

Going faster would mean encoding only the columns that change per cell, or bringing in NEON —
the same class of work as the two-stage / streaming item under "Not done yet" in
[`../PANEL-DRIVER.md`](../PANEL-DRIVER.md) (referred to as `PANEL.md` in earlier notes).
**Noted for later**; it does not affect functional correctness.

## 5. Debug switches

| Variable | Effect |
|---|---|
| `RM_AUTO_SLEEP_SECS` | idle threshold for auto sleep (default 1200 = the official 20 minutes; 0 = off) |
| `RM_SUSPEND_STATE` | overrides the `/sys/power/state` path (for host-side testing) |
| `RM_SLEEP_IMG` | overrides the sleep image (for host-side render previews) |
| `kill -USR1 <pid>` | dumps the current UI plane to `/tmp/rmdev.shot.pgm` (`tools/shot.sh` uses it for screenshots) |

## 6. Auto sleep

The official Battery settings' `Auto sleep` (`Settings.idleSuspendDelay`) = sleep after
20 minutes without input. The rewrite does the same but **leaves it off by default**
(`prefs.auto_sleep` defaults to false, matching the switch on the settings page): suspend
has not yet been verified with USB unplugged, and the wake path has only been tested over
USB, so enabling it by default would let a device left alone for twenty minutes go to sleep
with no proven way back — too risky. Once that is confirmed, setting the default of
`auto_sleep` in `settings/prefs.rs` to `true` gives the official behaviour.
