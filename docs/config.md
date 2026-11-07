# CDE configuration

CDE reads one KDL (v2) file: `$XDG_CONFIG_HOME/cde/config.kdl`, falling back to `$HOME/.config/cde/config.kdl`. A missing file means the built-in defaults, which are CDE's behaviour with no config. An invalid file is logged at startup as an error with its line and column, and CDE then runs on the defaults. Every key is optional. KDL v2 comments are valid: `//` line comments, `/* */` block comments, and `/-` to comment out a whole node.

Units: every size key carries an explicit KDL type annotation naming its unit — tiling `gap` / `ring` / `ring-inset`, header `size` / `divider` / `inset` and cursor `size`. `(px)N` is N logical pixels and must be an integer; header `size` (a font size) also accepts `(pt)N` (integer or decimal), converted to px as N × 96 / 72 rounded half away from zero. A bare number on a size key (`gap=6`, `size 14`), an unknown unit (`(em)`, `(PX)` — units are case-sensitive) or `(pt)` on a px-only key is a located config error naming the accepted units. Other numbers (seconds, scale, keyboard repeat) stay unannotated.

Reload: WP-5 reloads on file change; until then restart.

## Schema

### `terminal`

```kdl
terminal "<command>" exec-flag="-e"
```

The program spawned by `Super+Shift+Return` (the `run-terminal` action; `Super+Return` is now the launcher, `open-launcher`; `stack-window` is retired, every launch already stacks into the focused pane). Default: unset, which keeps the built-in lookup (`$TERMINAL`, else `foot`). `exec-flag` is the flag the terminal takes to run a program (default `-e`) and is used by the compositor when it spawns its overlay and wrapper terminals (CDE exports the terminal and flag to children as `CDE_TERMINAL` and `CDE_TERMINAL_EXEC_FLAG`; the program launcher decides terminal availability from `CDE_TERMINAL` alone and never reads the flag). The command must be non-empty; it is spawned directly (no shell), so give a path or a name on `$PATH`.

When the terminal program's basename is `cterm` (crucible-term), each CDE terminal spawn (not overlays, apps, foot or any other terminal) also gets `CRUCIBLE_WINDOW_GRANT`: a single-use 32-hex grant, valid while that spawn lives. A client hosting that spawn's window presents it on the compositor global `crucible_window_identity_manager_v1` (protocol `crucible-window-identity-v1`, `protocols/crucible-window-identity-v1.xml`) with `bind_surface` before the surface gets its xdg role; CDE checks the grant, the peer's UID and executable, and that `root_pid` is the peer's child, then places and classifies the window by its spawn rather than by the connection's PID. Without a bound identity a window is placed by its client PID as before.

A granted `cterm` spawn also gets `CRUCIBLE_TERM_SESSION`, a CDE-minted 32-hex session key (CDE → cterm client; the client strips it before the child); it is saved in the terminal's workspace record (`session=`) and re-exported when that record is restored.

Loss recovery: when a bound window disappears because its Wayland client disconnected (the cterm daemon died) rather than because it was closed, CDE treats the window as lost and keeps its slot. It spawns a placeholder from the window's last recorded state (kept in memory, refreshed every second; never read from `/proc` at loss time): a terminal becomes `crucible resume --lost --cwd <dir> [-- <command>]`, which shows the restore banner with a line saying the terminal was lost (Enter re-runs the command; with no command a shell starts at once), under the same session key; a lost `crucible relaunch` placeholder comes back as the same relaunch placeholder. Each placeholder takes the lost window's position in its pane stack, whatever order they map in. While a placeholder is pending (until it maps, or 30 s), that workspace is not auto-saved, so the saved commands are never overwritten by the half-replaced pane; the next save after it maps writes the placeholder in the lost window's place. A user close, `crucible msg close` or an app's own exit is never a loss, and unbound windows (foot, other apps) close as before.

### `tiling`

```kdl
tiling {
    terminal gap=(px)6 ring=(px)3
    app gap=(px)0 ring=(px)0 fill="#737373"
    terminal-args "-o" "pad={gap}x{gap} center"
}
```

