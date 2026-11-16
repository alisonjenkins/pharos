# ADR-0145: The phone keeps offline video on disk and serves it from its proxy

- **Status:** Accepted
- **Date:** 2026-11-16T23:30:00Z
- **Deciders:** Alison Jenkins

## Context

ADR-0143 makes an offline video the player's HLS stream stored by exact URL
behind the shared `VideoStore` port, and says the phone keeps it in its files
dir and answers from its asset proxy. The phone's WebView sees only a path and
query (no scheme or host), has no service worker, and loads every server path
through the Rust proxy `use_server_proxy`. RAM on the phone is scarce, and the
app already keeps downloaded music under a configurable cap (default 4 GiB).

## Decision

- `DiskVideoStore` (`pharos-ui` `native/kept_video_disk.rs`) implements
  `VideoStore` for any host with a directory. The phone registers it under
  `<files dir>/video` on the first render of the root component.
- Keys are `path + query` (`request_key`). Each key is a file named by a
  128-bit hash under `<root>/<user>/<item>/files/`, beside `manifest.json`
  (the `KeptVideo` record). A file is written as `.part` then renamed, so an
  existing file is a whole file; a restart fetches `missing_files` only.
- Bodies stream to disk chunk by chunk. The downloads run on one dedicated
  thread (a current-thread tokio runtime), two files at a time.
- The proxy asks `VideoStore::stored_file(key)` before the network and
  answers a hit from disk with the server's `Content-Type`, whether the phone
  is online or not (a stored copy saves mobile data). `Range` is not
  forwarded by the proxy today and the playlists carry no byte ranges, so a
  stored file is answered whole.
- Video shares the music cache's budget: the cap is the music cache's cap,
  and the music's bytes count against it, so one setting bounds what the phone
  keeps. The 2 GiB free-space guard applies, measured with `statvfs` on the
  video directory.
- Signals are log lines (the phone has no Prometheus): `video keep
  start|ready|failed|refused|resume|remove`, and `video request class=…
  verdict=served_offline|network` (playlists at info, segments at debug).

## Consequences

- Video cannot evict music or the reverse: a full shared budget refuses the
  new download and the viewer frees space by removing something.
- Before the media service starts there is no music cache to ask, so the
  default cap applies and the music counts as nothing; a cap raised in
  settings takes effect once the service is up.
- A stored file is read whole into the WebView response (a segment is a few
  MB); only the download path streams.
- A request is answered by key across users on the device; the key carries
  the per-session query only that user's player was given.

## Alternatives considered

- A separate 4 GiB video budget: simpler, but doubles what the phone may fill.
- Serving stored files through a loopback HTTP server: more moving parts and
  a listening socket for what the proxy already does.

## Invariants

V1463 (whole files on disk, proxy answers stored keys), V1464 (shared cap and
free-space guard) and V1465 (every outcome logged, offline start counts
videos) in `specs/001-pharos-baseline/invariants.md` guard this decision.
