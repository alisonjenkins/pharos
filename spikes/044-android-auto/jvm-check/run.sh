#!/usr/bin/env bash
# Spike 044 desktop check: generated classes + Rust library on a desktop JVM,
# against stand-in android.* classes (stubs/, test scaffolding only, never
# shipped). Needs a JDK and cargo. Prints ALL PASS on success.
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
(cd "$here/../auto" && cargo build -q)
(cd "$here/../classgen" && cargo run -q -- "$work/gen" >/dev/null)
javac -d "$work/stubs" $(find "$here/stubs" -name '*.java')
javac -cp "$work/stubs:$work/gen" -d "$work/main" "$here/Check.java"
java -Xverify:all -Djava.library.path="$here/../auto/target/debug" \
  -cp "$work/stubs:$work/gen:$work/main" Check
