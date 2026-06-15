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

Default keybindings are `Super`-prefixed and never squat shell/editor hotkeys: `split-right` /
`split-down` create a new pane, `stack-window` pushes onto the focused pane's stack.

## Status

Pre-alpha. Building toward the MVP described in [`plans/00-mvp-roadmap.md`](plans/00-mvp-roadmap.md):
a Smithay-based compositor (with XWayland) that tiles external terminals and native apps, with
explicit splits/stacks, Super-leader hotkeys, multi-monitor support, and persistent fuzzy-pickable
workspaces. A built-in GPU terminal, status bar, settings UI, and the remaining DE services are on the
roadmap.

## Build & run

Requires a recent stable Rust (1.85+, edition 2024).

```sh
cargo build
cargo test          # the layout/persist/config logic is unit-tested without a compositor
cargo fmt --check
cargo clippy -- -D warnings
```

During development the compositor will run nested via a `winit` backend inside your current session;
on a real TTY it uses the `udev`/DRM backend (Phase 1+).

## Contributing

See [AGENTS.md](AGENTS.md) for engineering principles (everything tested, modular/DRY, standard Rust
conventions, clippy + rustfmt, disciplined dependencies) and the plan-first workflow.

## License

[MIT](LICENSE)
