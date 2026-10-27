# CDE configuration

CDE reads one KDL (v2) file: `$XDG_CONFIG_HOME/cde/config.kdl`, falling back to `$HOME/.config/cde/config.kdl`. A missing file means the built-in defaults, which are CDE's behaviour with no config. An invalid file is logged at startup as an error with its line and column, and CDE then runs on the defaults. Every key is optional.

Reload: WP-5 reloads on file change; until then restart.

## Schema

### `terminal`

```kdl
terminal "<command>" exec-flag="-e"
```

The program spawned by `Super+Shift+Return` (the `run-terminal` action; `Super+Return` is now the launcher, `open-launcher`; `stack-window` is retired, every launch already stacks into the focused pane). Default: unset, which keeps the built-in lookup (`$TERMINAL`, else `foot`). `exec-flag` is the flag the terminal takes to run a program (default `-e`) and is used by the program launcher to run `Terminal=true` desktop entries (CDE exports the terminal and flag to children as `CDE_TERMINAL` and `CDE_TERMINAL_EXEC_FLAG`). The command must be non-empty; it is spawned directly (no shell), so give a path or a name on `$PATH`.

### `tiling`

```kdl
tiling {
    terminal gap=6 ring=3
    app gap=0 ring=0 fill="#737373"
    terminal-args "--window-padding-x={gap}" "--window-padding-y={gap}" "--window-padding-color=extend-always" "--window-padding-balance=true"
}
```

Tiles are edge to edge: they abut and cover the output, so no background shows between them. Each window class then gets its own gap and focus ring (both non-negative integers, in pixels; every key is optional):

- `terminal` — CDE-spawned terminals. The window fills its whole tile; `gap` is the padding the terminal draws itself (see `terminal-args`) and the focus ring (`ring`) is drawn inside the tile, in that padding. Defaults: `gap=6`, `ring=3`.
- `app` — every other window. The window is inset by `gap` on each side; the focus ring is drawn outside the window, in the gap, and the rest of the gap is painted `fill` (`"#rrggbb"`, default `#737373`) so no background shows. Defaults: `gap=0`, `ring=0` (no ring, nothing drawn).
- A `ring` larger than its `gap` is clamped to `gap` (logged as a warning).
- `terminal-args` — zero or more strings inserted before the exec flag on terminal spawns only (`Super+Shift+Return`, `run-in-terminal`, restored terminals; not overlays, which use `overlay.terminal-args`, and not apps). `{gap}` in an argument is replaced by the terminal `gap`. As a ghostty example, the arguments above make ghostty pad `gap` pixels on each side in its own background so the compositor ring never covers a cell. Default: none, in which case the terminal ring is drawn over the terminal's outermost pixels.

The old `gaps` node was removed; a config that still has one is rejected with an error naming `tiling`.

### `focus-follows-mouse`

```kdl
focus-follows-mouse #false
```

`#true` (default) or `#false`. When on, moving the pointer into a window or pane focuses it; when off, only clicking does. This replaces the old `--no-focus-follows-mouse` flag and `CDE_FOCUS_FOLLOWS_MOUSE` variable, which are gone.

### `picker`

```kdl
picker {
    root "/home/me/code"
    root "/srv/work"
}
```

Directories whose immediate subdirectories become workspace-picker candidates. Zero or more `root` nodes, each one non-empty path string; there is no `~` expansion. Default: none.

### `overlay`

```kdl
overlay {
    border #true
    style "n1-med"
    terminal-args "--window-padding-color=extend-always" "--window-padding-balance=true"
}
```

Presentation of the picker, launcher and settings overlays. Every child is optional and may appear once.

- `border` — `#true` (default) or `#false`. Draws a ring outside the overlay; while an overlay is open the pane focus ring and stack pips are not drawn over it.
- `style` — `"terminal"`, `"n1-dark"`, `"n1-med"` (default) or `"n1-light"`; any other value is a config error. `terminal` keeps the terminal's own look and the teal ring; the N1 styles paint opaque surrounds with an orange ring. The chosen style is passed to overlay programs as `CDE_OVERLAY_STYLE`.
- `terminal-args` — extra terminal arguments, zero or more strings, inserted before the exec flag on overlay spawns only. Default: none. As a ghostty example only, `terminal-args "--window-padding-color=extend-always" "--window-padding-balance=true"` fills ghostty's window padding (and the leftover strip where the cell grid does not divide the overlay evenly) with the N1 surround and spreads it evenly, so the surround reaches the border on every side (verified on host-b, ghostty 1.3.1; `--background-opacity=1` is not needed); `--config-file=<path>` adds to the user config, while `--config-default-files=false` replaces it.

