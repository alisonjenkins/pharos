#!/usr/bin/env bash
# Check a built Android TV APK carries what the TV needs (B1329, V1302):
# the Pharos label, launcher icon and TV banner from
# crates/pharos-ui/android/res, the dark AppTheme that keeps the launch
# window from flashing white, the TV launcher entry, and every config change
# the activity must survive. An APK built by running `dx` directly skips the
# res overlay in scripts/android-apk.sh and fails here.
#
#   just tv-apk-check path/to/pharos-tv.apk
#   bash scripts/android-apk-check.sh path/to/pharos-tv.apk   # from the repo
#
# AAPT2 names the aapt2 binary; without it the script asks the `.#android`
# dev shell for build-tools 35.0.0's. Each check prints `ok` or `FAIL` with
# the value it found; the exit status is the number of failures (0 = good).

set -u

apk=${1:-}
if [ -z "$apk" ] || [ ! -f "$apk" ]; then
  echo "android-apk-check: usage: android-apk-check.sh <apk>" >&2
  exit 64
fi

root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)
res=$root/crates/pharos-ui/android/res
if [ ! -d "$res" ]; then
  echo "android-apk-check: no resource overlay at $res (run from the repo)" >&2
  exit 64
fi

if [ -z "${AAPT2:-}" ]; then
  # shellcheck disable=SC2016 # expanded by the dev shell, not here
  AAPT2=$(cd "$root" && nix develop .#android -c sh -c 'echo "$ANDROID_HOME/build-tools/35.0.0/aapt2"')
fi
if [ ! -x "$AAPT2" ]; then
  echo "android-apk-check: aapt2 not found at '$AAPT2' (set AAPT2)" >&2
  exit 64
fi

failures=0
pass() { echo "ok    $1"; }
fail() { echo "FAIL  $1"; failures=$((failures + 1)); }

badging=$("$AAPT2" dump badging "$apk" 2>&1) || {
  echo "android-apk-check: aapt2 could not read $apk: $badging" >&2
  exit 64
}
manifest=$("$AAPT2" dump xmltree --file AndroidManifest.xml "$apk" 2>&1) || {
  echo "android-apk-check: aapt2 could not dump the manifest of $apk: $manifest" >&2
  exit 64
}
resources=$("$AAPT2" dump resources "$apk" 2>&1) || {
  echo "android-apk-check: aapt2 could not dump the resources of $apk: $resources" >&2
  exit 64
}

# The launcher shows the application label; dx's own is "PharosTv".
label=$(printf '%s\n' "$badging" | sed -n "s/^application-label:'\(.*\)'$/\1/p" | head -n 1)
if [ "$label" = "Pharos" ]; then pass "label is Pharos"; else fail "label is '$label', want 'Pharos'"; fi

# dx writes versionCode 1 every build; android-apk.sh raises it, since the
# Google TV launcher keeps an app's old label until the code rises.
version=$(printf '%s\n' "$badging" | sed -n "s/^package: .* versionCode='\([0-9]*\)'.*/\1/p" | head -n 1)
if [ "${version:-0}" -gt 1 ]; then pass "versionCode $version"; else fail "versionCode is '${version:-none}', want above 1 (built without android-apk.sh?)"; fi

# The TV launcher lists only leanback-launchable apps.
leanback=$(printf '%s\n' "$badging" | sed -n "s/^leanback-launchable-activity: name='\([^']*\)'.*/\1/p" | head -n 1)
if [ -n "$leanback" ]; then pass "TV launcher entry ($leanback)"; else fail "no leanback-launchable-activity: the TV launcher will not list the app"; fi

# The banner: android-apk.sh points it at pharos_banner; dx points it at
# the icon, which the TV draws as a stretched robot.
banner=$(printf '%s\n' "$badging" | sed -n "s/^application:.* banner='\([^']*\)'.*/\1/p" | head -n 1)
case "$banner" in
  */pharos_banner.png) pass "banner is $banner" ;;
  *) fail "banner is '$banner', want res/drawable*/pharos_banner.png" ;;