Tiles are edge to edge: they abut and cover the output, so no background shows between them. Each window class then gets its own gap and focus ring (both non-negative `(px)` integers; every key is optional):

- `terminal` — CDE-spawned terminals. The window fills its whole tile; `gap` is the padding the terminal draws itself (see `terminal-args`) and the focus ring (`ring`) is drawn inside the tile, in that padding. Defaults: `gap=(px)6`, `ring=(px)3`.
- `app` — every other window. The window is inset by `gap` on each side; the focus ring is drawn outside the window, in the gap, and the rest of the gap is painted `fill` (`"#rrggbb"`, default `#737373`) so no background shows. Defaults: `gap=(px)0`, `ring=(px)0` (no ring, nothing drawn).
- `terminal` also takes `ring-inset=(px)<n>` (default `0`): the focus ring is drawn that many pixels inside the tile edge instead of on it. `frame="#rrggbb"` (default unset) draws a frame of the same geometry (the ring position and thickness) in that colour around every **unfocused** terminal tile; the focus ring and mark ring draw over it. Both are terminal only; `app` rejects them as unknown properties.
- A `ring` larger than its `gap` is clamped to `gap`, and then `ring-inset` is clamped to `gap - ring` (each logged as a warning).
- `ring`, `ring-inset` and `frame` apply on hot reload. Terminal padding (`gap` through `terminal-args`) is a spawn argument, so it applies only to terminals spawned after the change.
- `terminal-args` — zero or more strings inserted before the exec flag on terminal spawns only (`Super+Shift+Return`, `run-in-terminal`, restored terminals, and launcher TERM rows via `run-term` -- applied only when the TERM program is the configured terminal, compared by basename; not overlays, which use `overlay.terminal-args`, and not apps). `{gap}` in an argument is replaced by the terminal `gap`. For foot (the CDE terminal), the arguments above make foot pad `gap` pixels on each side, centred, in its own background so the compositor ring never covers a cell. The ghostty equivalent (rollback) is `"--window-padding-x={gap}" "--window-padding-y={gap}" "--window-padding-color=extend-always" "--window-padding-balance=true"`, which overrides ghostty's own `window-padding`. Default: none, in which case the terminal ring is drawn over the terminal's outermost pixels.

A zellij-like look, a thin frame around each pane with the padding inside it (ring and frame sit 4px in, the text 9px in):

```kdl
tiling {
    terminal gap=(px)9 ring=(px)1 ring-inset=(px)4 frame="#3a3a3a"
    terminal-args "-o" "pad={gap}x{gap} center"
}
```

The old `gaps` node was removed; a config that still has one is rejected with an error naming `tiling`.

### `focus-follows-mouse`

```kdl
focus-follows-mouse #false
```

`#true` (default) or `#false`. When on, moving the pointer into a window or pane focuses it; when off, only clicking does. While an overlay is open, hovering does not move focus off it; when the pointer has moved into another pane or output meanwhile, the next overlay you open (or re-open) first moves focus there, so it opens on the monitor under the pointer. This replaces the old `--no-focus-follows-mouse` flag and `CDE_FOCUS_FOLLOWS_MOUSE` variable, which are gone.

### `confine-pointer-to-fullscreen`

```kdl
confine-pointer-to-fullscreen #false
```

`#true` (default) or `#false`. When on, a keyboard-focused fullscreen window (Wayland or X11) keeps the pointer on its monitor once the pointer is on it, so games cannot lose the mouse to another output. Moving focus away (the Super focus or workspace chords), opening an overlay, or locking the screen frees the pointer; a pointer already outside the monitor is never moved. It only applies on the TTY (udev) backend, not the nested winit one.

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

`slots` takes one boolean (`#true` or `#false`, at most once; default `#false`). When `#true`, picker rows for workspaces pinned to a slot (via `Super+Shift+<n>` or the `pin-slot` action) show a `⌘<n>` badge in the `slot` column before the name. The picker is a table with a header row (`kind  slot  name  root`, columns padded to the widest cell); the `slot` column is omitted when no row has a slot, e.g. `live    ⌘3    work`; folder rows never carry one, and the badge is not part of the fuzzy-filtered text.

