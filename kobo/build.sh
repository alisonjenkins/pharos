#!/usr/bin/env bash
# Builds the Kobo device package (KoboRoot.tgz) outside Nix: fetches Cobalt at
# the pinned commit, applies cobalt.patch, adds the app as examples/pharos,
# runs the app's tests and clippy, and packages. `nix build .#kobo` does the
# same reproducibly; this script is for working on the app.
#
# Usage: kobo/build.sh [work-dir]   (default: kobo/.cobalt, git-ignored)
# Needs: git, rustup with Cobalt's toolchain (it reads rust-toolchain.toml),
# and a musl C toolchain for the reader named by CC_armv7_unknown_linux_musleabihf
# and AR_armv7_unknown_linux_musleabihf. Cobalt uses zig: a CC wrapper running
# `zig cc -target arm-linux-musleabihf -fno-sanitize=undefined "$@"` with any
# --target=* argument dropped, and an AR wrapper running `zig ar "$@"`
# (default.nix has both). glibc's armhf gcc does not link bundled SQLite.
# PHAROS_KOBO_SERVER sets the server the app talks to.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=cobalt.env
. "$here/cobalt.env"
work="${1:-$here/.cobalt}"
mkdir -p "$work"
work="$(cd "$work" && pwd)"

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

cd "$work"
cargo test --locked -p kobo-pharos
cargo clippy --locked -p kobo-pharos --all-targets -- -D warnings
cargo build --locked -p kobo-cli
./target/debug/kobo package --out "$work/KoboRoot.tgz"
echo "built $work/KoboRoot.tgz"
