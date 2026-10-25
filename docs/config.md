# CDE configuration

CDE reads one KDL (v2) file: `$XDG_CONFIG_HOME/cde/config.kdl`, falling back to `$HOME/.config/cde/config.kdl`. A missing file means the built-in defaults, which are CDE's behaviour with no config. An invalid file is logged at startup as an error with its line and column, and CDE then runs on the defaults. Every key is optional.

Reload: WP-5 reloads on file change; until then restart.

## Schema

### `terminal`

```kdl
terminal "<command>" exec-flag="-e"
```

The program spawned by `Super+Return` and `Super+Shift+Return` (and the `run-terminal` / `stack-window` actions). Default: unset, which keeps the built-in lookup (`$TERMINAL`, else `foot`). `exec-flag` is the flag the terminal takes to run a program (default `-e`) and is used by later launchers. The command must be non-empty; it is spawned directly (no shell), so give a path or a name on `$PATH`.

### `gaps`

```kdl
gaps inner=6 outer=8
```

Pixel gaps between sibling panes (`inner`) and around the tiled area of each output (`outer`). Both are non-negative integers; either may be omitted. Defaults: `inner=6`, `outer=8`.

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
| `stack-window` | Spawn a terminal stacked into the focused pane |
| `focus <dir>` | Move pane focus |
| `move <dir>` | Swap the focused pane with its neighbour |
| `split <dir>` | Split the focused pane |
| `resize <dir>` | Shift the split divider |
| `cycle-stack <n>` | Cycle the pane's window stack by `n` (wraps; negative goes back) |
| `cycle-workspace <n>` | Switch workspace by `n` (wraps) |
| `new-workspace` | Create and switch to a new workspace |
| `move-window-to-workspace <n>` | Send the focused window `n` workspaces over |
| `open-picker` | Open the workspace picker (not available until WP-5) |
| `rename-workspace` | Rename the active workspace (not available until WP-5) |
| `none` | Unbind the chord |

`<dir>` is `left`, `right`, `up` or `down`; `<n>` is a signed integer such as `1`, `+1` or `-1`.

#### Default bindings

See the table in the README.

## Complete example

```kdl
terminal "/tmp/cde/ghostty-cde" exec-flag="-e"
gaps inner=8 outer=8
focus-follows-mouse #true
picker {
    root "<code-root>"
}
binds {
    bind "Super+P" "open-picker"
    bind "Super+Backslash" "split right"
    bind "Super+Shift+Q" "none"
    bind "Super+Ctrl+F5" "cycle-workspace -1"
}
```
