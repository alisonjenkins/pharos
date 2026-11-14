# 044 slice 2 plan — the phone app

- **Status:** draft, for Alison's review.
- **Date:** 2026-11-14
- **Scope:** slice 2 of `spec.md` ("Product app") plus the offline UI half of slice 3.
- **Tasks:** `tasks.md` (ids T1.. are local to this spec).
- **Sizes:** XS / S / M / L / XL only. No time estimates anywhere.
- **Not verified:** nothing here was built or run. `dx` is not installed in
  the planning environment, so claims about dx's generated project come from
  repo evidence (cited) and are re-checked by T1.

## Problem

Today the Android Auto service is a spike. It has test tones, a QuickConnect
code on the car screen, an app label of "Lighthouse", and no phone UI.
The owner wants one real app, for the phone and the car, in pure Rust.

## Decisions

| # | Decision | Why (short) |
|---|---|---|
| D1 | Keep `dx` as the packager. Add our generated classes to its APK after Gradle. | dx already makes the WebView activity; replacing it is XL. |
| D2 | One APK, one process, one `.so` (`libmain.so`). | Library, token, cache and player are Rust statics. Two `.so` files do not share them. |
| D3 | New app crate `pharos-phone` (own `Dioxus.toml`, manifest, id). | `Dioxus.toml` is per package; the TV app owns `pharos-ui`'s. |
| D4 | The UI calls the player by plain Rust calls through a port trait in `pharos-ui`. | Dioxus mobile runs the app code natively; only the DOM is in the WebView. |
| D5 | The spike moves into three workspace crates, kept off `workspace-hack`. | See "Crate layout". |
| D6 | Phone sign-in writes the files the car already reads. QuickConnect on the car stays behind a debug feature. | Spec US4. |

D1, D2 and D4 are load-bearing and hard to undo. They get **ADR-0131**
(task T30), Proposed until T1 confirms the open facts below.

## 1. How dx's Android build works today

Evidence, all from the repo:

- `scripts/android-apk.sh` runs `dx build --platform android --package
  pharos-ui --bin pharos-tv --features mobile`. It then works inside
  `target/dx/pharos-tv/<profile>/android/app/app/src/main` and runs
  `./gradlew assembleDebug`. So dx **does** generate a Gradle project and
  Gradle builds the APK.
- The script seds `app/build.gradle.kts` (`versionCode`) and the generated
  manifest (`android:banner`), and copies `crates/pharos-ui/android/res`
  over the generated `res/`. dx 0.7 has no setting for icon or app name
  (`crates/pharos-ui/android/res/README.md`).
- `crates/pharos-ui/android/AndroidManifest.xml` names the launcher activity
  `dev.dioxus.main.MainActivity`, with `android.app.lib_name = main`. That is
  dx's class, not ours.
- `specs/032-blitz-tv/spec.md` (Packaging row): "`dx` 0.7.10 and 0.8.0-alpha.3
  always generate a `WryActivity` (WebView) for Android".
- `specs/025-android-tv/spike-0.md`: dx "generates and runs the Gradle
  project (AGP 8.7, compileSdk / targetSdk 34, minSdk 24); no Kotlin/Java
  written by hand".

**Reading:** dx's template contains Java/Kotlin glue (the activity and the
wry WebView classes). It is **generated build output**, not repo source.
The pure-Rust rule was always worded that way:

- ADR-0061: "No Kotlin or Java is hand-written"; spec 025: "the Android glue
  `dx` generates is treated as build output".
- ADR-0127 uses the same line for our stubs: "build output, like the
  launcher `dx` generates for the TV app".

So D1 does not break the rule. It follows the rule the TV app already lives
under. Our own classes are generated from Rust (ADR-0127) and sit beside dx's.

Caveat: I could not open dx's template here. T1 lists the generated tree and
records the real class and lib names.

## 2. Packaging choice

