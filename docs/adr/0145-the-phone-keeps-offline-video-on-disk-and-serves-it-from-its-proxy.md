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
  before the network and answers a hit from disk, whether the phone
  is online or not (a stored copy saves mobile data). The hit's
  `Content-Type` is derived from the stored request key's extension
  (`kept_video::content_type_for`, sent by `offline_headers`); the
  server's own header is not stored with the file, so the two can differ
  for an unusual extension. `Range` is not
  forwarded by the proxy today and the playlists carry no byte ranges, so a
  stored file is answered whole. A stored file is answered with
  `cache-control: no-store`: the WebView's cache keys a response by URL
  alone, not by the signed-in user, so a cached copy could be answered to
  the next user on the phone. The file on disk is already the cache.
- The device token is sent only over https, or to the build's own server
  (`PHAROS_TV_SERVER`) whatever its scheme (`tv_page::token_allowed`). A
  build pointed at a plain-http server outside loopback (one on the LAN)
  sends the token to it unencrypted, readable by anyone on that network.
  That is an accepted risk for LAN builds: the build names its server, and
  https is the remedy.
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
- A user is paired with the token only for the token their `me` request was
  built with: the token that authenticated its session (never a fresh read
  of the file, which the media service may have replaced since). The token
  travels with the API as one value, `offline_video::Session`:
  `resume_session` makes it from the token it resumed with, and nothing sets
  or clears that token after. There is no process-wide copy. A re-open is a
  new value, so an in-flight `me` keeps its own session's token, and
  `use_me` takes the session with the identity generation. The resume runs
  behind a platform seam (`Resumer`), so the open, `me`, pairing and
  download chain runs on the host with fake transports. `me_request` takes
  the session's token, and the answer
  pairs through `token::store_user_if`, which compares that token with the
  one on file under the file's lock. A token stored since (another
  account's sign-in) is left unpaired (`verdict=refused
  reason=token_changed`), so a download check that needs its user fails
  closed. A new token is always written with no user. A sign-in resumes
  parked downloads only after the pairing has succeeded.
- `proxy_get` itself drops a token that may not go to its URL
  (`token_allowed`), as well as its caller and each redirect hop.
- A sign-out's token removal is synced to its folder (one retry). A sync
  that still fails, EINVAL or ENOTSUP included, leaves the device signed
  out and is reported (`Forgotten::NotDurable`, logged
  `verdict=signed_out_not_durable`); an unsupported sync is named as such,
  because the removal never reached the disk.
- A session end parks the ended user's runs before another user's token can
  be used: the session-end hook (`offline_video::forget_user`) calls
  `VideoStore::park_runs`, for every run when whose session it was is not
  known. A sign-in resumes that user's parked downloads
  (`VideoStore::resume_user`). The binding is the guarantee, and the park
  stops the work. A parked run stalled mid-body still has its staging file:
  until that writer stops, its bytes count against the cap
  (`State::draining`), and a resumed run waits before it plans.
- The stale share an admitted plan could not prune is a field of its record
  (`KeptVideo::stale_on_disk`, not saved), so a removal or a replacing
  record drops it with the record.
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
  cannot be read (`VideoStore::discard_unreadable`), on a thread of its own
  so the UI keeps running, with the button disabled until it ends; a folder
  that will not delete stays counted as a tombstone. Manifest fields that only describe the
  video default, so a field added later does not strand older manifests.
- One lock is shared between the video store and the music cache:
  `pharos_content::admission_guard()`, taken through
  `Budget::admission_guard`. A video admission or start holds it while it
  reads the music figures and reserves its own bytes; the cache holds it
  while a track lands and the cache trims, and while it approves a write.
  The order is fixed: the shared lock first, then the cache's or the
  store's own lock, never the reverse.
- Accounting that is not ready is a wait, never a failure. The music
  downloader keeps a track's `.part` and retries with backoff while the
  video store's size is unknown (`NoRoom::AccountingUnknown`); a video run
  waits before it plans until the music accounting is ready. Readiness only
  moves from false to true in the app, so neither waits for long.
- Two downloads can store the same request key: the index keeps one copy
  per item folder, and a removal drops only its own.
- A Ready record is published only after the folders above it (the user's,
  the store's and the store's parent) are synced, as well as its own.
- A record is adopted only in the folder its user and item name, at open as
  when a held folder is read again; one elsewhere stays held.
- A sweep that measures a folder again claims it with the folder's epoch;
  its walk lifts the block only if the folder is still at that epoch and
  idle, and is logged `verdict=stale` otherwise.
- A sign-in that lands while a refused run is parking (it still holds its
  id) is kept as a pending resume and started as the run leaves.
- A refused reconnect of a session that was Ready is cleaned up once, by
  the session-end hook, while its user is still known; the connect cleans
  up only a first resume that never reached Ready. A refused resume drops
  the token only while it is the refused one (`token::forget_if`, under the
  file's lock); a replacement is kept and resumed in turn.
- The connect advances the identity generation before it publishes a new
  session (`offline_video::session_replaced`): a `me` asked on the replaced
  session installs nobody and writes no preference.
- A replaced session whose token is not paired with the user known fences
  the identity (`Identity::Fenced`): nothing is served from disk until the
  new session's user is established, and a session end still parks the
  fenced user's runs. Fence, keep and lift are logged `video user fence`.
- Work that one side hands to another is never lost between them. Every
  run start claims its id in the hold that decided it; a run refused its
  owner's token marks itself parking in that same hold, and a sign-in for
  a parking or still active run is kept as a pending resume. A pin folded
  into a running music job is requeued if the job gives up. A sign-in that
  finds no video store yet owes its resume, under one lock, and the store's
  registration replays it while that user and their token still hold.
- Bytes on disk are counted once: a music reservation records the length at
  which it was taken and owes only what has not landed since, and a
  promoted pre-plan playlist leaves the run's owed figure. A speculative
  unknown-length window is checked without eviction. A partial track keeps
  the validator it was fetched under until a new version's part opens. The
  music cap is saved in the admission hold that applies it.
- A recovered run measures its own folder before it plans. A start's
  rollback follows the starts not yet saved. A playlist path that is not a
  regular file fails the download with a reason to remove it. A discard
  removes a held folder whose record names another folder, and never
  reports success for a folder still on disk.
- The Downloads page polls the unreadable count with the videos. The
  discard's thread is made with `thread::Builder`; one that cannot start is
  reported and the button enabled again.
- A registration whose folder could not be created opened nothing and gives
  its claim back, so a later caller tries again; the music hook goes in once
  and stays fail closed. One that reached the store keeps its claim.
- A token the proxy dropped on one redirect stays dropped for the rest of
  the request, back on its own origin too. The library's sign-out logs
  `token forget verdict=signed_out_not_durable` when its removal could not
  be synced, as the phone does, and still signs out in memory.
- Only the authorized offline owner reaches the downloads. A cold start
  installs its remembered user only when no session or identity change
  overtook its reads, and, once a device-token session exists, only the user
  paired with that token. The Downloads page and the local-copy choice use
  that owner, never a pinned account result; a listing read for one user is
  never shown beside another user's Remove.
- A session that ends drops its own token, conditionally, so a replacement
  token is kept (`token forget verdict=forgotten|kept_replacement
  reason=session_ended`). A phone sign-out whose token file could not be
  removed still signs out, says so (`SignOutReply::NotSaved`), and refuses
  that token for the rest of the process. The offline cleanup of a sign-out
  runs once, through the session-end hook.
- A sign-in that finds a start whose record is not saved yet defers its
  resume until the save, which replays it with a run of its own; a rollback
  or removal drops the deferral.
- A removal turns its folder unknown under the music admission lock, and
  the music hook reads the video figures in one snapshot. A settled staging
  file stays reserved through the plan admitted after it. A run that parks
  before its plan gives back the playlist allowances it did not use, the
  bytes on disk still counted.
- On the music side, a pin folded into a job whose track is missing after
  the job is queued again; a replacement body owes only what it has not
  written since its file was emptied; and a body whose advertised length has
  landed is finished, not refused, while the accounting is unknown.
- A registration whose store's runtime or thread could not start gives its
  claim back too (`Registration::NotOpened`). A service-only process reruns
  the registration on each later connect and prefetch until it is done
  (`video accounting retry when=… verdict=…`); music stays refused meanwhile.
- A request key's authority ends at the first `/`, `?` or `#`, so an
  absolute URL with no path keeps its query under `/`. Stored keys are
  unchanged: the server's URLs always have a path.
