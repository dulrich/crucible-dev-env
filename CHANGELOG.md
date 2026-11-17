# Changelog

All notable changes to Crucible Dev Env are recorded here, in the [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) style.

CDE uses pre-1.0 semantic versioning: a breaking change to the config, `session.kdl` or the IPC protocol bumps the minor version; anything else bumps the patch.

## 0.7.9 — 2026-11-17

### Added

- `header { active }`: `"fill"` (default, unchanged), `"text"` or `"text-bold"`. `text` drops the highlight block and draws the active workspace label in the highlight colour; `text-bold` also uses the bold face of the header font (fontconfig `<family>:bold`; a font path or a missing bold face falls back to the regular face with one warning).

### Fixed

- `overlay { alpha }` had no effect under the N1 styles: the overlay painted every cell with an explicit background, which terminals draw opaque. With foot, cterm or urxvt and no explicit `overlay.terminal-args`, CDE now sets `CDE_OVERLAY_SURROUND=default` and the overlay leaves its surround on the terminal's default background (already the style's colour), so foot and cterm overlays are translucent. Other terminals and explicit `terminal-args` keep the opaque surround.

## 0.7.8 — 2026-11-17

### Fixed

- Logging out of a `cde-login` session showed console text between CDE and the greeter. CDE now tells `cde-login` when you quit and keeps its last frame on screen. The daemon brings the greeter up on VT 7 in the background and switches once the greeter has drawn (4 s fallback), and CDE then exits on its background VT. A login made while the old session is still closing waits for it to close before VT 8 opens. A CDE crash or kill still returns to the greeter as before.

## 0.7.7 — 2026-11-17

### Fixed

- After a `cde-login` login, CDE took no keyboard or pointer input. On a start on a background VT, CDE opened the input devices while its session was inactive, which gave it no live devices, and never reopened them at activation. It now reopens them when the session first becomes active.

## 0.7.6 — 2026-11-17

### Fixed

- `cde-login` failed to start the greeter: it ran every command through the account's login shell, and the greeter account `_greetd` has `/usr/sbin/nologin`, which rejected the command (`unrecognized option --profile`). A shell that `/etc/shells` does not list is replaced by `/bin/sh`, as greetd does.

## 0.7.5 — 2026-11-17

### Added

- **`cde-login`, a CDE login daemon that replaces greetd.** It runs as root from `cde-login.service`, runs the greeter as `_greetd` on VT 7 and serves the greetd protocol on `GREETD_SOCK` (the greeter is unchanged), authenticates through PAM (`dist/pam.d/cde-login`, `cde-login-greeter`), and starts the user session on VT 8 in graphics mode while the greeter stays in front. It switches to VT 8 only once CDE reports its first frame ready (or after `--ready-timeout`, default 10 s), so a greeter login no longer shows the console or an empty frame. On logout it returns to the greeter. greetd stays installed but disabled as the rollback; see docs/greeter.md "Setup".
- CDE can start on an inactive (background) VT: outputs and clients come up while the session is paused, and DRM is adopted on activation with a ready first frame (the restored or picker windows drawn, or after ~3 s). When `CDE_LOGIN_READY_FD` is set, CDE writes `ready` to that fd once the frame is ready. `startup: background start` / `startup: foreground start` log lines record which path ran and the ready time. A tty1 `cde-session` start behaves as before.

### Changed