Live and saved rows also show their **workspace root** (set with `capture-workspace-root` or the root prompt; not the `root` config folders above) in the `root` column after the name, with `$HOME` shown as `~`, e.g. `live    work  ~/code/work`; a workspace without a root leaves the column empty. The launcher is likewise a table with a `class  name` header row. A saved workspace shows the root from its saved file even if it was never opened this session.

In the picker, **Delete** on a saved (not open) row asks for confirmation (`Delete again to remove saved workspace <name>`); a second Delete on the same row moves its record to `deleted/` (see `delete-workspace`), clears its slot pins and closes the picker. Any other key cancels the confirmation. On an open workspace Delete only shows `open workspace: use delete-workspace`; folder and `new workspace` rows ignore it. Over IPC this is `delete <name>` (a live name is refused).

The **workspace-root prompt** (`set-workspace-root`, default `Super+Ctrl+Shift+D`; `crucible pick --root <path>`) opens the picker with the prompt `root> ` prefilled with the current root (`$HOME` as `~`; empty when none) and lists only the `root` config folders' subdirectories. Enter, in this order: an empty line clears the root; a line starting with `/` or `~` sets that path (a leading `~` is expanded) even if a row is selected, so the prefilled value sends itself unchanged; otherwise the selected folder row sets its path; otherwise `type a path or pick a folder` is shown. The compositor refuses a path that is not an existing absolute directory and the error appears in the prompt.

### `launcher`

```kdl
launcher {
    tui "codex-*"
    tui "lazydocker" "k9s"
}
```

Adds TUI programs to the program launcher (`Super+Return`), beside its built-in list: htop, btop, nvtop, top, nano, vim, nvim, mc, ranger, yazi, lazygit, tig, ncdu, claude, codex, glances, bottom, lazydocker, k9s, ncmpcpp, weechat, irssi, cmus and newsboat.

Each `tui` node takes one or more string arguments (no properties); each is a bare program name or a glob, non-empty, containing no `/` and no `:`. In a glob, `*` matches any run of characters (possibly empty) and is the only wildcard. Duplicate entries are collapsed; a duplicate `launcher` block or any other child is an error.

A program is listed only when it is found as an executable file on `PATH`; one that is not installed produces no row and no error. `PATH` is the one CDE spawns the launcher with (CDE's own plus the `crucible` executable's directory); there is no `PATH` setting. A glob lists every matching executable, the earlier `PATH` directory winning a duplicate name. Rows run the resolved absolute path in the configured terminal (CDE wraps it in `crucible resume --now --exit`), which closes when the program exits (unless it crashes: `resume` then shows a crash banner, and Enter reruns it), and are omitted when the terminal is unknown.

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
- `terminal-args` — extra terminal arguments, zero or more strings, inserted before the exec flag on overlay spawns only. Default: when the terminal program's basename is `ghostty` or `ghostty-cde`, CDE adds `--window-padding-color=extend-always --window-padding-balance=true --keybind=clear`; when the basename is exactly `foot`, CDE adds `-o key-bindings.<action>=none` for each of foot 1.28's default bindings and the other plain actions (foot has no `keybind=clear`), so no default binding, nor a `foot.ini` rebinding of those actions, takes a key before an overlay (`pipe-*`, `regex-*` and `color-theme-switch-*` binds in a `foot.ini` are not cleared); with an N1 style it also sets foot's background to that style's surround colour, opaque (`-o colors-dark.background=<rgb> -o colors-dark.alpha=1.0`), since foot cannot extend the edge colour into the strip the cell grid leaves, which otherwise shows as a gap inside the border; when the basename is exactly `cterm`, an N1 style sets cterm's background to that style's surround colour, opaque, with zero padding centred (`-o colors.background=<rgb> -o colors.alpha=1.0 -o 'pad=0x0 center'`), since cterm paints its padding and the strip the cell grid leaves in its own background, which otherwise shows as a gap inside the border; when the basename is exactly `urxvt` or `rxvt-unicode`, CDE adds `+sb` (no scrollbar, whatever the style), and an N1 style also sets urxvt's background to that style's surround colour and its internal border to zero (`-bg #<rgb> -b 0`), for the same reason; with any other terminal (`footclient` too), nothing. Any explicit `terminal-args` replaces the default, and an empty `terminal-args` node turns it off. These ghostty defaults fill ghostty's window padding (and the leftover strip where the cell grid does not divide the overlay evenly) with the N1 surround and spread it evenly, so the surround reaches the border on every side (verified on host-b, ghostty 1.3.1; `--background-opacity=1` is not needed), and `--keybind=clear` drops every ghostty keybind (the user config's included) so keys such as Escape reach the overlay; `--config-file=<path>` adds to the user config, while `--config-default-files=false` replaces it.

