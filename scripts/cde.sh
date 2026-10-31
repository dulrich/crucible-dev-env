#!/usr/bin/env bash
# Launch the CDE (crucible-dev-env) compositor on a bare TTY with logging.
# Usage: scripts/cde.sh [extra crucible args...]   (or ~/cde.sh via a symlink to it)
#   env overrides: TERMINAL (default ghostty), RUST_LOG (default info), CDE_MODE (default --tty-udev)
# Logs: /tmp/cde/cde-<timestamp>.log, newest symlinked as /tmp/cde/latest.log
set -uo pipefail

for arg in "$@"; do
	case "$arg" in
	--help | -h)
		cat <<'USAGE'
Usage: cde.sh [--help|-h] [extra crucible args...]

Launch the CDE (crucible-dev-env) compositor on a bare TTY with logging. Runs
`cargo build` first, then ./target/debug/crucible with the mode flag followed by
any extra args, which are passed through unchanged. With CDE_BIN set, the build
is skipped and that binary runs instead.

Environment overrides:
  TERMINAL   terminal CDE spawns (default: /tmp/cde/ghostty-cde, a wrapper that
             runs ghostty with --gtk-single-instance=false)
  RUST_LOG   log filter (default: info)
  CDE_BIN    path to a prebuilt crucible binary (e.g. ~/.local/bin/crucible):
             skips `cargo build` and runs it instead of ./target/debug/crucible;
             exits 1 if set but not executable (default: unset)
  CDE_MODE   crucible mode flag (default: --tty-udev)
  CDE_WALLPAPER  set to 0 to skip the startup wallpaper (default: in --tty-udev
             mode, scripts/wallpaper-rotate.sh runs once the compositor
             is listening; cron keeps rotating it)

Logs: /tmp/cde/cde-<timestamp>.log, newest symlinked as /tmp/cde/latest.log
USAGE
		exit 0
		;;
	esac
done

# CDE_BIN: a prebuilt binary to run instead of the dev build. Validate before anything starts.
if [ -n "${CDE_BIN:-}" ]; then
	case "$CDE_BIN" in /*) ;; *) CDE_BIN="$PWD/$CDE_BIN" ;; esac
	if [ ! -f "$CDE_BIN" ] || [ ! -x "$CDE_BIN" ]; then
		echo "cde.sh: CDE_BIN is not an executable file: $CDE_BIN" >&2
		exit 1
	fi
fi

# shellcheck source=lib/build-id.sh
. "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/lib/build-id.sh"
build_flags "${BASH_SOURCE[0]}" "$@"

repo=$(cd "$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")/.." && pwd)
logdir=/tmp/cde
mkdir -p "$logdir"
log="$logdir/cde-$(date +%Y%m%d-%H%M%S).log"
ln -sfn "$log" "$logdir/latest.log"

# ghostty defaults to GTK single-instance: a new launch is handed over the user D-Bus to the
# already-running ghostty (i3 on VT7), so windows open there. CDE spawns $TERMINAL with no
# args, so wrap it to force a separate instance bound to CDE's Wayland socket.
ghostty_wrap="$logdir/ghostty-cde"
cat >"$ghostty_wrap" <<'EOF'
#!/bin/sh
exec ghostty --gtk-single-instance=false "$@"
EOF
chmod +x "$ghostty_wrap"

export TERMINAL="${TERMINAL:-$ghostty_wrap}"
export RUST_LOG="${RUST_LOG:-info}"
mode="${CDE_MODE:---tty-udev}"

cd "$repo" || { echo "cde.sh: repo not found: $repo" >&2; exit 1; }

{
	echo "== cde.sh $(date -Is) on $(tty 2>/dev/null || echo no-tty)"
	echo "== head $(git rev-parse --short HEAD 2>/dev/null) dirty=$(git status --porcelain 2>/dev/null | wc -l)"
	echo "== TERMINAL=$TERMINAL RUST_LOG=$RUST_LOG mode=$mode args=$*"
} | tee "$log"

# Rebuild so code changes since the last run are picked up (no-op when current).
if [ -n "${CDE_BIN:-}" ]; then
	echo "== CDE_BIN=$CDE_BIN (skipping cargo build)" | tee -a "$log"
elif ! cargo build 2>&1 | tee -a "$log"; then
	echo "cde.sh: build failed, see $log" >&2
	exit 1
fi

# Startup wallpaper: only on a bare TTY (nested modes would hit the parent session's
# WAYLAND_DISPLAY). Backgrounded; waits up to 30 s for crucible's socket, then exits.
if [ "$mode" = --tty-udev ] && [ "${CDE_WALLPAPER:-1}" != 0 ]; then
	env -u WAYLAND_DISPLAY "$repo/scripts/wallpaper-rotate.sh" --wait 30 >>"$log" 2>&1 </dev/null &
fi

# Portal reset (bare TTY only; nested modes share the parent session's portals). When CDE
# exits, xdg-desktop-portal-gtk loses its display and xdg-desktop-portal re-activates it at
# once against the dead display; the failed activation stays pending in dbus for 120 s, and
# every GTK4 app (ghostty) started meanwhile blocks before mapping (host-a 2026-10-31 14:44).
# Stopping both units leaves them to dbus activation with the next session's environment.
reset_portals() {
	[ "$mode" = --tty-udev ] || return 0
	systemctl --user stop xdg-desktop-portal.service xdg-desktop-portal-gtk.service >/dev/null 2>&1
	systemctl --user reset-failed xdg-desktop-portal-gtk.service >/dev/null 2>&1
	echo "== portals reset ($1)" | tee -a "$log"
}

reset_portals start
"${CDE_BIN:-./target/debug/crucible}" "$mode" "$@" 2>&1 | tee -a "$log"
status=${PIPESTATUS[0]}
reset_portals exit
echo "== crucible exited $status at $(date -Is)" | tee -a "$log"
exit "$status"
