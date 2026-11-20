# Build the Android app page's HLS engine (spec 031): pharos-ui's
# `pharos-tv-hls` bin to wasm, with its wasm-bindgen glue, into
# crates/pharos-ui/assets/generated/, where the TV and phone builds include it.
# Run inside the default devShell (it has the wasm32 target and the
# wasm-bindgen CLI matching Cargo.lock): `nix develop -c sh scripts/tv-hls.sh`.
set -eu

root=$(git rev-parse --show-toplevel)
cd "$root"
target_dir=${CARGO_TARGET_DIR:-$root/target}
out=crates/pharos-ui/assets/generated

cargo build --release --target wasm32-unknown-unknown -p pharos-ui \
  --bin pharos-tv-hls --features web

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
wasm-bindgen --target no-modules --no-typescript --out-dir "$tmp" \
  "$target_dir/wasm32-unknown-unknown/release/pharos-tv-hls.wasm"

mkdir -p "$out"
cp "$tmp/pharos-tv-hls.js" "$out/tv-hls.js"
base64 -w0 "$tmp/pharos-tv-hls_bg.wasm" > "$out/tv-hls.wasm.b64"
echo "tv-hls: wasm $(wc -c < "$tmp/pharos-tv-hls_bg.wasm") bytes -> $out" >&2
