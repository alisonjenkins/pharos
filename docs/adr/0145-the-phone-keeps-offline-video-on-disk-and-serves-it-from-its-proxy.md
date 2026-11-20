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

- The phone keeps each downloaded video as its HLS files on disk, one file
  per request key (path and query), under `<files dir>/video`, behind the
  shared `VideoStore` port (`DiskVideoStore`, opened once per process). A
  file exists only when whole.
- The Rust asset proxy answers a request from disk, before the network and
  whether the phone is online or not, only for a file of the signed-in
  user's own Ready download. A running, failed or paused download serves
  nothing, and the player is pointed at the page's own origin only for a
  Ready one. Stored answers are never cached by the WebView.
- Downloads run on one thread of their own and stream to disk; only the
  owner's device token goes with a download's requests, and every check
  that cannot be made fails closed.
- The asset proxy follows redirects by hand, one hop at a time
  (`redirect_hop`). It keeps the device token only on the same origin,
  and there only where the token may go at all. A cross-origin https
  redirect (artwork on another host, say) is followed without the token
  (`Hop::FollowWithoutToken`), and an https-to-http downgrade is refused.
  The download client does not go hop by hand: its own reqwest redirect
  policy (`Policy::custom`) follows a redirect only to the same origin, so
  a stored file always comes from the server. A playlist redirected to
  another key under the server's base path is stored with its references
  rebased onto that key, as absolute paths. A download is refused when a
  file it needs falls outside the base path, or outside the routes the
  phone's proxy serves: such a file could not be played offline. Neither
  client falls back to a default one.
- The device token goes only over https or to the build's own server. A
  LAN build on plain http sends it unencrypted: an accepted risk, with
  https as the remedy.
- Video shares the music cache's cap and the 2 GiB free-space guard, under
  one admission lock shared with the music cache. Accounting that cannot
  be read is a wait or a refusal, never a zero.
- `pharos_content::token` owns the device token file. A token that a
  sign-out or session end could not remove is marked revoked on disk, and
  one that could be neither removed nor marked is refused for the rest of
  the process, in the media service's library too.
- The token file holds the token alone, exactly as the build before this
  ADR writes and reads it, so a rollback to that build reads a usable
  token. The user the token was issued to lives in a pairing file beside
  it (`<token path>.user`): the user id and the token's fingerprint,
  written under the same lock after the token. A pairing counts only
  while its fingerprint matches the token on file, so a token whose
  pairing is missing or stale is unpaired and fails closed. A token file
  in this build's earlier two-line layout is split on read.
- Revocation markers are keyed by token fingerprint, one file per token,
  and a store never touches a marker naming another token. It clears
  the new token's own marker durably before writing anything, and
  refuses the sign-in with nothing written when that fails. A marker is
  never unlinked: a clearing first writes a guard, a copy of the marker
  that a reader takes as the marker, and syncs the folder; then renames
  a fsynced temp file holding `cleared` over the marker and syncs the
  folder; and only then removes the guard. A reader takes a cleared
  marker with no guard as none, so no failure before the guard goes
  leaves the token unguarded, and a crash after it can only bring the
  guard back. A folder that can never be synced
  (EINVAL or ENOTSUP on both attempts) is accepted and logged as not
  durable, since refusing would make sign-in impossible there.
- Who may reach the downloads is one offline-owner state. A session end
  (the server refused the session, or it ended) fences that state first
  and tears down after: it parks the user's runs, then clears the token.
  An explicit sign-out on Android runs in another order. `app.rs` calls
  `platform::sign_out()`, which removes the token through
  `forget_token()`. Only once the machine applies the reply does
  `offline_video::session_ended` fence the owner and park the runs. Every
  resume starts under the same fence.
- Signals are log lines: the phone has no Prometheus.

The mechanics that carry these decisions out (lock orders, generation
rules, the log lines and their fields) are in V1464 in
`specs/001-pharos-baseline/invariants.md`, under "ADR-0145 mechanics".

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
  Once no such track is left, the trim and music's low-disk admission
  evict retained music parts no job holds and no pin wants, oldest first
  (`verdict=evicted kind=part`); the downloader then drops the state it
  kept to resume them, so the track starts again from zero.
- A music job with a retained part, resumable or not, passes the low-disk
  preflight on the part's bytes, so its response is inspected before any
  byte is written: a 206 goes through the normal admission, a 200 through
  the replacement one with the part counted as room. A pin refused for low
  disk, at the preflight or at either admission, is parked with its part
  and a backoff, and the downloader's one worker moves on; a reconnect
  ends the backoff.
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
