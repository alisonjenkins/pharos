#!/usr/bin/env bash
# Desktop check: the generated classes and the Rust JNI library on a desktop
# JVM, against stand-in android.* classes (stubs/, test scaffolding only,
# never shipped). Needs a JDK and cargo. Prints ALL PASS on success.
#
# The JNI library is still the spike's until spec 044 T8 moves it into
# crates/pharos-android.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
jni_crate="$here/../../../spikes/044-android-auto/auto"
jni_lib=pharos_auto
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
(cd "$jni_crate" && cargo build -q)
(cd "$here/.." && cargo run -q -p pharos-classgen -- --lib "$jni_lib" "$work/gen" >/dev/null)
javac -d "$work/stubs" $(find "$here/stubs" -name '*.java')
javac -cp "$work/stubs:$work/gen" -d "$work/main" "$here/Check.java"
java -Xverify:all -Djava.library.path="$jni_crate/target/debug" \
  -cp "$work/stubs:$work/gen:$work/main" Check
