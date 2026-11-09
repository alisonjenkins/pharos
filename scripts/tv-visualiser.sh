# Build the Android TV app page's visualiser (B1032): pharos-ui's
# `pharos-tv-visualiser` bin to wasm, with its wasm-bindgen glue, into
# crates/pharos-ui/assets/generated/, where the TV build includes it.
# Run inside the default devShell (it has the wasm32 target and the
# wasm-bindgen CLI matching Cargo.lock): `nix develop -c sh scripts/tv-visualiser.sh`.
set -eu

root=$(git rev-parse --show-toplevel)
cd "$root"
target_dir=${CARGO_TARGET_DIR:-$root/target}
out=crates/pharos-ui/assets/generated

cargo build --release --target wasm32-unknown-unknown -p pharos-ui \
  --bin pharos-tv-visualiser --features web

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
wasm-bindgen --target no-modules --no-typescript --out-dir "$tmp" \
  "$target_dir/wasm32-unknown-unknown/release/pharos-tv-visualiser.wasm"

mkdir -p "$out"
cp "$tmp/pharos-tv-visualiser.js" "$out/tv-visualiser.js"
base64 -w0 "$tmp/pharos-tv-visualiser_bg.wasm" > "$out/tv-visualiser.wasm.b64"
echo "tv-visualiser: wasm $(wc -c < "$tmp/pharos-tv-visualiser_bg.wasm") bytes -> $out" >&2
