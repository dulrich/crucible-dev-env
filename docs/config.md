# CDE configuration

CDE reads one KDL (v2) file: `$XDG_CONFIG_HOME/cde/config.kdl`, falling back to `$HOME/.config/cde/config.kdl`. A missing file means the built-in defaults, which are CDE's behaviour with no config. An invalid file is logged at startup as an error with its line and column, and CDE then runs on the defaults. Every key is optional. KDL v2 comments are valid: `//` line comments, `/* */` block comments, and `/-` to comment out a whole node.

Reload: WP-5 reloads on file change; until then restart.

## Schema

### `terminal`

```kdl
terminal "<command>" exec-flag="-e"
```

The program spawned by `Super+Shift+Return` (the `run-terminal` action; `Super+Return` is now the launcher, `open-launcher`; `stack-window` is retired, every launch already stacks into the focused pane). Default: unset, which keeps the built-in lookup (`$TERMINAL`, else `foot`). `exec-flag` is the flag the terminal takes to run a program (default `-e`) and is used by the compositor when it spawns its overlay and wrapper terminals (CDE exports the terminal and flag to children as `CDE_TERMINAL` and `CDE_TERMINAL_EXEC_FLAG`; the program launcher decides terminal availability from `CDE_TERMINAL` alone and never reads the flag). The command must be non-empty; it is spawned directly (no shell), so give a path or a name on `$PATH`.

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
- `terminal-args` — zero or more strings inserted before the exec flag on terminal spawns only (`Super+Shift+Return`, `run-in-terminal`, restored terminals, and launcher TERM rows via `run-term` -- applied only when the TERM program is the configured terminal, compared by basename; not overlays, which use `overlay.terminal-args`, and not apps). `{gap}` in an argument is replaced by the terminal `gap`. As a ghostty example, the arguments above make ghostty pad `gap` pixels on each side in its own background so the compositor ring never covers a cell. Default: none, in which case the terminal ring is drawn over the terminal's outermost pixels.

The old `gaps` node was removed; a config that still has one is rejected with an error naming `tiling`.

### `focus-follows-mouse`

```kdl
focus-follows-mouse #false
```

`#true` (default) or `#false`. When on, moving the pointer into a window or pane focuses it; when off, only clicking does. While an overlay is open, hovering does not move focus off it; when the pointer has moved into another pane or output meanwhile, the next overlay you open (or re-open) first moves focus there, so it opens on the monitor under the pointer. This replaces the old `--no-focus-follows-mouse` flag and `CDE_FOCUS_FOLLOWS_MOUSE` variable, which are gone.

### `restore-session`

```kdl
restore-session #false
```

`#true` (default) or `#false`. When on, CDE records the open workspaces per output and the focused one in `$XDG_STATE_HOME/cde/session.kdl` (beside the `workspaces/` store) and reopens them at startup, with no seed workspace and no picker. Workspaces no longer saved on disk are skipped; workspaces of a disconnected output open on the first output. With nothing restorable, or when off, startup opens a seed workspace and the picker (when saved workspaces exist) as before.

### `lock-command`

```kdl
lock-command "swaylock -f -c 000000"
```

The screen locker the `lock` action (default `Super+Escape`) spawns, detached (not into a pane). Default `"swaylock"`; must not be empty. The string is split on whitespace into the program and its arguments (no shell quoting). A failed spawn only logs a warning.

#### Screen lock

CDE implements `ext-session-lock-v1`, so any Wayland locker (swaylock, gtklock, waylock, …) works. While locked only the locker is shown (over black) and only it receives input; no CDE chord runs except VT switching (`Ctrl+Alt+F<n>`). If the locker dies the screen stays black and locked; running the locker again (from a VT, or over ssh with `WAYLAND_DISPLAY` set) takes over the lock, and unlocking it unlocks the session.

### `autostart`

```kdl
autostart "dbus-update-activation-environment --systemd WAYLAND_DISPLAY DISPLAY XDG_CURRENT_DESKTOP XDG_SESSION_TYPE"
autostart "nm-applet & sleep 1; some-daemon >> /tmp/some-daemon.log 2>&1"
```

