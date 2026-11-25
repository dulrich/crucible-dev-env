# Driving CDE from scripts

Any program can control a running CDE through its IPC socket, using the `crucible msg` client. This is the interface for external tools such as a time-of-day or mood script driving the night light.

## The socket

CDE listens on a Unix socket at `$XDG_RUNTIME_DIR/cde/<wayland-socket>.sock` (for example `/run/user/1000/cde/wayland-1.sock`) and exports its path to every process it starts as `CDE_SOCKET`. Anything CDE launches — terminals, `autostart` commands, `run` / `run-on` jobs — therefore already has it.

A process started outside CDE (a systemd user timer, cron, an ssh session) does not, and `crucible msg` exits 2 without it. Set it yourself:

```sh
: "${CDE_SOCKET:=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/cde/${WAYLAND_DISPLAY:-wayland-1}.sock}"
export CDE_SOCKET
```

With more than one CDE instance (for example a nested `crucible --winit` test session), each has its own socket; pick the one whose Wayland socket you mean.

## `crucible msg`

`crucible msg <request…>` sends one request line and prints the one-line reply. `crucible msg --help` is the authoritative list of requests; the ones scripts use most:

- `exec <action>` — run any keybinding action, written as in the config's `binds` (the action table in [`config.md`](config.md#action-grammar)): `crucible msg exec focus left`, `crucible msg exec night-light 4000`.
- `run <command…>` / `run-in-terminal <command…>` — launch into the focused pane (the command line is encoded for you).
- `run-on <workspace> <command…>` — launch in the background into a workspace; replies `ok <pid>`.
- `focus-pid <n>` — focus the window process `<n>` (or its nearest ancestor, matched as `relaunch` does) stands for, switching to its workspace; replies `ok <workspace>` or `err no window for pid <n>`.
- `workspace-of-pid <n>` — the workspace (hidden ones too) holding that window, without changing focus; replies `ok <workspace>` or `err no window for pid <n>`.
- `focus-live <name>` — switch to the live workspace `<name>`; never restores a saved one, never brings it to another output. Replies `ok <name>` or `err no live workspace <name>`. Both focus verbs answer `err input busy` while an overlay, snip or the logout modal holds input.
- `capture <abs path>` — write a PNG of the focused output; replies `ok <w>x<h>`.
- `outputs` — the enabled outputs. On the wire the reply is one line of space-separated `<name>=<x>,<y>,<w>x<h>@<scale>` records (logical geometry, ordered by x, then y, then name). `crucible msg outputs` prints one line per output instead: `<name>`, TAB, `<x>,<y>`, TAB, `<w>x<h>`, TAB, `<scale>`.
- `candidates`, `bindings` — read-only queries (the picker's rows, the effective keybindings).

Replies start with `ok` or `err <reason>`. Exit status: 0 on `ok`, 1 on `err`, 2 on a usage error or a missing `$CDE_SOCKET`. Except for `run`, `run-in-terminal`, `run-on`, `run-tui` and `run-term`, arguments are sent as typed, joined with single spaces, so percent-encode `%`, `:`, TAB, LF and CR yourself (`crucible msg open my%3Aname`).

While the session is locked, every request except `candidates`, `bindings`, `outputs`, `save`, `workspace-of-pid` and `exec set-layout <name>` answers `err session locked`. A script that runs on a timer should treat that as "try again next tick", not as a failure.

## `crucible url` and `cde:` links

`crucible url <url>` opens a `cde:` link against the running session; the deploy registers it as the `x-scheme-handler/cde` handler (`cde-url.desktop`), so a `cde:` link in a terminal, a browser or a notification focuses the window or workspace it names.

- `cde:focus?pid=<n>` — focus the window of process `<n>` (as `focus-pid`).
- `cde:focus?workspace=<name>` — switch to the live workspace `<name>` (as `focus-live`).
- Both together: the pid is tried first and the workspace is the fallback. `cde://focus?…` is an accepted alias, the scheme is case-insensitive and a `#fragment` is ignored.
- Only the `focus` action and the `pid` / `workspace` parameters exist; unknown or repeated parameters, empty values and a bad pid are errors. Values use RFC 3986 percent-encoding (`+` is a literal plus), so `cde:focus?workspace=my%20ws%26co` names `my ws&co`.

The socket is `$CDE_SOCKET` when set, else the single `*.sock` in `$XDG_RUNTIME_DIR/cde/`, so a launcher without the session environment still works. Exit status: 0 focused, 1 nothing focused (reason on stderr and via `notify-send` when installed), 2 malformed URL.

The deploy prints `url-handler=verified|failed|skipped` on each host's `RESULT:` line. With `CDE_INSTALL_DIR` set it leaves the real handler alone unless `CDE_REGISTER_URL=1`.

## Night light

The [`night-light`](config.md#night-light) action takes `on`, `off`, `toggle`, `auto` or a temperature in kelvin (2500–6500; 6500 is no tint):

```sh
crucible msg exec night-light 3800   # force 3800 K
crucible msg exec night-light auto   # hand control back to the config schedule
```

Any value other than `auto` is a **manual override**: it holds until `night-light auto` or a CDE restart, and while it holds the config's `night-light` schedule is ignored — saving the config does not clear it. So a script either owns the temperature entirely (set it every tick) or ends with `auto`. Nothing tells a script that the user changed it by hand in between; the next value it sends wins.

CDE writes a new ramp only when the temperature changes, so sending the same value every few minutes is cheap. Night light has no effect under the nested `winit` backend.

A minimal timer-driven script (dash `sh`):

```sh
#!/bin/sh
# Warm the screen in the evening; leave daytime to the config schedule.
set -eu
: "${CDE_SOCKET:=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/cde/${WAYLAND_DISPLAY:-wayland-1}.sock}"
export CDE_SOCKET
hour=$(date +%H)
case $hour in
	18|19) k=5000 ;;
	2[0-3]|0[0-5]) k=4000 ;;
	*) k=auto ;;
esac
crucible msg exec night-light "$k" >/dev/null || [ $? -eq 1 ]   # err (e.g. locked): retry next tick
```