| Option | What it is | Verdict |
|---|---|---|
| (a) Inject into dx's Gradle project | dx builds `libmain.so` and the WebView activity. We add our service, provider and receiver classes as an extra dex. | **Recommended.** |
| (b1) Drop dx; generated WebView activity | We write the WebView host in Rust over JNI and replace wry. | XL. wry's Android half is Kotlin compiled by Gradle; we would re-implement it and the Dioxus IPC. Rejected. |
| (b2) Drop dx; `NativeActivity` + Dioxus Native (Blitz) | Fully no-Gradle, one `.so`. | Not for the phone. ADR-0077 is still Proposed; Blitz is experimental and has no web-phone-layout parity. Revisit if the TV line adopts it (spec 032). |
| (c) Two `.so` files | Our `libpharos_auto.so` beside `libmain.so`. | Rejected (see below). |

### How (a) works

1. `dx build --platform android --package pharos-phone --features mobile
   --target aarch64-linux-android` makes the Gradle project and `libmain.so`.
2. `pharos-classgen` writes our `.class` files (service, session callback,
   player listener, art provider, search activity, receivers).
   `System.loadLibrary` is given `"main"`, not `"pharos_auto"`.
3. `d8` turns them into `classes2.dex`.
4. The script runs `gradlew assembleDebug` as today, then **adds
   `classes2.dex` to the finished APK** (the spike's step 5 already does
   this with a zip append), then `zipalign` and `apksigner`.

Why after Gradle and not a jar in `libs/`: no edit to `build.gradle.kts`, no
risk of R8 stripping classes only the manifest names, and the same signing
step the TV script already has. A `classesN.dex` beside `classes.dex` is
loaded by ART as is.

Other script edits, all of the kind the TV script already makes:
`minSdk` 24 to 26 in `build.gradle.kts` (audio focus request needs API 26),
`versionCode`, the res overlay, and a check script like
`scripts/android-apk-check.sh`.

### One `.so`: how the JNI exports get in

- `pharos-android` is an **rlib**. It holds the `#[no_mangle] extern
  "system" fn Java_com_alisonjenkins_pharos_auto_*` functions.
- `pharos-phone`'s bin links it. dx builds that bin into `libmain.so`.
- The generated classes load `main`, so their `native` methods resolve in the
  same library as the Dioxus app.
- **Risk:** a cdylib may drop `#[no_mangle]` symbols that come from a
  dependency nothing references. Fix if it happens: a `pharos_android::link()`
  call from `main` that references every export, plus a `-Wl,--undefined=...`
  link arg from a `build.rs` in `pharos-phone`. The check script runs
  `llvm-nm -D` on `libmain.so` and fails the build if a `Java_` symbol is
  missing. T1 finds out which fix is needed.
- No `JNI_OnLoad` is defined in our code (the spike has none; it takes the
  `JavaVM` from `env` in `onCreate`). dx/wry may define one. Keep it so.

### Why not two `.so` files

Each `.so` has its own copy of every Rust static: the `Library` singleton,
the token cache, the audio cache index, the player state. Sharing them
means passing raw pointers through Java. It also doubles tokio, rustls and
`ring` in the APK. One `.so` has none of these problems.

### Cold start

Android Auto can start the service with no activity running. Then `libmain.so`
is loaded by the service class, and Dioxus `main` never runs. So all shared
state lives behind `pharos_android::runtime()`, an idempotent init that either
entry point calls first (service `onCreate` or app start). The `Library` has
its own thread (`content/library.rs`, thread `pharos-content`), so it does not
need the Dioxus runtime.

### Risks of (a)

| Risk | Mitigation |
|---|---|
| dx 0.8 is an alpha (ADR-0078); template can change per pre-release | Check script asserts the manifest and the dex. Re-run T28 on each bump. |
| Dropped JNI exports (above) | `llvm-nm` check; link-arg fallback. |
| Provider authority clash with the installed spike APK | Authority built from the phone's application id; uninstall the spike. |
| `POST_NOTIFICATIONS` (API 33+) needs a runtime ask from the activity | Ask from the phone UI on first play (T10, T27). |

## 3. Bridge between the Dioxus UI and the native player

Facts:

- `dioxus::mobile` runs the app's Rust natively. Only the DOM is in the
  system WebView. Existing code already calls JNI from the UI process
  (`native/android_page.rs` builds a `JavaVM` from `ndk_context`).
