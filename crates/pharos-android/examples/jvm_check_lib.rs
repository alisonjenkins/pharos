//! The JNI library the host JVM check loads (`crates/pharos-classgen/jvm-check`):
//! pharos-android with its test catalogue, linked the way the app's cdylib
//! links it, through `pharos_android::link`.

#[used]
static KEEP: fn() = pharos_android::link;