A shell command line to run at startup. The node is repeatable: each one takes exactly one non-empty string, and they run in file order (no deduplication). Each runs once per CDE start via `/bin/sh -c`, after XWayland is ready (or has failed to start), so `DISPLAY` is already exported when XWayland works. Because it goes through `sh -c`, compound lines (`a; b`, redirects) work. Edits take effect on the next CDE start: a config hot reload never re-runs autostart. Each command inherits `WAYLAND_DISPLAY`, `DISPLAY`, `CDE_SOCKET` and the XDG session variables (`XDG_CURRENT_DESKTOP=CDE`, `XDG_SESSION_TYPE=wayland`). Intended for background programs: a window an autostart command opens is placed and saved like any other app window. A failed spawn only logs a warning and the remaining entries still run. See [`session.md`](session.md).

### `picker`

```kdl
picker {
    root "/home/me/code"
    root "/srv/work"
    slots #true
}
```

Directories whose immediate subdirectories become workspace-picker candidates. Zero or more `root` nodes, each one non-empty path string; there is no `~` expansion. Default: none.

`slots` takes one boolean (`#true` or `#false`, at most once; default `#false`). When `#true`, picker rows for workspaces pinned to a slot (via `Super+Shift+<n>` or the `pin-slot` action) show a `⌘<n>` badge before the name, e.g. `live   ⌘3 work`; folder rows never carry one, and the badge is not part of the fuzzy-filtered text.

### `launcher`

```kdl
launcher {
    tui "codex-*"
    tui "lazydocker" "k9s"
}
```

Adds TUI programs to the program launcher (`Super+Return`), beside its built-in list: htop, btop, nvtop, top, nano, vim, nvim, mc, ranger, yazi, lazygit, tig, ncdu, claude, codex, glances, bottom, lazydocker, k9s, ncmpcpp, weechat, irssi, cmus and newsboat.

Each `tui` node takes one or more string arguments (no properties); each is a bare program name or a glob, non-empty, containing no `/` and no `:`. In a glob, `*` matches any run of characters (possibly empty) and is the only wildcard. Duplicate entries are collapsed; a duplicate `launcher` block or any other child is an error.

A program is listed only when it is found as an executable file on `PATH`; one that is not installed produces no row and no error. `PATH` is the one CDE spawns the launcher with (CDE's own plus the `crucible` executable's directory); there is no `PATH` setting. A glob lists every matching executable, the earlier `PATH` directory winning a duplicate name. Rows run the resolved absolute path in the configured terminal (CDE wraps it in `crucible resume --now --exit`), which closes when the program exits, and are omitted when the terminal is unknown.

A program is not listed twice: if a desktop entry's `Exec` starts with the same program name (compared by basename, e.g. `Exec=htop`), only the desktop entry is shown. Wrapper commands such as `Exec=env FOO=1 vim` are not recognised, so such a program can appear twice.

Desktop entries whose `Categories` contain `TerminalEmulator` show as `TERM` rows. They launch the bare emulator program (the first word of `Exec`; desktop arguments are dropped) as a CDE terminal through the `run-term` request, so the window gets the terminal gap and ring and is saved and restored as a terminal. TERM rows are shown even when the terminal is unknown, and TERM wins over `Terminal=true`. A TERM emulator that is not the configured terminal is restored with the configured terminal, keeping its cwd and command. The program must resolve to an executable on `PATH` (or an executable path), else `run-term` answers `err unknown command <word>`.

Enter with no matching row shows `unknown app <query>`, and Enter on an empty `!` command shows `type a command after !`; both leave the launcher open and clear on the next edit. `!bogus` still sends, and the compositor's `unknown command bogus` reply shows.

A config save takes effect on the next launcher open (the pre-warmed launcher is restarted when these entries change).

### `overlay`

```kdl
overlay {
    border #true
    style "n1-med"
}
```

Presentation of the picker, launcher and settings overlays. Every child is optional and may appear once.

