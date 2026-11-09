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

echo "android-apk: building ($profile)" >&2
nix develop .#android -c dx build --platform android --package pharos-ui \
  --bin pharos-tv --features mobile --target armv7-linux-androideabi \
  "${dx_flags[@]}"

# dx writes <target>/dx/pharos-tv/<profile>/android/app/app/build/outputs/apk/<profile>/*.apk
apk_dir=$target_dir/dx/pharos-tv/$profile/android/app/app/build/outputs/apk/$profile
built=$(find "$apk_dir" -maxdepth 1 -name '*.apk' | head -n 1)
if [ -z "$built" ]; then
  echo "android-apk: no APK found under $apk_dir" >&2
  exit 1
fi

# shellcheck disable=SC2016 # expanded by the dev shell, not here
build_tools=$(nix develop .#android -c sh -c 'echo "$ANDROID_HOME/build-tools/35.0.0"')
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
