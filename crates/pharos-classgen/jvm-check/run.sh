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
java -Xverify:all -Djava.library.path="$lib_dir" \
  -cp "$work/stubs:$work/gen:$work/main" Check