Overlay terminals are pre-started: one hidden overlay terminal (running `crucible overlay-host`) is kept ready and is handed the overlay when one opens. It uses the same `terminal` program, exec flag and `overlay.terminal-args`; changing any of them on hot reload restarts it. There is nothing to configure.

### `header`

```kdl
header {
    mode "toggle"
    start "hidden"
    font "DejaVu Sans"
    size (px)14
    divider (px)0
    slots #false
}
```

A clickable workspace strip at the top of every output, listing that output's workspaces in stack order with the active one highlighted in the focus-ring accent. Every child is optional and may appear once.

- `mode` — `"off"`, `"always"` or `"toggle"` (default); any other value (including the old `"alt"`) is a config error. `off`: no strip and no input effect; `toggle-header` does nothing. `always`: the strip is always shown and its height is reserved at the top of each output (tiles, rings and X11 placement all shrink by exactly that height). `toggle`: the strip is hidden until the `toggle-header` action (default `Super+B`) shows it; while shown it reserves its height exactly like `always` (every window is re-configured to the reflowed rects), and `Super+B` again hides it and gives the space back. `toggle-header` acts on the focused output only; each output keeps its own shown/hidden state, remembered per connector name in `$XDG_STATE_HOME/cde/session.kdl`: it survives restarts, unplugging the monitor and config mode switches (e.g. `toggle` -> `always` -> `toggle`), and a saved state wins over `start`.
- `start` — `"shown"` or `"hidden"` (default); any other value is a config error. The `toggle`-mode state of a monitor that has never been toggled (no saved state); `always` and `off` ignore it.
- `font` — a font file path (a value containing `/`) or a fontconfig family name resolved with `fc-match`. Default `"DejaVu Sans"`. If no font can be loaded the strip still draws its background and highlight, without labels (one warning is logged).
- `size` — label size, `(px)N` (an integer) or `(pt)N` (integer or decimal), from 6 to 96 px after conversion; default `(px)14`. The strip height is the size plus padding. `(pt)` converts as px = round(pt × 96 / 72), half away from zero, so `(pt)14` is 19 px, `(pt)10.5` is 14 px and `(pt)12` is 16 px; the range is checked on the converted value (`(pt)80` = 107 px is an error).
- `divider` — width of a divider drawn between adjacent workspace labels, `(px)N` from 0 to 16; default `(px)0` (off). It takes its own space between labels (never before the first or after the last), is drawn in the inactive label colour by default, never sits under the active highlight, and a click on it switches nothing. Negative, above 16, non-integer or unannotated values are config errors. Optional properties: `inset=(px)<n>` (0 to 32, default 0) trims the line that many pixels from its top and bottom, and draws no line when nothing is left; `color="#rrggbb"` (default `#cdd0d6`, the label colour) sets its colour. Both are inert when the width is 0, and invalid values are config errors. Example: `divider (px)1 inset=(px)6 color="#5c6370"`.
- `slots` — `#true` or `#false` (default `#false`). When on, the label of a workspace pinned to a slot shows the slot digit with a Super glyph (`⌘` + digit, e.g. `⌘3 name`); if the header font lacks the glyph the badge falls back to `S-3 name`. Unpinned labels are unchanged.