- Music plays today in an `<audio>` element (`native/listen.rs`,
  `views/player.rs`), driven by `platform::{play,pause,seek_ms,position_ms}`.
  WebView audio stops with the screen and is invisible to the car.

Design:

```text
pharos-ui (platform-neutral)         pharos-android (JNI, no UI)
  trait NativeAudio  <-- impl in -->  Player facade -> MediaPlayer + MediaSession
  trait OfflineStore <-- pharos-phone  Library + Cache (pharos-content)
        ^                                   ^
        '--------- same process, plain Rust calls ---------'
```

- `pharos-ui` defines the two **port traits** and a registration slot
  (`OnceLock`). It never depends on `pharos-android`. (Boundaries rule from
  spec 009: each external dependency sits behind an adapter.)
- `pharos-phone` implements both traits over `pharos_android` and registers
  them before `launch`.
- **Commands** (play queue at index, pause, resume, seek, next, previous,
  stop) are direct calls. They hop to a worker thread that attaches to the
  JVM, as the spike's `on_worker` does. The UI thread never blocks on JNI.
- **State** (playing/paused/buffering, position, duration, current item id,
  queue) flows back as a snapshot on a `tokio::sync::watch`. The UI reads it
  in a `use_future` loop into a signal. No JS, no `document::eval`.
- **One engine, two clients.** The car's session callbacks and the phone UI
  call the same `Player` facade, so lock screen, notification, car and UI
  always agree.
- **Playback reporting stays in the UI.** The server still hears progress and
  stop through the existing client code, fed from the snapshot.
- **Sign-in:** the UI's sign-in already writes `device-token` and `server` in
  the app files dir (`native/android.rs`). The spike reads `device_token` and
  `server_url`. T6 makes the library read the UI's files and adds
  `credentials_changed()`, called after sign-in or sign-out. The car then
  needs no sign-in step.
- **Offline:** `OfflineStore` exposes cap get/set, used bytes, free-space
  guard, pin/unpin by container, and the downloads list. Backed by
  `Library::pin`, `unpin` and `content::cache`.

Unknown to settle in T21: the "stage" and hand-over logic (ADR-0052,
ADR-0053) assumes a DOM media element. The native engine must satisfy the
same hand-over contract. It is the largest UI-side task (L).

## 4. Crate layout (moving the spike out of `spikes/`)

| New crate | From | Holds | Depends on |
|---|---|---|---|
| `crates/pharos-classgen` | `spikes/044-android-auto/classgen` | Class-file writer, class list, `pharos-classgen` bin (takes `--lib main`), `jvm-check/` | nothing |
| `crates/pharos-content` | `auto/src/content/**` | `Library`, tree, cache, prefetch, http, fuzzy, token, resume (new). No JNI, no UI. Host-testable. | `pharos-client`, `pharos-api` |
| `crates/pharos-android` | the rest of `auto/src/*` | JNI: service, session, player facade, notification, noisy receiver, focus, art provider, search, compat, voice, network, debug. rlib. | `pharos-content`, `jni` |
| `crates/pharos-phone` | new | Bin `pharos-phone`, `Dioxus.toml`, `AndroidManifest.xml`, res overlay, port impls | `pharos-ui` (+`mobile`), `pharos-android`, all Android-target only |

Also:

- `tv_auth.rs` is `#[path]`-included from `pharos-ui` by the spike. It is
  std-only. T2 moves it into `pharos-client` and `pharos-ui` re-exports it,
  so `pharos-content` does not reach into another crate's source tree.
- Names: files keep the `Java_com_alisonjenkins_pharos_auto_*` JNI names, so
  the generated classes do not change. Application id is
  `com.alisonjenkins.pharos.phone`. The TV app keeps `...pharos.tv`.

### hakari and workspace-hack (CLAUDE.md)

- Add all four to `[workspace] members`.
- Add `pharos-classgen`, `pharos-content`, `pharos-android`, `pharos-phone`
  to `[final-excludes]` in `.config/hakari.toml`. Reason: `workspace-hack`
  pulls sqlx and the server stack; an Android crate must not depend on it.
  Same reasoning the file gives for `pharos-client` and `pharos-ui`.
