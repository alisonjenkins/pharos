# Feature specification: 3D movies in pharos-ui

**Feature directory**: `specs/022-ui-3d/`
**Created**: 2026-11-03T23:45:00Z
**Status**: in progress. US1 shipped in #1055 (B770, V720, V722). US2, US3
and US4 remain; the 2026-11-09 amendment below renumbers their ids and adds
Android TV.
**Input**: "We also need support for 3D movies" (Alison, 2026-11-03). The
headsets in use are a Meta Quest and a Steam Frame. "Also we need to implement
3d support still" (Alison, 2026-11-09): 3D files must play in 3D, not only be
labelled.
**Depends on**: 018 (3D detection, `Video3DFormat`, `Is3D`, `StereoOutput`),
3D edition grouping (#450, `AlternateMediaSource.video_3d_format`, `has_3d`),
#479 (a playback picks a grouped edition), 017 (native API), ADR-0039
(`render/` WebGL2 layer), 013 (watch page)
**Ids**: bugs from B960, invariants from V920, ADR from 0088 (amended
2026-11-09; the ids first written here, B350 to B351 and V290 to V292, never
reached the baseline and are retired)

## Problem

The server side of 3D is done (018). The scanner recognises half and full
side-by-side (SBS), half and full top-and-bottom (TAB) and MVC rips, groups a
3D rip as an edition of its 2D title, and the Jellyfin API offers
`Video3DFormat` and `Is3D`. Kodi, Infuse and the VR players can use all of
that today.

pharos-ui can use none of it, because the native API carries no 3D at all:

1. `ItemSummary` and `ItemDetail` have no 3D field, so the library cannot say
   which titles are 3D, and `ItemQuery` cannot filter by it.
2. `ItemDetail` lists no editions, and `Playback.negotiate` takes no edition.
   A title whose 3D rip was grouped under its 2D file can only ever play the
   2D file. The 3D rip cannot be reached from pharos-ui at all.
3. A lone packed 3D file (one with no 2D sibling) plays as its raw frame: two
   squeezed pictures side by side, or one above the other. Nothing in the
   watch page knows the frame is packed. This is B960.
4. There is no way to watch in stereo on a headset. Quest Browser supports
   WebXR, so pharos-ui could show each eye its own half of the frame.

## What already exists

- `pharos_core::Video3DFormat`: `HalfSideBySide`, `FullSideBySide`,
  `HalfTopAndBottom`, `FullTopAndBottom`, `Mvc`. It is set on
  `MediaProbe.video_3d_format` and on each `AlternateMediaSource`.
- `media_items.has_3d` is true when the primary file or any grouped edition is
  3D. The Jellyfin `Is3D` filter reads it.
- `services::playback` picks `StereoOutput::Preserve` unless the client asks
  for mono (`StereoMode=mono`) or a `CodecProfile` rejects 3D. The native
  `negotiate` never asks, so native playback is always `Preserve`: the packed
  frame reaches the browser untouched. Burned-in subtitles are drawn into each
  eye (B291).
- The Jellyfin path already plays a chosen edition by `MediaSourceId`
  (#479). The native path has no way in.
- `pharos_playback_3d_decision_total{reason}` counts the server's stereo
  choice.
- Since #1055 (2026-11-09): `ItemSummary.stereo`, `ItemDetail.versions` (each
  with its `stereo`), `ItemQuery.stereo` and `negotiate(version)` exist; tiles
  carry a 3D badge, grids a 3D filter, and the title page a Version picker
  whose pick is remembered per title on the device (`versions::version_key`).
  The watch page knows the playing version's packing but draws the whole
  frame, so B960 still stands.
- Text subtitles are drawn by pharos-subs on a WebGL canvas over the frame
  (spec 026, `subs_web.rs`), placed by `content_box(frame, videoWidth,
  videoHeight)`. The native `::cue` path is only its fallback.

## User stories

### US1: see which titles are 3D (P1)

A movie or episode that is 3D, or has a 3D edition, shows a small "3D" badge
on its poster tile, in rails and in library grids. The library filter bar has
a "3D" chip that lists only those titles. The detail page names each version:
"2D" and "3D (half side-by-side)", for example.

**Acceptance**:

1. Given a 2D movie with a grouped half-SBS edition, its tile shows "3D" and
   the "3D" filter lists it.
2. Given a 2D-only movie, it shows no badge and the "3D" filter leaves it out.
3. Given an MVC rip, its badge reads "3D" and its version reads "3D (MVC,
   plays in 2D here)" (see US3 and Decisions).

