#!/usr/bin/env bash
# Desktop check: the generated classes and the Rust JNI library on a desktop
# JVM, against stand-in android.* classes (stubs/, test scaffolding only,
# never shipped). Needs a JDK and cargo. Prints ALL PASS on success.
#
# The JNI library is pharos-android's jvm_check_lib example: the crate with
# its test catalogue, linked through pharos_android::link as the app is.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../../.." && pwd)
jni_lib=jvm_check_lib
lib_dir="$root/target/debug/examples"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
(cd "$root" && cargo build -q -p pharos-android --example "$jni_lib" --features test-catalogue)

# The JVM resolves a native method on its first call, so a dropped export
# would pass -Xverify. Every Java_ function in the source must be in the .so.
want=$(grep -rhoE '\bJava_[A-Za-z0-9_]+' "$root/crates/pharos-android/src" | sort -u)
have=$(nm -D --defined-only "$lib_dir/lib$jni_lib.so" | grep -oE '\bJava_[A-Za-z0-9_]+' | sort -u)
missing=$(comm -23 <(echo "$want") <(echo "$have"))
if [ -n "$missing" ]; then
  echo "FAIL exports missing from lib$jni_lib.so:" >&2
  echo "$missing" >&2
  exit 1
fi
echo "ok exports: $(echo "$have" | wc -l) Java_ symbols in lib$jni_lib.so"

(cd "$root" && cargo run -q -p pharos-classgen -- --lib "$jni_lib" "$work/gen" >/dev/null)
javac -d "$work/stubs" $(find "$here/stubs" -name '*.java')
javac -cp "$work/stubs:$work/gen" -d "$work/main" "$here/Check.java"
# The library logs to stderr on a desktop; keep a copy to check below.
java -Xverify:all -Djava.library.path="$lib_dir" \
  -cp "$work/stubs:$work/gen:$work/main" Check 2>"$work/rust.log" || java_status=$?
cat "$work/rust.log" >&2
[ "${java_status:-0}" -eq 0 ] || exit "$java_status"

# T29: the decision lines of the notification (T10), becoming-noisy (T11)
# and resume positions (T12) are part of the contract: a log query on a
# phone finds each of these, so a dropped or reworded line fails here.
log_failures=0
while IFS= read -r line; do
  [ -n "$line" ] || continue
  if grep -qE -- "$line" "$work/rust.log"; then
    echo "ok log: $line"
  else
    echo "FAIL log line missing: $line" >&2
    log_failures=$((log_failures + 1))
  fi
done <<'LINES'
^pharos-auto: notify: channel created id=
^pharos-auto: notify: foreground=on state=3$
^pharos-auto: notify: foreground=off notification=kept state=2$
^pharos-auto: notify: foreground=off notification=removed state=7$
^pharos-auto: noisy: receiver registered action=android\.media\.AUDIO_BECOMING_NOISY$
^pharos-auto: noisy: action=pause$
^pharos-auto: noisy: action=none$
^pharos-auto: noisy: receiver unregistered$
^pharos-auto: resume: id=tone:440 verdict=start reason=none_saved duration_ms=1200000$
^pharos-auto: resume: id=tone:440 verdict=saved at_ms=300000 duration_ms=1200000 why=pause$
^pharos-auto: resume: id=tone:440 verdict=seek at_ms=300000 duration_ms=1200000$
LINES
if [ "$log_failures" -ne 0 ]; then
  echo "FAIL $log_failures decision log line(s) missing" >&2
  exit 1
fi
echo "ok log lines: every decision line present"
