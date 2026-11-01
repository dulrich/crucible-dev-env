#!/usr/bin/env bash
# Build foot 1.28.0 (dev host only) into a relocatable tarball for scripts/foot/install.sh.
set -euo pipefail

FOOT_VER=1.28.0
FCFT_VER=3.3.3
TLLIST_VER=1.1.0
UTF8PROC_VER=2.9.0
MESON_VER=1.12.1
NINJA_VER=1.13.2

usage() {
	cat <<'USAGE'
Usage: build.sh [--help|-h] [--build|--version]

Build foot 1.28.0 (fcft 3.3.3 and tllist 1.1.0 as static subprojects, utf8proc 2.9.0
as a static lib, meson 1.12.1 run as `python3 -I meson.py`, ninja 1.13.2) and package
it for scripts/foot/install.sh. Dev host only: needs gcc, python3, pkg-config,
wayland-scanner, unzip, curl or wget, and the foot -dev packages.

Sources are sha256-pinned and cached under target/foot-build/dl/ (seeded from
target/spike-foot/dl/ when present). Output, under target/foot-build/:
  foot-1.28.0-<arch>.tar.gz       top dir foot-1.28.0/ with bin/, share/
  foot-1.28.0-<arch>.ldd.txt      ldd of foot and footclient; any `not found` fails the build

  --build, --version   print the tool-scoped build id (this repo's: last commit touching it)
  --help, -h           print this usage
USAGE
}

here=$(dirname "$(readlink -f "${BASH_SOURCE[0]}")")
for arg in "$@"; do
	case "$arg" in --help | -h)
		usage
		exit 0
		;;
	esac
done
# shellcheck source=../lib/build-id.sh
. "$here/../lib/build-id.sh"
build_flags "${BASH_SOURCE[0]}" "$@"
if [ "$#" -gt 0 ]; then
	echo "build.sh: unexpected argument: $1" >&2
	exit 2
fi

repo=$(cd "$here/../.." && pwd)
work=$repo/target/foot-build
dl=$work/dl
spike=$repo/target/spike-foot/dl
arch=$(uname -m)
[ "$arch" = x86_64 ] || {
	echo "build.sh: only x86_64 is pinned (ninja-linux.zip); got $arch" >&2
	exit 1
}

# name|relative path|url|sha256
pins=(
	"foot|foot/foot-$FOOT_VER.tar.gz|https://codeberg.org/dnkl/foot/archive/$FOOT_VER.tar.gz|4296be402b5684d049534598e69db92b918f92beac9dab76b585207045f0b037"
	"fcft|fcft/fcft-$FCFT_VER.tar.gz|https://codeberg.org/dnkl/fcft/archive/$FCFT_VER.tar.gz|b0c0f4a599f43723736c8565b8b84337c4195077f07f1bb8bb3252bb13a2306a"
	"tllist|tllist/tllist-$TLLIST_VER.tar.gz|https://codeberg.org/dnkl/tllist/archive/$TLLIST_VER.tar.gz|0e7b7094a02550dd80b7243bcffc3671550b0f1d8ba625e4dff52517827d5d23"
	"utf8proc|utf8proc/utf8proc-$UTF8PROC_VER.tar.gz|https://github.com/JuliaStrings/utf8proc/archive/refs/tags/v$UTF8PROC_VER.tar.gz|18c1626e9fc5a2e192311e36b3010bfc698078f692888940f1fa150547abb0c1"
	"meson|meson/meson-$MESON_VER.tar.gz|https://github.com/mesonbuild/meson/releases/download/$MESON_VER/meson-$MESON_VER.tar.gz|ab0a6ca09f8ef70c564c8241fb5a23957886a0b53fb58412b5e07eaf07dba743"
	"ninja|ninja/ninja-linux.zip|https://github.com/ninja-build/ninja/releases/download/v$NINJA_VER/ninja-linux.zip|5749cbc4e668273514150a80e387a957f933c6ed3f5f11e03fb30955e2bbead6"
)

sha_ok() { [ -f "$1" ] && [ "$(sha256sum "$1" | cut -d' ' -f1)" = "$2" ]; }

fetch() {
	local url=$1 out=$2
	if command -v curl >/dev/null; then
		curl -fL --retry 3 -o "$out" "$url"
	else
		wget -O "$out" "$url"
	fi
}

