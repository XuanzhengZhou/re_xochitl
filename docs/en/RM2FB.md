# "Screen Frozen but the System Alive": Root Cause and Fix

> English translation of [`../RM2FB.md`](../RM2FB.md) (Chinese original).

> **Update: this whole chain has been torn out.**
> `src/panel/` holds a pure-Rust panel driver (it parses `.wbf` itself, encodes the
> waveform, and writes LCDIF). rmdev no longer starts xochitl and no longer loads the
> rm2fb client — it writes `/dev/fb0` directly.
> This post-mortem is kept because it explains **why such care was necessary at the time**.

## 1. Symptoms

- The screen is stuck on one frame; tapping does nothing.
- `ssh` works; `uptime` and `free` are both normal, load 0.0x.
- `/tmp/rmdev.log` stops at one input event and then nothing more.
- In `ps`, rmdev is in state `S` (interruptible sleep), not `D`.
- The mtime of `/dev/shm/swtfb.01` stops at some instant and never moves again.

**The key counter-intuitive point**: the program believed it was refreshing the screen
normally. `refresh()` always returned success, not one ioctl reported an error, and the log
did not even contain a `[fb] ioctl` → `r=-1` line.

## 2. rm2fb Architecture

```
          ┌──────────────────────────────┐
client A ─┤  /dev/shm/swtfb.01 (shared)  ├─ server ─→ EPDC (the only writer to the panel)
client B ─┤  (fake fb, 1404×1872 16bpp)  │   ↑
          └──────────────────────────────┘   └─ official xochitl + librm2fb_server.so.1
```

- Only the **server** can write EPDC. The rM2 panel is not 16bpp RGB565, and `rm2fb` does
  the format conversion, so there is **no way around it** (writing `/dev/fb0` directly was
  tried; the geometry is wrong).
- Other processes write frames into shared memory and the server flushes them to the panel.

## 3. Root Cause: A Wrong Liveness Check

Toltec's `/opt/etc/xochitl.env.d/rm2fb-preload.env`:

```bash
if [[ -f /dev/shm/swtfb.01 ]]; then
    export LD_PRELOAD="$LD_PRELOAD:/opt/lib/librm2fb_client.so.1"
else
    echo "rm2fb server is not running: starting without rm2fb client"
fi
```

**It decides whether the server is alive by whether the file exists — but that file is
persistent.**

Once the server dies the file is still there → afterwards **every** newly started process
loads only the client library → their writes to shared memory always "succeed" → **but
nothing pushes those frames to EPDC**.

That is the entire cause of "screen frozen while every process looks fine".

## 4. Why the Server Dies

Two independent paths:

### 4.1 `rm2fb.service` Does Not Self-Heal

```
Nov 07 04:52:46  xochitl[193]: STARTING RM2FB
Nov 07 05:29:42  systemd: rm2fb.service: Succeeded.      ← gone right here
```

`Restart=on-failure`, and xochitl **exits cleanly (exit 0)** on SIGTERM → systemd rates it
`Succeeded` and **does not restart it** → the server is gone for good.

### 4.2 The Deploy Script Kills It

`tools/deploy.sh` used to carry its own "flush the EPDC queue" step:

```sh
systemctl stop remux
start remux   # it brings up the app, and that app may be acting as the server
stop remux    # ← takes the server down with it
```

The journal matches:

```
06:15:49  remux[2622]: Sourcing /opt/etc/xochitl.env.d/rm2fb-preload.env
06:15:50  remux[2622]: REPLACING THE IMAGE with shared memory    ← it became the server
06:15:50  remux[2622]: EPD platform plugin loaded!
06:15:52  systemd: remux.service: Main process exited, status=15 ← killed 2 s later
06:15:57  pt_i2c_adapter: PT_GET_HID_DESCRIPTOR_TIMEOUT=2500
...       the screen does not refresh again for 17 minutes
```

That `PT_GET_HID_DESCRIPTOR_TIMEOUT` line is a **result**, not the cause: the touch
controller got reset while the screen had already stopped refreshing.

## 5. Fixes

### 5.1 `tools/deploy.sh` Rewritten

It does three things only: **probe for the server → use the launcher to stop only our own
app → start it again**. Unless necessary it **never touches** `remux` / `xochitl` /
`rm2fb.service`.

### 5.2 `tools/rm2fb-guard.sh` (New)

Scan `/proc` for `comm == rm2fb-server` instead of trusting that shared-memory file:

```sh
for p in /proc/[0-9]*; do
    case "$(cat "$p/comm" 2>/dev/null)" in
        rm2fb-server*) exit 0 ;;
    esac
done
exit 1
```

### 5.3 `tools/rmdev.sh`

Run the guard before starting; if there is no server, `systemctl start rm2fb` to bring it
back, waiting at most 12 s; retry on `exit(3)` (first-frame watchdog).

**It cannot act as the server itself**: `LD_PRELOAD=librm2fb_server.so.1 ./rmdev` was tried
and **measured SIGSEGV (rc=139)** — that library is a Qt QPA platform plugin, and any
non-Qt program that loads it crashes.

### 5.4 xochitl HOME Isolation (Important Side Effect)

Running `systemctl start rm2fb` to rescue the server also starts the official xochitl, and
it **scans and rewrites** any `.content` file it does not recognize under
`~/.local/share/remarkable/xochitl`:

```diff
- "fileType": "txt"      ← written by rmdev
+ "fileType": "notebook" ← rewritten by xochitl
```

The result is that every e-book turns into a blank notebook (all thumbnails white, page
count 1). The fix:

- `/etc/systemd/system/rm2fb.service.d/isolate-home.conf`
  → `Environment="HOME=/home/root/xochitl-sandbox"`
- `call=` in `/opt/etc/draft/xochitl.draft` points to `/home/root/dev/xochitl-isolated.sh`
  (sets an empty HOME, then execs)

The program reads its library through the **absolute** `DEFAULT_ROOT` path, so it is not
affected by HOME.

## 7. Reproduction and Verification

```sh
# reproduce the failure (the screen will drop; recover with the command below)
kill -TERM <pid of rm2fb-server>
/home/root/dev/rm2fb-guard.sh; echo $?     # → 1

# recover
setsid sh /home/root/dev/rmdev.sh &
tail -n 10 /home/root/dev/rmdev.log
# expected: no framebuffer server, so rm2fb.service is started
#           then [fb] ioctl returns r=0
```
