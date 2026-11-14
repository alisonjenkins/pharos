#!/usr/bin/env bash
# Spike 044: build the pure-Rust Android Auto test APK with no Gradle and no
# Java source. Run inside `nix develop .#android` from anywhere in the repo:
#
#   nix develop .#android -c bash spikes/044-android-auto/build-apk.sh
#
# Signing uses the same variables as scripts/android-apk.sh:
#   PHAROS_ANDROID_KEYSTORE, PHAROS_ANDROID_KEYSTORE_PASS_FILE, PHAROS_ANDROID_KEY_ALIAS
# Output: target/spike-044/pharos-auto-spike.apk
set -euo pipefail

for var in ANDROID_HOME PHAROS_ANDROID_KEYSTORE PHAROS_ANDROID_KEYSTORE_PASS_FILE PHAROS_ANDROID_KEY_ALIAS; do
  if [ -z "${!var:-}" ]; then echo "build-apk: $var is not set" >&2; exit 1; fi
done
here=$(cd "$(dirname "$0")" && pwd)
root=$(git -C "$here" rev-parse --show-toplevel)
out=$root/target/spike-044
platform=$ANDROID_HOME/platforms/android-34/android.jar
bt=$ANDROID_HOME/build-tools/35.0.0
rm -rf "$out"; mkdir -p "$out/classes" "$out/dex" "$out/stage/lib/arm64-v8a" "$out/res"

echo "build-apk: 1/6 generating the stub classes from Rust" >&2
(cd "$here/classgen" && cargo run -q --release -- "$out/classes")

echo "build-apk: 2/6 dexing them with d8 against android-34" >&2
mapfile -t classes < <(find "$out/classes" -name '*.class')
"$bt/d8" --release --min-api 26 --lib "$platform" --output "$out/dex" "${classes[@]}"

echo "build-apk: 3/6 building libpharos_auto.so for arm64" >&2
(cd "$here/auto" && cargo build -q --release --target aarch64-linux-android)
cp "$here/auto/target/aarch64-linux-android/release/libpharos_auto.so" "$out/stage/lib/arm64-v8a/"
cp "$out/dex/classes.dex" "$out/stage/"

echo "build-apk: 4/6 linking the manifest and resources with aapt2" >&2
"$bt/aapt2" compile --dir "$here/apk/res" -o "$out/res/compiled.zip"
"$bt/aapt2" link -o "$out/base.apk" -I "$platform" \
  --manifest "$here/apk/AndroidManifest.xml" \
  --min-sdk-version 26 --target-sdk-version 34 "$out/res/compiled.zip"

echo "build-apk: 5/6 adding classes.dex and the library" >&2
python3 -I - "$out/base.apk" "$out/stage" <<'PY'
import os, sys, zipfile
apk, stage = sys.argv[1], sys.argv[2]
with zipfile.ZipFile(apk, "a", zipfile.ZIP_DEFLATED) as z:
    for dirpath, _, files in os.walk(stage):
        for f in files:
            full = os.path.join(dirpath, f)
            z.write(full, os.path.relpath(full, stage))
PY

echo "build-apk: 6/6 aligning and signing" >&2
"$bt/zipalign" -p -f 4 "$out/base.apk" "$out/aligned.apk"
"$bt/apksigner" sign --ks "$PHAROS_ANDROID_KEYSTORE" \
  --ks-pass "file:$PHAROS_ANDROID_KEYSTORE_PASS_FILE" \
  --ks-key-alias "$PHAROS_ANDROID_KEY_ALIAS" \
  --out "$out/pharos-auto-spike.apk" "$out/aligned.apk"
"$bt/apksigner" verify "$out/pharos-auto-spike.apk"
echo "$out/pharos-auto-spike.apk"