- `border` — `#true` (default) or `#false`. Draws a ring outside the overlay; while an overlay is open the pane focus ring and stack pips are not drawn over it.
- `style` — `"terminal"`, `"n1-dark"`, `"n1-med"` (default) or `"n1-light"`; any other value is a config error. `terminal` keeps the terminal's own look and the teal ring; the N1 styles paint opaque surrounds with an orange ring. The chosen style is passed to overlay programs as `CDE_OVERLAY_STYLE`.
- `terminal-args` — extra terminal arguments, zero or more strings, inserted before the exec flag on overlay spawns only. Default: when the terminal program's basename is `ghostty` or `ghostty-cde`, CDE adds `--window-padding-color=extend-always --window-padding-balance=true`; with any other terminal, nothing. Any explicit `terminal-args` replaces the default, and an empty `terminal-args` node turns it off. These ghostty defaults fill ghostty's window padding (and the leftover strip where the cell grid does not divide the overlay evenly) with the N1 surround and spread it evenly, so the surround reaches the border on every side (verified on host-b, ghostty 1.3.1; `--background-opacity=1` is not needed); `--config-file=<path>` adds to the user config, while `--config-default-files=false` replaces it.

Overlay terminals are pre-started: one hidden overlay terminal (running `crucible overlay-host`) is kept ready and is handed the overlay when one opens. It uses the same `terminal` program, exec flag and `overlay.terminal-args`; changing any of them on hot reload restarts it. There is nothing to configure.

### `header`

```kdl
header {
    mode "toggle"
    font "DejaVu Sans"
    size 14
    divider 0
    slots #false
}
```

A clickable workspace strip at the top of every output, listing that output's workspaces in stack order with the active one highlighted in the focus-ring accent. Every child is optional and may appear once.

- `mode` — `"off"`, `"always"` or `"toggle"` (default); any other value (including the old `"alt"`) is a config error. `off`: no strip and no input effect; `toggle-header` does nothing. `always`: the strip is always shown and its height is reserved at the top of each output (tiles, rings and X11 placement all shrink by exactly that height). `toggle`: the strip is hidden until the `toggle-header` action (default `Super+B`) shows it; while shown it reserves its height exactly like `always` (every window is re-configured to the reflowed rects), and `Super+B` again hides it and gives the space back.
- `font` — a font file path (a value containing `/`) or a fontconfig family name resolved with `fc-match`. Default `"DejaVu Sans"`. If no font can be loaded the strip still draws its background and highlight, without labels (one warning is logged).
- `size` — label size in pixels, an integer from 6 to 96; default 14. The strip height is the size plus padding. The size is in logical pixels, while terminals usually size fonts in points; at scale 1 the conversion is px ≈ pt × 96/72 (14 pt ≈ 19 px).
- `divider` — width in logical pixels of a divider drawn between adjacent workspace labels, an integer from 0 to 16; default 0 (off). It takes its own space between labels (never before the first or after the last), is drawn in the inactive label colour by default, never sits under the active highlight, and a click on it switches nothing. Negative, above 16 or non-integer values are config errors. Optional properties: `inset=<px>` (0 to 32, default 0) trims the line that many pixels from its top and bottom, and draws no line when nothing is left; `color="#rrggbb"` (default `#cdd0d6`, the label colour) sets its colour. Both are inert when the width is 0, and invalid values are config errors. Example: `divider 1 inset=6 color="#5c6370"`.
- `slots` — `#true` or `#false` (default `#false`). When on, the label of a workspace pinned to a slot shows the slot digit with a Super glyph (`⌘` + digit, e.g. `⌘3 name`); if the header font lacks the glyph the badge falls back to `S-3 name`. Unpinned labels are unchanged.

Clicking a workspace name focuses that output and switches it to that workspace (the press and its release are consumed by the strip, never sent to the window beneath). A click never hides the strip; in `toggle` mode only `toggle-header` does. With an overlay open the strip is inert: a click on it closes the overlay like any other outside click. Changes take effect on hot reload.

### `output`

```kdl
output "DP-1" {
    scale 1.4167
}
```

Per-output settings, keyed by the connector name (as in `DP-1`, `HDMI-A-1`). The block may repeat, once per connector; a second block for the same connector is a config error. The only child, `scale`, is optional and may appear once.

