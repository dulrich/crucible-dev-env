# Changelog

All notable changes to Crucible Dev Env are recorded here, in the [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) style.

CDE uses pre-1.0 semantic versioning: a breaking change to the config, `session.kdl` or the IPC protocol bumps the minor version; anything else bumps the patch.

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
