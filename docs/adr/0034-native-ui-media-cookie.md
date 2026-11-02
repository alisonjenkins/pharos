# ADR-0034: The native web UI authenticates element fetches with a `pharos_media` cookie

- **Status:** Accepted
- **Date:** 2026-11-02T08:30:00Z
- **Deciders:** Alison Jenkins

## Context

pharos-ui signs in with `POST /pharos/session`, which sets the `pharos_session`
cookie on `Path=/pharos` (ADR-0029). That cookie reaches the `/pharos/rpc`
upgrade and nothing else. The UI shows posters with `<img>` and plays with
`<video>` and hls.js. The image URLs the native API returns
(`/Items/{id}/Images/Primary`) and the stream and HLS URLs a native
`Playback.negotiate` returns carry no token. Those routes require a bearer
(V189), and an element fetch cannot send a header, so every poster and stream
would answer 401 (T145).

## Decision

We will have `POST /pharos/session` also set a `pharos_media` cookie holding
the same token: HttpOnly, SameSite=Strict, Secure, `Path=/`, the same 400-day
lifetime. `DELETE /pharos/session` clears it alongside `pharos_session`.
`extract_media_token` reads it after `JellyfinAuth`, so it has exactly
`JellyfinAuth`'s scope (V191): GET and HEAD on routes that serve item bytes to
element URLs. Every write and every JSON API route still ignores it.

It is a separate cookie, not `JellyfinAuth`, so that jellyfin-web's sign-in
and sign-out on the same origin cannot replace or clear it, and the native
UI's sign-out cannot clear jellyfin-web's.

## Consequences

- Posters, direct play and HLS work in the native UI with no token in a URL
  or in script.
- The native UI's token is exposed exactly as jellyfin-web's is under
  ADR-0031: a page on a sibling subdomain can make the browser fetch item
  bytes as the viewer (it cannot read them back), including starting
  transcode work through an HLS playlist.
- A browser signed in to jellyfin-web and the native UI as different users
  authorises element fetches as the jellyfin-web user, because
  `JellyfinAuth` is read first. Signing out of jellyfin-web hands them to the
  native UI's user.
- T145's signed URLs remain the way to remove the cookie entirely. That needs
  a server key and signing every URL a playlist emits, which is more work
  than the first usable UI should wait for.

## Alternatives considered

- Short-lived signed URLs (T145): item, tag, expiry and an HMAC on every
  image, stream, playlist and segment URL. Narrower, but needs key handling
  and HLS playlist rewriting. Deferred, not rejected.
- Widen `pharos_session` to `Path=/`: one cookie, but the same cookie would
  then authorise both the full native API (through the upgrade) and element
  fetches, and a change to its path changes ADR-0029. Rejected.
- Set `JellyfinAuth` at native sign-in: jellyfin-web's sign-out on the same
  origin would clear it and break the native UI's images mid-session.
  Rejected.
- Put `api_key` in the URLs the native API returns: the token would sit in
  the DOM, in history and in proxy logs. Rejected.

## References

T145, V189, V191, V205 in `specs/001-pharos-baseline/`; ADR-0029, ADR-0031.
