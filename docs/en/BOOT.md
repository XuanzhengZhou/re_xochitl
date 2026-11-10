# Boot Chain (remux brings up the UI)

> English translation of [`../BOOT.md`](../BOOT.md) (Chinese original).

At boot the device runs **only one UI: `rmdev`**, launched directly by the official
launcher `remux`; `xochitl` no longer runs.

## Current state (**remux is out of the path**)

```text
systemd → rmdev.service (enabled)
            └─ ExecStart=/home/root/rmdev         ← started directly, no middleman
                 HOME=/home/root
                 RM_AUTO_SLEEP_SECS=0             ← development only
```

**Why remux was removed** (three things hit on real hardware; all of them show up as
"white screen / very sluggish / grey screen at boot"):

1. `remux.service` carries `Restart=on-failure` + `StartLimitInterval=30` + `StartLimitBurst=5`.
   If rmdev crashes 5 times in a row during boot, systemd stops the **whole remux unit** — the
   panel freezes on the last frame and **never comes back by itself** (manual reboot only).
   `rmdev.service` instead sets `StartLimitIntervalSec=0`, so it keeps retrying however often it
   crashes.
2. remux is a "multi-app launcher" and it also touches the framebuffer/EPDC (it loads
   `librm2fb_client` by default). The panel driver here programs the EPDC itself, so two writers
   alternating waveform pushes = corrupted screen / grey screen / "two layers stacked".
3. Every extra layer of the `xochitl.draft` → shell → `exec` chain adds another
   `LD_PRELOAD`/`HOME` trap (the old script had to `unset LD_PRELOAD` by hand so it would not
   fight over the ioctl).

The boot chain now:

```text
systemd → rmdev.service                  ← enabled
          rmdev-wifi.service             ← wpa_supplicant + udhcpc + campus portal
          tailscaled.service             ← userspace TUN + socks5 :1055
```

- `remux.service`: **disabled** (the file is still there, kept for rollback).
- `xochitl.service` / `draft.service` / `rm2fb.service`: all disabled.
- `rmdev-autostart.service`: still present but disabled; it is the older "launcherctl start-app"
  approach.
- `call=` in `/opt/etc/draft/xochitl.draft` is now **read by nobody** (remux does not run); it is
  kept as a rollback path.

### Deployment (no longer through remux)

```sh
cargo build --release --target armv7-unknown-linux-gnueabihf
scp target/armv7-unknown-linux-gnueabihf/release/rmdev rm2:/home/root/rmdev.new
ssh rm2 'systemctl stop rmdev; mv /home/root/rmdev.new /home/root/rmdev;
         chmod +x /home/root/rmdev; systemctl start rmdev'
```

> Overwriting a running executable yields ETXTBSY, so the order must be stop → mv → start.
> The remux path in `tools/deploy.sh` (`systemctl restart remux`) is obsolete and needs
> replacing.

### Verification

```sh
ssh rm2 'systemctl is-active rmdev; journalctl -u rmdev -b | grep -c "Started rmdev"'
# active / 1        ← one instance, no restart loop
ssh rm2 'for p in /proc/[0-9]*; do tr "\0" " " < $p/cmdline; echo; done | grep -cE "rmdev|remux"'
# 1 (only rmdev, no remux)
```

Both cold boots reached the document library within 40 s, the panel looked normal, no white or
grey screen.

## Networking

Once `xochitl` is stopped, nothing manages WiFi any more: `wpa_supplicant` does not start, and
after suspend `brcmfmac` hangs (`failed backplane access over SDIO` in `dmesg`). So two system
services were added:

| Service | What it does |
|---|---|
| `rmdev-wifi.service` → `/home/root/dev/rmdev-wifi.sh` | Starts `wpa_supplicant`, gets an address via `udhcpc`, runs `campus_wlan_login.sh --auto` on Campus-WLAN; after 3 failed association attempts in a row with SDIO errors in `dmesg`, does `rmmod/insmod brcmfmac`; stays quiet when the user turns WiFi off with `rfkill block` |
| `tailscaled.service` (modified) | The device has no kernel TUN (`tun.ko` does not exist), so it runs `--tun=userspace-networking --socks5-server=127.0.0.1:1055`. It was originally written as `After=network-online.target` and often failed to start at boot; **`network.target` is the correct setting** |