### US2: pick the 3D version and watch it on a flat screen (P1)

On a title with both versions, the detail page offers a version choice next
to Play (shipped in #1055; the primary plays until a version is picked).
Playing a packed 3D version on a flat screen shows the left eye only, at the
eye's own shape (a half-SBS 1920x1080 frame shows as one 16:9 picture, not a
squeezed 8:9 one). A player control, "Both eyes", shows the raw frame for
people with a 3D TV that unpacks SBS or TAB itself. Subtitles stay centred on
the picture in both modes.

**Acceptance**:

1. Given a half-SBS file, when it plays on a desktop browser, then the picture
   is the left half of the frame stretched to 16:9 and direct play is kept
   (no transcode is started for 3D).
2. Given the same file with "Both eyes" on, the whole frame shows.
3. Given a TAB file, the picture is the top half, stretched to the eye's
   aspect.
4. Given a text subtitle on, its cue is centred under the visible picture,
   not under the middle of the packed frame.
5. Given a full-SBS 3840x1080 file, the picture is 16:9, not 32:9.
6. Given a 3D episode, the episode turn (B840) shows one eye on its faces, not
   the packed frame.

### US3: watch in stereo on a headset (P1)

In a browser with WebXR (Quest Browser, and any other browser where
`navigator.xr.isSessionSupported("immersive-vr")` resolves true), a packed 3D
version shows a "Watch in VR" button on the player. Pressing it enters an
immersive session in which each eye sees its own half of the frame on a large
virtual screen in front of the viewer. Play, pause, seek and leaving VR work
from the headset's controllers; leaving VR returns to the normal watch page at
the same position.

**Acceptance** (checked by hand on a Quest; Playwright has no WebXR):

1. Given a half-SBS film playing in Quest Browser, pressing "Watch in VR"
   shows the film in stereo: the left eye sees the left half, the right eye
   the right half.
2. Given a TAB film, the top half goes to the left eye.
3. Pressing the controller's trigger toggles play; the session's end (button
   or headset removal) returns to the watch page, still playing, at the same
   position.
4. Given a browser with no WebXR, there is no "Watch in VR" button.
5. Given an MVC version, there is no "Watch in VR" button (it plays in 2D).

### US4: watch 3D on Android TV (P2)

The TV app (spec 025) plays a packed 3D version as one eye, exactly as the
desktop does, with the same "Both eyes" choice for a 3D TV that unpacks SBS or
TAB itself. The TV's WebView draws the crop; the server is never asked for
2D.

