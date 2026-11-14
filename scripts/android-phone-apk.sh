# Build, align and sign the Pharos phone APK (spec 044, T17). Run from
# anywhere in the repo as `nix run .#android-phone-apk [-- --release]`.
#
# One libmain.so carries the shared UI and the car's media service: dx
# builds it with the dx activity, pharos-classgen writes the car's entry
# classes against it, and they go in as classes2.dex beside dx's own.
#
# Signing uses a keystore YOU provide, as for the TV APK:
#   PHAROS_ANDROID_KEYSTORE            path to the .keystore / .jks
#   PHAROS_ANDROID_KEYSTORE_PASS_FILE  file holding the store+key password
#   PHAROS_ANDROID_KEY_ALIAS           key alias in that keystore
# Optional:
#   PHAROS_ANDROID_APK_OUT             output path (default target/pharos-phone-<profile>.apk)

set -euo pipefail

profile=debug
dx_flags=()
for arg in "$@"; do
  case "$arg" in
    --release) profile=release; dx_flags+=(--release) ;;
    *) echo "android-phone-apk: unknown argument: $arg" >&2; exit 2 ;;
  esac
done

missing=0
for var in PHAROS_ANDROID_KEYSTORE PHAROS_ANDROID_KEYSTORE_PASS_FILE PHAROS_ANDROID_KEY_ALIAS; do
  if [ -z "${!var:-}" ]; then
    echo "android-phone-apk: $var is not set" >&2
    missing=1
  fi
done
[ "$missing" -eq 0 ] || exit 1
for f in "$PHAROS_ANDROID_KEYSTORE" "$PHAROS_ANDROID_KEYSTORE_PASS_FILE"; do
  if [ ! -f "$f" ]; then
    echo "android-phone-apk: no such file: $f" >&2
    exit 1
  fi
done
PHAROS_ANDROID_KEYSTORE=$(realpath "$PHAROS_ANDROID_KEYSTORE")
PHAROS_ANDROID_KEYSTORE_PASS_FILE=$(realpath "$PHAROS_ANDROID_KEYSTORE_PASS_FILE")

root=$(git rev-parse --show-toplevel)
cd "$root"
target_dir=$root/target
out=${PHAROS_ANDROID_APK_OUT:-$target_dir/pharos-phone-$profile.apk}
gradle_root=$target_dir/dx/pharos-phone/$profile/android/app
main_dir=$gradle_root/app/src/main

# shellcheck disable=SC2016 # expanded by the dev shell, not here
android_home=$(nix develop .#android -c sh -c 'echo "$ANDROID_HOME"')
build_tools=$android_home/build-tools/35.0.0
platform=$android_home/platforms/android-34/android.jar

# Phones are arm64; a 32-bit-only phone is not a target (spec 044).
echo "android-phone-apk: 1/6 building libmain.so for arm64 ($profile)" >&2
# The manifest names xml/automotive_app_desc, which only the overlay in step
# 3 supplies, and dx has no setting to add resources. So dx's own gradle run
# fails at resource linking after it has built libmain.so and written the
# project; step 4 runs gradle again with the resources in place. Removing the
# old library first means one found afterwards came from this build.
rm -rf "$main_dir/jniLibs"
dx_status=0
nix develop .#android -c dx build --platform android --package pharos-phone \
  --bin pharos-phone --features mobile --target aarch64-linux-android \
  "${dx_flags[@]}" || dx_status=$?
[ -f "$main_dir/jniLibs/arm64-v8a/libmain.so" ] && [ -f "$gradle_root/gradlew" ] || {
  echo "android-phone-apk: dx build failed with status $dx_status and left no libmain.so or gradle project (see its error above)" >&2
  exit 1
}
if [ "$dx_status" -ne 0 ]; then
  echo "android-phone-apk: dx assemble status=$dx_status, expected before the resource overlay; libmain.so is built, continuing" >&2
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "android-phone-apk: 2/6 writing the car's entry classes for libmain" >&2
nix develop -c cargo run -q -p pharos-classgen -- --lib main "$tmp/classes"
mapfile -t classes < <(find "$tmp/classes" -name '*.class')
[ "${#classes[@]}" -gt 0 ] || {
  echo "android-phone-apk: pharos-classgen wrote no classes" >&2
  exit 1
}
mkdir -p "$tmp/dex"
nix develop .#android -c "$build_tools/d8" --release --min-api 26 \
  --lib "$platform" --output "$tmp/dex" "${classes[@]}"
mv "$tmp/dex/classes.dex" "$tmp/classes2.dex"

# dx has no setting for the launcher icon or app_name: lay the shared
# Pharos resources, then the phone's own, over the project it generated.
echo "android-phone-apk: 3/6 laying the Pharos resources over dx's" >&2
cp -r crates/pharos-ui/android/res/. "$main_dir/res/"
cp -r crates/pharos-phone/android/res/. "$main_dir/res/"
rm -f "$main_dir/res/README.md"

# dx writes versionCode 1 every build; count minutes since 2026-01-01 UTC so
# every build installs as newer (as the TV script does).
version_code=${PHAROS_ANDROID_VERSION_CODE:-$((($(date -u +%s) - 1767225600) / 60))}
sed -i "s/versionCode = [0-9]*/versionCode = $version_code/" "$gradle_root/app/build.gradle.kts"
grep -q "versionCode = $version_code" "$gradle_root/app/build.gradle.kts" || {
  echo "android-phone-apk: could not set versionCode $version_code" >&2
  exit 1
}

echo "android-phone-apk: 4/6 assembling with gradle" >&2
apk_dir=$gradle_root/app/build/outputs/apk
rm -rf "$apk_dir"
(cd "$gradle_root" && nix develop "$root#android" -c ./gradlew --quiet assembleDebug) || {
  echo "android-phone-apk: gradle assembleDebug failed (see its error above)" >&2
  exit 1
}
built=$(find "$apk_dir" -name '*.apk' | head -n 1)
[ -n "$built" ] || {
  echo "android-phone-apk: no APK found under $apk_dir" >&2
  exit 1
}

echo "android-phone-apk: 5/6 adding classes2.dex" >&2
cp "$built" "$tmp/unsigned.apk"
if unzip -l "$tmp/unsigned.apk" classes2.dex >/dev/null 2>&1; then
  echo "android-phone-apk: gradle already wrote a classes2.dex; refusing to replace it" >&2
  exit 1
fi
(cd "$tmp" && zip -q -0 unsigned.apk classes2.dex)

AAPT2=$build_tools/aapt2 bash "$root/scripts/android-phone-apk-check.sh" "$tmp/unsigned.apk" || {
  echo "android-phone-apk: the built APK failed its checks; not signing it" >&2
  exit 1
}

echo "android-phone-apk: 6/6 aligning and signing" >&2
mkdir -p "$(dirname "$out")"
"$build_tools/zipalign" -f -p 4 "$tmp/unsigned.apk" "$tmp/aligned.apk"
nix develop .#android -c "$build_tools/apksigner" sign \
  --ks "$PHAROS_ANDROID_KEYSTORE" \
  --ks-pass "file:$PHAROS_ANDROID_KEYSTORE_PASS_FILE" \
  --ks-key-alias "$PHAROS_ANDROID_KEY_ALIAS" \
  --out "$out" "$tmp/aligned.apk"
nix develop .#android -c "$build_tools/apksigner" verify --print-certs "$out" | sed -n '1,2p'
ls -l "$out"
