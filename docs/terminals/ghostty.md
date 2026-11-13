# ghostty

**Support level: basic.** Version 1.3.1 (the version of the stale data). Mode: standalone. Measurements: [ghostty row of the matrix](../spikes/terminal-matrix/WP-1-FINDINGS.md#matrix).

ghostty is not installed on the measurement host and was not reinstalled, so it is rated basic from the 2026-10-31 data, which is **stale** (different method, an older compositor; do not compare it with the other rows). Its theme fragment and padding defaults are built and unit-tested, but not exercised against a running ghostty in this round.

## Theme

Every `theme` key maps: `font-family`, `font-size`, `foreground`, `background`, `cursor-color`, `background-opacity` and `palette = i=#rrggbb` ([table in docs/config.md](../config.md#theme)). Nothing is inexpressible.

ghostty has no include from its own config here, so CDE passes the fragment: with a `theme` block it adds `--config-file=~/.config/cde/terminal-theme/ghostty.conf` to every terminal and overlay spawn of `ghostty` or `ghostty-cde`. A ghostty you start yourself does not get it.

## Standalone or daemon

Only standalone was measured, stale: first frame 360.4 ms (n=5, client-side method), 56.8 to 58.7 MiB PSS at one window, 1.728 s for the 50 MB `cat`. No daemon mode is supported.

## CDE settings

```kdl
terminal "ghostty" exec-flag="-e"
```

Built-in `terminal-args` default: `--window-padding-x={gap} --window-padding-y={gap} --window-padding-color=extend-always --window-padding-balance=true` (overrides ghostty's own `window-padding`). Override it with your own `terminal-args` node; an empty node turns it off ([docs/config.md](../config.md)).

## Minimal command carve-out

ghostty ships many default keybinds (not enumerated here). For overlays CDE adds `--keybind=clear`, which drops every ghostty keybind (the user config's included) so keys such as Escape reach the overlay (`overlay.terminal-args` in [docs/config.md](../config.md)). Ordinary windows keep their bindings; clear a colliding one with `keybind = <chord>=unbind` in your ghostty config.
