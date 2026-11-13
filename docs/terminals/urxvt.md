# urxvt (rxvt-unicode)

**Support level: full (standalone).** Version rxvt-unicode 9.31. Modes: standalone; daemon (`urxvtd` + `urxvtc`) is not rated. Measurements: [matrix](../spikes/terminal-matrix/WP-1-FINDINGS.md#matrix).

Why: exec, cwd, padding, restore and the overlay host all pass standalone. In daemon mode the restore check was inconclusive ([integration checks](../spikes/terminal-matrix/WP-1-FINDINGS.md#integration-checks)), so the daemon is listed but not rated. urxvt is an X11 client: it runs through XWayland.

## Theme

`font` (`URxvt.font: xft:<family>:size=<N>`), `foreground`, `background`, `cursor` (`URxvt*cursorColor`) and `palette` (`*color0-15`) map. `alpha` is only partly expressible: it becomes a `[NN]` percentage prefix on `URxvt*background`, needs `background` in the theme, and shows only with a 32-bit visual (`URxvt.depth: 32` in your own `~/.Xresources`; the theme never emits it). See [docs/config.md](../config.md#theme).

CDE merges the fragment itself: at XWayland start it runs `xrdb -merge` on `~/.Xresources` then `~/.config/cde/terminal-theme/urxvt.Xresources`, so the theme wins, and re-merges on a reload. Open windows keep their colours.

## Standalone or daemon

Measured on host-a: warm first frame about 40 ms standalone against 14 ms with `urxvtd` (poll method, coarse), 142 ms when the daemon starts cold. PSS is unreadable because urxvt is setgid on the host; RSS per window was 39.1 MiB standalone against 14.0 MiB with the daemon. Idle CPU is at most 2 ticks over 60 s in both. The daemon is faster and smaller, but its restore is unverified; use standalone for a supported setup. `urxvtc` gets no built-in padding.

## CDE settings

```kdl
terminal "urxvt" exec-flag="-e"
```

Built-in `terminal-args` default: `-b {gap}` (internal border). Override it with your own `terminal-args` node; an empty node turns it off ([docs/config.md](../config.md)). The cwd flag is `-cd`.

## Minimal command carve-out

urxvt takes no Super chords. Its defaults that matter: `Shift+PageUp` / `Shift+PageDown` scroll the scrollback, and the ISO 14755 unicode-input mode starts on `Ctrl+Shift`, which swallows `Ctrl+Shift` chords. Disable the latter in `~/.Xresources` with `URxvt.iso14755: false` and `URxvt.iso14755_52: false` (urxvt manual; not exercised in the matrix). The scrollbar is cleared for overlays by the overlay default `+sb` (`overlay.terminal-args` in [docs/config.md](../config.md)); add `URxvt.scrollBar: false` for ordinary windows.
