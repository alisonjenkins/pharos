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
- Redirects keep the origin: the proxy follows them by hand on its own
  origin, and the download client only to the same origin. A playlist
  redirected to another key is stored with its references rebased onto that
  key, as absolute paths. Neither client falls back to a default one.
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