- Add `pharos-phone` to `[traversal-excludes]` too: it turns on
  `dioxus/mobile`, which pulls wry and GTK on Linux, and hakari would unify
  that into host builds (the file's existing comment about `pharos-ui`).
- `pharos-phone`: put `pharos-ui` (with `mobile`) and `pharos-android` under
  `[target.'cfg(target_os = "android")'.dependencies]`, and keep a host
  `main` that prints "Android only", as `bin/tv.rs` does. Then
  `cargo nextest run --workspace` on the host never builds wry.
- Run `just hakari-regen` after each `Cargo.toml` change; CI's
  `just hakari-check` fails on a stale hack crate.
- `Cargo.lock` merges the spike's two lock files. Nix builds from
  `Cargo.lock`, so no separate Nix regen.
- Spike deps that are new to the workspace lock (for example `httparse`,
  `webpki-roots` 0.26, `tokio-rustls`) are checked by T3 against what
  `pharos-client` already uses, to avoid duplicate versions.
- Lints: the workspace denies `unwrap_used` and `expect_used`. The spike's
  `content` already does; T8 must keep it for the JNI modules.

## 5. Features that are not in the spike yet

| Feature | Design | Task |
|---|---|---|
| Foreground media notification | `Notification.MediaStyle` on the session token; channel; `startForeground` with type `mediaPlayback`; actions previous / play-pause / next | T10 |
| Becoming noisy | Generated `BroadcastReceiver` subclass; registered while playing; pause + log verdict. The spike already generates a receiver class for debug, so the shape exists | T11 |
| Audio focus | Done in the spike (`focus.rs`); move as is | T8 |
| Resume positions | `content::resume`: save on pause, stop, every N seconds; restore on play; clear at end. Music only after a length threshold; audiobooks later (server has none) | T7, T12 |
| Cache cap setting | Pref `offline-cap`, default 4 GiB, applied by `OfflineStore`; the 2 GiB free-space guard is another lane's work and the setting shows its value | T24 |
| Keep for offline | Toggle on album and mix pages calls `Library::pin` / `unpin` | T25 |
| Downloads view | New route listing pinned containers, size, per-track state, remove | T26 |
| Label | `Pharos` in manifest and res; the voice-test label stays out of the product | T16 |

### Voice caveat

Spec "Voice findings": Gemini "play X on Pharos" misheard and went to
another app. The label returning to "Pharos" brings that back. Not a
regression of this slice; the AppFunctions route stays the real fix. Noted
in the open questions.

## Observability (ODD)

Android has no Prometheus. The signal is logcat, tag `pharos-auto`, plus the
`pharos-content` lines routed through the same sink.

- Name the query before each fix: `adb logcat -s pharos-auto | grep <verdict>`.
- Every new decision logs inputs, verdict and the offending value: noisy
  pause (route, was-playing), resume (item id, saved ms, restored or
  dropped and why), cache cap change (old, new, free bytes), notification
  (shown, channel, permission state).
- Failure paths log as richly as success paths (symmetry).
- T29 asserts these lines in tests and checks the test fails when the log
  call is removed.
- Never log tokens, URLs with `api_key`, or media titles in test fixtures.

## Parallel lanes

Disjoint file sets, so lanes do not conflict. The only shared file is
`Cargo.toml` (workspace members), touched once, by T3.

| Lane | Name | Files it owns |
|---|---|---|
| A | Crates | `crates/pharos-content/**`, `crates/pharos-classgen/**`, `Cargo.toml` members, `.config/hakari.toml`, `workspace-hack/` |
| B | Android runtime | `crates/pharos-android/**` |
| C | Packaging | `crates/pharos-phone/**`, `scripts/android-phone-*`, `flake.nix` (one app entry), `justfile`, `.github/workflows/ci.yml` |
| D | UI | `crates/pharos-ui/**`, `crates/pharos-client/**` (T2 only) |
| E | Device verification | no source; a checklist and captured logcat |
| F | Docs | `docs/adr/**`, `specs/044-android-auto/**`, `specs/001-pharos-baseline/{bugs,invariants}.md` (append) |

Order: Wave 0 (T1, T2, T4, T20, T30), Wave 1 (T3, then T5 to T7), Wave 2
(T8, then T13), Wave 3 (T10 to T12, T14, T16, T17, T18, T22), Wave 4 (T21,
T23 to T27), Wave 5 (T28, T31, T32). Details and dependencies in `tasks.md`.

## Open questions

1. **dx's generated tree (T1).** Answered 2026-11-14 from a bare stub
   (`pharos-phone` bin plus a `jnidep` rlib), debug, `aarch64-linux-android`.
   - Versions: `dx` 0.8.0-alpha.3 (the shell's; `scripts/android-apk.sh`
     comments still say 0.7), NDK 29.0.14206865, build-tools 35.0.0. The
     generated Gradle has compileSdk 34, minSdk 24, targetSdk 34.
   - Tree: `target/dx/pharos-phone/debug/android/app/`. The glue is Kotlin
     only, in `app/src/main/kotlin/dev/dioxus/main/` (`MainActivity`,
     `WryActivity`, `Rust`, `Ipc`, `Logger`, `PermissionHelper`, and wry's
     WebView classes). `app/src/main/java/` exists and is empty.
   - Names: launcher `dev.dioxus.main.MainActivity` (extends `WryActivity`).
     `android.app.lib_name` is `main`. One library,
     `app/src/main/jniLibs/arm64-v8a/libmain.so`, loaded by `Rust.kt` with
     `System.loadLibrary("main")`. The application id comes from the cargo
     package name (`com.example.PharosPhone` for the stub), so T-tasks must
     set it explicitly.
   - No `JNI_OnLoad` is exported; wry's natives are
     `Java_dev_dioxus_main_*`. Our `Java_com_alisonjenkins_*` names do not
     clash.
   - **Dependency `Java_` exports are dropped.** An unreferenced
     `#[no_mangle] extern "system"` fn in an rlib is absent from
     `llvm-nm -D libmain.so`. A `link()` fn called from `main` that takes
     each export's address through `std::hint::black_box` keeps it
     (`T Java_com_example_t1_Probe_ping`). The `build.rs`
     `-Wl,--undefined=` arg alone did **not** keep it (cause not found).
     So `pharos_android::link()` is required, not a fallback, and the
     check script's `llvm-nm` assertion stays. Not verified: release
     profile (LTO, strip) and armv7.
   - `llvm-nm` is not on the dev shell's PATH; use
     `$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/*/bin/llvm-nm`.
   - `#[manganis::ffi("<dir>")] extern "Kotlin" { ... }` generates
     Rust-to-Kotlin JNI calls. The path must be a Gradle module directory
     (copied to `plugins/<name>`) or a prebuilt `.aar` (copied to
     `app/libs/`); a bare `.jar` is rejected
     (`dioxus-cli-0.8.0-alpha.3/src/build/android.rs`,
     `install_android_artifacts`). Its metadata rides in the binary's
     assets symbol, so the declaring crate must be linked. Our generated
     `.class` files would need wrapping in an `.aar` to use it; the
     post-Gradle `classes2.dex` append needs no wrapper, so it stays the
     plan. Not exercised end to end.
2. **Does `ndk_context` give the Activity or the Application?** It decides
   how the UI asks for `POST_NOTIFICATIONS`.
3. **Cache cap default.** 4 GiB is still an assumption from the spec, not
   confirmed by Alison.
4. **Spoken name.** Label "Pharos" is requested. Voice routing to the app is
   unsolved either way (spec, Open questions).
5. **Credential file format.** T6 must confirm `device-token` (UI) and the
   library's token file are byte-compatible, including the 0600 mode the
   library enforces (`content/token.rs`).
6. **Two Android apps.** Phone and TV are separate ids. Is the phone app
   also meant to run on the TV? Assumed no.
7. **Spike APK.** Uninstall it before testing; its provider authority clashes
   until T16 derives the authority from the application id.
