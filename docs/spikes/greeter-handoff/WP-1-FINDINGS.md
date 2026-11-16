# Greeter hand-off: keep-lit spike (WP-1)

Measured 2026-11-16 on host-b (ThinkPad T450s, i915, one eDP-1 panel, kernel 6.12.116, `quiet` cmdline, fbcon bound to `i915drmfb`), greetd on VT 7, spike build `a1a585e-dirty` in both the user session (`~/.local/bin/crucible`) and the greeter (`/opt/cde/bin/crucible`). Variants were switched by files in `/opt/cde/spike/`, because greetd's environment needs sudo to change. The patch is [`wp1-variants.patch`](wp1-variants.patch). 0.7.2 was restored afterwards (`--build` = 450f37f for both binaries) and `/opt/cde/spike` was removed.

The variants:

- **A** (`KEEP_CONNECTORS`, user session only): `DrmDevice::new(fd, false)`.
- **B** (`GREETER_RETAIN`, greeter only): on a successful login the greeter closes its scanout framebuffer handles with `DRM_IOCTL_MODE_CLOSEFB` (raw ioctl `0xC00864D0`) and then calls `std::process::exit`. That skips every Drop: smithay's surface clear (`surface/atomic.rs:1056-1069`) and the `AtomicDrmDevice` restore (`device/atomic.rs:238-276`).
- **C** (`GREETER_CLEAR_VT`, greeter only, on top of B): before exiting, the greeter writes a clear-screen and hide-cursor sequence to its controlling tty.

## Logins

| Login | Variants | What the user saw | CRTC 47 seen by CDE |
|---|---|---|---|
| 1 (×2) | A | "a true black/off, then a briefly visible screen of logs, and a short mid-black after that"; second cycle the same, without logs | fb 113 from 77 ms until `initialize_output` (885 ms) |
| 2 | A+B | "straight to the screen with logs, no true black (or at most a single frame)" | fb 113, 75 ms → 883 ms |
| 3 | A+B+C | logs still shown | fb 113, 77 ms → 889 ms |

At the first `queue_frame` (about 1060 ms) the CRTC shows CDE's own fb (115 or 117) in every run.

## Findings

1. **fb 113 is the kernel's fbdev console, not the greeter's frame.** The greeter's own scanout fbs were 120 and 122 (greeter log: `closefb crtc::Handle(47) framebuffer::Handle(122) rc=0`). When the greeter releases DRM, the fbdev helper restores its console framebuffer (fb 113). That happens with or without B. The text console is what the user calls "logs".
2. **Variant A keeps the panel lit inside CDE.** With `disable_connectors = false` the CRTC stays active on fb 113 through `DrmDevice::new` and `scan_connectors`, right up to CDE's first commit. The forced eDP probe drops from 649 ms (panel off, earlier spike login) to **1.2 ms**.
3. **Variant B removes the true black/off.** Without B the greeter's Drop clears its surface and restores state, which the user sees as a panel power-off before the console comes back. With B the panel goes straight from the greeter to the console.
4. **Variant C does not help.** The VT write succeeded (`clear-vt /dev/tty -> Ok(())`), but the restored console still showed text, so clearing from the greeter does not reach what fbdev displays. C is dropped.
5. **The console stays up until CDE's first commit, at about 0.9 s.** Getting rid of it needs a host change (turning fbdev emulation off, which also removes the text console and the getty fallback) or a shorter CDE start. The user chose not to change the host (2026-11-16). The console flash goes to the backlog.
6. **HDMI-A-1 DDC is still about 224 ms** when forced (112 ms unforced) on the empty port. Under the decision rule it goes to the backlog and is not fixed here.

## Decision for WP-2

Ship **A+B**: `DrmDevice::new(fd, false)` with the unused-connector cleanup, and the greeter's retain-on-exit (CLOSEFB on the scanout fbs, then exit without Drop, in greeter mode only after a successful login), plus the wallpaper deferral. The expected result is greeter → console (about 0.9 s, panel lit) → wallpaper, with no power-off and no black or clear-colour frame from CDE.

## WP-4 verification on the shipped 0.7.3 (2026-11-16)

Build `f128b46` (tag v0.7.3). The user logged in through the greeter on host-b and reports the transition as expected: greeter, then the console with the panel lit, then the wallpaper, with no power-off and no black step. du-c300 (no greeter) confirms the tty1 text login path still works. host-a has not been restarted on 0.7.3 yet.

host-b `/tmp/cde/cde-20261116-025939.log` (elapsed_ms from CDE start):

| ms | line |
|---:|---|
| 77 | `startup: crtc state at="cde start" crtc=47 active=true fb=113` (CRTCs 63 and 79 inactive) |
| 376 | `startup: scan_connectors done` (was 649 ms for the forced eDP probe alone with the panel off) |
| 377 | `startup: adoption deferred until the wallpaper output=eDP-1` |
| 379 | `startup: main loop starting` |
| 679 | `startup: crtc state at="adoption commit" crtc=47 active=true fb=113` |
| 679 | `startup: adopting output output=eDP-1 deferred=true` |
| 744 | `startup: first flip completed output=eDP-1` |
| 1024 | `startup: background layer mapped` |
| 1137 | `startup: background buffer committed` |
| 1144 | `startup: first flip after background buffer` |

du-c300 `/tmp/cde/cde-20261116-025920.log`: `crtc state at="cde start" crtc=53 active=true fb=96`, `scan_connectors done` at 2457, main loop at 2476, `adopting output deferred=true` at 2777 (the 300 ms timeout, with no background buffer in the log), first flip at 2899.

### Readings

1. **No power-off.** The CRTC is active on the console fb at CDE start and still at the adoption commit, on both hosts. Variants A and B hold on the shipped build.
2. **The adoption on host-b came from the timeout, not from the wallpaper.** The main loop started at 379 ms. Adoption ran at 679 ms, which is exactly the 300 ms timeout, and the wallpaper client's buffer arrived only at 1137 ms. So CDE's first frame (744 ms) was its scene without the wallpaper, and the wallpaper came about 400 ms later. The user did not see that frame as a black step. Waiting for the wallpaper would have meant a timeout of about 760 ms. That keeps the fbdev console up about 400 ms longer instead, and slows a start that has no wallpaper client by the same amount. The constant stays at 300 ms. The lever is a faster wallpaper client start, which goes to the backlog.
3. **A start with no wallpaper client costs the 300 ms timeout, as the plan allowed** (du-c300).