Device egress path:

```text
rmdev ──http://100.64.0.1:8787──▶ socks5://127.0.0.1:1055 ──▶ tailscaled(userspace) ──▶ home-server
```

`server.json` must contain `"proxy": "socks5://127.0.0.1:1055"` (`open_stream`/`request`/
`put_blob` in `net.rs` all consume this field). Without it only a direct connection is possible —
and behind a restrictive network the server is unreachable.

**Verification** (also works with the USB cable unplugged): `systemctl is-active rmdev-wifi tailscaled`
both active; `iw dev wlan0 link` shows an SSID; the device is not offline in
`/opt/bin/tailscale status`.

## Two pitfalls (already hit — don't repeat them)

1. **`LD_PRELOAD`**: `remux.service.d/rm2fb-preload.conf` sets
   `LD_PRELOAD=/opt/lib/librm2fb_client.so.1` for the whole service tree. The panel driver here
   drives the EPDC itself, and wrapped in this shim it floods the journal with
   `UNHANDLED IOCTL 17926` (= `0x4606`) and may also hijack the ioctls on `/dev/fb0`.
   Just `unset LD_PRELOAD` in the startup script.
2. **`HOME`**: in the environment remux inherits, `HOME` is not guaranteed to be `/home/root`, and
   `~/.config/rmdev` (settings, lock-screen password, pairing token) lives there.
   Set `export HOME=/home/root` explicitly in the startup script.

## remux gestures (making room for the page slider)

By default `remux` installs `direction=up` swipe gestures on the **right edge** (zone
`1263 0 1404 1872`) and on the two lower corners to bring up its own app launcher. The document
page slider also uses the right edge and the two compete, so:

```ini
# /home/root/.config/remux/remux.conf
launch_gesture=
back_gesture=
```

In the journal, `ADDED SWIPE GESTURE` turning into `RESETTING MT GESTURES` + `SETTING UP GESTURES`
means it took effect. To restore the official gestures: delete these two lines and
`systemctl restart remux`.

## Rollback (three steps, each works on its own)

```sh
# 1) back to systemd starting it directly
ssh remarkable 'systemctl disable --now remux; systemctl enable --now rmdev-autostart'

# 2) back to remux→xochitl (sandboxed HOME, so it doesn't scan the library)
ssh remarkable 'cp /home/root/xochitl.draft.orig /opt/etc/draft/xochitl.draft'

# 3) restore remux's default gestures
ssh remarkable 'rm /home/root/.config/remux/remux.conf; systemctl restart remux'
```

Backup files: `/home/root/xochitl.draft.orig`, `/home/root/xochitl.service.orig`.

## Why xochitl is not left running in the background

Previously remux launched an `xochitl-isolated.sh` (sandboxed `HOME`); its only purpose was
"so remux does not think there is no app". The cost:
- wasted memory/CPU;
- it still opened `/dev/input/event0/1/2` (buttons/pen/touch) and wrote telemetry to disk;
- early on, while scanning the library, it **rewrote** unrecognised `.content` files (changing
  `fileType` from txt/epub to notebook) — that is where the library's two bad entries came from.

Now it never starts at all, and the library can no longer be rewritten by a third party.

## Remote testing notes for sleep/wake

A short press of the power button really suspends the whole device (`/sys/power/state=mem`), and
**the USB NIC is suspended with it**: ssh necessarily drops, and an RTC alarm is not guaranteed to
wake it (`/sys/class/rtc/rtc0/device/power/wakeup` is enabled, but in practice a 300 s alarm did not
bring the USB back up). Therefore:

- testing suspend remotely means asking someone to press the power button; or
- set a short alarm with `echo +20 > /sys/class/rtc/rtc0/wakealarm` and inject it at once; after
  wake-up, **unplug and replug the USB cable** before reconnecting (the gadget may not
  re-enumerate).

## What the white screen is, and the watchdog

