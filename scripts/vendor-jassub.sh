#!/usr/bin/env bash
# Rebuild crates/pharos-ui/assets/vendor/jassub/ from pinned npm tarballs
# (ADR-0057).
#
# JASSUB 2.x ships ES modules with bare imports, so the two scripts the
# page and its worker load are bundled here with esbuild; the wasm builds
# and the fallback font are copied unmodified. Every tarball is checked
# against the integrity npm published for it before anything is unpacked.
#
# Usage: nix develop -c scripts/vendor-jassub.sh   (from the repository root;
# the dev shell supplies curl and openssl)
set -euo pipefail

OUT=crates/pharos-ui/assets/vendor/jassub
# name version integrity: jassub and its runtime dependencies as npm
# resolved them for jassub@2.5.23.
PINS=(
  "jassub 2.5.23 sha512-KvWvkZOQvLUZKDoJTfAZzbmxCN2Avs37d1VnZlscKvmfPIjcS+l2guou0WdGW6z/+Uxb8F7ha8Nrq5h5+k9cLw=="
  "abslink 1.3.1 sha512-ukFoU6q/RBMjoC7fu0Zx2+eVH2QFMfV+aJ2cI3Ypl0Shm0ikGjZ0T2VN0xvLl0a8imhWqvfVrqu2n2Ml3eSMyQ=="
  "lfa-ponyfill 1.1.1 sha512-Had4tsGEHtIj58MRtf6SlubNDltxinxMSdxIn1GdwrWbye8TD+pZ+D6qKETnJ9+vQOfG9q/Ds4WUPskhyAMp5Q=="
  "rvfc-polyfill 1.0.8 sha512-uA+0wwTkZ4OT8v45pfDfH+7Yq8mY6MvNngiF5Sq6VBgjJsvsfgt7Q18cyZqZjfAhW9rhkgXPX0cW0R9uw7yElA=="
  "throughput 1.0.2 sha512-jvK1ZXuhsggjb3qYQjMiU/AVYYiTeqT5thWvYR2yuy2LGM84P5MSSyAinwHahGsdBYKR9m9HncVR/3f3nFKkxg=="
)
# The 2.x tarball carries no notices for the C libraries compiled into the
# wasm (libass, FreeType, HarfBuzz, fribidi, ...); 1.8.8's dist/COPYRIGHT
# lists them, so it ships beside the build.
NOTICES="jassub 1.8.8 sha512-Vtw27Z0/hN2Tm2Qs4HAR+eONwwSD5ULo/hisPvmiaW9Ya6aFCt7esvWQSZy6nvBASbRyKmpghgeioIPqXGHA0Q=="

[[ -f flake.nix && -d crates/pharos-ui ]] || { echo "run from the repository root" >&2; exit 2; }

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# fetch NAME VERSION INTEGRITY DIR: unpack the checked tarball into DIR.
fetch() {
  local tgz="$WORK/$1-$2.tgz" got
  curl -fsSL --retry 4 -o "$tgz" "https://registry.npmjs.org/$1/-/$1-$2.tgz"
  got="sha512-$(openssl dgst -sha512 -binary "$tgz" | base64 -w0)"
  if [[ "$got" != "$3" ]]; then
    echo "$1@$2: integrity $got, expected $3" >&2
    exit 1
  fi
  mkdir -p "$4"
  tar -xzf "$tgz" -C "$4" --strip-components=1
}

for pin in "${PINS[@]}"; do
  read -r name version integrity <<<"$pin"
  fetch "$name" "$version" "$integrity" "$WORK/node_modules/$name"
done
read -r name version integrity <<<"$NOTICES"
fetch "$name" "$version" "$integrity" "$WORK/notices"

PKG="$WORK/node_modules/jassub"
# esbuild from the flake's own nixpkgs, so the bundle is reproducible.
ESBUILD=(nix shell --inputs-from . nixpkgs#esbuild --command esbuild)
"${ESBUILD[@]}" --version

rm -rf "$OUT"
mkdir -p "$OUT"
# The page script: a classic script defining window.JASSUB, which
# platform::styled_subtitles loads. Every URL it would derive from
# import.meta.url is passed in as an option, so none is left empty.
"${ESBUILD[@]}" "$PKG/dist/jassub.js" --bundle --format=iife --minify \
  --global-name=JASSUBModule --footer:js='globalThis.JASSUB=JASSUBModule.default;' \
  --log-override:empty-import-meta=silent \
  --outfile="$OUT/jassub.js"
# The worker: JASSUB starts it as a module worker.
"${ESBUILD[@]}" "$PKG/dist/worker/worker.js" --bundle --format=esm --minify \
  --outfile="$OUT/jassub-worker.js"
cp "$PKG/dist/wasm/jassub-worker.wasm" "$PKG/dist/wasm/jassub-worker-modern.wasm" \
  "$PKG/dist/default.woff2" "$OUT/"
cp "$PKG/LICENSE" "$OUT/LICENSE"
cp "$WORK/notices/dist/COPYRIGHT" "$OUT/COPYRIGHT"
# The bundled dependencies' licences, beside JASSUB's.
for dep in abslink lfa-ponyfill rvfc-polyfill throughput; do
  lic=$(find "$WORK/node_modules/$dep" -maxdepth 1 -iname 'licen[cs]e*' | head -n1)
  if [[ -n "$lic" ]]; then
    cp "$lic" "$OUT/LICENSE.$dep"
  else
    # Some tarballs carry no licence file; record what package.json says.
    sed -n 's/^ *"license": *"\(.*\)",*$/\1/p' "$WORK/node_modules/$dep/package.json" |
      sed "s/^/$dep: /; s/\$/ (package.json; the tarball has no licence file)/" > "$OUT/LICENSE.$dep"
  fi
done

(cd "$OUT" && sha256sum -- *)