mkdir -p "$dl"
for pin in "${pins[@]}"; do
	IFS='|' read -r name rel url sum <<<"$pin"
	f=$dl/$rel
	mkdir -p "$(dirname "$f")"
	if ! sha_ok "$f" "$sum" && sha_ok "$spike/$rel" "$sum"; then
		cp "$spike/$rel" "$f"
	fi
	if ! sha_ok "$f" "$sum"; then
		echo "build.sh: fetching $name" >&2
		fetch "$url" "$f.part"
		mv "$f.part" "$f"
	fi
	sha_ok "$f" "$sum" || {
		echo "build.sh: sha256 mismatch for $f (want $sum)" >&2
		exit 1
	}
done

# Fresh unpack of every source each build (the cache holds only the verified archives).
src=$work/src
rm -rf "$src" "$work/build" "$work/stage" "$work/utf8proc-out"
mkdir -p "$src" "$work/stage" "$work/utf8proc-out/pc"
tar -xzf "$dl/foot/foot-$FOOT_VER.tar.gz" -C "$src"
tar -xzf "$dl/fcft/fcft-$FCFT_VER.tar.gz" -C "$src"
tar -xzf "$dl/tllist/tllist-$TLLIST_VER.tar.gz" -C "$src"
tar -xzf "$dl/utf8proc/utf8proc-$UTF8PROC_VER.tar.gz" -C "$src"
tar -xzf "$dl/meson/meson-$MESON_VER.tar.gz" -C "$src"
mkdir -p "$src/ninja"
unzip -q -o "$dl/ninja/ninja-linux.zip" -d "$src/ninja"
chmod +x "$src/ninja/ninja"

foot_src=$src/foot
rm -rf "$foot_src/subprojects/fcft" "$foot_src/subprojects/tllist"
cp -r "$src/fcft" "$foot_src/subprojects/fcft"
cp -r "$src/tllist" "$foot_src/subprojects/tllist"

# utf8proc: static lib so the tarball has no libutf8proc runtime dependency.
u=$src/utf8proc-$UTF8PROC_VER
uo=$work/utf8proc-out
gcc -O2 -fPIC -std=c99 -DUTF8PROC_STATIC -I"$u" -c "$u/utf8proc.c" -o "$uo/utf8proc.o"
ar rcs "$uo/libutf8proc.a" "$uo/utf8proc.o"
cat >"$uo/pc/libutf8proc.pc" <<PC
Name: libutf8proc
Description: utf8proc $UTF8PROC_VER (static, built by scripts/foot/build.sh)
Version: $UTF8PROC_VER
Cflags: -I$u -DUTF8PROC_STATIC
Libs: $uo/libutf8proc.a
PC

export PATH=$src/ninja:$PATH
export PKG_CONFIG_PATH=$uo/pc
meson=(python3 -I "$src/meson-$MESON_VER/meson.py")
"${meson[@]}" setup "$work/build" "$foot_src" --buildtype=release --prefix=/usr -Db_lto=true \
	--wrap-mode=nodownload -Dgrapheme-clustering=enabled -Ddocs=disabled -Dtests=false \
	-Dfcft:docs=disabled -Dfcft:test-text-shaping=false -Dfcft:grapheme-shaping=enabled \
	-Dfcft:run-shaping=enabled -Dfcft:svg-backend=nanosvg -Ddefault_library=static -Dterminfo=disabled
ninja -C "$work/build"
DESTDIR=$work/stage "${meson[@]}" install -C "$work/build"

# Re-root /usr -> foot-<ver>/ and ship the terminfo source for install.sh's fallback.
top=foot-$FOOT_VER
rm -rf "${work:?}/pkg"
mkdir -p "$work/pkg"
mv "$work/stage/usr" "$work/pkg/$top"
mkdir -p "$work/pkg/$top/share/foot/terminfo"
sed 's/@default_terminfo@/foot/g' "$foot_src/foot.info" >"$work/pkg/$top/share/foot/terminfo/foot.info"

tarball=$work/$top-$arch.tar.gz
manifest=$work/$top-$arch.ldd.txt
tar --sort=name --owner=0 --group=0 --numeric-owner -C "$work/pkg" -czf "$tarball" "$top"

: >"$manifest"
for b in foot footclient; do
	echo "== $b" >>"$manifest"
	ldd "$work/pkg/$top/bin/$b" >>"$manifest"
done
if grep -q 'not found' "$manifest"; then
	echo "build.sh: unresolved libraries:" >&2
	grep 'not found' "$manifest" >&2
	exit 1
fi

"$work/pkg/$top/bin/foot" --version
echo "tarball:  $tarball"
echo "manifest: $manifest"
sha256sum "$tarball"