On `SIGTERM` the program **clears the screen with one frame and then exits** (behaviour added
later, so the panel does not stop mid-waveform). `remux`, however, launches the App only once at
startup and does **not** restart it automatically. So any operation that kills rmdev
(`systemctl restart remux`, `killall rmdev`, a crash) leaves the screen white unless it is
immediately followed by a relaunch.

A fallback was added:

```text
rmdev-guard.timer  (enabled, every 30 s)
  └─ rmdev-guard.service (oneshot)
       sleep 5; exit if pidof rmdev finds it, otherwise systemctl restart remux
```

The 5 s is there to stay clear of the `killall → mv → restart remux` window in `deploy.sh`, and not
start the old binary while the copy is still in progress. Manual recovery is simple:
`ssh remarkable 'systemctl restart remux'`.

## The panel INIT cannot be skipped

On a cold boot, when `Panel::open()` cannot find the panel state it runs the 111-frame INIT
sequence. **This cannot be skipped**:

| Approach | Result |
|---|---|
| Run INIT | subsequent frames display normally (library UI) |
| Skip INIT (`RM_PANEL_INIT=0`) | phases are computed and `pan#1..6` are sent, but **not a single pixel changes on the panel** — it stays on the official `reMarkable is starting` |

So INIT is not merely "establishing a baseline"; it is a precondition for the driver being visible
afterwards (presumably it initialises the EPDC waveform/polarity state to the set the encoder
assumes). It must be enabled by default.

### Two known issues that follow from this

1. **The state file lives in `/tmp`** (`panel::state_path()`, default `/tmp/rm2epd.state`).
   `/tmp` is a RAM disk and is **wiped on every reboot** → every cold boot has `fresh = true` →
   INIT runs every time. Which grey level the panel lands on afterwards depends on what was on
   screen before the reboot, hence "sometimes normal, sometimes the library UI is a pale grey that
   looks like ghosting". Before moving it to `/home/root`, the point above must be cleared up first
   (otherwise `fresh = false` → no INIT → fully black screen).
2. **The second full-screen refresh emits no frames**: measured after adding the boot progress bar
   (`boot_progress`), the first full-screen refresh emits `pan#1..6`, but the second full-screen
   refresh right after it emits **not a single `pan#`**. That is what blocks the boot progress bar
   (the screen stops on the white/bar). The function is kept for now and marked `dead_code`.

### Recovery

When the panel has been left in a bad state (white screen / pale grey / official splash):

```sh
ssh rm2 'rm -f /tmp/rm2epd.state; systemctl restart rmdev'
```

Deleting the state file → `fresh = true` → INIT runs again → the image comes back.

## Root cause of the intermittent white screen: wrong slot at boot

```text
[panel] LCDIF scanning slot 0 (judged from CUR_BUF-SLOT)    ← misjudged at boot
[panel] slot corrected: 0 → 1 (re-probe CUR_BUF after pan)  ← the truth after pan is 1
```

The LCDIF is **double-buffered** (slot 0 / slot 1). The slot the panel is scanning is judged from
the `CUR_BUF` register, but **at boot the LCDIF has not been programmed yet**, so `CUR_BUF` reads
back as 0 or garbage and `probe_slot` falls back to slot 0 — if the panel is actually scanning
slot 1, every frame goes onto a canvas nobody is looking at:

* a full-screen frame is "sent but not a single pixel changes" → **white screen** (stuck on the
  official splash);
* a partial refresh only "washes" the old image to grey → **pale grey / ghosting**;
* when the slot is judged correctly everything is fine → hence **intermittent**.

### Fix (`panel::emit` / `raw::reprobe`)

1. **Full-screen refresh writes both slots** (`write_slot_at(0/1)`): 1.4 MB more DRAM traffic
   (about 5 ms), in exchange for "visible whichever slot the panel is scanning", so the first
   frame after boot always lands.
2. **After a pan, wait one frame and re-probe `CUR_BUF`**: by then the kernel has certainly
   pointed `CUR_BUF` at the new slot, so the value read is the real one; if it differs, correct
   `slot`, and every subsequent partial refresh writes the right slot.
   (Reading immediately after `pan` returns the stale value and actually sets the slot wrong — so
   the order must be pan → wait one frame → re-probe.)