**Acceptance** (on the G08, by hand, after #1002 lands):

1. Given a half-SBS film, it plays on the TV as one 16:9 picture with no
   dropped-frame rise over its 2D version (the player's quality readout).
2. Given "Both eyes" chosen on the TV, the whole frame shows, and the choice
   is kept on that TV only.

## Devices and modes

| Device | Default for a packed version | Other modes |
|---|---|---|
| Desktop browser | One eye | Both eyes |
| Quest Browser | One eye, with "Watch in VR" | Both eyes; VR |
| Steam Frame browser | One eye; "Watch in VR" only if it reports `immersive-vr` | Both eyes; VR if supported |
| Android TV app | One eye | Both eyes |
| Any device, MVC | 2D (base view) | none |

"One eye" and "Both eyes" are one per-device setting, `pharos.player.stereo`.
VR is a session the viewer enters from the player, never a stored default.

## Requirements

- **FR-001**: The native schema carries each item's 3D format on
  `ItemSummary` (the primary's format, or the first 3D edition's when the
  primary is 2D, so the badge follows `has_3d`).
- **FR-002**: `ItemDetail` lists the item's versions: the primary and each
  grouped edition, each with an id, a name and a 3D format.
- **FR-003**: `Playback.negotiate` takes the version to play; empty means the
  primary. The server resolves it with the same code the Jellyfin
  `MediaSourceId` path uses, and refuses an id the item does not have with
  `Error.kind.notFound`.
- **FR-004**: `ItemQuery` takes a tri-state 3D filter (any, 3D only, 2D only)
  that maps to the store's existing `is_3d` filter.
- **FR-005**: On a flat screen the watch page shows one eye of a packed
  frame by default (V290). It never asks the server to convert to 2D for this:
  the crop is drawn in the browser, so direct play survives and switching
  modes needs no new negotiation.
- **FR-006**: Text subtitles are drawn by pharos-ui over the visible picture
  while the frame is cropped (the browser's own cue box would centre on the
  whole packed frame).
- **FR-007**: In a WebXR browser, a packed 3D version can be watched in
  stereo. The button only appears when the browser reports `immersive-vr`
  support and the version is SBS or TAB.
- **FR-008**: The UI reports XR outcomes through the 016 telemetry catalogue:
  `xrSessionStarted` (with the 3D format and the path used, layers or WebGL)
  and `xrSessionFailed` (with the reason).
- **FR-009**: All schema changes are append-only (017 FR-009) and arrive in
  one PR with one `schemaVersion` bump. A client talking to an older server
  sees no 3D fields and behaves as today.
- **FR-010**: Every reader of the picture's geometry goes through the one eye
  function (V921) while a packed version plays: the crop's own box, the
  pharos-subs content box (`subs_web.rs`), `cue_box_for` (`web.rs`), the image
  subtitle box (`ImageSubtitles`), the JASSUB canvas fit and lift, and the
  episode turn's snapshot (`turn_web.rs`). Server burn-in needs nothing: it is
  drawn into each eye already (B291).
- **FR-011**: The viewing mode is the device's own. It is stored only on the
  device, never sent to the server and never carried by SyncPlay or Together
  (V922); members of one group may watch the same version in different modes.
- **FR-012**: The Android TV app uses the same crop as the desktop (US4). It
  never sets `StereoMode=mono` for a 3D version.
- **FR-013**: Picture-in-picture shows the whole frame (the browser draws it
  outside the page's CSS); the player's full screen (the player element)
  keeps the crop. The player never puts the `<video>` itself in full screen.

## Decisions

1. **The flat-screen crop is client-side.** The server could transcode to 2D
   (`StereoOutput::Mono2D`), but that costs a transcode for every 3D play and
   a renegotiation to toggle. A CSS crop of the `<video>` inside a clipping box
   costs nothing and keeps direct play. The server's mono path stays for
   Jellyfin clients.
2. **MVC plays in 2D in pharos-ui.** Browsers decode the H.264 base view and
   ignore the second view, and the server has no MVC-to-packed conversion
   (FFmpeg has no H.264 MVC decoder). MVC titles are badged as 3D and play
   in 2D. A server-side MVC-to-SBS path would be its own spec.
3. **WebXR first via the WebXR Layers API, WebGL second.** Quest Browser
   supports `XRMediaBinding.createQuadLayer(video, { layout })` with
   `stereo-left-right` and `stereo-top-bottom`: the compositor draws the video
   straight into each eye at full quality, with no per-frame copy. Where
   layers are missing, the fallback draws the video as a texture on a quad
   per eye with the `render/` WebGL2 layer (ADR-0039), using an
   `xrCompatible` context. The ADR for WebXR ships with the PR that adds it.
4. **The Steam Frame is not promised.** Whether its browser exposes WebXR is
   not known on 2026-11-03. Feature detection means it gets the VR button if
   and only if its browser supports `immersive-vr`; otherwise it gets the
   flat-screen crop and the server's Jellyfin path for VR player apps.
5. **The default version follows the device.** On a headset browser
   (`immersive-vr` supported) the 3D version is preselected, elsewhere the 2D
   one. The last choice per title is remembered on the device. *As built in
   #1055*: the primary plays until a version is picked; the headset default
   lands with US3.
6. **The crop is a transform, not a bigger element** (2026-11-09). The
   `<video>` is laid out at the eye's contained box (`aspect-ratio` from the
   eye function), drawn with `object-fit: fill`, stretched by `scale: 2 1`
   (SBS) or `scale: 1 2` (TAB) from its start edge, and clipped to its own
   first half by `clip-path`. Its layout box is then exactly the eye's
   picture, so code that fits to the element (JASSUB, offsets) sees the eye,
   and the DOM is unchanged (the video stays the frame's only diffed child).
   `object-view-box` would be simpler but is Chromium-only.
7. **WebXR goes through a small `wasm_bindgen` extern block**, not
   `--cfg=web_sys_unstable_apis`: the flag would rebuild the whole crate graph
   on CI and changes every crate's RUSTFLAGS.
8. **Android TV crops on the client** (recommended to Alison 2026-11-09; the
   other road is a server 2D transcode of every 3D play). If the G08 drops
   frames under the crop, the fallback is `StereoMode=mono` for that device,
   decided with Alison first.

## Invariants (to be added to the baseline)

- **V920** (was V290): A packed 3D frame (SBS or TAB) is never drawn whole on
  a flat screen unless the viewer chose "Both eyes" (or uses picture-in-picture,
  FR-013). Guard: a Playwright case measuring the drawn picture's aspect for a
  half-SBS and a full-SBS fixture.
- **V921** (was V292): The eye rectangle and the eye's aspect for each packing
  are computed in one pure function (`native/stereo.rs`), used by the flat
  crop, every geometry reader in FR-010, the layers path and the WebGL path
  alike. Guard: its unit tests over all five packings.
- **V922**: The 3D viewing mode is per device: it is read and written only
  through the device's settings store, and no request, SyncPlay or Together
  message carries it. Guard: the stereo round-trip tests, the Playwright
  case reading it back from `localStorage`, and the SyncPlay wire tests
  unchanged.
- **V923**: "Watch in VR" shows only when `navigator.xr.isSessionSupported(
  "immersive-vr")` resolved true and the playing version is SBS or TAB.
  Guard: a Playwright case without WebXR finds no button; a unit test of the
  packing gate.
- V291 is covered by V720 (#1055).

## Bugs (to be added to the baseline)

- **B960** (was B350): pharos-ui showed a packed 3D version as its raw frame,
  two squeezed pictures, though since #1055 it knew the version's packing.
  Fixed by FR-005 and FR-010; guarded by V920.
- **B961**: the episode turn snapshots the whole `<video>` frame, so a 3D
  episode's turn shows both eyes side by side. Fixed by FR-010; guarded by
  V921.
- B351 is B770 (#1055).

## Out of scope

- Converting MVC to a packed layout on the server.
- Anaglyph output for red/cyan glasses.
- 180/360 degree and other immersive (non-flat-screen) VR video.
- A 3D TV handshake (HDMI 3D frame packing); "Both eyes" covers TVs that
  unpack SBS or TAB themselves.

## Success criteria

- **SC-001**: Every 3D title in the live library shows a badge, and the "3D"
  filter count matches `/Items?Is3D=true` for the same library.
- **SC-002**: A half-SBS title plays on desktop as one undistorted picture
  with `pharos_playback_decision_total{decision="direct_play"}` unchanged
  against the same title's 2D version.
- **SC-003**: On a Quest, a half-SBS title plays in stereo, and
  `xrSessionStarted` events arrive with `path="layers"`.
