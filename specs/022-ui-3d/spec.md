# Feature specification: 3D movies in pharos-ui

**Feature directory**: `specs/022-ui-3d/`
**Created**: 2026-11-03T23:45:00Z
**Status**: draft
**Input**: "We also need support for 3D movies" (Alison, 2026-11-03). The
headsets in use are a Meta Quest and a Steam Frame.
**Depends on**: 018 (3D detection, `Video3DFormat`, `Is3D`, `StereoOutput`),
3D edition grouping (#450, `AlternateMediaSource.video_3d_format`, `has_3d`),
#479 (a playback picks a grouped edition), 017 (native API), ADR-0039
(`render/` WebGL2 layer), 013 (watch page)
**Ids**: bugs from B350, invariants from V290 (lower ids are allocated to
work in flight)

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
   watch page knows the frame is packed. This is B350.
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
to Play, defaulting to 2D on a flat screen and to 3D in a headset browser.
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
   one. The last choice per title is remembered on the device.

## Invariants (to be added to the baseline)

- **V290**: A packed 3D frame (SBS or TAB) is never drawn whole on a flat
  screen unless the viewer turned "Both eyes" on. Guard: a Playwright case
  measuring the drawn picture's aspect for a half-SBS fixture.
- **V291**: A title badged 3D can play its 3D version from pharos-ui: the
  version it names is in `ItemDetail.versions` and `negotiate` accepts its
  id. Guard: an RPC integration test over a 2D primary with a 3D edition.
- **V292**: The eye rectangle for each `Video3DFormat` is computed in one pure
  function, used by the flat crop, the layers path and the WebGL path alike.
  Guard: its unit tests over all five formats.

## Bugs (to be added to the baseline)

- **B350**: pharos-ui showed a packed 3D file as its raw frame, two squeezed
  pictures, because the native API carried no 3D format. Fixed by FR-001 and
  FR-005; guarded by V290.
- **B351**: a 3D rip grouped as an edition of its 2D title could not be played
  from pharos-ui, because the native API had no versions. Fixed by FR-002 and
  FR-003; guarded by V291.

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
