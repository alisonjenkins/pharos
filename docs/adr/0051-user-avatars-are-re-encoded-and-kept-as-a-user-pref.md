# ADR-0051: User avatars are re-encoded by the `image` crate and kept as a user pref

- **Status:** Accepted
- **Date:** 2026-11-04T00:00:00Z
- **Deciders:** Ali

## Context

Users want a profile picture on the web UI's account button (B422, V362).
Jellyfin clients show one too, read from `/UserImage` and
`UserDto.PrimaryImageTag`, so a picture set in either place should show in
both. An upload is untrusted bytes from any signed-in user, and the picture
is then served anonymously to anyone who knows the user id (an `<img>` sends
no credential, as in Jellyfin).

## Decision

- **Decode and re-encode every upload.** pharos-server depends on the `image`
  crate (0.25, default features off; PNG, JPEG, WebP and GIF decoders only).
  An upload is capped at 10 MiB before decoding, its header dimensions at
  8192 and its decode allocation at 256 MiB, at most two decode at once,
  off the async workers. The result is cropped to a centred square of at
  most 512 px and written fresh: PNG when it has transparency, else JPEG.
  Nothing of the upload's bytes (metadata, trailing data, a polyglot) is
  stored, and the served type is always the one pharos wrote.
- **Stored as a user pref.** The re-encoded bytes live under the `avatar`
  key of `UserPrefStore`, which both backends already have; the native
  `Prefs` API does not expose that key. The tag is the xxh3 of the bytes.
- **One store, two surfaces.** `/UserImage` and `/Users/{id}/Images/Primary`
  (Jellyfin) and `Session.setAvatar`/`removeAvatar` (native) call the same
  `crate::avatar` functions.

## Consequences

- A new decoder dependency is attack surface; the caps above bound what a
  hostile file can cost, and the crate is pure Rust.
- Pictures sit in the database rather than on disk, so they go with a
  backup and need no new path; a 512 px JPEG is tens of KiB.
- The tag is computed by reading the picture, so a user listing reads every
  user's picture. Fine for a household; storing the tag beside the bytes is
  the fix if listings get large.
- The native call is capped at 768 KiB by the 1 MiB WebSocket frame, so the
  web UI scales a larger picture down in the browser before sending it.
