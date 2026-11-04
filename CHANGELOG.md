# Changelog

All notable changes to Crucible Dev Env are recorded here, in the [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) style.

CDE uses pre-1.0 semantic versioning: a breaking change to the config, `session.kdl` or the IPC protocol bumps the minor version; anything else bumps the patch.

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
