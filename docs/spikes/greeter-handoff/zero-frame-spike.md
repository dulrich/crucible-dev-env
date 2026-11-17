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

## Follow-up (2026-11-17): logind controller kept alive, and greetd's own text switch

- `TakeControl(force=true)` is root-only: `logind-session-dbus.c:389` refuses any non-root caller that passes `force` ("Only owner of session may take control"). A helper running as the greeter user cannot take the controller away from the greeter.
- Rev 5 holder: a small program copied the greeter's system-bus socket with `pidfd_getfd` (yama `ptrace_scope` is 0), held it after the greeter exited, and released it when the user session closed. It worked as designed in two logins (held fd 10 through the greeter's exit, saw user sessions 577 and 579, released at logout), and VT switching still worked. **CDE still started on fb 113 both times, and the user saw console text.**
- The reason is greetd itself. greetd 0.10.3 `greetd/src/session/worker.rs:178-185` runs for every session it starts, the user session included: `kd_setmode(KdMode::Text)` then `term_clear()` on the session's VT, with the comment "Other login managers set this to graphics, but that disallows start of textual applications, which greetd aims to support". `server.rs` `reset_vt` does the same. On the foreground VT, that forces the fbdev restore before CDE starts.
- The text on the restored console is `cde-session`'s three `== …` header lines, which it tees to the terminal after greetd's clear.

Conclusion: with stock greetd a zero-frame hand-off is not reachable. Keeping logind's controller alive is possible without root (the rev-5 approach), but greetd's text switch comes after it. The next step is a bounded own-vs-depend review of greetd (user, 2026-11-17).

## Follow-up (2026-11-17): CDE started on a background VT

For the owned-login-daemon design (session started on a second VT while the greeter's VT stays in front), the user started `cde-session` on VT 2 and switched to the greeter's VT 7 before it began. CDE 0.7.4 is **not** ready for a background start. With the session inactive it logged `Unable to become drm master`, its 300 ms adoption timeout still fired, every format's mode-set test failed (`Error testing state`), the output was dropped, and disabling the unadopted CRTC failed (`Permission denied`). At the switch to VT 2 it logged `resuming session` with no output, never re-adopted, and the display and input were wedged until it was killed. Required before the daemon: no adoption while the session is paused, adoption on activation with an already-ready first frame, and a readiness signal to the daemon. Graphics-to-graphics VT switches between two *running* sessions were clean in both directions (greetd dependency review, 2026-11-17).

## Result (2026-11-17): cde-login 0.7.6 on host-b — zero-frame hand-off

After the cutover (plan 45 WP-6; greetd disabled, `cde-login` enabled, `/opt/cde` root-owned at `6e8ed7d`), the user reported a greeter login with no console text and no intermediate frame ("zero frame swap"). 0.7.5's daemon had not started the greeter at all (`_greetd`'s shell is `/usr/sbin/nologin`, fixed in 0.7.6 `a209f6c`). Read back from the journal and `/tmp/cde/latest.log` for that boot:

- **Timeline:** `cde-login` authenticates `dulrich` at 18:17:40.890 and starts the session (pid 1444) at 40.941. CDE logs `startup: background start` at 41.003, and the daemon logs `session ready (ready)` at 42.0709, 0.03 ms after CDE's `startup: login ready` (`elapsed_ms=1075`, `timed_out=false`, `signalled=true`). CDE logs `startup: adopting at session activation` (`ready=true`) at 42.076, `first frame queued` on eDP-1 at 42.152, and the greeter exits cleanly (code 0) at 42.366.
- **Ready fd:** the daemon places the pipe at fd 3 (`READY_FD`, env `CDE_LOGIN_READY_FD=3`). CDE signalled on it and the daemon read `ready`, not EOF or the 10 s fallback.
- **Ready inputs:** `outputs: 1, outputs_with_background: 1, awaited_spawns: 0, windows_without_buffer: 0`. This login was a **cold start** (`no workspace of the last session is saved`), so readiness meant wallpaper plus picker; the restore path with spawned windows was not exercised.
- **logind:** session 4: `Service=cde-login`, `Class=user`, `Type=wayland`, `TTY=tty8` (PAM_TTY in the `ttyN` form was accepted), `VTNr=8`, `Active=yes`, `State=active`; the foreground VT is tty8. The `VT_ACTIVATE` switch is enough; no logind `ActivateSession` fallback was needed.
- While paused, CDE logged `Unable to become drm master, assuming unprivileged mode` (expected) and read CRTC 47 still active on the greeter's fb 120. The adoption commit put its own fb 114 on it.

## Result (2026-11-17): 0.7.7 — input, restore and the remaining checks

- **0.7.6 took no input** after a `cde-login` login. Its compositor held 0 `/dev/input` fds (host-a's foreground start holds 10): libinput bound the seat while the session was inactive, and resume on activation was a no-op on a never-suspended context. 0.7.7 (`d5a319c`) suspends libinput at a background start. With 0.7.7, libinput reports its devices after activation, and the user confirmed keyboard and pointer work.
- **Restore readiness:** the 12:29 and 12:30 logins logged `last session restored`. The restored cterm (pid 4402) mapped 9 ms after spawn and had a buffer before the ready check, so `login ready` at 1102 ms had `awaited_spawns: 0, windows_without_buffer: 0`, and the restored window is in the first frame.
- **User-confirmed:** wrong password and retry, `Ctrl+Alt+F2` and back to F8, logout returns to the greeter.
- **Remaining:** on logout, console text shows on VT 8 between CDE's exit and the daemon's switch to the restarted greeter (journal: session exit 12:29:09.539, greeter started 09.579). This is the reverse of the login problem and is out of scope for plan 45.
- A logged-out session can stay `closing` on tty8 when user processes outlive it (the `cterm daemon`), which is ordinary logind behaviour, the same under greetd.
