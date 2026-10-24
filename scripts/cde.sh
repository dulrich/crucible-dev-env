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
any extra args, which are passed through unchanged.

Environment overrides:
  TERMINAL   terminal CDE spawns (default: /tmp/cde/ghostty-cde, a wrapper that
             runs ghostty with --gtk-single-instance=false)
  RUST_LOG   log filter (default: info)
  CDE_MODE   crucible mode flag (default: --tty-udev)

Logs: /tmp/cde/cde-<timestamp>.log, newest symlinked as /tmp/cde/latest.log
USAGE
		exit 0
		;;
	esac
done

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
if ! cargo build 2>&1 | tee -a "$log"; then
	echo "cde.sh: build failed, see $log" >&2
	exit 1
fi

./target/debug/crucible "$mode" "$@" 2>&1 | tee -a "$log"
status=${PIPESTATUS[0]}
echo "== crucible exited $status at $(date -Is)" | tee -a "$log"
exit "$status"
