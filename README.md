---
title: README
created: 2026-06-15
modified: 2026-07-19
word-count: 581
tags: ""
---

# Crucible Dev Env (CDE)

> The language-agnostic desktop environment that turns your entire developer machine into an IDE.

CDE is a **Wayland compositor**, written in Rust, that fuses three things developers normally bolt
together by hand:

- a **tiling window manager** with intuitive, explicit splits (no hidden container tree),
- a **terminal multiplexer's persistence model** — named, fuzzy-pickable workspaces that survive a
  restart,
- a **lightweight desktop environment** that owns the services raw i3 leaves to you.

## Concepts

`Output → Workspace → Pane → Window`, with **stack** as a cross-cutting modifier:

- **Output** — a physical monitor.
- **Workspace** — a named, *stackable* arrangement of panes; also the unit that is saved to disk and
  surfaced in the fuzzy startup picker.
- **Pane** — a slice of screen geometry; *stackable*, so it can hold a stack of windows you cycle through.
- **Window** — the X11/Wayland surface (or external terminal) inside a pane.

The word *tab* is intentionally reserved for applications' own internal tabs — CDE never overloads it.

## Status

Pre-alpha, but **Phases 1 & 2 of [the roadmap](plans/00-mvp-roadmap.md) are implemented**: CDE is a
working Smithay-based tiling Wayland compositor you can run today.

**What works now:**

- **Backends** — nested `winit` (single output, for development inside your current session) and
  `udev`/DRM (`--tty-udev`, multi-output, on a real TTY).
- **Tiling** — directional `split-right`/`split-down` over a binary split tree; `focus`/`move` panes
  by direction; `resize`; no hidden container climbing.
- **Stacking** — multiple windows per pane, cycled in place; a per-pane pip indicator shows your
  position in the stack.
- **Workspaces** — each output owns an independent stack of workspaces; create / switch / move windows
  between them.
- **Multi-monitor** — every monitor shows its own active workspace; hot-unplugging a monitor migrates
  its windows onto a surviving output without closing them.
- **XWayland** — X11 apps tile alongside native Wayland clients.
- **Visuals** — a focus-ring border on the focused window plus the stack pips (theming is Phase 3).

**Not yet (Phase 3+):** persistent / fuzzy-pickable workspaces, KDL config + hot-reload, interactive
workspace rename + the fuzzy picker UI, the status bar, a built-in GPU terminal, and the remaining DE
services. Keybindings and gaps/borders are currently sensible hardcoded defaults.

## Running

Requires a recent stable Rust (1.85+, edition 2024) and a terminal emulator on `$PATH` (defaults to
`foot`; set `$TERMINAL` to override — e.g. `alacritty`, `kitty`).

```sh
# Nested in your current desktop session (development):
cargo run -- --winit

# On a bare TTY (real DRM/KMS hardware, multi-output):
cargo run -- --tty-udev
```

Set `$TERMINAL` before launching if you don't have `foot` installed, e.g. `TERMINAL=alacritty cargo
run -- --winit`. Logging verbosity is controlled by `RUST_LOG` (e.g. `RUST_LOG=debug`).

Once inside, press **`Super+Return`** to spawn your first terminal, then **`Super+\`** to split. See
the full keymap below.

## Keybindings

All bindings are `Super`-prefixed (the `Super`/`Mod4`/"logo" key) and never squat shell/editor
hotkeys. Defaults (Phase 3 will make these configurable):

| Chord | Verb |
|-------|------|
| `Super+Return` | Spawn a terminal into the focused pane |
| `Super+Shift+Return` | Spawn a terminal **stacked** into the current pane |
| `Super+Q` | Quit the compositor |
| `Super+H` / `J` / `K` / `L` | Focus pane left / down / up / right |
| `Super+Shift+H` / `J` / `K` / `L` | Move (swap) the focused pane left / down / up / right |
| `Super+\` | Split right (new pane to the right) |
| `Super+-` | Split down (new pane below) |
| `Super+Tab` / `Super+Shift+Tab` | Cycle the focused pane's window stack forward / back |
| `Super+Ctrl+H` / `J` / `K` / `L` | Resize the focused pane (shift the split divider) |
| `Super+]` / `Super+[` | Next / previous workspace (on the focused output) |
| `Super+N` | New workspace (auto-named) |
| `Super+Shift+]` / `Super+Shift+[` | Move the focused window to the next / previous workspace |
| `Ctrl+Alt+F1`…`F12` | Switch virtual terminal (`--tty-udev` only) |

Splitting creates an **empty** focused pane; the next terminal you spawn (`Super+Return`) fills it.
Spawning into a pane that already holds a window **stacks** it (only the focused one is visible —
cycle with `Super+Tab`). Workspace/window verbs act on the monitor your pointer last clicked.

## Build & test

```sh
cargo build
cargo test --all    # crucible-layout's model/geometry logic is unit-tested without a compositor
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
```

The compositor crates link native libraries (libinput, libgbm, libdrm, …); see
[`plans/01-compositor-up.md`](plans/01-compositor-up.md) for the system dependencies.

## Contributing

See [AGENTS.md](AGENTS.md) for engineering principles (everything tested, modular/DRY, standard Rust
conventions, clippy + rustfmt, disciplined dependencies) and the plan-first workflow.

## License

[MIT](LICENSE)