Clicking a workspace name focuses that output and switches it to that workspace; the click acts on release (the press and its release are consumed by the strip, never sent to the window beneath). A drag starts once the pointer has moved 5 logical pixels from the press; while it is live every strip reflows to show where the workspace will land (also across outputs), and nothing changes until the release. Dragging a name and releasing on another name on the same output moves that workspace to the other's position and reorders the stack (`session.kdl` records the new order); releasing on the same output's empty strip area moves it to the end; releasing on another output's name moves the workspace there at that position, and on another output's empty strip area appends it there (the last workspace on an output cannot be dragged away: a no-op); releasing anywhere else, or after a lock or overlay opened, does nothing. A click never hides the strip; in `toggle` mode only `toggle-header` does. With an overlay open the strip is inert: a click on it closes the overlay like any other outside click. Changes take effect on hot reload.

### `keyboard`

```kdl
keyboard {
    layouts "us" "es"
}
```

- `layouts` — the xkb layout names, in group order. Default `layouts "us"`; a `keyboard` block without `layouts` keeps the default. Zero names, an empty name, or a name containing `,` is a located config error (list layouts as separate arguments; no variants or options).
- The seat's keymap is built from the list at startup. A list xkb cannot compile (for example an unknown layout name) logs a warning and falls back to the default `us` keymap; startup never fails on it.
- Hot reload recompiles the keymap when `layouts` changed (a failed compile keeps the previous keymap). The active layout resets to the first listed layout after such a reload.
- The `set-layout <name>` action switches to the layout by its position in `layouts` (not by xkb's long layout name). A name not in the list is a logged no-op. Defaults: `Super+E` is `set-layout us` and `Super+S` is `set-layout es`; with the default `layouts "us"`, `set-layout es` does nothing.
- Chords match the keysym the active layout produces, not the physical key. Under `es`, punctuation chords (`Super+backslash`, `Super+minus`, `Super+bracketleft/right`) and the US shifted-digit `pin-slot` aliases (`Super+Shift+!` and friends) therefore follow the Spanish keys; letters and digits are unaffected. Rebind via `binds` if you want different punctuation chords under another layout.

### `idle`

```kdl
idle {
    lock-after 300
    blank-after 600
}
```

Built-in idle timers, both **off** unless configured. Each child is optional and may appear once; the value is a positive whole number of seconds. Zero, a negative number, a non-integer or an unknown child is a located config error (never rounded or clamped).

- `lock-after` — after this many seconds without input, CDE spawns the `lock-command` (the same path as the `lock` action). It fires at most once per idle period and never while the session is already locked or locking.
- `blank-after` — after this many seconds without input, CDE powers the monitors off. It fires once per idle period and may fire while the session is locked. `blank-after` may be shorter than `lock-after` (blank first, lock later while still blanked).

Idle time does not accumulate while idle is **inhibited**: when a client holds a `zwp_idle_inhibitor_v1` on a surface that is visible (mapped on an output's active workspace, or a non-workspace surface such as a layer surface, the overlay or a lock surface), or when a fullscreen window is on the active workspace of any output (this covers XWayland games that never send an inhibitor). A fullscreen window on a background workspace does not inhibit. When inhibition ends the idle period starts over.

Any input event (key, pointer, touch, gesture) restarts the idle period and wakes blanked monitors; the waking event is still delivered to the focused window as usual. A hot reload applies the new timeouts and restarts the current idle period.

Blanking powers the monitors off on the TTY (DRM) backend; under the nested `winit` backend it does nothing. CDE also advertises `ext-idle-notify-v1` and `zwp-idle-inhibit-manager-v1`, so third-party idle clients and idle-inhibiting apps work whether or not an `idle` block is present.

### `cursor`

```kdl
cursor {
    theme "Adwaita"
    size (px)24
}
```

The pointer cursor theme and size, both optional (each child may appear once). Without a child, CDE uses the `XCURSOR_THEME` / `XCURSOR_SIZE` environment variables, then `default` and 24. `theme` is the name of an installed XCursor theme (a non-empty string); `size` is `(px)N`, a whole number of logical pixels from 1 to 256 (`(pt)` is not accepted). An empty theme, a non-string theme, a zero, negative, fractional, unannotated or too large size, a repeated child or an unknown child is a located config error (never coerced or clamped).

CDE reads the theme from the installed cursor themes (`XCURSOR_PATH`, the XDG data dirs, `~/.icons`, following `Inherits=`), picks the image nearest `size × output scale` and draws it 1:1. When no theme can be loaded it draws a built-in arrow. It also advertises `wp_cursor_shape_manager_v1`, so clients can ask for a named shape. When `theme` or `size` is set, CDE exports `XCURSOR_THEME` / `XCURSOR_SIZE` to the programs it spawns (including XWayland); a hot reload rebuilds CDE's own cursor and applies the new values to later spawns only.

### `screenshot`

```kdl
screenshot {
    destination "clipboard"
    directory "~/Pictures/Screenshots"
}
```

Where a screenshot goes, both children optional (each may appear once). `destination` is `"clipboard"` (the default: the PNG becomes the clipboard selection as `image/png`, readable by Wayland and X11 apps), `"file"` (write a PNG into `directory`) or `"both"`. `directory` defaults to `~/Pictures/Screenshots`; a leading `~` is expanded to `$HOME` when the screenshot is taken, and the directory is created (with its parents) on the first file write. Files are named `cde-<YYYYmmdd-HHMMSS>.png` in local time, e.g. `cde-20261106-142530.png`; if that name exists, `-1`, `-2`, ... is appended and an existing file is never overwritten. An unknown destination value, an empty directory, a repeated child or an unknown child is a located config error. Every failure while taking a screenshot (locked session, powered-off output, encode or write error) is logged as a warning and does nothing else.

Default binds: `Super+Print` runs `screenshot` (the whole focused output, without the pointer cursor) and `Super+Shift+Print` runs `snip` (region snip; not implemented yet, it logs and does nothing). A bare `Print` is left unbound so applications receive it. Neither action runs while the session is locked.

### `output`

```kdl
output "DP-1" {
    scale 1.4167
}
```

Per-output settings, keyed by the connector name (as in `DP-1`, `HDMI-A-1`). The block may repeat, once per connector; a second block for the same connector is a config error. The only child, `scale`, is optional and may appear once.

- `scale` — a number (a float such as `1.4167` or an integer such as `2`) from 0.5 to 4.0 inclusive; anything outside the range is a config error, never clamped. The value is quantized to the nearest 1/120 (the unit of Wayland fractional scale), so `1.4167` becomes 170/120. An `output` block without `scale` means 1.

An output with no `output` block has scale 1. Every Wayland app grows with the scale, the terminal included, so retune its font size when you set one. X11 apps follow the largest scale among all configured `output` blocks (whether or not that connector is currently connected) unless `CDE_XWAYLAND_SCALE` is set, which overrides it; with no `output` blocks it is 1. XWayland's scale is fixed when XWayland starts, so a change to the largest scale reaches X11 apps after a CDE restart.

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
| `close-window` | Politely ask the focused window to close (xdg `close` for Wayland windows; `WM_DELETE_WINDOW` for X11 windows that support it — an X11 window without it is destroyed, the standard X WM close). No-op on an empty pane. Default `Super+Shift+C` |
| `run-terminal` | Spawn the terminal |
| `run <command…>` | Run a command line into the focused pane, spawned via `/bin/sh -c 'exec <command…>'` (the rest of the action text, verbatim; needs a command; the first word must resolve against `PATH`, else nothing is spawned and the keybind logs a warning). `stack-window` is retired and is rejected with an error |
| `run-in-terminal <command…>` | Run a command line in a new terminal (`crucible resume --now --cwd <dir> -- <command…>`, then an interactive shell stays open); same verbatim rule and `PATH` pre-flight as `run`. No default chord |
| `run-on <workspace> <command…>` | Run a command line in the background into workspace `<workspace>` (one word); same verbatim rule and `PATH` pre-flight as `run`. Its first window joins the end of that workspace's focused pane's stack and is focused within that stack, except in the globally focused pane, where it joins unfocused behind the current window (an empty focused pane takes it, with keyboard focus). No output's active workspace changes. A live workspace (on any output) is used as is; a saved but not live one is refused (`err saved workspace <name> exists`); an invalid name is refused as `new` refuses it; any other name is created on the focused output, appended and not shown, as an **ephemeral** workspace. Ephemeral workspaces are never written to the workspace store, `session.kdl` or `slots.kdl` (an explicit `save` skips them, `pin-slot` refuses them, a rename keeps them ephemeral, and `close-workspace` closes one without a flush); one is removed once its last window closes and nothing is pending for it, unless it is its output's only workspace. Over IPC it answers `ok <pid>`. No default chord |
| `focus <dir>` | Move pane focus |
| `move <dir>` | Swap the focused pane with its neighbour |
| `split <dir>` | Split the focused pane |
| `resize <dir>` | Shift the split divider |
| `cycle-stack <n>` | Cycle the pane's window stack by `n` (wraps; negative goes back) |
| `cycle-workspace <n>` | Switch workspace by `n` (wraps) |
| `reorder-workspace <n>` | Move the active workspace `n` places within its output's stack (clamped at the ends, no wrap; unbound by default) |
| `new-workspace` | Create and switch to a new workspace |
| `move-window-to-workspace <n>` | Send the focused window `n` workspaces over |
| `mark-window` | (default chord `Super+M`) Mark the focused window; the next press elsewhere moves it into the focused pane (any workspace or output); a press in its own pane clears the mark |
| `swap-marked-window` | (default chord `Super+Shift+M`) Like `mark-window`, but swaps: the marked window takes the focused pane's active window's place and that window takes the marked one's; with no mark it marks; into an empty pane it moves |
| `close-workspace` | Save, then close the active workspace and end its windows (refused on the output's last workspace) |
| `delete-workspace` | (unbound by default) Delete the active workspace: end its windows, move its saved record to `deleted/` (beside the `workspaces/` store, `$XDG_STATE_HOME/cde/deleted/`; a later delete of the same name replaces the earlier file) and clear its slot pins. No final save; refused on the output's last workspace and while a restore is pending. Restore a deleted workspace by moving its file back into `workspaces/` |
| `close-all-workspaces` | Save and close every open workspace, leaving one fresh workspace per output (unbound by default) |
| `move-workspace-to-output <n>` | Send the active workspace `n` outputs over (wraparound) |
| `slot <n>` | Switch to the workspace pinned to slot `n` (0-9); a closed or saved one reopens on the monitor it was last viewed on, an unpinned slot opens the workspace picker, and the workspace chosen there is pinned to `n`; a slot pinned to a workspace that is neither live nor saved opens the picker in pin mode the same way, while other open failures do nothing |
| `pin-slot <n>` | Pin the active workspace to slot `n` (0-9); pins persist in `$XDG_STATE_HOME/cde/slots.kdl` |
| `open-picker` | Open the workspace picker (`crucible pick`) |
| `open-launcher` | Open the program launcher (`crucible launch`) |
| `open-settings` | Open the settings overlay (`crucible keys`): the effective keybindings, grouped under area headings (panes & windows, stacks, workspaces, outputs, launch & overlays, session & system) until you type, then one flat filtered list; Enter runs the selected binding |
| `set-layout <name>` | Switch the keyboard to the layout `<name>` from `keyboard { layouts … }` (by position in that list; unknown name is a logged no-op). Defaults `Super+E` = `set-layout us`, `Super+S` = `set-layout es` |
| `toggle-header` | Show/hide the workspace header strip on the focused output (`header { mode "toggle" }` only; a no-op in `always` and `off`) |
| `toggle-fullscreen` | Toggle fullscreen for the focused window (see [Fullscreen](#fullscreen)). No-op on an empty pane. Default `Super+F` |
| `rename-workspace` | Rename the active workspace (the picker in rename mode) |
| `capture-workspace-root` | (default chord `Super+Shift+D`) Make the focused terminal's current directory the active **workspace root**, the folder new terminals and apps in this workspace start in (running windows are untouched). Fails and changes nothing if the focused window is not a terminal. Not the picker `root` config folders |
| `set-workspace-root` | (default chord `Super+Ctrl+Shift+D`) Open the workspace-root prompt in the picker overlay, prefilled with the current root; an empty answer clears it. `crucible msg root <path>` / `root -` does the same over IPC |
| `lock` | Lock the screen: spawn `lock-command` (default chord `Super+Escape`) |
| `screenshot` | Capture the focused output and deliver it per [`screenshot`](#screenshot) (default chord `Super+Print`) |
| `snip` | Region snip (default chord `Super+Shift+Print`; not implemented yet, logs only) |
| `none` | Unbind the chord |

`<dir>` is `left`, `right`, `up` or `down`; `<n>` is a signed integer such as `1`, `+1` or `-1`.

`crucible msg close-pid <n>` politely asks every window of process `<n>` to close (an xdg toplevel gets `close`, an X11 window gets `WM_DELETE_WINDOW`; an X11 window without `WM_DELETE_WINDOW` is destroyed) and answers `ok <count>`, or `err no window for pid <n>`. For a cterm window bound on the identity protocol, `close-pid <cterm client pid>` (the thin client CDE spawned) closes that one window, not the daemon's others; such a window is also captured (cwd, foreground command, CDE wrapper) from its root process under the daemon rather than from the client. It covers windows on hidden workspaces and lower in a pane stack, but not overlay windows. It sends no signal. The running compositor must be restarted onto a binary that knows the request, else it answers `err unknown request`.

#### Default bindings

See the table in the README.

### Window decorations

Not a config key. CDE answers every Wayland app that asks over `xdg-decoration` that the compositor draws the window frame (server-side), and draws none: windows are tiled and get CDE's focus ring instead, so toolkits such as winit drop their own title bar and controls. Apps that always draw their own frame and never ask (some GTK apps) are unaffected.

### Fullscreen

Not a config key. A window goes fullscreen when it asks (Wayland `xdg_toplevel.set_fullscreen`, or X11 `_NET_WM_STATE_FULLSCREEN`, e.g. a video player or a game) or on `toggle-fullscreen` (default `Super+F`). At most one window per workspace is fullscreen, always on its own workspace's output (an output the client names is ignored). It covers the whole output: the header strip, gaps, focus ring and the workspace's other panes are hidden and take no input, while each output's own header shown/hidden state is kept and returns when fullscreen ends. It ends when the window asks, on `toggle-fullscreen`, when the window closes or moves to another workspace or pane, and before any verb on that workspace that changes focus, splits or moves (`focus`, `move`, `split`, `cycle-stack`, a new window opening there, the mark verbs). Switching workspaces away and back keeps it. The screen lock and overlays (picker, launcher, settings) still show above a fullscreen window and take input. Fullscreen is not persisted: a saved or restored workspace is always tiled.

### X11 apps (XWayland)

Not a config key. When XWayland starts, CDE loads `$HOME/.Xresources` into it with `xrdb -merge` (when the file exists and `xrdb` is installed), as an X session's startup would, so X11 apps such as urxvt get their resources. Edits take effect on the next CDE start, or run `xrdb -merge ~/.Xresources` yourself from a CDE terminal.

## Complete example

```kdl
terminal "/home/you/.local/opt/foot/bin/foot" exec-flag="-e"
tiling {
    terminal gap=(px)6 ring=(px)3
    terminal-args "-o" "pad={gap}x{gap} center"
    app gap=(px)0 ring=(px)0
}
focus-follows-mouse #true
confine-pointer-to-fullscreen #true
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
    start "hidden"
    font "DejaVu Sans"
    size (px)14
}
idle {
    lock-after 300
    blank-after 600
}
cursor {
    theme "Adwaita"
    size (px)24
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
