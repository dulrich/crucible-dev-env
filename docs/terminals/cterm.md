# cterm (crucible-term)

**Support level: full.** Version 0.5.1. Modes: standalone and daemon. Measurements: [matrix](../spikes/terminal-matrix/WP-1-FINDINGS.md#matrix).

## Theme

Every `theme` key maps: `font`, `foreground`, `background`, `cursor`, `alpha` and `palette` all become a `font` node and a `colors { }` block in `cterm.kdl` ([table in docs/config.md](../config.md#theme)). Nothing is inexpressible.

CDE writes `~/.config/cde/terminal-theme/cterm.kdl`; cterm reads it through `include`. Add to `~/.config/crucible-term/config.kdl`:

```kdl
include "~/.config/cde/terminal-theme/cterm.kdl"
```

Keys after the `include` override the theme. cterm reads included files per window (and at daemon start), so a changed theme applies to the next window.

## Standalone or daemon

Daemon mode is chosen in cterm's own config (`mode "daemon"`, or `-o mode=daemon` in `terminal-args`); the daemon starts on demand and stays resident. Measured on host-a: warm first frame 7 ms standalone against 6 ms with the daemon, and 9 ms for a cold daemon start; 10-window PSS 102.2 MiB standalone against 81.2 MiB with the daemon (18.1 MiB resident with no window, 5.7 MiB per added window against 9.5); idle CPU 0 ticks in both. Daemon mode also lets a session survive a lost display connection. Prefer the daemon unless you want no resident process.

## CDE settings

```kdl
terminal "cterm" exec-flag="-e"
```

cterm gets the built-in `terminal-args` default `-o "pad={gap}x{gap} center"`. Override it with your own `tiling { terminal-args ... }`; an empty `terminal-args` node turns it off ([docs/config.md](../config.md)). CDE also grants each cterm spawn a window identity, which is what places and restores its windows.

## Minimal command carve-out

cterm has few fixed bindings and no rebinding: `Ctrl+Shift+C` and `Ctrl+Shift+V` copy and paste, and `Shift+PageUp`, `Shift+PageDown`, `Shift+Home` and `Shift+End` scroll the viewport. They are handled only with exactly those modifiers, so no Super chord is taken: CDE's Super chords never reach cterm, and no common application Ctrl chord (`Ctrl+C`, `Ctrl+D`, `Ctrl+R`, ...) is consumed. The only collision is a full-screen app that wants plain `Ctrl+Shift+C/V` or `Shift+PageUp`. Nothing needs clearing for overlays; cterm's overlay defaults are only the opaque background and zero padding (`overlay.terminal-args`). Source: crucible-term key handling and README.