- A token that a sign-out or session end could not remove is marked
  revoked: a synced `<token file>.revoked` marker names its user and a
  fingerprint of the token, written under the token file's lock. Every read
  of the token file refuses the token the marker names, so nothing resumes
  or downloads with it, here or after a restart; a new sign-in's store
  clears the marker, and a different token is never blocked by it. A token
  that could be neither removed nor marked is reported as such
  (`Ended::Unsafe`, `restart_safety=not_guaranteed`), and the process stays
  signed out, its download credentials included, behind the in-process
  fence until a new sign-in.
- Who the offline owner is lives in one state behind one lock: the
  identity, its generation, the session's token and the cold-start flags.
  A cold start reads that state once, and fills in the user only while it
  is still Unknown at that generation, without advancing it, so a `me`
  asked on the current session still lands. A resumed session is published
  only while the connection machine still accepts that attempt: a sign-out
  that won keeps it out.
- A stored video's HLS master is given to the player through the page's
  own origin, so it plays from the phone's copy, not from the server.
- Lock order on the video side is admission, then the manifest saves, then
  the state. A resumed plan widened by a kept staging file is checked
  against the shared cap as a fresh one; a staging listing that falls short
  keeps the folder unknown; a run's terminal figure counts the staging files
  other runs left; and a stale run gives back no playlist allowance from a
  replacement record. On the music side, a part is discarded or truncated
  under the cache's lock, so a trim never evicts for bytes that went, and a
  part whose length cannot be read is unknown, never 0.
