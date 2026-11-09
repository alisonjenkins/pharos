# ADR-0093: Baked-in black bars are found once per file and cropped by the player

- **Status:** Accepted (Alison, 2026-11-09)
- **Date:** 2026-11-09T21:40:00Z
- **Deciders:** Alison Jenkins

## Context

Some files carry black bars inside the picture itself. Every Bewitched episode
in the library is a 4:3 picture stored pillarboxed in a 1920x1080 frame
(B1017). The file says nothing about it: square pixels, a 16:9 display aspect,
no side data. A player that fits the frame to its window shows the baked-in
bars plus its own, and the picture is about a quarter smaller than it could be.
Video must dominate the player (project rule).

Where the picture sits can only be found by decoding frames. The server
already runs per-file analyses in background sweeps on the libav worker pool
behind the shared background-I/O gate (intro detection, integrity, song tempo,
ADR-0050). RAM and CPU on the server are scarce, and work during playback must
stand down.

## Decision

We will find the picture's rectangle once per file on the server and let the
player crop to it.

- A background sweep, shaped like the song analysis sweep, checks films and
  episodes (not 3D files) one at a time on the libav pool behind a `bg_io`
  permit. It samples 7 frames by keyframe seek at 10% to 70% of the runtime,
  takes a row or column as picture when its mean luma is above 24, and keeps
  the union over the usable frames (V976). The result is stored on the media
  row against the file signature and `PICTURE_CHECK_VERSION`, so it is redone
  only for a changed file or a new detector.
- The native API's `Version` carries it as `pictureX/Y/Width/Height` in coded
  pixels (schemaVersion 34), width 0 when there are no bars or the file is not
  checked yet.
- The web player crops with CSS, the way it already crops a 3D frame to one
  eye (V921): the video takes the picture's shape and the frame is stretched
  and clipped to it (V977). The file and the stream are untouched, so direct
  play, the transcode and its cache are unchanged.

## Consequences

- A pillarboxed or windowboxed file fills the player as a clean file would, at
  no cost during playback.
- The library is decoded a little once: 7 single-frame seeks per video file,
  at background priority.
- Jellyfin clients see no change: they get the whole frame as before.
- ASS subtitles drawn by JASSUB fit the whole frame's shape inside the cropped
  picture, so they draw a little smaller on a cropped file. Text and image
  subtitles fit the picture (V977, V991).
- A file whose dark opening frames all fall on the samples gets no area, and
  the player shows it uncropped, as before.

## Alternatives considered

- **Detect in the browser while playing**: no server work, but every viewer's
  device samples frames during playback, dark scenes make it unreliable, and a
  crop that changes mid-play moves the picture. Weak TVs pay for it.
- **Crop in the transcode**: helps Jellyfin clients too, but forces a
  transcode or a different cache key for direct-playable files, and a 1080p
  Remux would lose quality to fix a layout problem.
- **Re-encode the files cropped**: lossy for a Remux and outside pharos.

## References

- B1017, V976, V977 and V991 in `specs/001-pharos-baseline/`.
- ADR-0050 (song analysis sweep), spec 022 / V921 (3D eye crop).
