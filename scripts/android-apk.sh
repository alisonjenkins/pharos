# Build, align and sign the Android TV APK (spec 025). Run from anywhere in
# the repo as `nix run .#android-apk [-- --release]`.
#
# Signing uses a keystore YOU provide; this script never creates keys, and
# nothing here puts one in the repo or the nix store:
#   PHAROS_ANDROID_KEYSTORE            path to the .keystore / .jks
#   PHAROS_ANDROID_KEYSTORE_PASS_FILE  file holding the store+key password
#   PHAROS_ANDROID_KEY_ALIAS           key alias in that keystore
# Optional:
#   PHAROS_ANDROID_APK_OUT             output path (default target/pharos-tv-<profile>.apk)
#   CARGO_TARGET_DIR                   cargo/dx target dir (default <repo>/target)

profile=debug
dx_flags=()
for arg in "$@"; do
  case "$arg" in
    --release) profile=release; dx_flags+=(--release) ;;
    *) echo "android-apk: unknown argument: $arg" >&2; exit 2 ;;
  esac
done

missing=0
for var in PHAROS_ANDROID_KEYSTORE PHAROS_ANDROID_KEYSTORE_PASS_FILE PHAROS_ANDROID_KEY_ALIAS; do
  if [ -z "${!var:-}" ]; then
    echo "android-apk: $var is not set" >&2
    missing=1
  fi
done
[ "$missing" -eq 0 ] || exit 1
for f in "$PHAROS_ANDROID_KEYSTORE" "$PHAROS_ANDROID_KEYSTORE_PASS_FILE"; do
  if [ ! -f "$f" ]; then
    echo "android-apk: no such file: $f" >&2
    exit 1
  fi
done

# Absolute before the cd below, so a path relative to where this was run
# still names the same file for apksigner.
PHAROS_ANDROID_KEYSTORE=$(realpath "$PHAROS_ANDROID_KEYSTORE")
PHAROS_ANDROID_KEYSTORE_PASS_FILE=$(realpath "$PHAROS_ANDROID_KEYSTORE_PASS_FILE")

root=$(git rev-parse --show-toplevel)
cd "$root" || exit 1
target_dir=${CARGO_TARGET_DIR:-$root/target}
out=${PHAROS_ANDROID_APK_OUT:-$target_dir/pharos-tv-$profile.apk}

echo "android-apk: building the page visualiser" >&2
nix develop -c sh scripts/tv-visualiser.sh

gradle_root=$target_dir/dx/pharos-tv/$profile/android/app
main_dir=$gradle_root/app/src/main
dx_build() {
  echo "android-apk: building $1 ($profile)" >&2
  nix develop .#android -c dx build --platform android --package pharos-ui \
    --bin pharos-tv --features mobile --target "$1" \
    "${dx_flags[@]}" || {
    echo "android-apk: dx build for $1 failed (see its error above)" >&2
    exit 1
  }
}

# One APK for every TV: 64-bit boxes (NVIDIA Shield) run arm64-v8a natively,
# 32-bit-only ones (the TCL G08) take armeabi-v7a. dx builds one target per
# run into the same gradle project, so build arm64 first, keep its library,
# and put it back beside the armv7 one before gradle assembles below.
arm64_lib=$target_dir/pharos-tv-$profile-arm64-v8a.so
dx_build aarch64-linux-android
cp "$main_dir/jniLibs/arm64-v8a/libmain.so" "$arm64_lib" || {
  echo "android-apk: the arm64 build left no jniLibs/arm64-v8a/libmain.so" >&2
  exit 1
}
dx_build armv7-linux-androideabi
mkdir -p "$main_dir/jniLibs/arm64-v8a"
cp "$arm64_lib" "$main_dir/jniLibs/arm64-v8a/libmain.so"
rm -f "$arm64_lib"

# dx 0.7 has no setting for the launcher icon, TV banner or app_name: lay
# crates/pharos-ui/android/res over the gradle project it generated, point
# the banner at ours, and assemble again (crates/pharos-ui/android/res/README.md).
cp -r crates/pharos-ui/android/res/. "$main_dir/res/"
rm -f "$main_dir/res/README.md"
sed -i 's|android:banner="@mipmap/ic_launcher"|android:banner="@drawable/pharos_banner"|' \
  "$main_dir/AndroidManifest.xml"
grep -q 'android:banner="@drawable/pharos_banner"' "$main_dir/AndroidManifest.xml" || {
  echo "android-apk: could not point the manifest banner at pharos_banner" >&2
  exit 1
}
# dx writes versionCode 1 every build. The Google TV launcher keeps an app's
# label until its versionCode rises (a reinstall or reboot does not refresh
# it), so count minutes since 2026-01-01 UTC: every build installs as newer.
version_code=${PHAROS_ANDROID_VERSION_CODE:-$((($(date -u +%s) - 1767225600) / 60))}
sed -i "s/versionCode = [0-9]*/versionCode = $version_code/" "$gradle_root/app/build.gradle.kts"
grep -q "versionCode = $version_code" "$gradle_root/app/build.gradle.kts" || {
  echo "android-apk: could not set versionCode $version_code" >&2
  exit 1
}
apk_dir=$gradle_root/app/build/outputs/apk
rm -rf "$apk_dir"
# dx runs assembleRelease only when [bundle.android] signing is configured,
# which it is not here (this script signs), so the APK is always under debug/.
echo "android-apk: assembling with the Pharos icon and banner" >&2
(cd "$gradle_root" && nix develop "$root#android" -c ./gradlew --quiet assembleDebug) || {
  echo "android-apk: gradle assembleDebug failed (see its error above)" >&2
  exit 1
}

built=$(find "$apk_dir" -name '*.apk' | head -n 1)
if [ -z "$built" ]; then
  echo "android-apk: no APK found under $apk_dir" >&2
  exit 1
fi

# shellcheck disable=SC2016 # expanded by the dev shell, not here
build_tools=$(nix develop .#android -c sh -c 'echo "$ANDROID_HOME/build-tools/35.0.0"')

# B1329: refuse an APK missing the TV's label, icon, banner, dark theme or
# config changes before it is signed (scripts/android-apk-check.sh).
echo "android-apk: checking the APK's TV resources" >&2
AAPT2=$build_tools/aapt2 bash "$root/scripts/android-apk-check.sh" "$built" || {
  echo "android-apk: the built APK failed its checks; not signing it" >&2
  exit 1
}
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
mkdir -p "$(dirname "$out")"
"$build_tools/zipalign" -f -p 4 "$built" "$tmp/aligned.apk"
# JAVA_HOME comes from the dev shell; apksigner is a java launcher.
nix develop .#android -c "$build_tools/apksigner" sign \
  --ks "$PHAROS_ANDROID_KEYSTORE" \
  --ks-pass "file:$PHAROS_ANDROID_KEYSTORE_PASS_FILE" \
  --ks-key-alias "$PHAROS_ANDROID_KEY_ALIAS" \
  --out "$out" "$tmp/aligned.apk"
verified=$(nix develop .#android -c "$build_tools/apksigner" verify --print-certs "$out")
printf '%s\n' "$verified" | sed -n '1,2p'
ls -l "$out"
