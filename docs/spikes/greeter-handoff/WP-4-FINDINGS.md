# Greeter → CDE hand-off on host-b (plan 43 WP-4)

Measured 2026-11-15 on host-b (ThinkPad T450s, i915, one eDP-1 panel), CDE 0.7.2 (`450f37f`), greetd on VT 7. Two logins through the greeter, 23:23:06 and 23:23:30 CST; logs `/tmp/cde/cde-20261115-232306.log` and `/tmp/cde/cde-20261115-232330.log`. The two runs agree within 50 ms, so one run's numbers are quoted.

User report: "it goes through several black and less black screens".

## Timeline (23:23:30 login)

`elapsed_ms` is from `mark_process_start()` at backend entry (≈ 23:23:30.622).

| t (ms) | Event | What the screen shows (inferred) |
|---|---|---|
| −57 | logind opens the greetd user session (TimestampMonotonic 4295.421457 s, converted: 23:23:30.565) | greeter frame, or black once the greeter's DRM master is gone |
| 0 | CDE backend entry (first log line 23:23:30.625) | — |
| 3 | `GPU manager created` | — |
| 228 | `DrmDevice::new done`. `DrmDevice::new(fd, true)` disables every connector, which powers the eDP panel off | **black, backlight off** |
| 298 | `render node added` (EGL) | black |
| 312 | `DrmOutputManager::new done` | black |
| **1324** | **`scan_connectors done`: 1012 ms in the connector scan alone** | black |
| 1324 | `connector set up` (eDP-1), modeset | panel on, nothing drawn yet: **"less black"** |
| 1591 | `main loop starting` (267 ms after connector set-up: modeset, first output state, XWayland and the services before the loop) | — |
| 1595 / 1605 | first frame queued / first flip completed | CDE `CLEAR_COLOR` (0.05, 0.05, 0.08): **dark blue-grey** |
| 1596 | background layer mapped (swaybg role map, no buffer yet) | — |
| 1704 / 1705 | first background buffer committed / first flip after it | **wallpaper** |
| 1715 | session restore starts spawning clients (cterm overlay host, …) | windows appear |

The 23:23:06 run: DrmDevice 229, DrmOutputManager 269, scan_connectors 1315 (1046 ms), main loop 1583, first flip 1597, wallpaper flip 1664.

Session open → CDE start is about 0.06 s and is not a factor. Greeter exit → session open is not observable as `dulrich`: the greetd journal is unreadable without `adm`/`systemd-journal` (`journalctl -u greetd` → "No entries"). The `Unable to become drm master, assuming unprivileged mode` warning shows up on host-a too and is not specific to the hand-off.

## Dominant interval

**`scan_connectors`, about 1.0 s, on host-b only.** On host-a (DisplayPort desktop monitors) the old logs put the whole EGL → connector window at 0.03 s. The scan follows directly after `DrmDevice::new(fd, true)` has switched every connector off. With i915 eDP, probing a panel that was just powered down plausibly goes through the panel power sequencing delays (power-cycle and power-on waits, then an EDID read) before `GETCONNECTOR` returns. This is the leading hypothesis, not a measurement: nothing here separates the probe cost from the power-sequencing wait.

The visible sequence matches the user's report: greeter → black (connectors disabled at ≈0.23 s) → ≈1.1 s of black → panel back on (≈1.3 s) → dark blue-grey clear colour (≈1.6 s) → wallpaper (≈1.7 s). Those are the black and less-black steps.

Secondary: 267 ms from connector set-up to the main loop, and 100 ms from the first frame to the wallpaper. Each costs one visible shade change.

## Candidate fixes, for the follow-up plan

1. **Keep the outputs on through start-up:** `DrmDevice::new(fd, false)`, so the greeter's last mode stays lit and the first CDE commit does the modeset. If the hypothesis holds, this removes both the panel power-off and most of the 1 s scan. It is the first thing to spike: one line, measured by the same `startup:` lines on host-b. Risk: stale CRTC/plane state that the greeter leaves behind must not break the first atomic commit (smithay's `DrmOutputManager` resets planes on the first commit).
2. **No visible clear-colour frame before the wallpaper:** paint the first frames with the greeter's or wallpaper's colour, or hold the first flip until the first background buffer arrives or a short timeout (≈150 ms) passes. Removes the dark blue-grey step.
3. **Defer the slow step:** if the scan is still slow with the connectors kept on, move connector probing out of the critical path before the main loop. This is a larger change.
4. Not supported by this data: speeding up session open, or greetd itself.

Next step: a follow-up plan that opens with candidate 1 as a measurement spike on host-b, then candidate 2 (backlog item in `INITIATIVE.md`).

## Addendum (2026-11-15, after close)

- `sudo journalctl -u greetd -b` shows only the PAM lines, at one-second resolution: greeter session 23:23:22, user session 23:23:30. The greeter's `session started; greeter exiting` line is not in the unit journal, so the moment the greeter exits is still not measured.
- A connector probe from a process that is not DRM master is a no-op. `get_connector(force = true)` returns in about 3 µs on both hosts, because the kernel only re-probes for the current master. So the 1 s cost can only be measured from inside CDE (master) at login. A host-side probe while the greeter holds master tells us nothing.

## Spike login (2026-11-15 23:59, measurement-only build `450f37f-dirty`)

The build logged CRTC state and timed each connector query at three points: before `DrmDevice::new`, after it, and before `scan_connectors`. Its patch is `runs/spike/handoff-probe/spike.patch`. The probes themselves add time, so this run reached the wallpaper at 2049 ms. 0.7.2 was restored afterwards (`--build` = 450f37f).

| ms | Observation |
|---|---|
| 76 | Before `DrmDevice::new`: CRTC 47 is **active** at 1920x1080 with fb 113. The greeter's last image is still on screen and the panel is lit. CDE is not DRM master yet (`Unable to become drm master`). |
| 76–208 | Connector queries, panel on: eDP 7 µs, **HDMI-A-1 116 ms** (DDC on an empty port), DP-1 6 ms, DP-2 8 ms. |
| 208 → 373 | `DrmDevice::new(fd, true)`: 165 ms. Afterwards every CRTC has mode None, so **the panel is off**. |
| 539 → 1188 | Forced probe, panel off: **eDP 649 ms**, then HDMI-A-1 224 ms, DP-1 15 ms, DP-2 16 ms. |
| 1445 → 1701 | The real `scan_connectors`: 256 ms, compared with 1012 ms in the 0.7.2 runs. The spike's probes had already paid the eDP power-up. |

Reading: the 1 s scan is about 650 ms of eDP panel power-up plus 115–225 ms of HDMI-A-1 DDC on an empty port. The power-up happens because CDE's own `DrmDevice::new(fd, true)` has just switched the lit panel off. Not measured yet:

- whether the panel stays lit through CDE's first commit with `disable_connectors = false`. At 76 ms the greeter still held master. When it exits, the kernel removes its framebuffer, and smithay's `AtomicDrmDevice` `Drop` restores the state captured at its own start (`smithay …/backend/drm/device/atomic.rs:238-276`). Either may blank the screen.
- the forced eDP probe cost while the panel is lit.