### `binds`

```kdl
binds {
    bind "Super+Backslash" "split right"
    bind "Super+Shift+Q" "none"
}
```

Each `bind "<chord>" "<action>"` replaces the default binding of that chord only; every other default stays. The action `none` unbinds the chord. Later entries win. A malformed entry is rejected with a message naming it, and the previous bindings stay in force. VT switching (`Ctrl+Alt+F<n>`) is hard-wired and cannot be rebound.

#### Chord grammar

`Super[+Shift][+Ctrl]+<key>`

- `Super` is mandatory; `Shift` and `Ctrl` (or `Control`) are optional, in any order, case-insensitive. A chord matches exactly its modifier set.
- `<key>` is an X11 keysym name, matched case-insensitively. Supported names: `a`-`z`, `0`-`9`, `F1`-`F12`, `Return`, `Tab`, `ISO_Left_Tab`, `backslash`, `minus`, `plus`, `equal`, `bracketleft`, `bracketright`, `space`, `Escape`, `BackSpace`, `Delete`, `Home`, `End`, `Page_Up`, `Page_Down`, `Left`, `Right`, `Up`, `Down`, `comma`, `period`, `slash`, `semicolon`, `apostrophe`, `grave`, `Print`.
- A letter binds both cases (so the chord still works under Caps Lock). `Super+Shift+Tab` also binds `ISO_Left_Tab`, which many layouts deliver for it.

#### Action grammar

| Action | Effect |
|---|---|
| `quit` | Stop the compositor |
| `close-pane` | Close the focused pane (only if empty and not the last) |
| `run-terminal` | Spawn the terminal |
| `run <command…>` | Run a command line into the focused pane, spawned via `/bin/sh -c 'exec <command…>'` (the rest of the action text, verbatim; needs a command; the first word must resolve against `PATH`, else nothing is spawned and the keybind logs a warning). `stack-window` is retired and is rejected with an error |
| `run-in-terminal <command…>` | Run a command line in a new terminal (`crucible resume --now --cwd <dir> -- <command…>`, then an interactive shell stays open); same verbatim rule and `PATH` pre-flight as `run`. No default chord |
| `focus <dir>` | Move pane focus |
| `move <dir>` | Swap the focused pane with its neighbour |
| `split <dir>` | Split the focused pane |
| `resize <dir>` | Shift the split divider |
| `cycle-stack <n>` | Cycle the pane's window stack by `n` (wraps; negative goes back) |
| `cycle-workspace <n>` | Switch workspace by `n` (wraps) |
| `new-workspace` | Create and switch to a new workspace |
| `move-window-to-workspace <n>` | Send the focused window `n` workspaces over |
| `close-workspace` | Save, then close the active workspace and end its windows (refused on the output's last workspace) |
| `move-workspace-to-output <n>` | Send the active workspace `n` outputs over (wraparound) |
| `slot <n>` | Switch to the workspace pinned to slot `n` (0-9); a closed or saved one reopens on the monitor it was last viewed on, an unpinned slot does nothing |
| `pin-slot <n>` | Pin the active workspace to slot `n` (0-9); pins persist in `$XDG_STATE_HOME/cde/slots.kdl` |
| `open-picker` | Open the workspace picker (`crucible pick`) |
| `open-launcher` | Open the program launcher (`crucible launch`) |
| `open-settings` | Open the read-only settings overlay (`crucible keys`): the effective keybindings |
| `rename-workspace` | Rename the active workspace (the picker in rename mode) |
| `none` | Unbind the chord |

`<dir>` is `left`, `right`, `up` or `down`; `<n>` is a signed integer such as `1`, `+1` or `-1`.

#### Default bindings

See the table in the README.

## Complete example

```kdl
terminal "/tmp/cde/ghostty-cde" exec-flag="-e"
tiling {
    terminal gap=6 ring=3
    app gap=0 ring=0
}
focus-follows-mouse #true
picker {
    root "<code-root>"
}
overlay {
    border #true
    style "n1-med"
    terminal-args "--window-padding-color=extend-always" "--window-padding-balance=true"
}
binds {
    bind "Super+P" "open-picker"
    bind "Super+Backslash" "split right"
    bind "Super+Shift+Q" "none"
    bind "Super+Ctrl+F5" "cycle-workspace -1"
}
```
