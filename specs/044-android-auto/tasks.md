# 044 tasks — slice 2 (phone app)

Ids T1.. are local to this spec. Sizes: XS / S / M / L / XL. No time
estimates. Plan and reasons: `plan.md`.

Each task is one commit (or a short series of atomic commits). A revert of
one task must leave the workspace compiling.

## Lanes

| Lane | Name |
|---|---|
| A | Crates (pure Rust, host-testable) |
| B | Android runtime (JNI) |
| C | Packaging and CI |
| D | UI (`pharos-ui`) |
| E | Device verification |
| F | Docs |

## Task list

| Id | Lane | Size | Depends on | Task |
|---|---|---|---|---|
| T1 | C | S | none | **Gate.** Build a bare `pharos-phone` stub with dx. List the generated Gradle tree, record the real activity and lib names, check that `Java_` exports from a dependency rlib survive in `libmain.so` (`llvm-nm -D`), and read how dx 0.8 `#[manganis::ffi]` modules work. Write the facts into `plan.md` open question 1. |
| T2 | D | S | none | Move `tv_auth.rs` from `pharos-ui` into `pharos-client`; `pharos-ui` re-exports it so existing paths still work. Tests move with it. |
| T3 | A | M | T2 | Create `crates/pharos-content` from `spikes/.../auto/src/content/**`. Workspace member; `final-excludes` in `.config/hakari.toml`; `just hakari-regen`; tests pass on the host; dependency versions unified with `pharos-client`. Spike keeps building until T32. |
| T4 | A | S | none | Create `crates/pharos-classgen` from `spikes/.../classgen` (+ `jvm-check/`). Add a `--lib` argument (default `main`). Workspace member; hakari exclusion. |
| T5 | A | S | T3 | `pharos-content`: cache API for the UI. Cap get/set, used bytes, free-space reading, downloads list (pinned containers, size, per-track complete). Pure, tested. |
| T6 | A | S | T3 | `pharos-content`: read the UI's `device-token` and `server` files; `Library::reload_credentials()`; confirm file format and 0600 mode are shared (open question 5). Tests with a temp dir. |
| T7 | A | S | T3 | `pharos-content`: `resume` store (item id to position ms; save, load, clear). Rules for what counts as long enough to resume. Tests. |
| T8 | B | L | T3, T4 | Create `crates/pharos-android` from `spikes/.../auto/src/*` (everything but `content`). Split `lib.rs` into modules (service, session, player, browse, search, art, focus, network, voice, compat, debug). Remove the test tones and the "spike" strings. Behaviour unchanged. Keep `unwrap`/`expect` denied. |
| T13 | B | L | T8 | `Player` facade and `runtime()`: idempotent init callable from the service or the app; commands (play queue at index, pause, resume, seek, next, previous, stop); `watch` snapshot of state. Session callbacks switch to it. |
| T14 | B | S | T8 | Car sign-in row says to open Pharos on the phone. The QuickConnect code row moves behind a `spike-quickconnect` feature (off by default). |
| T10 | B | M | T13 | Foreground media notification: channel, `MediaStyle`, actions, `startForeground` type `mediaPlayback`, update on state and metadata, remove on stop. Logs shown, hidden, permission state. |
| T11 | B | S | T13, T4 | Becoming-noisy: generated receiver class (new `ClassSpec`), registered while playing, pauses and logs the verdict. Extend `jvm-check`. |
| T12 | B | S | T7, T13 | Resume positions: save on pause, stop and at an interval; restore on play; clear at the end. Logs item id, saved ms, verdict, reason. |
| T29 | B | S | T10, T11, T12 | ODD tests: assert the decision log lines of T10 to T12 and cache-cap changes; confirm each test fails when its log call is removed. |
| T16 | C | M | T4, T8 | Create `crates/pharos-phone`: bin, `Dioxus.toml` (id `com.alisonjenkins.pharos.phone`), `AndroidManifest.xml` (activity, media service, provider with authority from the application id, search activity, permissions), res overlay reused from `pharos-ui/android/res`, label "Pharos". Android-only deps; host `main` prints a message. Hakari `final-excludes` and `traversal-excludes`. |
| T17 | C | L | T1, T4, T16 | `scripts/android-phone-apk.sh` and its check script: dx build for arm64, classgen, `d8`, add `classes2.dex` after Gradle, `minSdk` 26, `versionCode`, zipalign, apksigner. Check asserts label, service, provider, dex classes and `Java_` symbols. Flake app entry. |
| T18 | C | S | T16 | `just check-phone`: clippy for `pharos-phone` and `pharos-android` on `aarch64-linux-android`; CI job beside the TV type-check; CI runs `jvm-check`. |
| T20 | D | M | T2 | Ports in `pharos-ui`: `NativeAudio` and `OfflineStore` traits, snapshot types, registration slot, host fake. No JNI. Tests with the fake. |
| T22 | C | S | T13, T20, T16 | In `pharos-phone`: implement both ports over `pharos_android` and register them before launch. |
| T21 | D | L | T20 | Music uses `NativeAudio` when registered: `listen.rs` and the stage stop rendering `<audio>`; position and state come from the snapshot; scrub, next, previous, queue; hand-over (ADR-0052/0053) kept; server progress reports unchanged. Tests with the fake. |
| T23 | D | S | T6, T14, T22 | Phone sign-in or sign-out calls `credentials_changed()`; car lists refresh; verified signed in on the phone means no sign-in step in the car. |
| T24 | D | M | T5, T20 | Settings, "Offline music": cap choice (default 4 GiB), used bytes, free-space guard value, clear cache. Stored as a pref, applied through `OfflineStore`. |
| T25 | D | M | T5, T20 | "Keep for offline" toggle on album and mix pages with a state chip; uses `pin`/`unpin`. Phone only. |
| T26 | D | M | T5, T20 | Downloads view: route, pinned containers, size, per-track state, remove, offline banner. |
| T27 | D | S | T16 | Phone pass on the narrow layout inside the app WebView: safe areas, back button, now-playing bar; ask for notification permission on first play. CSS and one JNI call. |
| T28 | E | M | T17, T21, T10, T11, T12 | Device checklist on a phone and the Desktop Head Unit: locked-screen play, notification actions, unplug headphones, focus loss, resume, offline in airplane mode, pin then browse offline, car reuses the phone sign-in. Capture the logcat query output per row. |
| T30 | F | S | none | New ADR, id pending (one APK, dx Gradle project plus injected dex, one `.so`, in-process bridge). Own commit, row in `docs/adr/README.md`. Proposed until T1 closes, then Accepted. |
| T31 | F | XS | T28 | Update `spec.md` slices 2 and 3; append bugs and invariants found; set status. |
| T32 | F | XS | T17, T3, T4, T8 | Delete `spikes/044-android-auto/` (history keeps it). Point `spike-0.md` at the new crates. |

