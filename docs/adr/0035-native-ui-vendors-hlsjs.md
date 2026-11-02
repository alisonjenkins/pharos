# ADR-0035: The native web UI plays HLS through a vendored, pinned hls.js

- **Status:** Accepted
- **Date:** 2026-11-02T09:19:00Z
- **Deciders:** Alison Jenkins

## Context

The native watch page plays what `Playback.negotiate` returns: a direct file,
or an HLS master playlist for a remux or a transcode. A `<video>` element
plays HLS itself only in Safari. Firefox and Chromium need a Media Source
Extensions player, and without one every HLS decision fails on the desktop.

The UI's Content-Security-Policy allows script from `'self'` only (ADR-0033,
`ui::content_security_policy`), so a player cannot load from a CDN. The
watch page must not need the player when the browser plays the stream
itself, and spec 009's Boundaries rule puts every external dependency behind
a swappable adapter.

pharos's VP9 HLS surface serves audio as a separate rendition
(`EXT-X-MEDIA`), and the light hls.js build drops alternate audio.

## Decision

We will vendor hls.js **1.7.4**, the full `dist/hls.min.js` build, unmodified,
at `crates/pharos-ui/assets/vendor/hls.min.js` with its Apache-2.0 licence
beside it. `pharosUiBundle` installs both under `assets/vendor/`, so the
server serves them from the UI's own origin.

- Source: `https://registry.npmjs.org/hls.js/-/hls.js-1.7.4.tgz`, the tarball
  `compat-playwright/package-lock.json` already pins (integrity
  `sha512-3GlxGdPvT2YHvQYOt+yOzT/DEior/qJVNIylHA77kgeoBTdQQqi70GwefNEUmHQeNGyjC8U62+bruqW/Tpm+tg==`).
- `hls.min.js` sha256:
  `cfc70cc9e9fc63d071919e087670abc760c8fd1bc889b88588026e4d22193bbe`.
- The page adds the `<script>` only when it plays HLS on a browser without
  native HLS but with MSE (`native::playback::choose_engine`). No other page
  fetches it.
- hls.js sits behind `native::playback::PlaybackEngine`, beside the
  `<video>`-only engine. Replacing it is one engine and one file.

To update: download the new version's npm tarball, check it against its
published `integrity`, copy `package/dist/hls.min.js` and `package/LICENSE`,
record the version and both hashes in a new ADR that supersedes this one, and
move `compat-playwright`'s pin in the same change.

## Consequences

- HLS titles play in Firefox and Chromium. The CSP keeps `script-src 'self'`.
- The bundle grows by 624 KB that only the watch page loads. The file is not
  content-hashed, so it revalidates on each load rather than caching forever.
- The file is reviewed by its hash, not its diff (`.gitattributes` marks the
  vendor directory `-diff`). Security fixes upstream reach pharos only when
  someone updates the pin; nothing watches for them.
- The source map hls.js names is not shipped, so devtools reports a missing
  map on the watch page.

## Alternatives considered

- **Load hls.js from a CDN.** Needs a CSP exception for a third-party origin
  and a network dependency at play time.
- **Fetch the tarball in the Nix build (`fetchurl` with the hash).** Keeps the
  repository small, but `dx serve` and anyone building without Nix would have
  no player. Vendoring is one reviewed file.
- **The light build (389 KB).** Drops alternate audio renditions, which the
  VP9 surface uses.
- **Transcode for native HLS only.** Safari is a minority of desktop
  browsers; the rest would get nothing.

## References

- `crates/pharos-ui/src/native/playback.rs`, `crates/pharos-ui/src/native/web.rs`
- ADR-0033 (UI at the root, its CSP), ADR-0034 (media cookie the segment
  fetches carry)
- https://github.com/video-dev/hls.js