- `/opt/cde` (the greeter's `crucible`, `cde-login` and the stock background) is root-owned. On a greeter host the deploy stages these files under `~/.local/share/cde/opt-staging`, prints one `sudo` install command, and reports `sudo install pending` until the installed `--build` matches; it never runs sudo itself.

## 0.7.4 — 2026-11-16

### Fixed

- The key press that wakes the blanked greeter no longer lands in the username field. libinput delivers its device-added events just ahead of the first key after the greeter starts, and those counted as activity and woke the screens first. Device add and remove no longer count as idle activity.
- The greeter writes its log to `/tmp/cde-greeter/greeter-<unix-seconds>-<pid>.log` (the five newest are kept). greetd does not keep the greeter's stderr, so it had no log at all; docs/greeter.md wrongly pointed at the greetd journal.

### Added

- `idle: outputs woken by input event=<kind>` log lines, and in the greeter one `greeter: first key press after blanking` line after each blank (whether it woke the screens; never the key).

## 0.7.3 — 2026-11-16

### Changed

- Greeter hand-off on the TTY backend: CDE keeps the panel lit when it takes over the outputs (connectors are no longer reset at start), and the greeter no longer tears down its last frame on exit, so a greeter login no longer flashes several black screens (the kernel's brief console restore between greeter and session remains). A normal CDE exit to a getty still restores the console.
- At start-up each output is adopted once its wallpaper buffer arrives (or after 300 ms), so the first frame CDE shows is the wallpaper, never a black or clear frame. A tty start with no wallpaper client starts at most 300 ms later.

### Added

- `startup: crtc state` info log lines (CRTC active and framebuffer at CDE start and at each output's adoption commit), plus `startup: adopting output` lines, for verifying the hand-off.

## 0.7.2 — 2026-11-15

### Added

- **Config:** a `greeter { position }` child places the greeter's login panel on a screen region (`center`, `upper`, `lower`, `upper-left`, `upper-right`, `lower-left`, `lower-right`); the default `lower` keeps the stock background's centre clear. The profile carries it as `position "<name>"`.
- The greeter blanks the screens after `idle { blank-after }` seconds (published in the profile as `blank-after`), or 600 s when unset; it never locks. The key press that wakes the screens is consumed, so it never lands in the login field.
- `startup:` info log lines with `elapsed_ms` on the TTY backend (GPU manager, DRM device steps, connector set-up, first frame and flip per output, first wallpaper buffer and the flip after it), for measuring where startup time goes.

## 0.7.1 — 2026-11-15

### Added

- The greeter's login panel shows the active keyboard layout name (right of the prompt) when the profile has two or more layouts, and updates it on each `set-layout`.

### Changed

- `docs/greeter.md`: greetd runs on VT 7 (Debian's `greetd.service` conflicts with `getty@tty7`, so `vt = 1` would fight the tty1 getty); try it with `systemctl start` before enabling.

## 0.7.0 — 2026-11-15

### Added

- **Config:** a `header { style }` child and an `overlay { alpha }` child (0.0–1.0, default opaque) for translucent "glass" overlays; the style names `terminal`, `n1-dark`, `n1-med` and `n1-light` are shared by the header, overlay, lock screen and greeter (docs/config.md § Surface styles).
- **Config:** a `lock { style background }` block. With it, and a `lock-command` whose program is `swaylock`, CDE writes a generated swaylock config to `$XDG_CONFIG_HOME/cde/swaylock/config` and passes it with `-C`; `crucible lock-config` prints it. Without the block nothing changes.
- **Config:** a `greeter { style background session }` block and `crucible greeter`, a kiosk compositor mode that runs as a greetd greeter: it reads a profile CDE publishes to `/var/lib/cde-greeter` and logs in through greetd's IPC (new `crucible-greetd` crate). Setup is in `docs/greeter.md`.
- **IPC:** `crucible msg outputs` lists the enabled outputs with their logical geometry and scale; it is answered while locked.
- `cde-wallpaper-rotate --outputs separate` puts a distinct random wallpaper on each output (default `mirror`, unchanged); the state file now holds one `<output>\t<path>` line per output.
- A stock CDE background, installed by the deploy to `~/.local/share/cde/backgrounds/stock.png` (and `/opt/cde/share/backgrounds/` when that directory exists), usable as `background "stock"`.

## 0.6.0 — 2026-11-13

### Changed

- **Breaking (config):** `tiling.terminal-args` now has built-in per-terminal padding defaults for cterm, foot, urxvt and ghostty (`{gap}` is the terminal gap), so a configured terminal with a known basename and no `terminal-args` is padded for the focus ring. To restore the pre-0.6 behaviour, add an empty `terminal-args` node; to turn the default off for one program, add an empty `terminal-args program="<basename>"` node. A repeated `terminal-args` node is no longer an error: the last one wins.

### Added

- `tiling.terminal-args` takes an optional `program="<basename>"` property, so each terminal can carry its own arguments.
- A `theme` block (font, foreground, background, cursor, alpha, 16-colour palette) renders one fragment per terminal into `$XDG_CONFIG_HOME/cde/terminal-theme/` on start and reload: foot and cterm include it, CDE merges the urxvt one via `xrdb` after `~/.Xresources`, and ghostty spawns get `--config-file`. Without the block nothing is written. `crucible terminal-theme <terminal>` prints a fragment.
- `docs/terminals/`: the supported-terminal list (cterm, foot and urxvt full, ghostty basic) with per-terminal pages, and the terminal matrix measurements in `docs/spikes/terminal-matrix/`.

## 0.5.9 — 2026-11-13

### Added

- `night-light` takes an optional `day-temperature` (kelvin, `temperature`..=6500, default 6500): the temperature outside the `start`-`end` window and the end the fades run to, so a mild tint can hold during the day too. Without it nothing changes. A `day-temperature` warmer than `temperature` is a config error; `night-light off` still forces 6500 K.

## 0.5.8 — 2026-11-09

### Changed

- The keyboard layout can be switched while the session is locked: `Super+E` / `Super+S` (any `set-layout` bind) work at the lock screen, and `crucible msg exec set-layout <name>` is admitted while locked, so a layout can also be fixed over ssh. Every other acting request still answers `err session locked`, and VT switching stays chord-only.
- The default `lock-command` is now `swaylock --indicator-idle-visible`, so with two or more `keyboard` layouts swaylock shows the active layout name without a keypress. A `lock-command` you set yourself is unchanged.

## 0.5.7 — 2026-11-09

### Changed

- The deploy script builds every host on the deploying machine with that host's `target-cpu` (host-a native, host-b broadwell, du-c300 silvermont) and ships the binary over ssh, so a target needs no checkout and no cargo. Each staged binary must pass its `--build` check on the target before anything is installed. `all` now covers du-c300 and continues past a failed host with a `RESULT: <host> FAILED <reason>` line.
- The deploy also installs `cde-session` (the tty1 launcher) and `cde-wallpaper-rotate` next to `crucible`, with the build id stamped in. `cde-session` runs the sibling `crucible` without a checkout. `docs/session.md`'s `~/.profile` block now execs it.

## 0.5.6 — 2026-11-08

### Fixed

- Switching workspaces or cycling a pane stack no longer flashes the background for clients that drop their content while suspended (e.g. cterm): CDE keeps the previous frame on screen until the revealed window has repainted, for at most 80 ms. An IPC `capture` of a held output waits for the release.

## 0.5.5 — 2026-11-08

### Changed

- `night-light` with `start` equal to `end` (e.g. `start "00:00"; end "00:00"`) now means all day: the configured `temperature` around the clock, with no fade. It was a config error before, so no existing config changes meaning.

### Added

- `docs/config.example.kdl`, a starting config with every setting at its built-in default (checked by a test), and `docs/scripting.md`, on driving CDE from external scripts over its socket.

## 0.5.4 — 2026-11-08

### Added

- Night light: a scheduled colour-temperature shift applied as a gamma ramp on every active output, configured by a new `night-light { temperature; start; end; fade-minutes }` block (default 4500 K, range 2500-6500 K; `start`/`end` are `"HH:MM"` local and may wrap midnight; hot-reloadable). Without the block, and without using the verb, CDE never writes a ramp.
- `night-light on|off|toggle|auto|<kelvin>` verb (unbound by default; also `crucible msg exec night-light 4000`): forces a temperature until `auto` or a restart. Night light works on the TTY (DRM) backend only; under nested `winit` it has no effect, and screenshots are unaffected.

## 0.5.3 — 2026-11-07

### Added

- `unpin-workspace` verb (unbound by default): clears every slot pin naming the active workspace, removing its `Super+<digit>` shortcuts without deleting the workspace. Bind it with `bind "<chord>" "unpin-workspace"` or run `crucible msg exec unpin-workspace`.

## 0.5.2 — 2026-11-07

### Fixed

- `crucible mem` no longer reports another session's cterm daemon: a daemon now matches only when its resolved Wayland socket path (an absolute `WAYLAND_DISPLAY`, else `XDG_RUNTIME_DIR` joined with it) equals the compositor's, so nested sessions and thin clients attach to their own daemon.
- A `run-on` workspace emptied by an X11 window that is destroyed without being unmapped is now pruned, like any other closed `run-on` workspace.
- `crucible msg capture` replies in request order and no longer races the connection: it passes the same admission (lock and snip refusals) as every other request, and pipelined requests are no longer answered ahead of it.
- the deploy script refuses to deploy from a checkout that is not on `master` (so every host installs the same build) and no longer re-enters itself by hostname when an ssh alias differs from the short hostname.

### Changed

- Internal restructuring from the 2026-11-06 code review (no config, `session.kdl` or IPC change). Picker `root` entries written with `~` are now expanded against `$HOME`.

## 0.5.1 — 2026-11-06

### Added

- Screenshots. `Super+Print` runs the new `screenshot` action: it captures the focused output (without the pointer cursor) and delivers a PNG. A bare `Print` stays unbound, so applications still receive it.
- Snip. `Super+Shift+Print` runs the new `snip` action: every output freezes and dims, then click a window to capture it, drag to capture a region (its pixel size is shown), or click the background or press `Enter` for the whole output. `Esc`, a right-click or `Super+Shift+Print` again cancels. While snipping, applications receive no input and only `snip` and VT switching run (IPC requests too).
- A `screenshot { destination; directory }` config block. `destination` is `"clipboard"` (the default), `"file"` or `"both"`; files go to `directory` (default `~/Pictures/Screenshots`) as `cde-<YYYYmmdd-HHMMSS>.png` and never overwrite an existing file.
- The clipboard image (`image/png`) is readable by Wayland, data-control (e.g. `wl-paste`) and X11 clients.
- `crucible msg capture <abs path>` writes a PNG of the focused output and answers `ok <w>x<h>`. The running compositor must be restarted onto 0.5.1 for it, and for the new actions.

## 0.5.0 — 2026-11-06

### Changed (breaking)

- Size settings now need an explicit unit, written as a KDL type annotation. `(px)N` is logical pixels. Header `size` also accepts `(pt)N` (points, converted as round(N × 96 / 72), so `(pt)14` is 19 px, matching a terminal's 14 pt font). The affected keys are tiling `gap` / `ring` / `ring-inset`, header `size` / `divider` / `inset`, and cursor `size`. A bare number, an unknown unit, or `(pt)` on a key that takes pixels only is a config error, and with an invalid config CDE runs on its defaults. Migrate before restarting:

  ```kdl
  // 0.4.x
  header { size 14; divider 1 inset=6 }
  tiling { terminal gap=6 ring=3 }
  // 0.5.0
  header { size (px)14; divider (px)1 inset=(px)6 }   // or size (pt)10.5
  tiling { terminal gap=(px)6 ring=(px)3 }
  ```

  0.4.x ignores the annotations, so a migrated config reads the same on both versions.

## 0.4.13 — 2026-11-06

### Fixed

- X11 apps get their X resources from the start: the session (restored apps, autostart, the overlay host) now starts after the `~/.Xresources` merge finishes (at most 1 s), instead of racing it. You no longer need to run `xrdb` by hand after CDE starts.
- Overlays redraw when their terminal is resized. An X11 terminal such as urxvt opens at its own default size and is resized afterwards, which used to cut off the top margin until a key was pressed.
- urxvt overlays have no scrollbar (`+sb` is added to its default overlay arguments).

### Changed

- Overlays keep two blank columns on each side (was one), so the side margin looks as wide as the one blank line at the top and bottom.

## 0.4.12 — 2026-11-06

### Fixed

- `run-on` windows now start: a window spawned into a hidden workspace gets its first configure, so a client that waits for it (a nested `crucible --winit`, and most toolkits) comes up instead of waiting forever. It gets its real size when its workspace is first shown.
- Focus-follows-mouse after a workspace switch: the pane under the pointer takes focus at once, instead of after the next pointer motion. Over a gap or the header strip the workspace keeps its own focused pane. While an overlay is open or a drag is held, the refocus waits until it ends.

## 0.4.11 — 2026-11-06

### Added

- `run-on <workspace> <command…>` verb (IPC `run-on`): spawns a command into a background scratch workspace on the focused output without changing the active workspace or focus. The workspace is ephemeral — it never appears in the workspace store or `session.kdl`, even after an explicit `save` or `rename` — and is removed once its last window closes. A live name is reused, and a saved name is refused. Agents use it for nested checks.
- Header drag across monitors: dropping a workspace header segment on another output's segment moves the workspace there, at that position. Dropping on the other output's empty strip appends it.

### Changed

- Header drags reflow the strips live, showing where the workspace will land (across outputs too). A short click still switches, and releasing off every strip does nothing.
- Overlays (picker, launcher, settings) keep a one-cell margin around their text. In settings, area headings sit level with the `keys>` prompt and rows are indented under them.
- With `terminal "urxvt"`, the overlays get urxvt-specific default arguments, so no gap shows at the edges.

### Fixed

- The session starts when XWayland first reports ready or failed (with a 5 s fallback), so restored X11 apps open on CDE's own display. Children spawned before XWayland is ready get an empty `DISPLAY` rather than inheriting another display.

## 0.4.10 — 2026-11-06

### Added

- `delete-workspace` verb: deletes the active workspace, ending its windows, moving its saved record to `$XDG_STATE_HOME/cde/deleted/` and clearing its slot pins. It is refused on an output's last workspace and while a restore is pending. To restore a workspace, move its file back into `workspaces/`.
- Picker: pressing Delete on a saved (not open) row asks for confirmation, and a second Delete removes it, closing the picker. Any other key cancels. Over IPC this is the new `delete <name>` request, which refuses a live name.
- `reorder-workspace <n>` verb: moves the active workspace `n` places within its output's stack (clamped at the ends, no wrap). The new order is saved in the `session.kdl` `output` line, whose format is unchanged.
- Header drag-to-reorder: dragging a workspace header segment onto another segment of the same output reorders them. A plain click still switches.
- `delete-workspace` and `reorder-workspace` are unbound by default, on purpose: deleting goes through the picker and reordering through a header drag. Bind them in `binds` if you want chords.

### Changed

- The settings overlay (`crucible keys`) groups its rows under functional-area headings (Panes & windows, Stacks, Workspaces, Outputs, Launch & overlays, Session & system). Typing a filter flattens the list.
- With `terminal "cterm"`, the overlays (picker, launcher, settings) get cterm-specific default arguments: the opaque surround as cterm's background and zero padding, so no gap shows at the edges, as foot already did.

## 0.4.9 — 2026-11-06

### Added

- The pointer cursor comes from the installed XCursor theme. A new optional `cursor { theme "…"; size N }` config block picks the theme and size; otherwise CDE uses `XCURSOR_THEME`/`XCURSOR_SIZE`, then the system `default` theme at 24 px. Each shape falls back through its alternate names, then `default` and `left_ptr`. When a theme or size is configured, CDE also exports it to the apps it spawns and to XWayland. See `docs/config.md`.
- The compositor offers `wp_cursor_shape_manager_v1` (version 2), so clients such as foot can ask for a cursor by name instead of loading a theme themselves.

### Changed

- When no cursor theme can be loaded, the fallback cursor is a built-in arrow instead of the white square.
- Writing a `cursor` block into `config.kdl` makes it fail to parse on 0.4.8 and earlier, so a rollback to `crucible.prev` needs the block removed first.

## 0.4.8 — 2026-11-06

### Changed

- `crucible mem`: a cterm daemon's row is now `cterm daemon (+N windows)` and holds only the daemon and its thin clients (which no longer get a row each). What runs in its windows counts toward its app, one row per app name (`claude`, `htop`, a bare shell as `bash`, an unconfirmed restore banner as `crucible resume`). Before, the daemon row summed everything in its windows.

## 0.4.7 — 2026-11-05

### Added

- cterm session keys (crucible-term plan 03): a granted `cterm` spawn also gets `CRUCIBLE_TERM_SESSION`, a CDE-minted session key. It is saved in the terminal's workspace record as an optional `session=` field (older `session.kdl` files load unchanged) and re-exported when the record is restored, so cterm 0.4.0 reattaches the still-live shell after a compositor restart.
- Loss recovery: when a bound window disappears because its Wayland client disconnected (the cterm daemon died) rather than because it was closed, CDE keeps its slot and spawns a placeholder there from the window's last recorded state. A terminal becomes `crucible resume --lost`, which shows the restore banner with a line saying the terminal was lost (with no command, a shell starts at once). Auto-save is held off for that workspace while a placeholder is pending. User closes and unbound windows behave as before. See `docs/config.md`.

## 0.4.6 — 2026-11-05

### Added

- Window identity for crucible-term's daemon mode: a new compositor global `crucible_window_identity_manager_v1` (protocol `crucible-window-identity-v1`, `protocols/crucible-window-identity-v1.xml`). When the terminal program's basename is `cterm`, each terminal spawn gets a single-use `CRUCIBLE_WINDOW_GRANT`; a client that binds it before the surface's xdg role (checked against the peer's UID, executable and parentage) has its window placed, classified, captured, relaunched and closed by that spawn's own shell (`WindowIdentity`) rather than by the connection's PID. Windows without a bound identity behave as before. See `docs/config.md`.
- `crucible mem` lists a running cterm daemon as `cterm daemon (N windows) › …`.
- Hidden Wayland windows (e.g. stacked behind another in a pane) get the standard xdg_toplevel `suspended` state, so clients that honour it (cterm 0.3.0) free their buffers; it is cleared when the window is placed again.

## 0.4.5 — 2026-11-05

### Fixed

- Clipboard tools: CDE now advertises the ext and wlr data-control protocols, so `wl-copy`, `wl-paste` and clipboard managers set and read the clipboard and primary selection directly. Before, each `wl-copy`/`wl-paste` briefly mapped as a window in the focused pane and took keyboard focus away from the terminal (which also made foot refuse OSC 52 copies as "unfocused").

## 0.4.4 — 2026-11-04

### Added

- Fullscreen pointer confine: while a fullscreen window (Wayland or X11) has keyboard focus, the pointer stays on its monitor once it is there, so a fullscreen game such as Witcher 3 under Proton no longer loses the mouse to the next output. Moving focus with a Super chord, switching workspace, opening an overlay or locking the screen frees it. On by default; turn it off with the new top-level `confine-pointer-to-fullscreen #false` (TTY backend only). Writing that key into `config.kdl` makes 0.4.3 and older reject the config as an unknown node, so remove it before rolling back to `crucible.prev`.

## 0.4.3 — 2026-11-04

### Added

- Pointer-constraint diagnostics: with `RUST_LOG=info,crucible_compositor::pointer_lock=debug`, CDE logs each pointer lock/confine as it is created, activated, refused or deactivated (naming the failed condition), dropped when pointer focus leaves, and removed. At `trace` it also logs each confine clamp. This is for finding out why a fullscreen game's pointer can leave its monitor.

## 0.4.2 — 2026-11-04

### Fixed

- Restored apps relaunch under a renamed binary: CDE recognises its own placeholder and terminal wrappers by the running executable's name as well as `crucible` (for example a distro-suffixed `crucible-noble`), so Enter on a placeholder no longer fails with "not a relaunch placeholder", saves no longer record CDE's own wrappers as commands, and `crucible mem` finds the compositor.
- An app with an empty app id is saved without one, and its placeholder names the program instead of reading "CDE restored .".

## 0.4.1 — 2026-11-03

### Fixed

- Steam restores on session restore: an app CDE spawned (launcher, `run`, autostart) records the command CDE launched, so Steam saves `steam` instead of its unrelaunchable `steamwebhelper` child.
- Proton/XWayland games get mouselook and pointer lock: CDE now advertises `zwp_relative_pointer_manager_v1` and `zwp_pointer_constraints_v1`, honours lock and confine on the TTY (udev) backend, releases the lock when you leave the game by keyboard or an overlay/lock screen takes input, and applies the game's cursor-position hint on unlock.

## 0.4.0 — 2026-11-03

The first versioned release: the MVP (roadmap Phases 0–3) as it exists today.

- Tiling and panes: directional `split-right`/`split-down` over a binary split tree, `focus`/`move`/`resize` by direction, and stacks of windows per pane cycled in place with a pip indicator.
- Workspaces and outputs: each output owns an independent stack of workspaces; multi-monitor with hot-unplug migration; XWayland apps tile alongside native clients.
- Visuals: focus ring, stack pips, and an optional clickable per-output workspace header strip.
- Persistence: workspaces auto-save and restore (layout, terminal directories and commands, app relaunch placeholders) through a fuzzy picker.
- Overlays: workspace picker, program launcher and settings view, with numbered workspace slots.
- Desktop basics: screen lock, auto-lock and idle screen blanking, keyboard layout switching, fullscreen (Wayland and X11), closing windows, and a tty1 session launcher.
- Control: the `crucible msg` IPC control socket.
- Backends: nested `winit` for development and `udev`/DRM (`--tty-udev`) on a real TTY.
- CLI: `crucible --build` prints the build id; `crucible --version` prints `crucible <semver> (<build id>)`.