- `scale` — a number (a float such as `1.4167` or an integer such as `2`) from 0.5 to 4.0 inclusive; anything outside the range is a config error, never clamped. The value is quantized to the nearest 1/120 (the unit of Wayland fractional scale), so `1.4167` becomes 170/120. An `output` block without `scale` means 1.

An output with no `output` block has scale 1. Every Wayland app grows with the scale, ghostty included, so retune its `font-size` when you set one. X11 apps follow the largest scale among all configured `output` blocks (whether or not that connector is currently connected) unless `CDE_XWAYLAND_SCALE` is set, which overrides it; with no `output` blocks it is 1. XWayland's scale is fixed when XWayland starts, so a change to the largest scale reaches X11 apps after a CDE restart.

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
| `mark-window` | (default chord `Super+M`) Mark the focused window; the next press elsewhere moves it into the focused pane (any workspace or output); a press in its own pane clears the mark |
| `swap-marked-window` | (default chord `Super+Shift+M`) Like `mark-window`, but swaps: the marked window takes the focused pane's active window's place and that window takes the marked one's; with no mark it marks; into an empty pane it moves |
| `close-workspace` | Save, then close the active workspace and end its windows (refused on the output's last workspace) |
| `close-all-workspaces` | Save and close every open workspace, leaving one fresh workspace per output (unbound by default) |
| `move-workspace-to-output <n>` | Send the active workspace `n` outputs over (wraparound) |
| `slot <n>` | Switch to the workspace pinned to slot `n` (0-9); a closed or saved one reopens on the monitor it was last viewed on, an unpinned slot opens the workspace picker, and the workspace chosen there is pinned to `n`; a slot pinned to a workspace that is neither live nor saved opens the picker in pin mode the same way, while other open failures do nothing |
| `pin-slot <n>` | Pin the active workspace to slot `n` (0-9); pins persist in `$XDG_STATE_HOME/cde/slots.kdl` |
| `open-picker` | Open the workspace picker (`crucible pick`) |
| `open-launcher` | Open the program launcher (`crucible launch`) |
| `open-settings` | Open the settings overlay (`crucible keys`): the effective keybindings; Enter runs the selected binding |
| `toggle-header` | Show/hide the workspace header strip (`header { mode "toggle" }` only; a no-op in `always` and `off`) |
| `rename-workspace` | Rename the active workspace (the picker in rename mode) |
| `lock` | Lock the screen: spawn `lock-command` (default chord `Super+Escape`) |
| `none` | Unbind the chord |

`<dir>` is `left`, `right`, `up` or `down`; `<n>` is a signed integer such as `1`, `+1` or `-1`.

#### Default bindings

See the table in the README.

### Window decorations

Not a config key. CDE answers every Wayland app that asks over `xdg-decoration` that the compositor draws the window frame (server-side), and draws none: windows are tiled and get CDE's focus ring instead, so toolkits such as winit drop their own title bar and controls. Apps that always draw their own frame and never ask (some GTK apps) are unaffected.

### X11 apps (XWayland)

Not a config key. When XWayland starts, CDE loads `$HOME/.Xresources` into it with `xrdb -merge` (when the file exists and `xrdb` is installed), as an X session's startup would, so X11 apps such as urxvt get their resources. Edits take effect on the next CDE start, or run `xrdb -merge ~/.Xresources` yourself from a CDE terminal.

## Complete example

```kdl
terminal "/tmp/cde/ghostty-cde" exec-flag="-e"
tiling {
    terminal gap=6 ring=3
    app gap=0 ring=0
}
focus-follows-mouse #true
restore-session #true
picker {
    root "<code-root>"
}
overlay {
    border #true
    style "n1-med"
}
header {
    mode "toggle"
    font "DejaVu Sans"
    size 14
}
output "DP-1" { scale 1.4167 }
autostart "dbus-update-activation-environment --systemd WAYLAND_DISPLAY DISPLAY XDG_CURRENT_DESKTOP XDG_SESSION_TYPE"
binds {
    bind "Super+P" "open-picker"
    bind "Super+Backslash" "split right"
    bind "Super+Shift+Q" "none"
    bind "Super+Ctrl+F5" "cycle-workspace -1"
}
```