esac

# The icon: the TV (API 26+) draws the adaptive icon, whose foreground
# drawable the overlay replaces under dx's own name. Its first path must
# be the Pharos mark's.
mark=$(sed -n 's/.*android:pathData="\([^"]*\)".*/\1/p' "$res/drawable-v24/ic_launcher_foreground.xml" | head -n 1)
foreground=$("$AAPT2" dump xmltree --file res/drawable-v24/ic_launcher_foreground.xml "$apk" 2>&1)
if [ -n "$mark" ] && printf '%s\n' "$foreground" | grep -qF "$mark"; then
  pass "launcher icon is the Pharos mark"
else
  fail "launcher icon foreground lacks the Pharos mark path '$mark' (dx's default icon?)"
fi

# The theme: the manifest's android:theme must be AppTheme, and AppTheme's
# window background Pharos Night, or the window is white until the WebView
# paints.
theme_ref=$(printf '%s\n' "$manifest" | sed -n 's/.*android:theme([^)]*)=@\(0x[0-9a-fA-F]*\).*/\1/p' | head -n 1)
apptheme_id=$(printf '%s\n' "$resources" | sed -n 's/^ *resource \(0x[0-9a-fA-F]*\) style\/AppTheme$/\1/p' | head -n 1)
if [ -n "$theme_ref" ] && [ "$theme_ref" = "$apptheme_id" ]; then
  pass "theme is AppTheme ($theme_ref)"
else
  fail "manifest theme is '${theme_ref:-none}', AppTheme is '${apptheme_id:-missing}'"
fi
# aapt2 35 prints a framework attribute in a style as its raw id
# (0x01010054=@color/pharos_night), others by name; the value may be the
# colour's name or its id. Either form counts.
night_id=$(printf '%s\n' "$resources" | sed -n 's/^ *resource \(0x[0-9a-fA-F]*\) color\/pharos_night$/\1/p' | head -n 1)
night=$(printf '%s\n' "$resources" | grep -A1 -E '^ *resource 0x[0-9a-fA-F]+ color/pharos_night$' | grep -ioE '#ff0e1116' | head -n 1)
window_bg=$(printf '%s\n' "$resources" | sed -n '/^ *resource 0x[0-9a-fA-F]* style\/AppTheme$/,/^ *resource /p' |
  grep -E '(windowBackground|0x01010054)' | grep -cE "=@(color/pharos_night|${night_id:-none})\b")
if [ -n "$night" ] && [ "$window_bg" -gt 0 ]; then
  pass "AppTheme has a Pharos Night window background"
else
  fail "AppTheme window background is not Pharos Night (pharos_night='${night:-missing}', windowBackground items=$window_bg)"
fi

# Config changes the activity handles in place (B1323): any other makes
# Android recreate the NativeActivity, which comes back to a dark window
# with no page. density|keyboard|keyboardHidden|navigation|orientation|
# screenLayout|screenSize|smallestScreenSize|uiMode.
want=$((0x1000 | 0x10 | 0x20 | 0x40 | 0x80 | 0x100 | 0x400 | 0x800 | 0x200))
raw=$(printf '%s\n' "$manifest" | sed -n 's/.*android:configChanges([^)]*)=\(([^)]*)\)\{0,1\}\([0-9a-fA-Fx]*\).*/\2/p' | head -n 1)
if [ -n "$raw" ]; then
  have=$((raw))
  if [ $((have & want)) -eq "$want" ]; then
    pass "configChanges $(printf '0x%x' "$have") covers 0x$(printf '%x' "$want")"
  else
    fail "configChanges $(printf '0x%x' "$have") lacks 0x$(printf '%x' $((want & ~have)))"
  fi
else
  fail "no android:configChanges on the activity"
fi

if [ "$failures" -eq 0 ]; then
  echo "android-apk-check: $apk is good"
else
  echo "android-apk-check: $failures check(s) failed for $apk" >&2
fi
exit "$failures"
