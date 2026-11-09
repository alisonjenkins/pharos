# Implementation plan: 3D movies in pharos-ui

**Spec**: [spec.md](spec.md)
**Created**: 2026-11-03T23:45:00Z

## Shape

```text
server                              pharos-ui
------                              ---------
ItemSummary.stereo  ───────────────▶ poster tile badge, "3D" filter chip
ItemQuery.stereo    ◀─────────────── library filter bar
ItemDetail.versions ───────────────▶ version picker beside Play
negotiate(version)  ◀─────────────── Player (chosen version)
                                     │
                                     ├─ flat screen: CSS crop to one eye
                                     │   (stereo::eye_rect, cue overlay)
                                     └─ WebXR: XRMediaBinding quad layer,
                                         else render/ WebGL2 quad per eye
```

`stereo::eye_rect(format, eye) -> Rect` (V292) is the one place that knows
which part of a packed frame belongs to which eye and what aspect the eye has.
Every drawing path reads it.

## Schema (one PR, one `schemaVersion` bump)

Ordinals are fixed when the PR is written, after rebasing over whatever
schema PRs have merged by then (#596 adds `Capabilities.imageSubtitles @5`;
020 PR 2 adds `ItemSummary` artist and album). Shape:

```capnp
enum Stereo3d { none @0; halfSideBySide @1; fullSideBySide @2;
                halfTopAndBottom @3; fullTopAndBottom @4; mvc @5; }
enum StereoFilter { any @0; only3d @1; only2d @2; }

struct ItemSummary { ...; stereo @N :Stereo3d; }       # FR-001
struct Version { id @0 :Text; name @1 :Text; stereo @2 :Stereo3d; primary @3 :Bool; }
struct ItemDetail  { ...; versions @N :List(Version); } # FR-002
struct ItemQuery   { ...; stereo @N :StereoFilter; }    # FR-004
negotiate @0 (..., version :Text)                       # FR-003, appended param
enum EventKind { ...; xrSessionStarted @N; xrSessionFailed @N+1; }  # FR-008
```

`none` and `any` as the zero values are the right meaning for an old peer, so
these enums need no option union. The copy in
`specs/017-native-api/contracts/pharos.capnp` changes with it.

## Pull requests

Each is one PR of atomic commits, reviewed with /review, fixed, fixups
squashed, then handed to the merge queue. SyncPlay PRs merge first when both
are ready.

1. **Native API carries 3D (FR-001 to FR-004, FR-009)**, about 1 day. Done (2026-11-08, schema 29, B770/V720; the ids V291/B351 below were never landed).
   Schema and contract copy; `rpc/library.rs` writes `stereo` and
   `versions`, reads `ItemQuery.stereo` into the store's `is_3d`;
   `GrantRequest` gains `edition: Option<&str>`, resolved like the Jellyfin
   `MediaSourceId` path; unknown id refused `notFound`. Guards: V291 RPC test
   (2D primary with a 3D edition: summary says 3D, detail lists both,
   negotiate plays the edition's file), the filter test, an older-client test
   that ignores the new fields. B351 and V291 land in the baseline here.
2. **Library shows 3D (US1)**, about half a day. Done (2026-11-08, V722; no per-device 3D default: the primary plays until a version is picked). Badge on `PosterTile`,
   "3D" chip in the filter bar, version picker on the detail page with the
   per-device default (Decision 5) and the remembered choice in
   `localStorage`. Guards: Dioxus render tests; a Playwright case over a
   seeded half-SBS fixture.
3. **Flat-screen eye crop (US2, FR-005, FR-010, FR-011, FR-013)**, about 1
   day. Amended 2026-11-09 with the spec's ids. `native/stereo.rs` gains
   `Eye` geometry (V921: the eye's rect in the frame and its picture size,
   unit-tested over all five packings) and `ViewMode` (`OneEye` default,
   `BothEyes`, stored under `pharos.player.stereo`, V922). The watch page sets
   `data-stereo` and `--pharos-eye-aspect` on `.pharos-video-frame`; pharos.css
   lays the `<video>` at the eye's contained box and stretches and clips it
   (spec Decision 6), so the DOM is unchanged. Geometry readers switched to
   the eye: `subs_web.rs` content box, `cue_box_for`, image subtitles, the
   episode turn snapshot (B961). A "3D: one eye / both eyes" row in the
   player's settings menu. The spec amendment and baseline rows (B960, B961,
   V920 to V922) land here. Guards: unit tests; the V920 Playwright case over
   seeded half-SBS and full-SBS fixtures measuring the drawn picture.
4. **WebXR stereo (US3, FR-007, FR-008, V923)**, about 1.5 days. ADR 0088
   (its own commit); a `wasm_bindgen` extern block for `navigator.xr`,
   `XRSession`, `XRMediaBinding` and `XRWebGLBinding` (spec Decision 7);
   "Watch in VR" shown after `isSessionSupported("immersive-vr")`;
   `requestSession` called inside the click handler with nothing awaited
   first (user activation); layers path via `XRMediaBinding.createQuadLayer`
   with the layout from the eye function, fallback quad per eye through
   `render/`; controller select toggles play, squeeze leaves; telemetry
   events. Guards: unit tests of the layout mapping and the fallback's per-eye
   UV rects; a Playwright check that the button is absent without WebXR; a
   manual Quest check, with exact steps, recorded in the PR.
5. **Android TV (US4, FR-012)**, about half a day, after #1002 merges. The
   TV shares the watch page, so the crop should need no TV code; this PR
   checks it on the G08 (dropped frames against the 2D version), adds the
   mode row to the TV's settings if the shared menu does not reach it, and
   records the result in the spec.

## Signals

- Server: `pharos_playback_3d_decision_total{reason}` already counts each 3D
  negotiation; PR 1 adds an `edition` field (`primary` or `alternate`) to the
  native negotiate log line so a 3D play is visible by query:
  `{namespace="pharos"} |= "pharos::rpc::playback" | json | edition="alternate"`.
  As built, `edition` carries the version's id, or `primary`:
  `{namespace="pharos"} |= "native playback negotiated" != "edition=primary"`.
- Client: `xrSessionStarted{format,path}` and `xrSessionFailed{reason}` in
  the 016 telemetry stream.

## Risks

- **WebXR bindings are unstable in web-sys.** Settled 2026-11-09: a small
  `wasm_bindgen` extern block, no cfg flag (spec Decision 7).
- **JASSUB with a full packing**: JASSUB fits its canvas to the element's
  box and the frame's `videoWidth`/`videoHeight`. For half packings that is
  the eye; for full SBS or TAB it is half the eye's height or width. Fixed by
  passing the eye size if JASSUB takes one, else noted as a known gap (ASS in
  a full-packed 3D rip is rare).
- **The browser's own cue box while cropped**: `::cue` is drawn inside the
  stretched `<video>`, so a WebVTT cue would be stretched and off centre. Text
  subtitles go through pharos-subs, which fits to the eye; the native cue path
  is only its fallback (spec 026). If that fallback shows up on a 3D play, the
  FR-006 overlay is the fix.
- **Steam Frame support is unknown** (spec Decision 4); feature detection
  keeps it safe either way.
- **Schema collisions** with #596 and 020: settled by rebasing PR 1 last
  among schema PRs in flight and taking the next free ordinals.
- **hls.js and the crop box**: the `<video>` keeps its parent; only CSS
  changes, which MSE tolerates (as 020 found).
