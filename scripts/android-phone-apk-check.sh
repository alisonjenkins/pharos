#!/usr/bin/env bash
# Check a built Pharos phone APK carries what the phone and the car need
# (spec 044, T17): the Pharos label, minSdk 26, the car's media service,
# art provider and voice search entry, the injected classes2.dex with the
# car's entry classes, and libmain.so exporting every `Java_` symbol those
# classes call. An APK whose library lost its exports (no `link()` use)
# loads, then throws UnsatisfiedLinkError in the car; this catches it here.
#
#   bash scripts/android-phone-apk-check.sh path/to/pharos-phone.apk
#
# AAPT2 names the aapt2 binary; without it the script asks the `.#android`
# dev shell. Each check prints `ok` or `FAIL` with the value it found; the
# exit status is the number of failures (0 = good).

set -u

apk=${1:-}
if [ -z "$apk" ] || [ ! -f "$apk" ]; then
  echo "android-phone-apk-check: usage: android-phone-apk-check.sh <apk>" >&2
  exit 64
fi
root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)

# shellcheck disable=SC2016 # expanded by the dev shell, not here
tools=$(cd "$root" && nix develop .#android -c sh -c \
  'echo "$ANDROID_HOME/build-tools/35.0.0/aapt2"; ls -d "$ANDROID_NDK_HOME"/toolchains/llvm/prebuilt/*/bin/llvm-nm')
AAPT2=${AAPT2:-$(printf '%s\n' "$tools" | sed -n 1p)}
LLVM_NM=${LLVM_NM:-$(printf '%s\n' "$tools" | sed -n 2p)}
for tool in "$AAPT2" "$LLVM_NM"; do
  if [ ! -x "$tool" ]; then
    echo "android-phone-apk-check: tool not found at '$tool' (set AAPT2 / LLVM_NM)" >&2
    exit 64
  fi
done

failures=0
pass() { echo "ok    $1"; }
fail() { echo "FAIL  $1"; failures=$((failures + 1)); }

badging=$("$AAPT2" dump badging "$apk" 2>&1) || {
  echo "android-phone-apk-check: aapt2 could not read $apk: $badging" >&2
  exit 64
}
manifest=$("$AAPT2" dump xmltree --file AndroidManifest.xml "$apk" 2>&1) || {
  echo "android-phone-apk-check: aapt2 could not dump the manifest: $manifest" >&2
  exit 64
}

label=$(printf '%s\n' "$badging" | sed -n "s/^application-label:'\(.*\)'$/\1/p" | head -n 1)
if [ "$label" = "Pharos" ]; then pass "label is Pharos"; else fail "label is '$label', want 'Pharos'"; fi

package=$(printf '%s\n' "$badging" | sed -n "s/^package: name='\([^']*\)'.*/\1/p")
if [ -n "$package" ]; then pass "package is $package"; else fail "no package name in badging"; fi

min_sdk=$(printf '%s\n' "$badging" | sed -n "s/^minSdkVersion:'\(.*\)'$/\1/p")
if [ "$min_sdk" = "26" ]; then pass "minSdk is 26"; else fail "minSdk is '$min_sdk', want '26'"; fi

for name in \
  com.alisonjenkins.pharos.auto.PharosMediaService \
  com.alisonjenkins.pharos.auto.PharosArtProvider \
  com.alisonjenkins.pharos.auto.PharosSearchActivity \
  dev.dioxus.main.MainActivity; do
  if printf '%s\n' "$manifest" | grep -q "\"$name\""; then pass "manifest declares $name"; else fail "manifest lacks $name"; fi
done
# The art provider's authority follows the application id, so this APK and
# the spike's can both be installed (art.rs derives the same value).
if printf '%s\n' "$manifest" | grep -q "\"$package.art\""; then
  pass "art authority is $package.art"
else
  fail "art authority is not $package.art"
fi
if printf '%s\n' "$manifest" | grep -q '"android.media.browse.MediaBrowserService"'; then
  pass "service answers MediaBrowserService"
else
  fail "service has no MediaBrowserService intent filter"
fi

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
if unzip -q -o "$apk" classes2.dex lib/arm64-v8a/libmain.so -d "$tmp" 2>/dev/null; then
  pass "APK has classes2.dex and lib/arm64-v8a/libmain.so"
else
  fail "APK lacks classes2.dex or lib/arm64-v8a/libmain.so"
fi
if [ -f "$tmp/classes2.dex" ]; then
  for class in PharosMediaService PharosArtProvider PharosSearchActivity PharosPlayerListener; do
    if grep -aq "Lcom/alisonjenkins/pharos/auto/$class;" "$tmp/classes2.dex"; then
      pass "classes2.dex holds $class"
    else
      fail "classes2.dex lacks $class"
    fi
  done
fi
if [ -f "$tmp/lib/arm64-v8a/libmain.so" ]; then
  exports=$("$LLVM_NM" -D --defined-only "$tmp/lib/arm64-v8a/libmain.so" | awk '$3 ~ /^Java_com_alisonjenkins_pharos_auto_/ {print $3}' | sort -u)
  count=$(printf '%s\n' "$exports" | grep -c . || true)
  if [ "$count" -gt 0 ]; then pass "libmain.so exports $count car entry points"; else fail "libmain.so exports no Java_com_alisonjenkins_pharos_auto_ symbol"; fi
  if printf '%s\n' "$exports" | grep -q "PharosMediaService_nativeOnCreate"; then
    pass "libmain.so exports PharosMediaService.nativeOnCreate"
  else
    fail "libmain.so lacks PharosMediaService.nativeOnCreate"
  fi
fi

exit "$failures"
