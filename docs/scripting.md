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
- `capture <abs path>` — write a PNG of the focused output; replies `ok <w>x<h>`.
- `candidates`, `bindings` — read-only queries (the picker's rows, the effective keybindings).

Replies start with `ok` or `err <reason>`. Exit status: 0 on `ok`, 1 on `err`, 2 on a usage error or a missing `$CDE_SOCKET`. Except for `run`, `run-in-terminal`, `run-on`, `run-tui` and `run-term`, arguments are sent as typed, joined with single spaces, so percent-encode `%`, `:`, TAB, LF and CR yourself (`crucible msg open my%3Aname`).

While the session is locked, every request except `candidates`, `bindings` and `save` answers `err session locked`. A script that runs on a timer should treat that as "try again next tick", not as a failure.

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
