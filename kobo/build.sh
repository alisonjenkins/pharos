#!/usr/bin/env bash
# Builds the Kobo device package (KoboRoot.tgz) outside Nix: fetches Cobalt at
# the pinned commit, applies cobalt.patch, adds the app as examples/pharos,
# runs the app's tests and clippy, and packages. With --setup it then installs
# that build on a reader plugged in over USB (`kobo setup --source`). See
# README.md, "Putting it on the Kobo".
#
# Usage: kobo/build.sh [--setup [kobo setup options]]
#   e.g. --setup --dry-run, --setup --menu, --setup --undo
# KOBO_COBALT_DIR picks the work directory (default kobo/.cobalt, git-ignored).
# Needs: git, rustup (it installs Cobalt's toolchain from rust-toolchain.toml),
# a host C compiler named cc (cargo links build scripts with it),
# and either zig on PATH or a musl C toolchain for the reader named by
# CC_armv7_unknown_linux_musleabihf and AR_armv7_unknown_linux_musleabihf.
# With zig and neither variable set, the script writes the same wrappers
# default.nix uses. glibc's armhf gcc does not link bundled SQLite.
# PHAROS_KOBO_SERVER sets the server the app talks to.
set -euo pipefail

setup=false
if [ "${1:-}" = "--setup" ]; then
  setup=true
  shift
elif [ $# -gt 0 ]; then
  echo "usage: kobo/build.sh [--setup [kobo setup options]]" >&2
  exit 2
fi

if ! command -v cc >/dev/null; then
  # cargo links every build script for this machine with cc, before any
  # Kobo code builds; without it the build fails at libc's build script.
  echo "kobo/build.sh: no cc on PATH; add a host C compiler (e.g. nixpkgs#gcc)" >&2
  exit 1
fi

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=cobalt.env
. "$here/cobalt.env"
work="${KOBO_COBALT_DIR:-$here/.cobalt}"
mkdir -p "$work"
work="$(cd "$work" && pwd)"
export PHAROS_KOBO_SERVER="${PHAROS_KOBO_SERVER:-https://prototype.aliflix.alison-jenkins.com}"

if [ ! -d "$work/.git" ]; then
  git init --quiet "$work"
  git -C "$work" remote add origin "$COBALT_REPO"
fi
git -C "$work" fetch --quiet --depth 1 origin "$COBALT_REV"
git -C "$work" checkout --quiet --force "$COBALT_REV"
git -C "$work" clean --quiet -fdx -e target
git -C "$work" apply "$here/cobalt.patch"
mkdir -p "$work/examples/pharos"
cp -r "$here/app/." "$work/examples/pharos/"

if [ -z "${CC_armv7_unknown_linux_musleabihf:-}" ] && command -v zig >/dev/null; then
  # The same wrappers as default.nix: zig spells the triple
  # arm-linux-musleabihf, so cc-rs's --target is dropped, and per-function
  # sections stay off because zig 0.16 then leaves unnamed undefined symbols.
  wrap="$work/target/zig-wrap"
  mkdir -p "$wrap"
  cat >"$wrap/cc" <<'CC'
#!/bin/sh
count=$#
index=0
while [ "$index" -lt "$count" ]; do
  argument="$1"
  shift
  case "$argument" in
    --target=*) ;;
    *) set -- "$@" "$argument" ;;
  esac
  index=$((index + 1))
done
exec zig cc -target arm-linux-musleabihf -fno-sanitize=undefined "$@" -fno-function-sections
CC
  printf '#!/bin/sh\nexec zig ar "$@"\n' >"$wrap/ar"
  chmod +x "$wrap/cc" "$wrap/ar"
  export CC_armv7_unknown_linux_musleabihf="$wrap/cc"
  export AR_armv7_unknown_linux_musleabihf="$wrap/ar"
fi

cd "$work"
cargo test --locked -p kobo-pharos
cargo clippy --locked -p kobo-pharos --all-targets -- -D warnings
cargo build --locked -p kobo-cli
# cargo honours a CARGO_TARGET_DIR set by the caller; look for the CLI there.
kobo_bin="${CARGO_TARGET_DIR:-$work/target}/debug/kobo"
"$kobo_bin" package --out "$work/KoboRoot.tgz"
echo "built $work/KoboRoot.tgz for $PHAROS_KOBO_SERVER"

if [ "$setup" = true ]; then
  # --source installs the tree just built, never an official release, and
  # setup waits for the reader to be plugged in.
  exec "$kobo_bin" setup --source --wait-for-reader "$@"
fi