## Waves (what can start when)

| Wave | Tasks |
|---|---|
| 0 | T1, T2, T4, T30 (T20 right after T2) |
| 1 | T3, then T5, T6, T7 |
| 2 | T8, then T13 |
| 3 | T10, T11, T12, T14, T16, T17, T18, T22, T29 |
| 4 | T21, T23, T24, T25, T26, T27 |
| 5 | T28, T31, T32 |

## Lane summary

| Lane | Tasks in order | Sizes |
|---|---|---|
| A Crates | T3, T4, T5, T6, T7 | M, S, S, S, S |
| B Android runtime | T8, T13, T14, T10, T11, T12, T29 | L, L, S, M, S, S, S |
| C Packaging | T1, T16, T17, T18, T22 | S, M, L, S, S |
| D UI | T2, T20, T21, T23, T24, T25, T26, T27 | S, M, L, S, M, M, M, S |
| E Verification | T28 | M |
| F Docs | T30, T31, T32 | S, XS, XS |

## File ownership (why the lanes do not collide)

- A: `crates/pharos-content`, `crates/pharos-classgen`, `.config/hakari.toml`,
  `Cargo.toml` members (T3 and T4 add them; T16 adds `pharos-android`'s and
  `pharos-phone`'s lines in the same two files, so land T3/T4 first).
- B: `crates/pharos-android` only. New generated classes are added in
  `crates/pharos-classgen` by T11; coordinate with lane A (T4 lands first).
- C: `crates/pharos-phone`, `scripts/android-phone-*`, `justfile`, CI,
  one `flake.nix` entry.
- D: `crates/pharos-ui`, plus `crates/pharos-client` in T2.
  `native/mod.rs` is touched by T2 and T20; they are serial in lane D.
- F: docs and spec files only.

## Done when

- One signed APK installs on a phone, opens the Pharos UI, signs in on the
  phone, plays music through the native player with the screen locked, shows
  a media notification, and appears in the Desktop Head Unit already signed
  in, with artwork.
- Offline: cap setting, keep-for-offline and downloads view work in airplane
  mode (T28 table filled).
- `just test-changed` and `just hakari-check` pass; `spikes/044-android-auto`
  is gone.
