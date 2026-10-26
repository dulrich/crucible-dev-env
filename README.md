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

**Not yet:** the status bar, a built-in GPU terminal, and the remaining DE services.

## Running

The TTY launcher is `scripts/cde.sh`: it rebuilds first, wraps ghostty so it opens as a separate instance, logs to `/tmp/cde/latest.log`, and can be symlinked as `~/cde.sh`; `scripts/cde.sh --help` lists the env overrides.

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

Focus follows the mouse by default: hovering a window focuses it, like clicking it. Turn it off with
`focus-follows-mouse #false` in `~/.config/cde/config.kdl`; see [`docs/config.md`](docs/config.md) for the
full config schema (terminal, gaps, picker roots, rebinding chords).

Once inside, press **`Super+Shift+Return`** to spawn your first terminal, then **`Super+\`** to split. See
the full keymap below.

## Keybindings

All bindings are `Super`-prefixed (the `Super`/`Mod4`/"logo" key) and never squat shell/editor
hotkeys. Defaults (rebindable in the config file, see `docs/config.md`):

| Chord | Verb |
|-------|------|
| `Super+Return` | Open the program launcher (`crucible launch`): desktop applications and `$PATH` executables, launched into the focused pane |
| `Super+Comma` | Open the settings overlay (`crucible keys`): the effective keybindings, read-only |
| `Super+Shift+Return` | Spawn a terminal into the focused pane (the window joins the pane's stack) |
| `Super+Shift+Q` | Quit the compositor |
| `Super+W` | Close the focused pane (only if empty and not the last pane; never closes windows) |
| `Super+H` / `J` / `K` / `L` | Focus pane left / down / up / right |
| `Super+Shift+H` / `J` / `K` / `L` | Move (swap) the focused pane left / down / up / right |
| `Super+\` | Split right (new pane to the right) |
| `Super+-` | Split down (new pane below) |
| `Super+Tab` / `Super+Shift+Tab` | Cycle the focused pane's window stack forward / back |
| `Super+Ctrl+H` / `J` / `K` / `L` | Resize the focused pane (shift the split divider) |
| `Super+]` / `Super+[` | Next / previous workspace (on the focused output) |
| `Super+N` | New workspace (auto-named) |
| `Super+P` | Open the workspace picker (live, saved and folder workspaces) |
| `Super+Shift+R` | Rename the active workspace (picker in rename mode) |
| `Super+Shift+]` / `Super+Shift+[` | Move the focused window to the next / previous workspace |
| `Ctrl+Alt+F1`…`F12` | Switch virtual terminal (`--tty-udev` only) |

Splitting creates an **empty** focused pane; the next terminal you spawn (`Super+Shift+Return`) fills it.
Spawning into a pane that already holds a window **stacks** it (only the focused one is visible —
cycle with `Super+Tab`). New windows, splits and workspace verbs act where the focused window is —
the window you last clicked or (with focus-follows-mouse) hovered.

## Workspaces, persistence and the picker

Every workspace is saved automatically (about once a second, when its layout or windows changed) to `$XDG_STATE_HOME/cde/workspaces/<name>.kdl` (default `~/.local/state/cde/workspaces/`), one KDL file per workspace. A workspace with a single empty pane is not written. `Super+Shift+Q` flushes once before quitting. What is saved: the split tree, each pane's window stack, and for every terminal CDE spawned its working directory and foreground command; for other apps their app id and command line, so they come back as a relaunch placeholder (apps without a readable command line are skipped).

`Super+P` opens the **picker**: a centred terminal overlay running `crucible pick`, listing live workspaces, then saved ones, then folders under the configured `picker` roots (see `docs/config.md`). Picking a saved workspace restores it: its panes come back and each terminal reopens in its old directory, with its old command waiting behind a "press Enter to run" banner (`crucible resume`). App windows come back as a placeholder (`crucible relaunch`); Enter relaunches the app in the same slot, Ctrl+C dismisses it. Picking a folder creates a workspace named after it, rooted there: new terminals in it start in that folder. `Super+Shift+R` opens the picker in rename mode for the active workspace (its saved file is renamed too). When saved workspaces exist, the picker opens automatically at startup. Delete a workspace by deleting its file.

Overlays (the picker, the launcher and the settings view) are modal: hovering never moves focus off an open overlay, and clicking outside it closes it.

The picker talks to the compositor over a Unix socket at `$XDG_RUNTIME_DIR/cde/<wayland-socket>.sock`, exported to every child as `CDE_SOCKET`. The config file (`~/.config/cde/config.kdl`) is re-read when it changes.

**Caveat — one process per terminal window.** Restored terminals are placed back into their panes by matching the PID CDE spawned to the window's client PID (or one of its ancestors). That works for terminals that run one process per window (foot, alacritty, ghostty with `--gtk-single-instance=false`, as `cde.sh` uses). A single-instance terminal daemon breaks the match: restored windows then open in the focused pane instead, and the picker overlay lands in a pane rather than floating.

**Caveat — relaunched apps.** Exact placement of a relaunched app is guaranteed only when the relaunched command starts a fresh process that owns the window (its client PID is the spawned PID or a descendant). Single-instance apps that hand off to an already running process (e.g. a second Firefox window of one running profile) open in the focused pane instead. The saved command line is what `/proc` showed for the app (e.g. `/usr/lib/firefox-esr/firefox-esr`), not necessarily what you typed.

### CLI

- `crucible pick [--rename <name>]` — the workspace picker TUI (needs `$CDE_SOCKET`); `--rename` is a single-line rename prompt (the name is percent-encoded).
- `crucible launch` — the program launcher TUI (needs `$CDE_SOCKET`; opened by `Super+Return`): fuzzy-filter installed desktop applications and `$PATH` executables, Enter launches into the focused pane. CDE exports `CDE_TERMINAL` and `CDE_TERMINAL_EXEC_FLAG` (the configured terminal and its exec flag) to every child it spawns; the launcher uses them for `Terminal=true` entries.
- `crucible keys` — the read-only settings overlay (needs `$CDE_SOCKET`; opened by `Super+Comma`): lists the effective keybindings (config overrides applied, fetched with the `bindings` IPC request) with fuzzy filtering; edit binds in `config.kdl`.
- `crucible resume [--now] --cwd <dir> [-- <command>]` — the restored-terminal banner: Enter runs the old command (then a shell), Ctrl+C or EOF gives a shell; the command never runs without Enter. `--now` (used by `run-in-terminal`) runs it immediately; a restored terminal still comes back behind the Enter banner.
- `crucible relaunch [--app-id <id>] -- <cmdline>` — the restored-app placeholder: Enter sends `relaunch` over `$CDE_SOCKET` and the compositor swaps this window for the app in the same pane slot; Ctrl+C or EOF dismisses it. It never runs the command itself.
- `crucible msg <request…>` — send one request line to the compositor and print the reply (exit 0 `ok`, 1 `err`, 2 usage). Arguments are sent as typed, so percent-encode `%`, `:`, TAB, LF and CR yourself: `crucible msg open my%3Aname`. `crucible msg run <command…>` runs a command line into the focused pane and encodes it for you: `crucible msg run firefox --new-window`; `crucible msg run-in-terminal <command…>` runs it in a new terminal instead. Both refuse a command whose first word does not resolve (a builtin, an existing path, or a name on `PATH`) with `err unknown command <word>`; commands with shell syntax in the first word (quotes, `$`, `=`, ...) are not judged.

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
