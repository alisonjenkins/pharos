# ADR-0131: One APK for the phone UI and the car service

- **Status:** Accepted
- **Date:** 2026-11-14T12:00:00Z
- **Deciders:** Alison (wants one real app for the phone and the car, in pure Rust)

## Context

Spec 044 ships one app that has a phone UI and an Android Auto service.

- The phone UI is Dioxus. `dx` packages it. `dx` generates a Gradle project
  and Gradle builds the APK.
- The car service is the set of generated Java classes from ADR-0127. The
  platform instantiates them from the manifest.
- The library, the auth token, the cache and the player are Rust statics.
  The UI and the service must see the same ones.
- Pure Rust means no hand-written Kotlin or Java (ADR-0061). The glue `dx`
  generates counts as build output.

Task T1 verified these facts on 2026-11-14 (`dx` 0.8.0-alpha.3, NDK 29,
`aarch64-linux-android`, debug):

- `dx` builds one library: `libmain.so`. It loads it with
  `System.loadLibrary("main")`.
- The launcher is `dev.dioxus.main.MainActivity`.
- The glue is Kotlin under `app/src/main/kotlin/`. `app/src/main/java/` is
  empty.
- `dx` exports no `JNI_OnLoad`. Its natives are `Java_dev_dioxus_main_*`.
  Our names do not clash.
- A `#[no_mangle] extern "system"` fn in a dependency rlib is dropped from
  `libmain.so` when nothing references it.
- A `link()` fn called from `main` keeps it. It takes each export's address
  through `std::hint::black_box`.
- The `build.rs` arg `-Wl,--undefined=<sym>` alone did not keep it. The
  cause is not found.
- `manganis::ffi` takes a Gradle module directory or an `.aar`. It rejects a
  `.jar`.

## Decision

We will ship one APK, one process and one `.so`.

1. **`dx` stays the packager.** `dx` builds the Gradle project and
   `libmain.so`. Gradle builds the APK as it does for the TV app.
2. **Our classes are an extra dex.** `pharos-classgen` writes the `.class`
   files. `d8` makes `classes2.dex`. The script appends it to the finished
   APK, then runs `zipalign` and `apksigner`. `System.loadLibrary` in the
   generated classes is given `"main"`.
3. **The `pharos-android` rlib holds the JNI exports.** `main` calls
   `pharos_android::link()`. It references each export through
   `std::hint::black_box`. This call is required, not a fallback.
4. **The UI calls the native player through in-process port traits.** The
   traits live in `pharos-ui`. Dioxus mobile runs app code natively. No JNI
   and no IPC sit between the UI and the player.
5. **A check script guards the exports.** It runs `llvm-nm -D` on
   `libmain.so` and fails if an expected `Java_` symbol is missing.

## Consequences

- Library, token, cache and player are shared statics. The phone UI and the
  car service see the same state.
- The pure-Rust rule holds. The Kotlin in the APK is `dx` build output. Our
  classes are generated from Rust (ADR-0127).
- No edit to `build.gradle.kts` for classes. R8 cannot strip classes that
  only the manifest names.
- **Risk: `dx` alpha churn.** `dx` 0.8.0-alpha.3 is an alpha. Class names,
  the library name, the generated tree and the `manganis` rules can change
  between releases. Pin the version. Re-run the T1 checks on every bump.
- **Risk: the export drop is silent.** A missing `Java_` symbol shows up on
  the device as `UnsatisfiedLinkError`. The check script moves that failure
  to build time.
- The check script needs `llvm-nm`. It is not on the dev shell's PATH. Take
  it from `$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/*/bin/llvm-nm`.
- Not verified: the release profile (LTO, strip) and armv7. The check script
  must run on a release build before the first release.
- The `dx` application id comes from the cargo package name. The phone crate
  must set it explicitly.
- Every new JNI export is added to `link()`. The check script lists it.

## Alternatives considered

- **Two `.so` files (`libmain.so` plus `libpharos_auto.so`).** Each library
  has its own copy of every Rust static. The service would not see the
  token, the cache or the player the UI owns. Rejected.
- **Drop `dx`, write a Rust WebView host.** wry's Android half is Kotlin
  built by Gradle. We would re-implement it and the Dioxus IPC. Size XL.
  Rejected.
- **Drop `dx`, use `NativeActivity` with Dioxus Native (Blitz).** One `.so`
  and no Gradle. Blitz is experimental and ADR-0077 is still Proposed.
  Revisit if the TV line adopts it.
- **A separate APK for the car service.** Two processes, two sets of
  statics, a second install and a second sign-in. Cross-app IPC would be
  needed for every player call. Rejected.
- **Wrap the classes in an `.aar` and use `manganis::ffi`.** It needs a
  wrapper and was not exercised end to end. The post-Gradle dex append needs
  none. Rejected for now.
- **`-Wl,--undefined=<sym>` in `build.rs` instead of `link()`.** It did not
  keep the export in T1. Rejected.

## References

- `specs/044-android-auto/plan.md` (D1, D2, D4, section 2, open question 1)
- ADR-0127 (generated Java entry points)
- ADR-0061 and spec 025 (pure-Rust Android TV app)
- ADR-0077 (Dioxus Native, Proposed)
