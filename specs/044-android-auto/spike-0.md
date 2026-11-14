# 044 slice 0 spike — results (2026-11-14)

**Verdict: go.** A phone app with no Java or Kotlin source works in Android
Auto. Code: `spikes/044-android-auto/` on branch
`claude/project-thread-7hfc6m`. Decision: ADR-0127.

## What was built

- `classgen/`: writes `PharosMediaService` (extends
  `android.service.media.MediaBrowserService`), `PharosSessionCallback`
  (extends `MediaSession.Callback`) and later `PharosSearchActivity` as class
  files from Rust. 832 + 419 bytes; no dependencies.
- `auto/`: the `pharos_auto` cdylib. JNI functions build the browse tree
  (Music: two test tones; Audiobooks: one test chapter), play a generated
  10 s WAV through `MediaPlayer`, publish `PlaybackState` and metadata, and
  log every call with its inputs and verdict (tag `pharos-auto`).
- `jvm-check/run.sh`: loads the generated classes and the Rust library on a
  desktop JVM with `-Xverify:all` against stand-in `android.*` classes and
  calls every entry point. Controls: without the library it fails with
  `UnsatisfiedLinkError`; with `max_stack` patched to 0 it fails with
  `VerifyError: Operand stack overflow`.
- `build-apk.sh`: no Gradle. `d8`, `aapt2`, `zipalign` and `apksigner`
  (build-tools 35.0.0, android-34) accepted everything; APK 299,565 bytes.

## Device run

Pixel 9 Pro, Android 16, Android Auto 18.0; Desktop Head Unit 2.0 on
ali-desktop over wireless adb.

| Check | Result |
|---|---|
| App listed in the Android Auto launcher | yes ("Pharos (spike)", default icon: none shipped) |
| `onCreate` → session + token | `08:27:07.176 onCreate: session active, token set` |
| Root / children | `onGetRoot package=com.google.android.projection.gearhead`; root 2, music 2, audiobooks 1 |
| Play from list | tone A, tone C, chapter all played (Alison confirmed by ear) |
| Transport | pause, play, next, previous, stop all reached Rust |
| Seek | advertised (`actions=3895`); not exercised, DHU console has no drag |
| Assistant binding | `onGetRoot package=com.google.android.googlequicksearchbox` browsed the tree unprompted |
| Crashes | none in the app; no `VerifyError`, `UnsatisfiedLinkError`, `ClassNotFoundException` |
| Search button | absent until the root extras carried `android.media.browse.SEARCH_SUPPORTED=true`; then shown |
| Phone-side voice activity | `am start -a MEDIA_PLAY_FROM_SEARCH -e query "test tone C"` → `verdict=Some("tone:523")`, played |

## Voice

Gemini turned "play test tone C on Pharos spike" into "Play Test Tone C on
Pharaoh's Spike", matched no app, and sent it to YouTube Music; the app got
no call. Relabelled "Lighthouse", the request still went to YouTube Music.
After adding a `MEDIA_PLAY_FROM_SEARCH` activity the app is listed beside
the other music apps for that action; the spoken retry is pending.

## Gotchas found

- Two adb versions on one desktop (37.0.1 in the devShell, 35.0.2 used by the
  TV logcat session) restart each other's server, dropping wireless
  connections and port forwards. Use one adb.
- Android Auto's head unit server wedges after a dropped session
  (`IllegalStateException: Already connected`); stop and start it from
  Android Auto's ⋮ menu.
- The first connection needs the car set up on the phone (Android Auto logs
  `Triggering FRX: car connection is not allowed` until accepted).
- `build-apk.sh` deletes `target/spike-044/`; keep test captures elsewhere.