- The TV page compares origins with the URL parser the HTTP client sends
  with, so a token or redirect decision sees the host the request reaches.
- Every sign-out and session end runs in one order: fence first, then tear
  down. Under the identity lock the state becomes signed out and its
  generation advances; then the ended user's runs are parked; then the
  preference is cleared and the session's token removed or marked. Every
  resume, a sign-in's and the replay at the store's publication, starts
  under that same lock and only while the generation it validated at still
  holds (`video keep resume verdict=not_resumed
  reason=identity_changed_since_validated`), so no run starts between the
  park and the token's invalidation.
- A token that a sign-out could neither remove nor mark is not handed to
  the media service as a reload. The phone queues a stay-signed-out call
  with the token's fingerprint instead (`credentials change=kept_unsafe`).
  The service runs in the app's own process, and its library holds the
  fence: every token read refuses that fingerprint (`token read
  verdict=refused reason=service_fence`) until a different token is on
  file. The library's own unsafe sign-out or revocation sets the same fence.
- Signals are log lines (the phone has no Prometheus): `video keep
  start|ready|failed|refused|resume|remove`, and `video request class=…
  verdict=served_offline|network` (playlists at info, segments at debug); a
  request with nobody signed in logs `verdict=not_served_offline
  reason=no_signed_in_user` and goes to the network. Held folders log
  `video held folder verdict=released|dropped|tombstoned|still_held|discarded|discard_failed`.

## Consequences

- A video admission never evicts anything: one that does not fit the
  shared budget is refused, and the viewer frees space by removing
  something.
- Music can make room for video. The music cache's trim (`evict_locked`)
  counts what video holds whenever it runs: when the video hook is
  installed (`Cache::set_outside_used`, also when the cache comes up with a
  pending hook), when the cap changes (`set_cap`), when a music track
  commits (`commit`) and when a container is unpinned (`unpin`). Video
  growth does not start a trim; it is counted at the next one. The trim
  evicts the least recently played finished music that is not pinned,
  queued or playing, and keeps the video. Video is never evicted for music.
- The cache can stay over its cap when all the music left is protected
  (pinned, queued or playing): nothing is evictable, and the cache logs
  `cache over cap ... nothing is evictable`.
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
