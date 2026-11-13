# foot

**Support level: full.** Version 1.28.0. Modes: standalone, and server (`foot --server` + `footclient`). Measurements: [matrix](../spikes/terminal-matrix/WP-1-FINDINGS.md#matrix).

## Theme

Every `theme` key maps ([table in docs/config.md](../config.md#theme)): `font` to `[main] font=`, the rest to `[colors-dark]` (`foreground`, `background`, `alpha`, `regular0-7` / `bright0-7`). One caveat: foot's `cursor=` takes the text colour and the cursor colour together, so CDE uses the theme's `background` as the text colour, or foot's default `002b36` when the theme sets none.

CDE writes `~/.config/cde/terminal-theme/foot.ini`; foot reads it through `include`. Add to `~/.config/foot/foot.ini`:

```ini
include=~/.config/cde/terminal-theme/foot.ini
```

Keys after the include override the theme.

## Standalone or server

Measured on host-a: warm first frame 9 ms standalone against 11.7 ms with a server (107 ms when the server must be started first); 10-window PSS 139.0 MiB standalone against 119.1 MiB with the server (2.7 MiB resident idle, 11.6 MiB per added window against 13.7); idle CPU 0 ticks against 9 ticks over 60 s (0.15 % of a core, within noise at n=1). Under a flooding neighbour the server's new-window latency rose from 11.7 to 16.3 ms. Standalone is the simpler default; use the server for the lower memory per window. `footclient` gets no built-in padding (set `terminal-args program="footclient"` yourself).

## CDE settings

```kdl
terminal "foot" exec-flag="-e"
```

Built-in `terminal-args` default: `-o "pad={gap}x{gap} center"`. Override it with your own `terminal-args` node; an empty node turns it off ([docs/config.md](../config.md)).

## Minimal command carve-out

foot's own defaults bind `Ctrl+Shift` chords (copy and paste, search, font size, spawn-terminal, URL and unicode input, prompt jump) and `Shift+PageUp` / `Shift+PageDown` for scrollback. foot has no Super defaults, so CDE's Super chords are not taken, but `Ctrl+Shift` chords never reach a full-screen app. foot has no `keybind=clear`; the overlay defaults set every default action to `none` with `-o key-bindings.<action>=none` (see `overlay.terminal-args` in [docs/config.md](../config.md)), which also overrides a `foot.ini` rebinding of those actions (`pipe-*`, `regex-*` and `color-theme-switch-*` binds are not cleared). For ordinary windows, clear a colliding action in your own `foot.ini` `[key-bindings]` section (`<action>=none`).
