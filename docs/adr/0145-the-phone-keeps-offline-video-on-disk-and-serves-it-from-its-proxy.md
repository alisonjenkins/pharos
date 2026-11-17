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
  `<files dir>/video` from both the root component's first render and the
  media service's init hook, each through `register_video_root`, guarded by
  `OpenOnce` so the store is opened once per process.
- Keys are `path + query` (`request_key`). Each key is a file named by a
  128-bit hash under `<root>/<user>/<item>/files/`, beside `manifest.json`
  (the `KeptVideo` record). A file is written as `.part` then renamed, so an
  existing file is a whole file; a restart fetches `missing_files` only.
- Bodies stream to disk chunk by chunk. The downloads run on one dedicated
  thread (a current-thread tokio runtime), two files at a time.
- The proxy asks `VideoStore::stored_file(user, key)` for the signed-in user
  before the network and answers a hit from disk with the server's `Content-Type`, whether the phone
  is online or not (a stored copy saves mobile data). `Range` is not
  forwarded by the proxy today and the playlists carry no byte ranges, so a
  stored file is answered whole.
- Video shares the music cache's budget: the cap is the music cache's cap,
  and the music's bytes count against it, so one setting bounds what the phone
  keeps. The 2 GiB free-space guard applies, measured with `statvfs` on the
  video directory.
- The device token file keeps the token on line 1 and the signed-in user's
  id on line 2, replaced whole by a temp file and a rename, so an id never
  sits beside another sign-in's token. On a cold start the remembered user
  counts only when it matches the token's user. The offline user is
  forgotten whenever the session leaves Ready for sign-in, by whatever
  event, and by a refused resume.
- Music admission is refused while the video store's size is unknown (a
  folder that cannot be read or created, a leftover staging file, a removal
  in flight), never counted as zero (V1464).
- Signals are log lines (the phone has no Prometheus): `video keep
  start|ready|failed|refused|resume|remove`, and `video request class=…
  verdict=served_offline|network` (playlists at info, segments at debug).

## Consequences

- Video cannot evict music or the reverse: a full shared budget refuses the
  new download and the viewer frees space by removing something.
- Before the music cache is enabled there is nothing to ask for the cap or
  the music's bytes, so new video downloads are refused until it is (V1464);
  a cap raised in settings takes effect once the cache is up.
- A stored file is read whole into the WebView response (a segment is a few
  MB); only the download path streams.
- A request is answered from disk only for the signed-in user's own
  downloads (lookup by user and key). When no user is known to be signed in,
  including after a sign-out, nothing is served from disk; the files stay on
  disk for that user's next sign-in.

## Alternatives considered

- A separate 4 GiB video budget: simpler, but doubles what the phone may fill.
- Serving stored files through a loopback HTTP server: more moving parts and
  a listening socket for what the proxy already does.

## Invariants

V1463 (whole files on disk, proxy answers stored keys), V1464 (shared cap and
free-space guard) and V1465 (every outcome logged, offline start counts
videos) in `specs/001-pharos-baseline/invariants.md` guard this decision.
