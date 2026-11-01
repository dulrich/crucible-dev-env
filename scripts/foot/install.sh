#!/usr/bin/env bash
# Install a foot tarball built by scripts/foot/build.sh under ~/.local/opt (bash + coreutils at run time).
set -euo pipefail

MIN_VER=1.20.2

usage() {
	cat <<USAGE
Usage: install.sh <tarball>
       install.sh [--help|-h] [--build|--version]

Install a foot tarball (foot-<ver>-<arch>.tar.gz from scripts/foot/build.sh, top dir
foot-<ver>/) for the current user:
  - unpack to ~/.local/opt/foot-<ver> and atomically repoint ~/.local/opt/foot at it
    (the stable absolute path CDE configures: ~/.local/opt/foot/bin/foot)
  - symlink foot and footclient into ~/.local/bin (interactive shells only; CDE does
    not rely on PATH)
  - ensure terminfo \`foot\` and \`foot-direct\` resolve, else \`tic -x\` the shipped source
    into ~/.terminfo
  - write ~/.local/share/applications/org.codeberg.dnkl.{foot,footclient}.desktop
    pointing at the installed binaries and a Hidden=true org.codeberg.dnkl.foot-server.desktop
  - fail on any \`not found\` in ldd, and refuse to finish unless
    ~/.local/opt/foot/bin/foot --version reports >= $MIN_VER (older foot, e.g. the distro
    1.16.2, crashes on CDE: upstream #1910)
Re-running with the same tarball is a no-op in effect. Honors \$HOME.

  --build, --version   print the tool-scoped build id (this repo's: last commit touching it)
  --help, -h           print this usage
USAGE
}

die() {
	echo "install.sh: $*" >&2
	exit 1
}

here=$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")
case " $* " in *" --help "* | *" -h "*) usage; exit 0 ;; esac
# shellcheck source=../lib/build-id.sh
. "$here/../lib/build-id.sh"
build_flags "${BASH_SOURCE[0]}" "$@"

[ "$#" -eq 1 ] || {
	usage >&2
	exit 2
}
tarball=$1
[ -f "$tarball" ] || die "not a file: $tarball"
[ -n "${HOME:-}" ] || die "HOME is not set"
case "$HOME" in /*) ;; *) die "HOME must be absolute: $HOME" ;; esac
case "$HOME" in *[[:space:]\|\&\\]*) die "HOME contains an unsupported character: $HOME" ;; esac

# version_ge A B: true when dotted version A >= B.
version_ge() {
	local IFS=. i
	local -a a b
	read -ra a <<<"$1"
	read -ra b <<<"$2"
	for i in 0 1 2; do
		if ((${a[i]:-0} > ${b[i]:-0})); then return 0; fi
		if ((${a[i]:-0} < ${b[i]:-0})); then return 1; fi
	done
	return 0
}

# foot_version BIN: print X.Y.Z parsed from `foot version: X.Y.Z ...`, or fail.
foot_version() {
	local out ver
	out=$("$1" --version 2>&1) || return 1
	ver=${out#foot version: }
	ver=${ver%% *}
	[[ $ver =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || return 1
	printf '%s\n' "$ver"
}

require_ge() {
	local bin=$1 ver
	ver=$(foot_version "$bin") || die "cannot parse a version from '$bin --version'"
	version_ge "$ver" "$MIN_VER" || die "$bin is foot $ver; need >= $MIN_VER (older foot crashes on CDE, upstream #1910) - refusing"
	echo "install.sh: $bin is foot $ver"
}

check_ldd() {
	local b out
	for b in "$1/bin/foot" "$1/bin/footclient"; do
		out=$(ldd "$b" 2>&1 || true) # exits 1 for static binaries
		if grep -q 'not found' <<<"$out"; then
			echo "$out" | grep 'not found' >&2
			die "unresolved shared libraries for $b; install the missing packages"
		fi
	done
}

# Tarball must hold exactly one top dir foot-<ver>, no absolute or .. members.
mapfile -t members < <(tar -tzf "$tarball")
[ "${#members[@]}" -gt 0 ] || die "empty or unreadable tarball: $tarball"
top=${members[0]%%/*}
[[ $top =~ ^foot-([0-9]+\.[0-9]+\.[0-9]+)$ ]] || die "tarball top dir '$top' is not foot-<X.Y.Z>"
ver=${BASH_REMATCH[1]}
for m in "${members[@]}"; do
	case "$m" in "$top" | "$top"/*) ;; *) die "tarball member outside $top/: $m" ;; esac
	case "/$m/" in */../*) die "tarball member with '..': $m" ;; esac
done

opt=$HOME/.local/opt
bindir=$HOME/.local/bin
apps=$HOME/.local/share/applications
prefix=$opt/$top
stable=$opt/foot
[ ! -e "$stable" ] || [ -L "$stable" ] || die "$stable exists and is not a symlink; move it away"
mkdir -p "$opt" "$bindir" "$apps"

# Unpack and vet before touching anything live.
tmp=$(mktemp -d "$opt/.install.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
tar -xzf "$tarball" -C "$tmp"
check_ldd "$tmp/$top"
require_ge "$tmp/$top/bin/foot"

rm -rf "$prefix"
mv "$tmp/$top" "$prefix"
ln -sfn "$prefix" "$opt/.foot.new"
mv -T "$opt/.foot.new" "$stable"

for b in foot footclient; do
	ln -sfn "$stable/bin/$b" "$bindir/$b"
done

# terminfo: ncurses-term usually provides both; otherwise compile the shipped source.
if ! infocmp foot >/dev/null 2>&1 || ! infocmp foot-direct >/dev/null 2>&1; then
	command -v tic >/dev/null || die "terminfo foot/foot-direct missing and tic (ncurses-bin) not found"
	mkdir -p "$HOME/.terminfo"
	tic -x -o "$HOME/.terminfo" "$stable/share/foot/terminfo/foot.info"
	if ! infocmp foot >/dev/null 2>&1 || ! infocmp foot-direct >/dev/null 2>&1; then die "terminfo foot/foot-direct still unresolved after tic"; fi
	echo "install.sh: compiled terminfo into ~/.terminfo"
fi

# Desktop entries: shadow the distro ones with absolute Exec paths.
for pair in foot:foot footclient:footclient foot-server:foot; do
	name=${pair%%:*}
	src=$stable/share/applications/$name.desktop
	dst=$apps/org.codeberg.dnkl.$name.desktop
	[ -f "$src" ] || die "tarball lacks $src"
	sed -E "s#^Exec=(foot|footclient)( |\$)#Exec=$stable/bin/\\1\\2#" "$src" >"$dst.new"
	if [ "$name" = foot-server ]; then
		sed -i '0,/^\[Desktop Entry\]/s//[Desktop Entry]\nHidden=true/' "$dst.new"
	fi
	mv "$dst.new" "$dst"
done

# Final guard through the stable path CDE uses.
require_ge "$stable/bin/foot"
echo "install.sh: installed $top -> $stable"
