# Zero-frame greeter hand-off — pre-plan spike (2026-11-16/17)

Goal under study: after a greeter login, the first thing on screen after the greeter is a ready session (cterm or application drawn), with no console and no empty or wallpaper-only frame. Host: host-b (i915, eDP-1, kernel 6.12.117, systemd 257.13, greetd 0.10.3, libseat 0.9.1), CDE 0.7.4 with plan 44's keep-lit hand-off (the greeter keeps its scanout fb on exit).

## Logins

The holder was a shell wrapper around the greeter binary, which opened `/dev/dri/card0` and logged a heartbeat.

| Rev | Holder | Result |
|---|---|---|
| 1 | opened the card before the greeter | the holder became DRM master; the greeter and a tty1 CDE both failed with `Failed to open device using libseat … Resource temporarily unavailable`. Reverted. |
| 2 | opened after the greeter's first flip (non-master fd), plain background subshell | killed at greeter exit (no `released` line); console shown |
| 3 | as 2, 20 ms heartbeat, TERM trapped | SIGKILLed about 1 ms after the greeter exited (no TERM). CDE opened DRM 75 ms later; `crtc state at="cde start"` fb 113 (console) |
| 4 | as 3, started with `setsid -f` (own session and process group) | **survived the whole gap** (opened 02:12:23.18, greeter gone 28.997, released 34.44); the greeter logged `scanout framebuffer kept lit on exit … fb=115`; CDE opened DRM at 29.152 and still found **fb 113**; the user saw console text |

Readings: greetd kills the greeter's process group, but not a `setsid` child, which stays in the greeter's session scope. Holding a DRM fd open does not prevent the console.

## Why: the restore is a VT text-mode switch, not a last close

- Kernel 6.12 `drm_fb_helper_set_par()` forces the fbdev restore when `FB_ACTIVATE_KD_TEXT` is set, even with an active DRM master. Its comment notes that "on modern systems where logind always keeps a drm fd open … [lastclose] doesn't work". `fbcon_blank()` (`fbcon.c:2233`) sets that flag when the VT returns to `KD_TEXT`.
- systemd 257 `logind-session.c`: `session_drop_controller()` (line 1648) runs when the session controller's bus name goes away (the greeter exits) or on `ReleaseControl`, and it calls `session_restore_vt()`, i.e. `vt_restore()` → `KD_TEXT` on the session's VT. `session_set_controller(force=true)` from another bus name releases the old controller **without** a VT restore. The VT fd is only reset in `session_restore_vt()`.
- So at greeter exit, logind switches VT 7 back to text and fbcon repaints the console (fb 113) over the greeter's kept frame. It stays until CDE's first commit, about 0.9 s later.

## Levers left

1. **Controller hand-over:** a helper in the greeter's session takes control with `TakeControl(force=true)` before the greeter exits, so no VT restore runs at greeter exit. The helper's own later drop restores the VT, a forced fbdev commit that can land over a running CDE. So it must outlive the moment CDE takes control and leave only when that restore is harmless, for example when the user session ends. This needs a D-Bus client that keeps its connection (busctl does not), so it is not testable from shell.
2. **Host:** unbind fbcon from the DRM fbdev (`fbcon=map:…`) or turn fbdev emulation off. Nothing paints text on a restore, but the text console goes away on that host (declined 2026-11-16).
3. **CDE side (independent of 1–2):** defer CDE's first commit until the active workspace is ready (the wallpaper plus the restored windows' first buffers), not just the wallpaper with a 300 ms timeout. Today's logs record no per-window first-commit time.
