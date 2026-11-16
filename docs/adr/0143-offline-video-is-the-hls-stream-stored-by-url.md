# ADR-0143: An offline video is the player's HLS stream, stored by exact URL

- **Status:** Accepted
- **Date:** 2026-11-16T22:20:00Z
- **Deciders:** Alison Jenkins

## Context

A viewer travelling with poor or no signal wants to download films and
episodes in the web app and watch them in the normal in-page player. The
phone app wants the same, with the code shared.

The web player plays HLS (hls.js by default) from a master URL the server
hands out over RPC. Every playlist and segment URL carries the same
per-session query (`PlaySessionId`, `VideoBitrate` and so on), and the
playlists are full VOD. Sources are often MKV or HEVC, which browsers will not
play as a file. The server already transcodes HLS segments on demand, on the
GPU, and caches them.

ADR-0110/0112 keep books in page-side Cache Storage and keep the service
worker away from per-user data (V1133).

## Decision

We will make a download the player's own HLS stream at one fixed quality:

- The Download control negotiates a stream forced to HLS at a chosen video
  bitrate (1, 2 or 4 Mbps) and records `Decision.stream_url` verbatim.
- `pharos_hls::offline` lists every URL the master leads to (media
  playlists, inits, segments) and estimates the size from `BANDWIDTH` and
  duration. Hosts fetch them one by one and store each response under its
  exact absolute URL, query included. Playlists are not rewritten.
- Playback of a downloaded item skips the server: the player is handed the
  stored master URL, and the host answers those URLs from the store.
- The store sits behind the `VideoStore` port (`pharos-ui`
  `native/kept_video.rs`), so the Download control, the Downloads page and the
  offline player are shared. The web host keeps bytes in Cache Storage
  `pharos-videos-<user>` and a record in `localStorage`; the phone host keeps
  them in its files dir and answers from its asset proxy.
- On the web, the service worker answers a same-origin, non-Range GET under
  `/videos/` from a `pharos-videos-*` cache on an exact URL match, and lets
  everything else through. This is a deliberate exception to V1133 for video
  only: the matched URL carries a per-session id only that user's page was
  given. Books stay page-only.
- The downloads share the 4 GiB default cap and the 2 GiB free-space guard of
  offline music.

## Consequences

- The player, its engine choice and its UI do not change for offline play.
- A download costs the server one transcode of the stream, as watching would;
  segments are fetched in order with low concurrency and retried, since an
  unready segment answers 404 and a full scheduler 500.
- A download is tied to the session id in its URLs; it never needs the server
  again, but a half-finished one must finish within the session's lifetime
  (6 h) or be started again.
- Offline progress is kept on the device and not yet sent to the server.

## Alternatives considered

- **Download the original file and play it with `<video src>`.** Lost: MKV,
  HEVC and many audio codecs do not play in browsers. It ships anyway as a
  plain "Download file" link for an external player (V1456).
- **A new server endpoint that remuxes or transcodes to one MP4.** Lost for
  now: a new long transcode path on a RAM-scarce server, and a progressive
  fragmented MP4 seeks poorly without an index.
- **Rewrite playlists to device-local URLs.** Lost: needs a rewrite per host
  and changes what the player sees; exact-URL keys need neither.

## References

- spec 048 (`specs/048-offline-video/`), V1457–V1459, B1475.
- ADR-0110, ADR-0112 (kept books), V1133.
