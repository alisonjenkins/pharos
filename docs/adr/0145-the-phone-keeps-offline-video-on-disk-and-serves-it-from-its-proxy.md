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
  `OpenOnce` so the store is opened once per process. The second caller can
  return while the first is still opening the store, so the UI never takes
  one read as final: the app's root waits for the slot to be published
  (`Slot::published`, in a task, never blocking the UI thread) and every
  reader re-renders then (`kept_video_view::shown_video_store`). No
  decision made at mount reads the store once: the slow-connect timer starts
  for every attempt and reads what is kept when it fires.
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
  stored file is answered whole. A stored file is answered with
  `cache-control: no-store`: the WebView's cache keys a response by URL
  alone, not by the signed-in user, so a cached copy could be answered to
  the next user on the phone. The file on disk is already the cache.
- The device token is sent only over https, or to the build's own server
  (`PHAROS_TV_SERVER`) whatever its scheme (`tv_page::token_allowed`).
- A download carries only its owner's token. Each run belongs to the user
  its record was kept for, also when an open restarts it. Every request reads
  the token and its user in one read of the token file, and is sent only
  while that user is the run's owner (`kept_video::token_for_owner`). No
  token, or a token whose user is not stored yet, sends nothing either: the
  check fails closed. A fetched file is promoted only if the owner still
  holds the token, checked under the store's lock. The token file's lock is
  the innermost, taken after the store's. A refused request or promotion
  discards the staged bytes and parks the run: the record and its files stay
  Downloading for the owner (`reason=token_owner_mismatch`).
- A session end parks the ended user's runs before another user's token can
  be used: the session-end hook (`offline_video::forget_user`) calls
  `VideoStore::park_runs`, for every run when whose session it was is not
  known. A sign-in resumes that user's parked downloads
  (`VideoStore::resume_user`). The binding is the guarantee, and the park
  stops the work.
- A staging file that is not promoted is deleted, and its accounting rolled
  back, in `Staged`'s `Drop`, under the shared admission lock and then the
  store's lock. Every path runs it: a named failure, a revoked run and a
  cancelled fetch.
- The proxy's client follows no redirect itself (`Policy::none()`). The
  proxy follows up to 5 hops by hand through `tv_page::redirect_hop`: the
  token is kept only on its own origin (an IPv6 host compared by address,
  whatever its spelling), dropped on another origin, and a
  hop down to http is refused. The download client follows a redirect only
  to the same scheme, host and port.
- Neither client has a fallback. One that cannot be built (its TLS config
  included) is kept as the error (`tv_page::built_client_for`): every
  request through it is refused with the logged reason, never sent through a
  default client with another TLS setup and redirect policy.
- Video shares the music cache's budget: the cap is the music cache's cap,
  and the music's bytes count against it, so one setting bounds what the phone
  keeps. The 2 GiB free-space guard applies, measured with `statvfs` on the
  video directory.
- The device token file keeps the token on line 1 and the signed-in user's
  id on line 2, replaced whole by a temp file and a rename, so an id never
  sits beside another sign-in's token. `pharos_content::token` owns the file
  and its lock: every replacement and read-modify-write, the car's sign-in
  included, goes through it. On a cold start the remembered user
  counts only when it matches the token's user. The offline user is
  forgotten whenever the session leaves Ready for sign-in, by whatever
  event, and by a refused resume.
- Music admission is refused while the video store's size is unknown (a
  folder that cannot be read or created, a leftover staging file, a removal
  in flight), never counted as zero (V1464). A folder held because its
  manifest could not be read is read again when the downloads are listed or
  one is started: a manifest that parses joins the list, a folder that is
  gone is dropped. The Downloads page offers to remove the ones that still
  cannot be read (`VideoStore::discard_unreadable`); a folder that will not
  delete stays counted as a tombstone. Manifest fields that only describe the
  video default, so a field added later does not strand older manifests.
- One lock is shared between the video store and the music cache:
  `pharos_content::admission_guard()`, taken through
  `Budget::admission_guard`. A video admission or start holds it while it
  reads the music figures and reserves its own bytes; the cache holds it
  while a track lands and the cache trims, and while it approves a write.
  The order is fixed: the shared lock first, then the cache's or the
  store's own lock, never the reverse.
- Signals are log lines (the phone has no Prometheus): `video keep
  start|ready|failed|refused|resume|remove`, and `video request class=…
  verdict=served_offline|network` (playlists at info, segments at debug); a
  request with nobody signed in logs `verdict=not_served_offline
  reason=no_signed_in_user` and goes to the network. Held folders log
  `video held folder verdict=released|dropped|tombstoned|still_held|discarded|discard_failed`.

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
